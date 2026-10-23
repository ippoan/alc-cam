//! カメラの SD 録画から、指定した時間帯を 1 本の MP4 にして R2 に置く Worker。
//!
//!   auth-worker ─Service Binding─▶ この Worker ─VPC (HTTP Service, ohishi-data の Tunnel)─▶ カメラ
//!                                       │ 重なる録画 (約 4 分ごと) を R2 にそろえ、moov を読み、
//!                                       │ 範囲を切ってつなぐ (再エンコードなし。crates/cam-mp4)
//!                                       └▶ R2 `clips/<カメラ名>/<開始>_<終了>.mp4`
//!
//! - `POST /api/cam-clips/clips` に `{"from": "...", "to": "..."}` (ISO 8601。時差が無ければ JST)
//! - `GET /api/cam-clips/clips/<カメラ名>/<開始>_<終了>.mp4` で取り出す (Range 対応 = シークできる)
//!
//! 外から直接届く口は持たない (workers_dev / route なし)。auth-worker の振り分け表 (`ALC_BINDING_ROUTES`)
//! から Service Binding で届き、JWT の検証は auth-worker が済ませて `X-Tenant-ID` / `X-User-Role` を付け直す
//! (alc 系の分割 Worker と同じ)。ここでは role が admin か、テナントがこのカメラの持ち主かだけを見る。

mod camera;
mod clip;
mod raw;

use cam_sd::{jst_iso, parse_time, safe_segment};
use camera::Camera;
use serde::Deserialize;
use worker::{event, Context, Env, Headers, Method, Range, Request, Response, Result};

pub(crate) struct Config {
    pub camera: String,
    pub kinds: Vec<String>,
    binding: String,
    max_clip_ms: i64,
    prefix: String,
    /// 空なら確かめない (カメラの持ち主のテナントが決まるまでの間)
    tenant: String,
}

impl Config {
    fn from_env(env: &Env) -> Self {
        let var = |k: &str, d: &str| env.var(k).map(|v| v.to_string()).unwrap_or_else(|_| d.to_string());
        Self {
            camera: var("CAM_NAME", ""),
            kinds: var("CAM_KINDS", "Schedule").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            binding: var("CAM_VPC_BINDING", "CAM_HTTP"),
            max_clip_ms: var("MAX_CLIP_SECS", "7200").parse::<i64>().unwrap_or(7200) * 1000,
            prefix: var("PUBLIC_PATH_PREFIX", ""),
            tenant: var("CAM_TENANT_ID", ""),
        }
    }
}

#[derive(Deserialize)]
struct ClipRequest {
    from: String,
    to: String,
}

fn json(status: u16, body: serde_json::Value) -> Result<Response> {
    Ok(Response::from_json(&body)?.with_status(status))
}

fn fail(status: u16, message: &str) -> Result<Response> {
    json(status, serde_json::json!({ "error": message }))
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let cfg = Config::from_env(&env);
    let full = req.path();
    // auth-worker からは振り分け表の path (/api/cam-clips/...) のまま届く
    let path = if cfg.prefix.is_empty() { full.as_str() } else { full.strip_prefix(cfg.prefix.as_str()).unwrap_or(&full) }.to_string();
    if path == "/health" {
        return Response::ok("ok");
    }
    // auth-worker が付け直したヘッダだけを信じる (この Worker に外から直接届く口は無い)
    let header = |k: &str| req.headers().get(k).ok().flatten().unwrap_or_default();
    if header("X-User-Role") != "admin" {
        return fail(403, "管理者だけが使えます");
    }
    if !cfg.tenant.is_empty() && header("X-Tenant-ID") != cfg.tenant {
        return fail(403, "このカメラのテナントではありません");
    }
    match (req.method(), path.as_str()) {
        (Method::Post, "/clips") => {
            let Ok(body) = req.json::<ClipRequest>().await else {
                return fail(400, "本文は {\"from\": \"...\", \"to\": \"...\"} の JSON にしてください");
            };
            create_clip(&env, &cfg, &body).await
        }
        (Method::Get, p) if p.starts_with("/clips/") => get_clip(&env, &req, &p["/clips/".len()..]).await,
        _ => fail(404, "Not Found"),
    }
}

async fn create_clip(env: &Env, cfg: &Config, body: &ClipRequest) -> Result<Response> {
    if !safe_segment(&cfg.camera) || cfg.kinds.iter().any(|k| !safe_segment(k)) {
        return fail(500, "CAM_NAME / CAM_KINDS の設定が正しくありません");
    }
    let (Some(from), Some(to)) = (parse_time(&body.from), parse_time(&body.to)) else {
        return fail(400, "時刻は 2026-10-23T12:58:00+09:00 の形にしてください (時差が無ければ JST)");
    };
    if from >= to {
        return fail(400, "to は from より後にしてください");
    }
    if to - from > cfg.max_clip_ms {
        return fail(400, &format!("一度に切り出せるのは {} 分までです", cfg.max_clip_ms / 60_000));
    }
    let auth = env.secret("CAM_AUTH")?.to_string();
    let mut cam = Camera::new(env, &cfg.binding, &auth)?;
    let bucket = env.bucket("RECORDINGS")?;
    match clip::make(cfg, &mut cam, &bucket, from, to).await {
        Ok(m) => json(
            if m.reused { 200 } else { 201 },
            serde_json::json!({
                "key": m.key,
                "size": m.size,
                "from": jst_iso(from),
                "to": jst_iso(to),
                "actual_from": jst_iso(m.actual_from_ms),
                "actual_to": jst_iso(m.actual_to_ms),
                "sources": m.sources,
                "reused": m.reused,
            }),
        ),
        Err(clip::Fail(status, message)) => fail(status, &message),
    }
}

/// `GET /clips/<カメラ名>/<開始>_<終了>.mp4`。Range (`bytes=a-b` / `bytes=a-`) に答える
async fn get_clip(env: &Env, req: &Request, rest: &str) -> Result<Response> {
    if rest.split('/').count() != 2 || !rest.split('/').all(safe_segment) {
        return fail(400, "取り出す名前が正しくありません");
    }
    let key = format!("clips/{rest}");
    let bucket = env.bucket("RECORDINGS")?;
    let Some(head) = bucket.head(key.clone()).await? else { return fail(404, "その切り出しはありません") };
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
