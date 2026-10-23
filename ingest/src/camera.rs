//! VPC の HTTP Service 越しにカメラへ投げる。Digest は 401 の challenge を受けてから付け直す。

use cam_sd::digest;
use wasm_bindgen::prelude::*;
use worker::{Env, Fetcher, Headers, Method, Request, RequestInit, Response, Result};

/// VPC Service の binding。JS 側の型名が公開されていないので、型名は見ずに Fetcher として受ける
/// (`src/tcp.rs` を持つ alc 系 Worker と同じ扱い。無ければ `get_binding` が Err)
pub struct VpcHttp(JsValue);

impl worker::EnvBinding for VpcHttp {
    const TYPE_NAME: &'static str = "Fetcher";

    fn get(val: JsValue) -> Result<Self> {
        Ok(Self(val))
    }
}

impl AsRef<JsValue> for VpcHttp {
    fn as_ref(&self) -> &JsValue {
        &self.0
    }
}

impl From<VpcHttp> for JsValue {
    fn from(v: VpcHttp) -> Self {
        v.0
    }
}

impl JsCast for VpcHttp {
    fn instanceof(_: &JsValue) -> bool {
        true
    }

    fn unchecked_from_js(val: JsValue) -> Self {
        Self(val)
    }

    fn unchecked_from_js_ref(val: &JsValue) -> &Self {
        // SAFETY: VpcHttp は JsValue 1 つだけを持つ repr と同じ形
        unsafe { &*(val as *const JsValue as *const Self) }
    }
}

/// URL の host は VPC Service の宛先には効かず Host ヘッダになるだけ。公開ホスト名を書くと VPC の外へ
/// 出ていく罠があるので (cf-flickr-cam-worker #17)、名前解決できない名前を置く
const BASE: &str = "http://camera.vpc.internal";

pub struct Camera {
    fetcher: Fetcher,
    user: String,
    pass: String,
    nc: u32,
}

impl Camera {
    pub fn new(env: &Env, binding: &str, auth: &str) -> Result<Self> {
        let vpc: VpcHttp = env.get_binding(binding)?;
        let (user, pass) = cam_sd::split_auth(auth).ok_or("CAM_AUTH は user:pass の形にしてください")?;
        Ok(Self { fetcher: Fetcher::unchecked_from_js(vpc.0), user: user.into(), pass: pass.into(), nc: 0 })
    }

    pub async fn get_text(&mut self, path: &str) -> Result<String> {
        let mut res = self.send(Method::Get, path, None).await?;
        if res.status_code() != 200 {
            return Err(format!("GET 一覧が {} で失敗", res.status_code()).into());
        }
        res.text().await
    }

    /// 本体を取りに行く。返った Response の body を呼び手が R2 へ流す
    pub async fn post_stream(&mut self, path: &str, body: String) -> Result<Response> {
        self.send(Method::Post, path, Some(body)).await
    }

    async fn send(&mut self, method: Method, path: &str, body: Option<String>) -> Result<Response> {
        let first = self.fetcher.fetch_request(build(method.clone(), path, body.clone(), None)?).await?;
        if first.status_code() != 401 {
            return Ok(first);
        }
        let challenge = first
            .headers()
            .get("WWW-Authenticate")?
            .and_then(|h| digest::parse(&h))
            .ok_or("カメラが Digest を求めていません")?;
        self.nc += 1;
        let cnonce = format!("{:016x}", (js_sys::Math::random() * 1e16) as u64);
        let auth = digest::authorization(&challenge, method.as_ref(), path, &self.user, &self.pass, self.nc, &cnonce);
        let res = self.fetcher.fetch_request(build(method, path, body, Some(&auth))?).await?;
        if res.status_code() == 401 {
            return Err("カメラの認証に失敗しました (CAM_AUTH を確かめてください)".into());
        }
        Ok(res)
    }
}

fn build(method: Method, path: &str, body: Option<String>, auth: Option<&str>) -> Result<Request> {
    let headers = Headers::new();
    if let Some(a) = auth {
        headers.set("Authorization", a)?;
    }
    if body.is_some() {
        // 管理画面 (axios) と同じ。JSON で送ると 400 になる
        headers.set("Content-Type", "application/x-www-form-urlencoded")?;
    }
    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if let Some(b) = body {
        init.with_body(Some(b.into()));
    }
    Request::new_with_init(&format!("{BASE}{path}"), &init)
}
