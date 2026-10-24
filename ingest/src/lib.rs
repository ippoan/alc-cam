//! カメラの SD 録画から、指定した時間帯を 1 本の MP4 にして R2 に置く Worker。
//!
//!   auth-worker ─Service Binding─▶ この Worker ─VPC Service (カメラごとの CAM_*, Tunnel)─▶ カメラ
//!                                       │ 重なる録画 (約 4 分ごと) を R2 にそろえ、moov を読み、
//!                                       │ 範囲を切ってつなぐ (再エンコードなし。crates/cam-mp4)
//!                                       └▶ R2 `clips/<tenant>/<camera_id>/<開始>_<終了>.mp4`
//!
//! 口 (path は `PUBLIC_PATH_PREFIX` = `/api/cam-clips` を剥がした後。中身は api/ の `cam_ingest_api`):
//! - `GET /cameras`・`POST /cameras`・`PATCH /cameras/{id}` — カメラの台帳 (alc_api.cameras)。運行管理者席の鍵は `GET /cameras` だけ
//!   (応答は `id`・`name`・`device_ids`)。宛先 (`vpc_binding`、
//!   VPC Service の binding 名。DB の列は `rec_ip`) を書けるのは `X-User-Developer: 1` (開発者の Google ログイン) のときだけ
//! - `GET /cameras/mine` — 拠点 PC の配信の鍵 (`X-Device-Role: device-cam-host`) だけ。鍵の端末 (`X-Device-Id`) が映すカメラの
//!   RTSP の接続先を、パスワードを復号して返す (鍵は Secrets Store の `SSO_ENCRYPTION_KEY`)
//! - `GET /device-candidates?days=30` — 映している端末を選ぶための、直近のイベントの端末の一覧
//! - `POST /clips` に `{"camera_id": "...", "from": "...", "to": "..."}` (ISO 8601。時差が無ければ JST)
//! - `GET /clips/<tenant>/<camera_id>/<開始>_<終了>.mp4` で取り出す (Range 対応 = シークできる)
//! - `GET /events?camera_id=<uuid>&date=YYYY-MM-DD` — そのカメラの JST の 1 日のイベントと、切り出し済みならその clip
//!
//! cron (wrangler.toml の `[triggers]`、1 時間ごと): 結び付けた端末のイベントの前後 10 秒を自動で切り出す (src/auto.rs)
//!
//! 外から直接届く口は持たない (workers_dev / route なし)。auth-worker の振り分け表 (`ALC_BINDING_ROUTES`)
//! から Service Binding で届き、JWT の検証は auth-worker が済ませて `X-Tenant-ID` と、画面用の proxy なら
//! `X-User-Role` / `X-User-Developer`、端末用の proxy (運行管理者席・拠点 PC の配信の鍵) なら `X-Device-Role` (と `X-Device-Id`) を付け直す (alc 系の分割 Worker と同じ)。
//!
//! カメラの資格情報は応答にもログにも出さない (宛先の IP は VPC Service の側に在り、Worker は持たない)。例外は拠点 PC の
//! 配信の鍵への `GET /cameras/mine` だけで、担当カメラの RTSP のパスワードを復号して返す。

// worker 本体は wasm32 専用。native の `cargo test` / `clippy` (workspace) では空の crate にして、api/ だけを検査させる
#![cfg(target_arch = "wasm32")]

mod auto;
mod camera;
mod clip;
mod db;
mod raw;

use cam_ingest_api::dest::VpcBinding;
use cam_ingest_api::pg::{self, ClipRecord};
use cam_ingest_api::{clip_r2_key, raw_r2_prefix, ApiError, Caller, Op, Reply};
use cam_sd::jst_iso;
use camera::Camera;
use uuid::Uuid;
use worker::{console_error, event, Context, Env, Headers, Method, Range, Request, Response, Result, ScheduleContext, ScheduledEvent};

/// 暗号化の鍵 (Secrets Store。auth-worker・alc-lineworks と同じ secret)
const SSO_ENCRYPTION_KEY_BINDING: &str = "SSO_ENCRYPTION_KEY";

fn json(status: u16, body: &serde_json::Value) -> Result<Response> {
    Ok(Response::from_json(body)?.with_status(status))
}

fn fail(e: &ApiError) -> Result<Response> {
    json(e.status, &e.body())
}

/// Secrets Store の鍵。取れなければ 500
async fn sso_key(env: &Env) -> std::result::Result<String, ApiError> {
    let value = match env.secret_store(SSO_ENCRYPTION_KEY_BINDING) {
        Ok(store) => store.get().await.ok().flatten(),
        Err(_) => None,
    };
    value.ok_or_else(|| {
        console_error!("alc-cam: no {SSO_ENCRYPTION_KEY_BINDING} value");
        ApiError::new(500, "暗号化の鍵がありません")
    })
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let var = |k: &str, d: &str| env.var(k).map(|v| v.to_string()).unwrap_or_else(|_| d.to_string());
    let prefix = var("PUBLIC_PATH_PREFIX", "");
    let max_clip_ms = var("MAX_CLIP_SECS", "7200").parse::<i64>().unwrap_or(7200) * 1000;
    let full = req.path();
    // auth-worker からは振り分け表の path (/api/cam-clips/...) のまま届く
    let path = if prefix.is_empty() { full.as_str() } else { full.strip_prefix(prefix.as_str()).unwrap_or(&full) }.to_string();
    if path == "/health" {
        return Response::ok("ok");
    }
    // auth-worker が付け直したヘッダだけを信じる (この Worker に外から直接届く口は無い)
    let caller = match cam_ingest_api::caller(|k| req.headers().get(k).ok().flatten()) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let method = req.method();
    let query = req.url()?.query().unwrap_or_default().to_string();
    let body = if matches!(method, Method::Post | Method::Patch) { req.text().await? } else { String::new() };
    let op = match cam_ingest_api::parse(method.as_ref(), &path, &query, &body, &caller, max_clip_ms) {
        Ok(op) => op,
        Err(e) => return fail(&e),
    };
    let result = match op {
        Op::GetClip { key } => return get_clip(&env, &req, key).await,
        Op::CreateClip { camera_id, from_ms, to_ms } => create_clip(&env, &caller, camera_id, from_ms, to_ms).await,
        Op::AutoRun { camera_id } => auto_run(&env, &caller, camera_id).await,
        op => run(&env, &caller, op).await,
    };
    match result {
        Ok(r) => json(r.status, &r.body),
        Err(e) => fail(&e),
    }
}

/// 1 時間ごとの自動切り出し (wrangler.toml の `[triggers]`、本番だけ)。中身は src/auto.rs
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    auto::run(&env).await;
}

/// 手動の自動切り出し (`POST /auto-run`)。cron と同じ [`auto::run_tenant`] を、呼び手のテナント
/// (auth-worker が付け直した X-Tenant-ID) だけに使う。他のテナントには触れない
async fn auto_run(env: &Env, caller: &Caller, camera_id: Option<Uuid>) -> std::result::Result<Reply, ApiError> {
    let mut pg = db::connect(env).await?;
    let mut key = None;
    let runs = auto::run_tenant(env, &mut pg, caller.tenant, camera_id, &mut key, auto::max_groups(env), "manual")
        .await
        .map_err(|e| db_error(&e))?;
    if camera_id.is_some() && runs.is_empty() {
        return Err(ApiError::new(404, "そのカメラはありません"));
    }
    let cameras: Vec<_> = runs.iter().map(|r| cam_ingest_api::auto::camera_run_json(r.camera_id, &r.result)).collect();
    Ok(Reply { status: 200, body: serde_json::json!({ "cameras": cameras }) })
}

/// 台帳と端末の候補の口 (鍵は DB の transaction の外で先に引く)。書く binding がこの Worker に無ければ 400
async fn run(env: &Env, caller: &Caller, op: Op) -> std::result::Result<Reply, ApiError> {
    if op.vpc_binding().is_some_and(|b| !camera::has_binding(env, b)) {
        return Err(ApiError::new(400, "その VPC binding はこの Worker に無い (wrangler.toml の [[vpc_services]] に足して deploy してください)"));
    }
    let key = if op.needs_key() { Some(sso_key(env).await?) } else { None };
    let mut pg = db::connect(env).await?;
    cam_ingest_api::execute(op, caller, &mut pg, key.as_deref()).await
}

fn db_error(e: &tokio_postgres::Error) -> ApiError {
    ApiError::new(500, format!("DB の処理に失敗しました ({})", alc_worker_db::kind(e)))
}

/// 手動の切り出し。カメラの binding 名・資格情報は DB から読み、binding 名は fetch の直前にもう一度検査する。
/// 結果 (ready / failed) は cam_clips に残す (カメラ・R2 の await は transaction の外)
async fn create_clip(env: &Env, caller: &Caller, camera_id: Uuid, from_ms: i64, to_ms: i64) -> std::result::Result<Reply, ApiError> {
    let tenant = caller.tenant;
    let mut pg = db::connect(env).await?;
    let Some(src) = pg::clip_source(&mut pg, tenant, camera_id).await.map_err(|e| db_error(&e))? else {
        return Err(ApiError::new(404, "そのカメラはありません"));
    };
    let Some(name) = src.vpc_binding else {
        return Err(ApiError::new(409, "このカメラは録画を取り込む設定になっていません"));
    };
    // 登録の時に検査した値でも、ここでもう一度通す。落ちたら fetch しない
    let Some(binding) = VpcBinding::parse(&name) else {
        console_error!("alc-cam: camera {camera_id} has an invalid vpc binding");
        return Err(ApiError::new(500, "カメラの VPC binding の設定が正しくありません"));
    };
    // 登録の後に wrangler.toml から binding を外した場合
    if !camera::has_binding(env, binding.as_str()) {
        console_error!("alc-cam: camera {camera_id}: vpc binding {} is not bound", binding.as_str());
        return Err(ApiError::new(500, "その VPC binding はこの Worker に無い"));
    }
    let (Some(user), Some(enc)) = (src.rec_username, src.rec_password_encrypted) else {
        return Err(ApiError::new(409, "カメラのユーザー名・パスワードが設定されていません"));
    };
    let key = sso_key(env).await?;
    let pass = alc_worker_crypto::secret::decrypt_secret(&enc, &key).map_err(|e| {
        console_error!("alc-cam: decrypt {e}");
        ApiError::new(500, "カメラのパスワードを復号できません")
    })?;

    let r2_key = clip_r2_key(tenant, camera_id, from_ms, to_ms);
    let made: std::result::Result<clip::Made, clip::Fail> = async {
        let mut cam = Camera::new(env, &binding, user, pass)?;
        let bucket = env.bucket("RECORDINGS")?;
        let sd_root = cam.sd_root().await?;
        let target = clip::Target { sd_root, key: r2_key.clone(), raw_prefix: raw_r2_prefix(tenant, camera_id) };
        clip::make(&target, &mut cam, &bucket, from_ms, to_ms).await
    }
    .await;
    let made = made.map_err(|clip::Fail(status, message)| ApiError::new(status, message));

    let record = match &made {
        Ok(m) => ClipRecord { camera_id, from_ms, to_ms, r2_key: r2_key.clone(), size_bytes: i64::try_from(m.size).ok(), status: "ready", error: None },
        Err(e) => ClipRecord { camera_id, from_ms, to_ms, r2_key: r2_key.clone(), size_bytes: None, status: "failed", error: Some(e.message.clone()) },
    };
    let clip_id = pg::record_clip(&mut pg, tenant, record).await.map_err(|e| db_error(&e))?;
    let m = made?;
    Ok(Reply {
        status: if m.reused { 200 } else { 201 },
        body: serde_json::json!({
            "clip_id": clip_id,
            "camera_id": camera_id,
            "key": m.key,
            "size": m.size,
            "from": jst_iso(from_ms),
            "to": jst_iso(to_ms),
            "actual_from": jst_iso(m.actual_from_ms),
            "actual_to": jst_iso(m.actual_to_ms),
            "sources": m.sources,
            "reused": m.reused,
            "truncated": cam_ingest_api::truncated(from_ms, to_ms, m.actual_from_ms, m.actual_to_ms),
        }),
    })
}

/// `GET /clips/<tenant>/<camera_id>/<開始>_<終了>.mp4` (キーの頭が呼び手のテナントであることは api が確かめた)。
/// Range (`bytes=a-b` / `bytes=a-`) に答える
async fn get_clip(env: &Env, req: &Request, key: String) -> Result<Response> {
    let bucket = env.bucket("RECORDINGS")?;
    let Some(head) = bucket.head(key.clone()).await? else { return fail(&ApiError::new(404, "その切り出しはありません")) };
    let size = head.size();
    let range = req.headers().get("Range")?.and_then(|r| parse_range(&r, size));
    let mut get = bucket.get(key);
    if let Some((start, end)) = range {
        get = get.range(Range::OffsetWithLength { offset: start, length: end - start + 1 });
    }
    let obj = get.execute().await?.ok_or("その切り出しはありません")?;
    let headers = Headers::new();
    headers.set("Content-Type", "video/mp4")?;
    headers.set("Accept-Ranges", "bytes")?;
    let (status, len) = match range {
        Some((start, end)) => {
            headers.set("Content-Range", &format!("bytes {start}-{end}/{size}"))?;
            (206, end - start + 1)
        }
        None => (200, size),
    };
    headers.set("Content-Length", &len.to_string())?;
    // R2 の本文を JS の stream のまま渡す (Rust を通すと CPU を使ううえ Content-Length が落ちて chunked になる)
    let body = obj.body().ok_or("本文が読めません")?.response_body()?;
    Ok(Response::from_body(body)?.with_status(status).with_headers(headers))
}

/// `bytes=a-b` / `bytes=a-` を (先頭, 末尾) に。外れたものは None (全体を返す)
fn parse_range(h: &str, size: u64) -> Option<(u64, u64)> {
    let (a, b) = h.strip_prefix("bytes=")?.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let last = size.checked_sub(1)?;
    let end = if b.is_empty() { last } else { b.parse::<u64>().ok()?.min(last) };
    (start <= end).then_some((start, end))
}
