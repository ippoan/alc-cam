//! VPC Network (binding `CAM_NET`) 越しにカメラへ投げる。Digest は 401 の challenge を受けてから付け直す。
//!
//! VPC Network は URL の host がそのまま宛先になり、binding は宛先を絞らない。宛先は検査済みの [`Dest`]
//! (私設帯の IPv4・ポート 80) からだけ組み立て、転送 (3xx) は追わずに失敗にする (`redirect: manual`。
//! `redirect: "error"` は workerd が受け付けない)。

use cam_ingest_api::dest::Dest;
use cam_sd::{digest, list_path, parse_listing, safe_segment};
use wasm_bindgen::prelude::*;
use worker::{Env, Fetcher, Headers, Method, Request, RequestInit, RequestRedirect, Response, Result};

/// VPC Network の binding 名 (wrangler.toml の `[[vpc_networks]]`)
pub const CAM_NET_BINDING: &str = "CAM_NET";

/// VPC の binding。JS 側の型名が公開されていないので、型名は見ずに Fetcher として受ける
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

pub struct Camera {
    fetcher: Fetcher,
    base: String,
    user: String,
    pass: String,
    nc: u32,
}

impl Camera {
    /// `dest` は fetch の直前の検査 ([`Dest::parse`]) を通したもの
    pub fn new(env: &Env, dest: &Dest, user: String, pass: String) -> Result<Self> {
        let vpc: VpcHttp = env.get_binding(CAM_NET_BINDING)?;
        Ok(Self { fetcher: Fetcher::unchecked_from_js(vpc.0), base: dest.base_url(), user, pass, nc: 0 })
    }

    /// SD のいちばん上のフォルダ (カメラ名。`<カメラ名>/<種類>/<日付>/<時>/` の頭)。1 つだけでなければ失敗
    pub async fn sd_root(&mut self) -> Result<String> {
        let dirs: Vec<String> = parse_listing(&self.get_text(&list_path("")).await?).dirs.into_iter().filter(|d| safe_segment(d)).collect();
        match <[String; 1]>::try_from(dirs) {
            Ok([root]) => Ok(root),
            Err(dirs) => Err(format!("SD のいちばん上のフォルダが 1 つではありません ({} 個)", dirs.len()).into()),
        }
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
        let first = self.fetch(build(&self.base, method.clone(), path, body.clone(), None)?).await?;
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
        let res = self.fetch(build(&self.base, method, path, body, Some(&auth))?).await?;
        if res.status_code() == 401 {
            return Err("カメラの認証に失敗しました (カメラのユーザー名・パスワードを確かめてください)".into());
        }
        Ok(res)
    }

    /// 転送 (3xx) は追わずに失敗にする (宛先を検査した IP の外へ出ない)
    async fn fetch(&self, req: Request) -> Result<Response> {
        let res = self.fetcher.fetch_request(req).await.map_err(|e| worker::Error::from(format!("カメラに届きません ({e})")))?;
        if (300..400).contains(&res.status_code()) {
            return Err(format!("カメラが転送 ({}) を返しました", res.status_code()).into());
        }
        Ok(res)
    }
}

fn build(base: &str, method: Method, path: &str, body: Option<String>, auth: Option<&str>) -> Result<Request> {
    let headers = Headers::new();
    if let Some(a) = auth {
        headers.set("Authorization", a)?;
    }
    if body.is_some() {
        // 管理画面 (axios) と同じ。JSON で送ると 400 になる
        headers.set("Content-Type", "application/x-www-form-urlencoded")?;
    }
    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers).with_redirect(RequestRedirect::Manual);
    if let Some(b) = body {
        init.with_body(Some(b.into()));
    }
    Request::new_with_init(&format!("{base}{path}"), &init)
}
