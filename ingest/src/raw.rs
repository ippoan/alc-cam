//! 元の録画ファイルを R2 にそろえる。カメラは Range に応えず毎回丸ごと返すので、切り出しの前に
//! いったん R2 に置き、以後は R2 の範囲読み出しで必要な所だけ読む。

use cam_sd::{Recording, DOWNLOAD_PATH};
use wasm_bindgen::prelude::*;
use worker::{console_log, Bucket, Data, HttpMetadata, ResponseBody, Result};

use crate::camera::Camera;

#[wasm_bindgen]
extern "C" {
    /// Workers の組み込み。長さが分かっている stream でないと R2 の put が受け付けない
    #[wasm_bindgen(js_name = FixedLengthStream)]
    type JsFixedLengthStream;

    #[wasm_bindgen(constructor, js_class = "FixedLengthStream")]
    fn new(length: f64) -> JsFixedLengthStream;

    #[wasm_bindgen(method, getter)]
    fn readable(this: &JsFixedLengthStream) -> web_sys::ReadableStream;

    #[wasm_bindgen(method, getter)]
    fn writable(this: &JsFixedLengthStream) -> JsValue;
}

/// 元ファイルの R2 のキー: `<tenant>/<camera_id>/` (`cam_ingest_api::raw_r2_prefix`) の後に SD と同じ並び
pub fn key(prefix: &str, rec: &Recording<'_>) -> String {
    format!("{prefix}/{}", rec.r2_key())
}

/// R2 に同じ大きさで在れば何もしない。無ければカメラから取って置く。戻り値は取りに行ったか
pub async fn ensure(cam: &mut Camera, bucket: &Bucket, prefix: &str, rec: &Recording<'_>, listed_size: u64) -> Result<bool> {
    let key = key(prefix, rec);
    if bucket.head(key.clone()).await?.is_some_and(|o| o.size() == listed_size) {
        return Ok(false);
    }
    console_log!("alc-cam: fetch {} ({listed_size} bytes)", rec.name);
    let res = cam.post_stream(DOWNLOAD_PATH, cam_sd::download_body(&rec.sd_path())).await?;
    let ctype = res.headers().get("Content-Type")?.unwrap_or_default();
    if res.status_code() != 200 || !ctype.starts_with("application/octet-stream") {
        return Err(format!("{} の取り出しが {} ({ctype}) で失敗", rec.name, res.status_code()).into());
    }
    // カメラの応答は Content-Length の無い HTTP/1.0 なので、長さは一覧の値を使う
    let length = res.headers().get("Content-Length")?.and_then(|v| v.parse::<u64>().ok()).unwrap_or(listed_size);
    let ResponseBody::Stream(body) = res.body() else {
        return Err(format!("{} の応答に本文がありません", rec.name).into());
    };
    // 本体は wasm を通さず JS の stream のまま R2 へ流す (100MB を Rust で受けると CPU 時間を食う)
    let fixed = JsFixedLengthStream::new(length as f64);
    let pipe_to: js_sys::Function = js_sys::Reflect::get(body, &"pipeTo".into())?.into();
    let piping: js_sys::Promise = pipe_to.call1(body, &fixed.writable())?.into();
    let put = bucket
        .put(key, Data::ReadableStream(fixed.readable()))
        .http_metadata(HttpMetadata { content_type: Some("video/mp4".into()), ..Default::default() })
        .execute();
    let (put, piped) = futures_util::future::join(put, wasm_bindgen_futures::JsFuture::from(piping)).await;
    piped?;
    put?;
    Ok(true)
}
