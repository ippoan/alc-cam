//! カメラの SD 録画を定期的に R2 へ取り込む (cron)。
//!
//!   cron ─▶ この Worker ─VPC (HTTP Service, ohishi-data の Tunnel)─▶ カメラ (sdcard.cgi で一覧 / recdownload で本体)
//!                 └─▶ R2 `<カメラ名>/<種類>/<YYYYMMDD>/<HH>/<ファイル名>`
//!
//! 昨日と今日 (JST) のフォルダを古い順に見て、R2 に同じ大きさで在るものは飛ばす (= 何度回しても同じ結果)。
//! 1 回の実行は本数と時間で打ち切り、残りは次の cron に回す。本体は wasm を通さず JS の
//! FixedLengthStream に直接流す (100MB を Rust で受けると CPU 時間を食う)。

mod camera;

use cam_sd::{list_path, parse_listing, safe_segment, scan_dates, wanted, Recording, DOWNLOAD_PATH};
use camera::Camera;
use wasm_bindgen::prelude::*;
use worker::{
    console_error, console_log, event, Bucket, Context, Data, Date, Env, HttpMetadata, Request, Response, ResponseBody,
    Result, ScheduleContext, ScheduledEvent,
};

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

struct Config {
    camera: String,
    kinds: Vec<String>,
    binding: String,
    max_files: u32,
    budget_ms: f64,
    days: u32,
    /// 切り分け用: R2 に書かず読み捨てて、届いたバイト数と時間だけログに出す
    drain_only: bool,
}

impl Config {
    fn from_env(env: &Env) -> Self {
        let var = |k: &str, d: &str| env.var(k).map(|v| v.to_string()).unwrap_or_else(|_| d.to_string());
        Self {
            camera: var("CAM_NAME", ""),
            kinds: var("CAM_KINDS", "Schedule").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            binding: var("CAM_VPC_BINDING", "CAM_HTTP"),
            max_files: var("MAX_FILES_PER_RUN", "12").parse().unwrap_or(12),
            budget_ms: var("RUN_BUDGET_SECS", "600").parse::<f64>().unwrap_or(600.0) * 1000.0,
            days: var("SCAN_DAYS", "2").parse().unwrap_or(2),
            drain_only: var("DRAIN_ONLY", "0") == "1",
        }
    }
}

#[derive(Default)]
struct Summary {
    uploaded: u32,
    bytes: u64,
    skipped: u32,
    stopped: Option<&'static str>,
}

#[event(scheduled)]
async fn scheduled(_ev: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    match run(&env).await {
        Ok(s) => console_log!(
            "sd-ingest: uploaded={} bytes={} skipped={} stopped={}",
            s.uploaded,
            s.bytes,
            s.skipped,
            s.stopped.unwrap_or("-")
        ),
        Err(e) => console_error!("sd-ingest: failed: {e}"),
    }
}

/// 外から届く口は持たない (workers_dev / route なし)。/health だけ答える
#[event(fetch)]
async fn fetch(req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    if req.path() == "/health" {
        return Response::ok("ok");
    }
    Response::error("Not Found", 404)
}

async fn run(env: &Env) -> Result<Summary> {
    let cfg = Config::from_env(env);
    if !safe_segment(&cfg.camera) || cfg.kinds.iter().any(|k| !safe_segment(k)) {
        return Err("CAM_NAME / CAM_KINDS が無いか、使えない文字を含みます".into());
    }
    let auth = env.secret("CAM_AUTH")?.to_string();
    let mut cam = Camera::new(env, &cfg.binding, &auth)?;
    let bucket = env.bucket("RECORDINGS")?;
    let started = Date::now().as_millis() as f64;
    let mut s = Summary::default();

    for date in scan_dates(started, cfg.days) {
        for kind in &cfg.kinds {
            let day_dir = format!("{}/{kind}/{date}", cfg.camera);
            let mut hours = parse_listing(&cam.get_text(&list_path(&day_dir)).await?).dirs;
            hours.retain(|h| safe_segment(h));
            hours.sort();
            console_log!("sd-ingest: {day_dir} hours={:?}", hours);
            for hour in hours {
                let rec_dir = format!("{day_dir}/{hour}");
                let mut files = parse_listing(&cam.get_text(&list_path(&rec_dir)).await?).files;
                files.sort_by(|a, b| a.name.cmp(&b.name));
                console_log!("sd-ingest: {rec_dir} files={}", files.len());
                for f in files.iter().filter(|f| wanted(&f.name) && safe_segment(&f.name)) {
                    let rec = Recording { camera: &cfg.camera, kind, date: &date, hour: &hour, name: &f.name };
                    let key = rec.r2_key();
                    if bucket.head(key.clone()).await?.is_some_and(|o| o.size() == f.size) {
                        s.skipped += 1;
                        continue;
                    }
                    if s.uploaded >= cfg.max_files {
                        s.stopped = Some("max_files");
                        return Ok(s);
                    }
                    if Date::now().as_millis() as f64 - started > cfg.budget_ms {
                        s.stopped = Some("time_budget");
                        return Ok(s);
                    }
                    copy_one(&mut cam, &bucket, &rec, f.size, cfg.drain_only).await?;
                    s.uploaded += 1;
                    s.bytes += f.size;
                }
            }
        }
    }
    Ok(s)
}

async fn copy_one(cam: &mut Camera, bucket: &Bucket, rec: &Recording<'_>, listed_size: u64, drain_only: bool) -> Result<()> {
    console_log!("sd-ingest: download {} ({listed_size} bytes listed)", rec.name);
    let mut res = cam.post_stream(DOWNLOAD_PATH, cam_sd::download_body(&rec.sd_path())).await?;
    let ctype = res.headers().get("Content-Type")?.unwrap_or_default();
    if res.status_code() != 200 || !ctype.starts_with("application/octet-stream") {
        return Err(format!("{} の取り出しが {} ({ctype}) で失敗", rec.name, res.status_code()).into());
    }
    // 長さは応答のものを優先 (一覧の値と違えば、R2 の put がそこで止まる)
    let length = res.headers().get("Content-Length")?.and_then(|v| v.parse::<u64>().ok()).unwrap_or(listed_size);
    if drain_only {
        return drain(&mut res, rec.name).await;
    }
    let ResponseBody::Stream(body) = res.body() else {
        return Err(format!("{} の応答に本文がありません", rec.name).into());
    };
    console_log!("sd-ingest: {} status={} type={ctype} length={length}", rec.name, res.status_code());
    let fixed = JsFixedLengthStream::new(length as f64);
    let pipe_to: js_sys::Function = js_sys::Reflect::get(body, &"pipeTo".into())?.into();
    let piping: js_sys::Promise = pipe_to.call1(body, &fixed.writable())?.into();
    let content_type = if rec.name.ends_with(".jpg") { "image/jpeg" } else { "video/mp4" };
    let put = bucket
        .put(rec.r2_key(), Data::ReadableStream(fixed.readable()))
        .http_metadata(HttpMetadata { content_type: Some(content_type.into()), ..Default::default() })
        .execute();
    let (put, piped) = futures_join(put, wasm_bindgen_futures::JsFuture::from(piping)).await;
    piped?;
    put?;
    Ok(())
}

async fn futures_join<A: std::future::Future, B: std::future::Future>(a: A, b: B) -> (A::Output, B::Output) {
    // 依存を増やさないための 2 本だけの join (put は pipe が流れないと進まないので逐次 await にはできない)
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let mut ra = None;
    let mut rb = None;
    std::future::poll_fn(|cx| {
        if ra.is_none() {
            if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let std::task::Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    (ra.unwrap(), rb.unwrap())
}

async fn drain(res: &mut Response, name: &str) -> Result<()> {
    use futures_util::StreamExt;
    let mut stream = res.stream()?;
    let t0 = Date::now().as_millis();
    let (mut n, mut last) = (0u64, t0);
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => n += c.len() as u64,
            Err(e) => {
                console_error!("sd-ingest: drain {name} error after {n} bytes / {}ms: {e}", Date::now().as_millis() - t0);
                return Err(e);
            }
        }
        if Date::now().as_millis() - last > 5000 {
            last = Date::now().as_millis();
            console_log!("sd-ingest: drain {name} {n} bytes");
        }
    }
    console_log!("sd-ingest: drain {name} done {n} bytes in {}ms", Date::now().as_millis() - t0);
    Ok(())
}
