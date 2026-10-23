//! #2-0 の確認用: VPC Network の binding (`CAM_NET`) から workers-rs で fetch できるかを見る口。
//! `PROBE_CAM_IP` (`.dev.vars` で渡す。repo に書かない) が無ければ 404 で、dev --remote でしか立てない。
//! 返す JSON では宛先の IP を `{cam}` に伏せる。
//!
//! - `GET /__probe?url=http://{cam}/&redirect=manual` → 素の fetch (Digest なし) の status と主なヘッダ (`&body` で本文も)
//! - `GET /__probe?list=<dir>` → `Camera` (Digest の付け直し) で SD の一覧を取る

use crate::camera::{Camera, VpcHttp};
use wasm_bindgen::JsCast;
use worker::{Env, Fetcher, Request, RequestInit, RequestRedirect, Response, Result};

const BINDING: &str = "CAM_NET";

pub async fn handle(req: &Request, env: &Env) -> Result<Response> {
    let Ok(ip) = env.var("PROBE_CAM_IP").map(|v| v.to_string()) else {
        return Response::error("Not Found", 404);
    };
    let q: std::collections::HashMap<String, String> =
        req.url()?.query_pairs().into_owned().collect();
    let hide = |s: String| s.replace(&ip, "{cam}");
    let out = if let Some(dir) = q.get("list") {
        list(env, &ip, dir).await
    } else {
        let url = q
            .get("url")
            .cloned()
            .unwrap_or_else(|| "http://{cam}/".into())
            .replace("{cam}", &ip);
        raw(
            env,
            &url,
            q.get("redirect").map(String::as_str),
            q.contains_key("body"),
        )
        .await
    };
    let body = match out {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    Response::ok(hide(body.to_string()))
}

async fn raw(
    env: &Env,
    url: &str,
    redirect: Option<&str>,
    with_body: bool,
) -> Result<serde_json::Value> {
    let vpc: VpcHttp = env.get_binding(BINDING)?;
    let fetcher = Fetcher::unchecked_from_js(vpc.into());
    let mut init = RequestInit::new();
    if redirect == Some("manual") {
        init.with_redirect(RequestRedirect::Manual);
    }
    let mut res = fetcher
        .fetch_request(Request::new_with_init(url, &init)?)
        .await?;
    let h = |k: &str| res.headers().get(k).ok().flatten();
    // challenge は scheme (Digest / Basic) と algorithm / qop の有無だけを見る (realm・nonce は返さない)
    let www = h("WWW-Authenticate").map(|w| {
        let scheme = w.split_whitespace().next().unwrap_or_default().to_string();
        serde_json::json!({ "scheme": scheme, "has_qop": w.contains("qop="), "has_nonce": w.contains("nonce=") })
    });
    let location = h("Location");
    let server = h("Server");
    let status = res.status_code();
    let bytes = res.bytes().await.unwrap_or_default();
    // `&body` を付けたときだけ本文 (先頭 2KB) も返す。出口の判定 (/cdn-cgi/trace) 用
    let body =
        with_body.then(|| String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).into_owned());
    Ok(
        serde_json::json!({ "status": status, "www_authenticate": www, "location": location, "server": server, "body_len": bytes.len(), "body": body }),
    )
}

async fn list(env: &Env, ip: &str, dir: &str) -> Result<serde_json::Value> {
    let auth = env.secret("CAM_AUTH")?.to_string();
    let mut cam = Camera::with_base(env, BINDING, &auth, &format!("http://{ip}"))?;
    let xml = cam.get_text(&cam_sd::list_path(dir)).await?;
    let files = cam_sd::parse_listing(&xml).files.len();
    Ok(serde_json::json!({ "status": 200, "body_len": xml.len(), "files": files }))
}
