//! ingest (`alc-cam-sd-ingest`) の口の中身。fetch・R2・Secrets Store を持たない部分をここに集め、
//! native のテスト (単体と tests/ の実 DB) で確かめる。直下の Worker は workers-rs に載せるだけ。
//!
//! 1 リクエストの流れ (Worker の `fetch`):
//! 1. [`caller`] — auth-worker が付け直したヘッダから呼び手を決める (管理者のログイン、または運行管理者席の鍵・tenant が UUID)
//! 2. [`parse`] — method・path・本文から [`Op`] を作る。入力の検査と、宛先 (`vpc_binding`) の形の検査 ([`dest`])、
//!    「宛先を書けるのは developer だけ」の検査 (POST の分) はここで終わる (DB に触らない)
//! 3. [`execute`] — カメラの台帳と端末の候補の口を DB で実行する (PATCH の「宛先を変えるか」は今の値と比べる)。
//!    切り出し (`CreateClip` / `GetClip`) は Worker が [`pg`] の関数とカメラ・R2 を組み合わせて行う
//!
//! 宛先 (`vpc_binding`) は VPC Service の binding 名 (宛先の IP は Cloudflare 側に在り、Worker は持たない)。DB では
//! `cameras.rec_ip` 列に入る ([`pg`])。その binding がこの Worker に在るかは Worker が確かめる ([`Op::vpc_binding`])。
//! 応答・ログに資格情報を出さない。一覧の応答は `vpc_binding` を返すが、パスワードは返さない。
//! 運行管理者席の鍵 ([`Role::TenkoManager`]) に許すのは一覧 (`GET /cameras`) だけで、応答は `id`・`name`・`device_ids` に絞る。

pub mod auto;
pub mod dest;
pub mod pg;

use alc_worker_db::PgClient;
use cam_sd::{jst_compact, jst_iso, parse_listing, parse_time, safe_segment};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::dest::VpcBinding;
use crate::pg::{CameraRow, Change, NewCamera, PatchCamera, UpdateOutcome};

/// 端末の候補を集計する日数の既定と上限
pub const DEFAULT_DAYS: i32 = 30;
pub const MAX_DAYS: i32 = 90;
/// 結び付ける端末の数の上限・端末 id の長さの上限
const MAX_DEVICES: usize = 50;
const MAX_DEVICE_ID_LEN: usize = 200;
const MAX_NAME_CHARS: usize = 100;
const MAX_USERNAME_CHARS: usize = 100;
const MAX_PASSWORD_CHARS: usize = 200;

/// 利用者に返す失敗。文言は固定の語で、宛先・資格情報・SQL の文を入れない
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn body(&self) -> Value {
        json!({ "error": self.message })
    }
}

/// 成功の応答 (status と JSON)
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

/// 呼び手の種類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 管理者のログイン (auth-worker の画面用の proxy が付ける `X-User-Role: admin`)。全部の口を使える
    Admin,
    /// 運行管理者席の鍵 (auth-worker の端末用の proxy が付ける `X-Device-Role: device-tenko-manager`)。
    /// 遠隔点呼の画面が乗務員の端末からカメラを引くための一覧 (`GET /cameras`) だけを使える
    TenkoManager,
}

/// 運行管理者席の鍵の role (auth-worker `DEVICE_ROLE_TENKO_MANAGER`)
pub const TENKO_MANAGER_DEVICE_ROLE: &str = "device-tenko-manager";

/// auth-worker が付け直したヘッダから決めた呼び手
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub tenant: Uuid,
    /// 開発者の Google ログイン (`X-User-Developer: 1`)。宛先 (`vpc_binding`) を書けるのはこのときだけ (管理者に限る)
    pub developer: bool,
    pub role: Role,
}

/// ヘッダから呼び手を決める。`header` は名前で引いて、無ければ `None`。
/// - `X-User-Role: admin` だけ → 管理者。`X-Device-Role: device-tenko-manager` だけ → 運行管理者席。
///   それ以外 (どちらも無い・他の値・両方ある) → 403。auth-worker はどちらか片方しか付けないので、両方は拒否側に倒す
/// - `X-Tenant-ID` が無い (空を含む) → 401、UUID でない → 400
pub fn caller(header: impl Fn(&str) -> Option<String>) -> Result<Caller, ApiError> {
    let role = match (header("X-User-Role"), header("X-Device-Role")) {
        (Some(u), None) if u == "admin" => Role::Admin,
        (None, Some(d)) if d == TENKO_MANAGER_DEVICE_ROLE => Role::TenkoManager,
        _ => return Err(ApiError::new(403, "管理者だけが使えます")),
    };
    let tenant = header("X-Tenant-ID")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::new(401, "テナントがありません"))?;
    let tenant = Uuid::parse_str(&tenant)
        .map_err(|_| ApiError::new(400, "テナントの形が正しくありません"))?;
    Ok(Caller {
        tenant,
        // 端末の経路で X-User-Developer は付かないが、付いていても席の鍵には宛先を書かせない
        developer: role == Role::Admin && header("X-User-Developer").as_deref() == Some("1"),
        role,
    })
}

/// 口ごとの仕事 (入力は検査済み)
pub enum Op {
    ListCameras,
    CreateCamera(CameraInput),
    UpdateCamera(Uuid, CameraPatchInput),
    DeviceCandidates {
        days: i32,
    },
    CreateClip {
        camera_id: Uuid,
        from_ms: i64,
        to_ms: i64,
    },
    /// R2 のキー (`clips/<tenant>/<camera_id>/<開始>_<終了>.mp4`)。頭のテナントは呼び手のもの
    GetClip {
        key: String,
    },
    /// カメラのその日 (JST) のイベントと切り出し。`[from, to)` は UNIX ms
    Events {
        camera_id: Uuid,
        from_ms: i64,
        to_ms: i64,
    },
    /// cron と同じ自動切り出しを呼び手のテナントだけで回す。`camera_id` があればその 1 台だけ
    AutoRun {
        camera_id: Option<Uuid>,
    },
}

/// `POST /cameras` の入力 (パスワードは平文のまま。暗号化は [`execute`] が鍵を受けて行う)
pub struct CameraInput {
    name: String,
    vpc_binding: Option<VpcBinding>,
    rec_username: Option<String>,
    rec_password: Option<String>,
    device_ids: Vec<String>,
}

/// `PATCH /cameras/{id}` の入力 (省略 = 変えない)
pub struct CameraPatchInput {
    name: Change<String>,
    vpc_binding: Change<Option<VpcBinding>>,
    rec_username: Change<Option<String>>,
    rec_password: Change<Option<String>>,
    device_ids: Option<Vec<String>>,
}

impl Op {
    /// Secrets Store の鍵 (`SSO_ENCRYPTION_KEY`) が要るか (パスワードを書くときだけ)
    pub fn needs_key(&self) -> bool {
        match self {
            Op::CreateCamera(c) => c.rec_password.is_some(),
            Op::UpdateCamera(_, p) => matches!(p.rec_password, Change::Set(Some(_))),
            _ => false,
        }
    }

    /// 書こうとしている binding 名 (null 化は除く)。Worker はこれが env に在るかを [`execute`] の前に確かめ、
    /// 無ければ 400 にする (この crate は env を持たない)
    pub fn vpc_binding(&self) -> Option<&str> {
        match self {
            Op::CreateCamera(c) => c.vpc_binding.as_ref(),
            Op::UpdateCamera(_, p) => p.vpc_binding.value().and_then(Option::as_ref),
            _ => None,
        }
        .map(VpcBinding::as_str)
    }
}

/// 「キーが在る (null を含む)」と「キーが無い」を分けて読む (`Option<Option<T>>`)
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraBody {
    #[serde(default, deserialize_with = "present")]
    name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    vpc_binding: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    rec_username: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    rec_password: Option<Option<String>>,
    #[serde(default)]
    device_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ClipBody {
    camera_id: String,
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct AutoRunBody {
    camera_id: Option<String>,
}

/// `POST /auto-run`。本文は空か `{"camera_id": "<uuid>"}` (省略・null = テナントの全カメラ)
fn auto_run_input(body: &str) -> Result<Op, ApiError> {
    if body.trim().is_empty() {
        return Ok(Op::AutoRun { camera_id: None });
    }
    let b: AutoRunBody = serde_json::from_str(body)
        .map_err(|_| bad("本文は空か {\"camera_id\": \"...\"} の JSON にしてください"))?;
    let camera_id = b
        .camera_id
        .map(|v| Uuid::parse_str(&v).map_err(|_| bad("camera_id の形が正しくありません")))
        .transpose()?;
    Ok(Op::AutoRun { camera_id })
}

fn bad(message: &str) -> ApiError {
    ApiError::new(400, message)
}

fn name_of(v: Option<String>) -> Result<String, ApiError> {
    let n = v.map(|s| s.trim().to_owned()).unwrap_or_default();
    if n.is_empty() || n.chars().count() > MAX_NAME_CHARS {
        return Err(bad("name は 1〜100 文字にしてください"));
    }
    Ok(n)
}

/// 宛先 (VPC Service の binding 名)。null は「取り込まない」
fn vpc_binding_of(v: Option<String>) -> Result<Option<VpcBinding>, ApiError> {
    v.map(|s| VpcBinding::parse(&s).ok_or_else(|| bad("vpc_binding は CAM_ と大文字英数・_ の名前 (64 文字まで) にしてください"))).transpose()
}

/// ユーザー名。null と空は「無し」
fn username_of(v: Option<String>) -> Result<Option<String>, ApiError> {
    let u = v.filter(|s| !s.is_empty());
    if u.as_ref()
        .is_some_and(|s| s.chars().count() > MAX_USERNAME_CHARS || s.contains(':'))
    {
        return Err(bad(
            "rec_username は 100 文字までで、: を含めないでください",
        ));
    }
    Ok(u)
}

fn password_ok(p: &str) -> Result<(), ApiError> {
    if p.chars().count() > MAX_PASSWORD_CHARS {
        return Err(bad("rec_password は 200 文字までにしてください"));
    }
    Ok(())
}

/// 端末 id の並び (前後の空白を落とし、重複を除いて並べる)
fn device_ids_of(v: Vec<String>) -> Result<Vec<String>, ApiError> {
    let mut ids: Vec<String> = v.into_iter().map(|s| s.trim().to_owned()).collect();
    if ids
        .iter()
        .any(|s| s.is_empty() || s.len() > MAX_DEVICE_ID_LEN)
    {
        return Err(bad("device_ids の要素は 1〜200 文字にしてください"));
    }
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_DEVICES {
        return Err(bad("device_ids は 50 件までにしてください"));
    }
    Ok(ids)
}

fn camera_input(body: &str, caller: &Caller) -> Result<CameraInput, ApiError> {
    let b: CameraBody = serde_json::from_str(body).map_err(|_| bad("本文は {\"name\", \"vpc_binding\", \"rec_username\", \"rec_password\", \"device_ids\"} の JSON にしてください"))?;
    let vpc_binding = vpc_binding_of(b.vpc_binding.flatten())?;
    if vpc_binding.is_some() && !caller.developer {
        return Err(ApiError::new(
            403,
            "宛先 (vpc_binding) を書けるのはシステム管理者だけです",
        ));
    }
    let rec_password = b.rec_password.flatten().filter(|p| !p.is_empty());
    if let Some(p) = &rec_password {
        password_ok(p)?;
    }
    Ok(CameraInput {
        name: name_of(b.name.flatten())?,
        vpc_binding,
        rec_username: username_of(b.rec_username.flatten())?,
        rec_password,
        device_ids: device_ids_of(b.device_ids.unwrap_or_default())?,
    })
}

fn camera_patch(body: &str) -> Result<CameraPatchInput, ApiError> {
    let b: CameraBody = serde_json::from_str(body).map_err(|_| bad("本文は {\"name\", \"vpc_binding\", \"rec_username\", \"rec_password\", \"device_ids\"} の JSON にしてください"))?;
    let name = match b.name {
        None => Change::Keep,
        Some(v) => Change::Set(name_of(v)?),
    };
    let vpc_binding = match b.vpc_binding {
        None => Change::Keep,
        Some(v) => Change::Set(vpc_binding_of(v)?),
    };
    let rec_username = match b.rec_username {
        None => Change::Keep,
        Some(v) => Change::Set(username_of(v)?),
    };
    // 省略と "" は変えない、null は消す
    let rec_password = match b.rec_password {
        None => Change::Keep,
        Some(Some(p)) if p.is_empty() => Change::Keep,
        Some(Some(p)) => {
            password_ok(&p)?;
            Change::Set(Some(p))
        }
        Some(None) => Change::Set(None),
    };
    Ok(CameraPatchInput {
        name,
        vpc_binding,
        rec_username,
        rec_password,
        device_ids: b.device_ids.map(device_ids_of).transpose()?,
    })
}

fn days_of(query: &str) -> Result<i32, ApiError> {
    let raw = query.split('&').find_map(|kv| kv.strip_prefix("days="));
    match raw {
        None => Ok(DEFAULT_DAYS),
        Some(v) => v
            .parse::<i32>()
            .ok()
            .filter(|d| (1..=MAX_DAYS).contains(d))
            .ok_or_else(|| bad("days は 1〜90 にしてください")),
    }
}

fn clip_input(body: &str, max_clip_ms: i64) -> Result<Op, ApiError> {
    let b: ClipBody = serde_json::from_str(body).map_err(|_| bad("本文は {\"camera_id\": \"...\", \"from\": \"...\", \"to\": \"...\"} の JSON にしてください"))?;
    let camera_id =
        Uuid::parse_str(&b.camera_id).map_err(|_| bad("camera_id の形が正しくありません"))?;
    let (Some(from_ms), Some(to_ms)) = (parse_time(&b.from), parse_time(&b.to)) else {
        return Err(bad(
            "時刻は 2026-10-23T12:58:00+09:00 の形にしてください (時差が無ければ JST)",
        ));
    };
    if from_ms >= to_ms {
        return Err(bad("to は from より後にしてください"));
    }
    if to_ms - from_ms > max_clip_ms {
        return Err(bad(&format!(
            "一度に切り出せるのは {} 分までです",
            max_clip_ms / 60_000
        )));
    }
    Ok(Op::CreateClip {
        camera_id,
        from_ms,
        to_ms,
    })
}

/// `GET /events?camera_id=<uuid>&date=YYYY-MM-DD` (JST の 1 日)
fn events_input(query: &str) -> Result<Op, ApiError> {
    let get = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('=')));
    let camera_id = get("camera_id")
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or_else(|| bad("camera_id の形が正しくありません"))?;
    let from_ms = get("date")
        .filter(|d| d.len() == 10)
        .and_then(|d| parse_time(&format!("{d}T00:00:00")))
        .ok_or_else(|| bad("date は 2026-10-23 の形にしてください (JST の 1 日)"))?;
    Ok(Op::Events {
        camera_id,
        from_ms,
        to_ms: from_ms + 86_400_000,
    })
}

/// 手動の切り出しが要求より短い範囲しか取れなかったか (頭が遅い・終わりが早い)。キーフレームに寄って
/// 頭が前に出るのは短縮ではない
pub fn truncated(from_ms: i64, to_ms: i64, actual_from_ms: i64, actual_to_ms: i64) -> bool {
    actual_from_ms > from_ms || actual_to_ms < to_ms
}

/// `GET /clips/<tenant>/<camera_id>/<開始>_<終了>.mp4`。頭が呼び手のテナントでなければ 404
fn clip_key(rest: &str, caller: &Caller) -> Result<String, ApiError> {
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 3 || !parts.iter().all(|p| safe_segment(p)) {
        return Err(ApiError::new(404, "その切り出しはありません"));
    }
    if parts[0] != caller.tenant.to_string() {
        return Err(ApiError::new(404, "その切り出しはありません"));
    }
    Ok(format!("clips/{rest}"))
}

/// method・path (`PUBLIC_PATH_PREFIX` を剥がした後)・query (`?` の後)・本文から仕事を決める。DB には触らない。
/// 運行管理者席の鍵は `GET /cameras` 以外を 403 にする (無い口も 404 でなく 403)
pub fn parse(
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    caller: &Caller,
    max_clip_ms: i64,
) -> Result<Op, ApiError> {
    if caller.role == Role::TenkoManager && (method, path) != ("GET", "/cameras") {
        return Err(ApiError::new(403, "運行管理者席はカメラの一覧だけを使えます"));
    }
    match (method, path) {
        ("GET", "/cameras") => Ok(Op::ListCameras),
        ("POST", "/cameras") => Ok(Op::CreateCamera(camera_input(body, caller)?)),
        ("PATCH", p) if p.starts_with("/cameras/") => {
            let id = Uuid::parse_str(&p["/cameras/".len()..])
                .map_err(|_| ApiError::new(404, "そのカメラはありません"))?;
            Ok(Op::UpdateCamera(id, camera_patch(body)?))
        }
        ("GET", "/device-candidates") => Ok(Op::DeviceCandidates {
            days: days_of(query)?,
        }),
        ("POST", "/clips") => clip_input(body, max_clip_ms),
        ("POST", "/auto-run") => auto_run_input(body),
        ("GET", "/events") => events_input(query),
        ("GET", p) if p.starts_with("/clips/") => Ok(Op::GetClip {
            key: clip_key(&p["/clips/".len()..], caller)?,
        }),
        _ => Err(ApiError::new(404, "Not Found")),
    }
}

/// 切り出しの R2 のキー
pub fn clip_r2_key(tenant: Uuid, camera_id: Uuid, from_ms: i64, to_ms: i64) -> String {
    format!(
        "clips/{tenant}/{camera_id}/{}_{}.mp4",
        jst_compact(from_ms),
        jst_compact(to_ms)
    )
}

/// 元ファイルの R2 のキーの頭 (この後に SD と同じ並びが続く)
pub fn raw_r2_prefix(tenant: Uuid, camera_id: Uuid) -> String {
    format!("{tenant}/{camera_id}")
}

/// SD のルートの一覧 (`sdcard.cgi?action=generate&dir=` の XML) から、いちばん上のフォルダ (カメラ名) を決める。
/// `safe_segment` を満たすフォルダが 1 つだけならそれ、0 か 2 以上なら `None` (呼び手が 502 にする)
pub fn sd_root(listing_xml: &str) -> Option<String> {
    let mut dirs = parse_listing(listing_xml)
        .dirs
        .into_iter()
        .filter(|d| safe_segment(d));
    let root = dirs.next()?;
    dirs.next().is_none().then_some(root)
}

fn camera_json(c: &CameraRow) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "vpc_binding": c.vpc_binding,
        "rec_username": c.rec_username,
        "has_password": c.has_password,
        "recording": c.vpc_binding.is_some(),
        "device_ids": c.device_ids,
        "rec_scanned_until": c.rec_scanned_until_ms.map(jst_iso),
    })
}

/// 運行管理者席への一覧の 1 件。画面がカメラを引くのに要る項目だけで、録画の設定 (宛先・資格情報・取り込みの進み) は返さない
fn camera_json_for_tenko_manager(c: &CameraRow) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "device_ids": c.device_ids,
    })
}

fn db_error(e: &tokio_postgres::Error) -> ApiError {
    if pg::is_vpc_binding_conflict(e) {
        return ApiError::new(409, "その VPC binding は既に使われています");
    }
    ApiError::new(
        500,
        format!("DB の処理に失敗しました ({})", alc_worker_db::kind(e)),
    )
}

fn encrypt(password: Option<String>, key: Option<&str>) -> Result<Option<String>, ApiError> {
    password
        .map(|p| {
            let key = key.ok_or_else(|| ApiError::new(500, "暗号化の鍵がありません"))?;
            alc_worker_crypto::secret::encrypt_secret(&p, key)
                .map_err(|_| ApiError::new(500, "暗号化に失敗しました"))
        })
        .transpose()
}

/// カメラの台帳と端末の候補の口を実行する。`key` は [`Op::needs_key`] のときだけ要る。
/// 切り出しの口 (`CreateClip` / `GetClip`) は Worker が行うので、ここに来たら 404
pub async fn execute(
    op: Op,
    caller: &Caller,
    pg: &mut PgClient,
    key: Option<&str>,
) -> Result<Reply, ApiError> {
    let tenant = caller.tenant;
    match op {
        Op::ListCameras => {
            let cameras = pg::list_cameras(pg, tenant, None)
                .await
                .map_err(|e| db_error(&e))?;
            let body = match caller.role {
                Role::Admin => json!({ "can_edit_destination": caller.developer, "cameras": cameras.iter().map(camera_json).collect::<Vec<_>>() }),
                Role::TenkoManager => json!({ "cameras": cameras.iter().map(camera_json_for_tenko_manager).collect::<Vec<_>>() }),
            };
            Ok(Reply { status: 200, body })
        }
        Op::CreateCamera(c) => {
            let new = NewCamera {
                name: c.name,
                vpc_binding: c.vpc_binding.map(VpcBinding::into_string),
                rec_username: c.rec_username,
                rec_password_encrypted: encrypt(c.rec_password, key)?,
                device_ids: c.device_ids,
            };
            let row = pg::create_camera(pg, tenant, new)
                .await
                .map_err(|e| db_error(&e))?;
            Ok(Reply {
                status: 201,
                body: camera_json(&row),
            })
        }
        Op::UpdateCamera(id, p) => {
            let rec_password_encrypted = match p.rec_password {
                Change::Keep => Change::Keep,
                Change::Set(v) => Change::Set(encrypt(v, key)?),
            };
            let patch = PatchCamera {
                name: p.name,
                vpc_binding: match p.vpc_binding {
                    Change::Keep => Change::Keep,
                    Change::Set(d) => Change::Set(d.map(VpcBinding::into_string)),
                },
                rec_username: p.rec_username,
                rec_password_encrypted,
                device_ids: p.device_ids,
            };
            match pg::update_camera(pg, tenant, id, patch, caller.developer)
                .await
                .map_err(|e| db_error(&e))?
            {
                UpdateOutcome::Updated(row) => Ok(Reply {
                    status: 200,
                    body: camera_json(&row),
                }),
                UpdateOutcome::NotFound => Err(ApiError::new(404, "そのカメラはありません")),
                UpdateOutcome::Forbidden => Err(ApiError::new(
                    403,
                    "宛先 (vpc_binding) を書けるのはシステム管理者だけです",
                )),
            }
        }
        Op::DeviceCandidates { days } => {
            let rows = pg::device_candidates(pg, tenant, days)
                .await
                .map_err(|e| db_error(&e))?;
            let list: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "device_id": r.device_id,
                        "counts": {
                            "alcohol": r.alcohol,
                            "blood_pressure": r.blood_pressure,
                            "timecard": r.timecard,
                            "license": r.license,
                            "vein_enroll": r.vein_enroll,
                        },
                        "last_at": jst_iso(r.last_at_ms),
                        "is_dev": r.is_dev,
                    })
                })
                .collect();
            Ok(Reply {
                status: 200,
                body: Value::Array(list),
            })
        }
        Op::Events {
            camera_id,
            from_ms,
            to_ms,
        } => {
            if pg::clip_source(pg, tenant, camera_id)
                .await
                .map_err(|e| db_error(&e))?
                .is_none()
            {
                return Err(ApiError::new(404, "そのカメラはありません"));
            }
            let rows = pg::day_events(pg, tenant, camera_id, from_ms, to_ms)
                .await
                .map_err(|e| db_error(&e))?;
            let list: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "source_table": r.event.source_table,
                        "source_id": r.event.source_id,
                        "kind": r.event.kind,
                        "occurred_at": jst_iso(r.event.at_ms),
                        "device_id": r.event.device_id,
                        "is_dev": r.event.is_dev,
                        "clip": r.clip.as_ref().map(|c| json!({
                            "clip_id": c.clip_id,
                            "key": c.key,
                            "status": c.status,
                            "error": c.error,
                        })),
                    })
                })
                .collect();
            Ok(Reply {
                status: 200,
                body: Value::Array(list),
            })
        }
        Op::CreateClip { .. } | Op::GetClip { .. } | Op::AutoRun { .. } => Err(ApiError::new(404, "Not Found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT: &str = "00000000-0000-4000-8000-000000000001";

    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn admin(developer: bool) -> Caller {
        Caller {
            tenant: Uuid::parse_str(TENANT).unwrap(),
            developer,
            role: Role::Admin,
        }
    }

    fn tenko_manager() -> Caller {
        Caller {
            tenant: Uuid::parse_str(TENANT).unwrap(),
            developer: false,
            role: Role::TenkoManager,
        }
    }

    fn status(r: Result<Op, ApiError>) -> u16 {
        match r {
            Ok(_) => 200,
            Err(e) => e.status,
        }
    }

    #[test]
    fn caller_requires_admin_role() {
        assert_eq!(
            caller(headers(&[("X-Tenant-ID", TENANT)]))
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            caller(headers(&[
                ("X-User-Role", "viewer"),
                ("X-Tenant-ID", TENANT)
            ]))
            .unwrap_err()
            .status,
            403
        );
        assert_eq!(
            caller(headers(&[
                ("X-User-Role", "Admin"),
                ("X-Tenant-ID", TENANT)
            ]))
            .unwrap_err()
            .status,
            403
        );
    }

    #[test]
    fn caller_requires_valid_tenant() {
        assert_eq!(
            caller(headers(&[("X-User-Role", "admin")]))
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            caller(headers(&[("X-User-Role", "admin"), ("X-Tenant-ID", "")]))
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            caller(headers(&[
                ("X-User-Role", "admin"),
                ("X-Tenant-ID", "not-a-uuid")
            ]))
            .unwrap_err()
            .status,
            400
        );
        assert_eq!(
            caller(headers(&[
                ("X-User-Role", "admin"),
                ("X-Tenant-ID", "x' OR 1=1 --")
            ]))
            .unwrap_err()
            .status,
            400
        );
    }

    #[test]
    fn caller_reads_developer_flag() {
        let c = caller(headers(&[
            ("X-User-Role", "admin"),
            ("X-Tenant-ID", TENANT),
        ]))
        .unwrap();
        assert_eq!(c, admin(false));
        let c = caller(headers(&[
            ("X-User-Role", "admin"),
            ("X-Tenant-ID", TENANT),
            ("X-User-Developer", "1"),
        ]))
        .unwrap();
        assert_eq!(c, admin(true));
        let c = caller(headers(&[
            ("X-User-Role", "admin"),
            ("X-Tenant-ID", TENANT),
            ("X-User-Developer", "true"),
        ]))
        .unwrap();
        assert!(!c.developer);
    }

    #[test]
    fn caller_accepts_tenko_manager_device_role() {
        let c = caller(headers(&[
            ("X-Device-Role", "device-tenko-manager"),
            ("X-Tenant-ID", TENANT),
        ]))
        .unwrap();
        assert_eq!(c, tenko_manager());
        // 席の鍵には宛先を書かせない (X-User-Developer が付いていても developer にしない)
        let c = caller(headers(&[
            ("X-Device-Role", "device-tenko-manager"),
            ("X-Tenant-ID", TENANT),
            ("X-User-Developer", "1"),
        ]))
        .unwrap();
        assert_eq!(c, tenko_manager());
        // テナントの検査は管理者と同じ
        assert_eq!(
            caller(headers(&[("X-Device-Role", "device-tenko-manager")]))
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            caller(headers(&[
                ("X-Device-Role", "device-tenko-manager"),
                ("X-Tenant-ID", "not-a-uuid")
            ]))
            .unwrap_err()
            .status,
            400
        );
    }

    #[test]
    fn caller_rejects_other_or_ambiguous_roles() {
        for pairs in [
            // 席の鍵以外の端末の鍵
            &[("X-Device-Role", "device-kiosk"), ("X-Tenant-ID", TENANT)][..],
            &[("X-Device-Role", "device-bp-station"), ("X-Tenant-ID", TENANT)][..],
            &[("X-Device-Role", "Device-Tenko-Manager"), ("X-Tenant-ID", TENANT)][..],
            &[("X-Device-Role", ""), ("X-Tenant-ID", TENANT)][..],
            // 両方ある (auth-worker はどちらか片方しか付けない) — 管理者を名乗っても拒否
            &[
                ("X-User-Role", "admin"),
                ("X-Device-Role", "device-tenko-manager"),
                ("X-Tenant-ID", TENANT),
            ][..],
            &[
                ("X-User-Role", "admin"),
                ("X-Device-Role", "device-kiosk"),
                ("X-Tenant-ID", TENANT),
            ][..],
            &[
                ("X-User-Role", "viewer"),
                ("X-Device-Role", "device-tenko-manager"),
                ("X-Tenant-ID", TENANT),
            ][..],
            // 画面のログインの manager は無い (users.role は admin / viewer / payroll)
            &[("X-User-Role", "manager"), ("X-Tenant-ID", TENANT)][..],
        ] {
            assert_eq!(caller(headers(pairs)).unwrap_err().status, 403, "{pairs:?}");
        }
    }

    #[test]
    fn tenko_manager_can_only_list_cameras() {
        let m = tenko_manager();
        assert!(matches!(
            parse("GET", "/cameras", "", "", &m, 60_000),
            Ok(Op::ListCameras)
        ));
        let id = "00000000-0000-4000-8000-0000000000aa";
        let clip = format!("/clips/{TENANT}/{id}/20261023T125800_20261023T125930.mp4");
        let patch = format!("/cameras/{id}");
        let events = format!("camera_id={id}&date=2026-10-23");
        let clip_body = format!(r#"{{"camera_id":"{id}","from":"2026-10-23T12:58:00","to":"2026-10-23T12:59:00"}}"#);
        for (method, path, query, body) in [
            ("POST", "/cameras", "", r#"{"name":"入口"}"#),
            ("PATCH", patch.as_str(), "", r#"{"name":"入口"}"#),
            ("GET", "/device-candidates", "", ""),
            ("POST", "/clips", "", clip_body.as_str()),
            ("GET", clip.as_str(), "", ""),
            ("GET", "/events", events.as_str(), ""),
            ("GET", "/cameras/", "", ""),
            ("GET", patch.as_str(), "", ""),
            ("DELETE", "/cameras", "", ""),
            ("GET", "/nothing", "", ""),
        ] {
            assert_eq!(status(parse(method, path, query, body, &m, 60_000)), 403, "{method} {path}");
        }
        // 管理者は今までどおり (同じ口が通る)
        assert!(matches!(
            parse("GET", "/device-candidates", "", "", &admin(false), 60_000),
            Ok(Op::DeviceCandidates { .. })
        ));
    }

    #[test]
    fn auto_run_takes_an_empty_body_or_a_camera_id() {
        let a = admin(false);
        let run = |body: &str| parse("POST", "/auto-run", "", body, &a, 60_000);
        assert!(matches!(run(""), Ok(Op::AutoRun { camera_id: None })));
        assert!(matches!(run("{}"), Ok(Op::AutoRun { camera_id: None })));
        let id = "00000000-0000-4000-8000-0000000000aa";
        let Ok(Op::AutoRun { camera_id: Some(got) }) = run(&format!(r#"{{"camera_id":"{id}"}}"#)) else {
            panic!("camera_id 付きは AutoRun{{Some}} になる");
        };
        assert_eq!(got, Uuid::parse_str(id).unwrap());
        assert_eq!(status(run(r#"{"camera_id":"not-a-uuid"}"#)), 400);
        assert_eq!(status(run("[")), 400);
    }

    #[test]
    fn tenko_manager_cannot_auto_run() {
        let m = tenko_manager();
        assert_eq!(status(parse("POST", "/auto-run", "", "", &m, 60_000)), 403);
    }

    #[test]
    fn tenko_manager_camera_json_has_no_recording_settings() {
        let row = CameraRow {
            id: Uuid::parse_str("00000000-0000-4000-8000-0000000000aa").unwrap(),
            name: "入口".to_owned(),
            vpc_binding: Some("CAM_TS_NS320W_55".to_owned()),
            rec_username: Some("viewer".to_owned()),
            has_password: true,
            device_ids: vec!["dev-1".to_owned(), "dev-2".to_owned()],
            rec_scanned_until_ms: Some(1_761_191_880_000),
        };
        assert_eq!(
            camera_json_for_tenko_manager(&row),
            json!({
                "id": "00000000-0000-4000-8000-0000000000aa",
                "name": "入口",
                "device_ids": ["dev-1", "dev-2"],
            })
        );
        // 管理者の 1 件は今までどおり録画の設定まで返す
        let full = camera_json(&row);
        for k in ["vpc_binding", "rec_username", "has_password", "recording", "rec_scanned_until"] {
            assert!(full.get(k).is_some(), "{k}");
        }
    }

    #[test]
    fn writing_vpc_binding_needs_developer() {
        let body = r#"{"name":"入口","vpc_binding":"CAM_TS_NS320W_55"}"#;
        assert_eq!(
            status(parse("POST", "/cameras", "", body, &admin(false), 0)),
            403
        );
        assert_eq!(
            status(parse("POST", "/cameras", "", body, &admin(true), 0)),
            200
        );
        // vpc_binding が null / 無しなら developer でなくても作れる
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"入口","vpc_binding":null}"#,
                &admin(false),
                0
            )),
            200
        );
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"入口","device_ids":["d1"]}"#,
                &admin(false),
                0
            )),
            200
        );
        // 名前の検査は developer でも通す (IP・小文字は不可。旧い項目名 rec_ip は知らない項目で 400)
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","vpc_binding":"192.168.1.64"}"#,
                &admin(true),
                0
            )),
            400
        );
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","vpc_binding":"cam_a"}"#,
                &admin(true),
                0
            )),
            400
        );
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","rec_ip":"192.168.1.64"}"#,
                &admin(true),
                0
            )),
            400
        );
    }

    #[test]
    fn vpc_binding_to_check_in_env() {
        let dev = admin(true);
        let binding = |method: &str, path: &str, body: &str| {
            parse(method, path, "", body, &dev, 0)
                .unwrap()
                .vpc_binding()
                .map(str::to_owned)
        };
        let path = format!("/cameras/{TENANT}");
        assert_eq!(
            binding("POST", "/cameras", r#"{"name":"x","vpc_binding":"CAM_A"}"#).as_deref(),
            Some("CAM_A")
        );
        assert_eq!(binding("POST", "/cameras", r#"{"name":"x"}"#), None);
        assert_eq!(
            binding("PATCH", &path, r#"{"vpc_binding":"CAM_B"}"#).as_deref(),
            Some("CAM_B")
        );
        // null 化と省略は env を見ない
        assert_eq!(binding("PATCH", &path, r#"{"vpc_binding":null}"#), None);
        assert_eq!(binding("PATCH", &path, r#"{"name":"y"}"#), None);
        assert_eq!(binding("GET", "/cameras", ""), None);
    }

    #[test]
    fn camera_body_is_checked() {
        let c = admin(true);
        assert_eq!(status(parse("POST", "/cameras", "", r#"{}"#, &c, 0)), 400);
        assert_eq!(
            status(parse("POST", "/cameras", "", r#"{"name":"  "}"#, &c, 0)),
            400
        );
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","extra":1}"#,
                &c,
                0
            )),
            400
        );
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","rec_username":"a:b"}"#,
                &c,
                0
            )),
            400
        );
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","device_ids":[""]}"#,
                &c,
                0
            )),
            400
        );
        assert_eq!(
            status(parse("POST", "/cameras", "", "not json", &c, 0)),
            400
        );
        assert_eq!(
            status(parse("PATCH", "/cameras/not-uuid", "", "{}", &c, 0)),
            404
        );
        assert_eq!(
            status(parse(
                "PATCH",
                &format!("/cameras/{TENANT}"),
                "",
                r#"{"name":null}"#,
                &c,
                0
            )),
            400
        );
        assert_eq!(
            status(parse(
                "DELETE",
                &format!("/cameras/{TENANT}"),
                "",
                "",
                &c,
                0
            )),
            404
        );
    }

    #[test]
    fn password_needs_key_only_when_written() {
        let c = admin(false);
        assert!(parse(
            "POST",
            "/cameras",
            "",
            r#"{"name":"x","rec_password":"p"}"#,
            &c,
            0
        )
        .unwrap()
        .needs_key());
        assert!(!parse(
            "POST",
            "/cameras",
            "",
            r#"{"name":"x","rec_password":""}"#,
            &c,
            0
        )
        .unwrap()
        .needs_key());
        let path = format!("/cameras/{TENANT}");
        assert!(parse("PATCH", &path, "", r#"{"rec_password":"p"}"#, &c, 0)
            .unwrap()
            .needs_key());
        assert!(!parse("PATCH", &path, "", r#"{"rec_password":""}"#, &c, 0)
            .unwrap()
            .needs_key());
        assert!(
            !parse("PATCH", &path, "", r#"{"rec_password":null}"#, &c, 0)
                .unwrap()
                .needs_key()
        );
    }

    #[test]
    fn days_are_bounded() {
        let c = admin(false);
        let days = |q: &str| match parse("GET", "/device-candidates", q, "", &c, 0) {
            Ok(Op::DeviceCandidates { days }) => days,
            Ok(_) => unreachable!(),
            Err(e) => -(e.status as i32),
        };
        assert_eq!(days(""), 30);
        assert_eq!(days("days=1"), 1);
        assert_eq!(days("x=1&days=90"), 90);
        assert_eq!(days("days=0"), -400);
        assert_eq!(days("days=91"), -400);
        assert_eq!(days("days=abc"), -400);
    }

    #[test]
    fn clip_request_is_checked() {
        let c = admin(false);
        let max = 7_200_000;
        let ok = format!(
            r#"{{"camera_id":"{TENANT}","from":"2026-10-23T12:58:00+09:00","to":"2026-10-23T12:59:30+09:00"}}"#
        );
        match parse("POST", "/clips", "", &ok, &c, max).unwrap() {
            Op::CreateClip { to_ms, from_ms, .. } => assert_eq!(to_ms - from_ms, 90_000),
            _ => unreachable!(),
        }
        // camera_id は必須
        assert_eq!(
            status(parse(
                "POST",
                "/clips",
                "",
                r#"{"from":"2026-10-23T12:58:00","to":"2026-10-23T12:59:00"}"#,
                &c,
                max
            )),
            400
        );
        let bad_id = r#"{"camera_id":"x","from":"2026-10-23T12:58:00","to":"2026-10-23T12:59:00"}"#;
        assert_eq!(status(parse("POST", "/clips", "", bad_id, &c, max)), 400);
        let reversed = format!(
            r#"{{"camera_id":"{TENANT}","from":"2026-10-23T12:59:00","to":"2026-10-23T12:58:00"}}"#
        );
        assert_eq!(status(parse("POST", "/clips", "", &reversed, &c, max)), 400);
        let long = format!(
            r#"{{"camera_id":"{TENANT}","from":"2026-10-23T10:00:00","to":"2026-10-23T12:00:01"}}"#
        );
        assert_eq!(status(parse("POST", "/clips", "", &long, &c, max)), 400);
    }

    #[test]
    fn clip_key_must_be_callers_tenant() {
        let c = admin(false);
        let mine = format!("/clips/{TENANT}/{TENANT}/20261023T125800_20261023T125930.mp4");
        match parse("GET", &mine, "", "", &c, 0).unwrap() {
            Op::GetClip { key } => assert_eq!(
                key,
                format!("clips/{TENANT}/{TENANT}/20261023T125800_20261023T125930.mp4")
            ),
            _ => unreachable!(),
        }
        let other =
            "/clips/00000000-0000-4000-8000-000000000002/x/20261023T125800_20261023T125930.mp4";
        assert_eq!(status(parse("GET", other, "", "", &c, 0)), 404);
        // 旧い形 (clips/<カメラ名>/...) と、パスの細工
        assert_eq!(
            status(parse("GET", "/clips/TS-NS320W-55/a.mp4", "", "", &c, 0)),
            404
        );
        assert_eq!(
            status(parse(
                "GET",
                &format!("/clips/{TENANT}/../x.mp4"),
                "",
                "",
                &c,
                0
            )),
            404
        );
    }

    #[test]
    fn sd_root_needs_exactly_one_folder() {
        let dir = |n: &str| format!(r#"<Dir name="{n}" path="/"></Dir>"#);
        let xml = |dirs: &[&str]| {
            format!(
                "<Root>{}</Root>",
                dirs.iter().map(|d| dir(d)).collect::<String>()
            )
        };
        assert_eq!(
            sd_root(&xml(&["TS-NS320W-55"])),
            Some("TS-NS320W-55".to_owned())
        );
        // 使えない名前は数えない (1 つ残ればそれ)
        assert_eq!(
            sd_root(&xml(&["..", "TS-NS320W-55"])),
            Some("TS-NS320W-55".to_owned())
        );
        assert_eq!(sd_root(&xml(&[])), None);
        assert_eq!(sd_root(&xml(&["a b"])), None);
        assert_eq!(sd_root(&xml(&["CAM-A", "CAM-B"])), None);
        assert_eq!(sd_root(""), None);
    }

    #[test]
    fn keys() {
        let t = Uuid::parse_str(TENANT).unwrap();
        let from = parse_time("2026-10-23T12:58:00+09:00").unwrap();
        let to = parse_time("2026-10-23T12:59:30+09:00").unwrap();
        assert_eq!(
            clip_r2_key(t, t, from, to),
            format!("clips/{TENANT}/{TENANT}/20261023T125800_20261023T125930.mp4")
        );
        assert_eq!(raw_r2_prefix(t, t), format!("{TENANT}/{TENANT}"));
    }

    #[test]
    fn events_query() {
        let c = admin(false);
        let cam = "00000000-0000-4000-8000-0000000000aa";
        let Ok(Op::Events { camera_id, from_ms, to_ms }) = parse("GET", "/events", &format!("camera_id={cam}&date=2026-10-23"), "", &c, 1) else {
            panic!("events");
        };
        assert_eq!(camera_id.to_string(), cam);
        // JST の 0 時から 24 時間
        assert_eq!(from_ms, parse_time("2026-10-22T15:00:00Z").unwrap());
        assert_eq!(to_ms - from_ms, 86_400_000);
        assert_eq!(status(parse("GET", "/events", &format!("date=2026-10-23&camera_id={cam}"), "", &c, 1)), 200);
        for q in [
            "date=2026-10-23".to_owned(),
            format!("camera_id={cam}"),
            "camera_id=x&date=2026-10-23".to_owned(),
            format!("camera_id={cam}&date=2026-1-3"),
            format!("camera_id={cam}&date=2026-10-23T00:00:00"),
            format!("camera_id={cam}&date=2026-13-01"),
            format!("camera_idx={cam}&date=2026-10-23"),
        ] {
            assert_eq!(status(parse("GET", "/events", &q, "", &c, 1)), 400, "{q}");
        }
    }

    #[test]
    fn truncation() {
        // 頭がキーフレームで前に出る・終わりが要求どおり → 短縮ではない
        assert!(!truncated(1000, 2000, 900, 2000));
        assert!(!truncated(1000, 2000, 1000, 2100));
        assert!(truncated(1000, 2000, 1001, 2000));
        assert!(truncated(1000, 2000, 900, 1999));
    }
}
