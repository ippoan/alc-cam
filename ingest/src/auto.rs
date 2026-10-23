//! cron (`#[event(scheduled)]`、1 時間ごと) の自動切り出し。テナント → 録画を取り込むカメラの順に巡り、
//! 1 カメラ分の流れ (`cam_ingest_api::auto::run_camera`) にカメラと R2 ([`CamRecorder`]) を渡す。
//! ログは件数・カメラ id・所要時間だけ (宛先・資格情報を出さない)。

use cam_ingest_api::auto::{run_camera, AutoCamera, ListedFile, Recorder, DEFAULT_MAX_GROUPS};
use cam_ingest_api::dest::VpcBinding;
use cam_ingest_api::pg::{self, RecordingCamera};
use cam_ingest_api::raw_r2_prefix;
use uuid::Uuid;
use worker::{console_error, console_log, Bucket, Env};

use crate::camera::{self, Camera};
use crate::{clip, db, raw, sso_key};

/// カメラと R2。SD のカメラ名は最初の一覧の時に 1 回だけ引く
struct CamRecorder {
    cam: Camera,
    bucket: Bucket,
    raw_prefix: String,
    sd_root: Option<String>,
}

impl CamRecorder {
    async fn sd_root(&mut self) -> worker::Result<String> {
        if self.sd_root.is_none() {
            self.sd_root = Some(self.cam.sd_root().await?);
        }
        Ok(self.sd_root.clone().unwrap_or_default())
    }
}

impl Recorder for CamRecorder {
    async fn list(&mut self, from_ms: i64, to_ms: i64) -> Result<Vec<ListedFile>, String> {
        let root = self.sd_root().await.map_err(|e| e.to_string())?;
        clip::list(&mut self.cam, &root, from_ms, to_ms, 0).await.map_err(|e| e.to_string())
    }

    async fn fetch_raw(&mut self, file: &ListedFile) -> Result<(), String> {
        let root = self.sd_root().await.map_err(|e| e.to_string())?;
        raw::ensure(&mut self.cam, &self.bucket, &self.raw_prefix, &clip::recording(&root, file), file.size)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn cut(&mut self, key: &str, files: &[&ListedFile], from_ms: i64, to_ms: i64) -> Result<u64, String> {
        let root = self.sd_root().await.map_err(|e| e.to_string())?;
        let target = clip::Target { sd_root: root, key: key.to_string(), raw_prefix: self.raw_prefix.clone() };
        clip::cut(&target, &self.bucket, files, from_ms, to_ms).await.map(|m| m.size).map_err(|clip::Fail(_, m)| m)
    }
}

/// 切り出しに使えるカメラなら Recorder を作る。使えなければ理由 (ログ用の固定の語)
fn recorder(env: &Env, tenant: Uuid, c: &RecordingCamera, key: &str) -> Result<CamRecorder, &'static str> {
    let binding = c.source.vpc_binding.as_deref().and_then(VpcBinding::parse).ok_or("invalid vpc binding")?;
    if !camera::has_binding(env, binding.as_str()) {
        return Err("vpc binding is not bound");
    }
    let (Some(user), Some(enc)) = (c.source.rec_username.clone(), c.source.rec_password_encrypted.as_deref()) else {
        return Err("no credentials");
    };
    let pass = alc_worker_crypto::secret::decrypt_secret(enc, key).map_err(|_| "decrypt failed")?;
    let cam = Camera::new(env, &binding, user, pass).map_err(|_| "no vpc binding")?;
    let bucket = env.bucket("RECORDINGS").map_err(|_| "no RECORDINGS bucket")?;
    Ok(CamRecorder { cam, bucket, raw_prefix: raw_r2_prefix(tenant, c.id), sd_root: None })
}

pub async fn run(env: &Env) {
    let started = js_sys::Date::now();
    let max_groups = env
        .var("AUTO_CLIP_MAX_GROUPS")
        .ok()
        .and_then(|v| v.to_string().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_GROUPS);
    let mut pg = match db::connect(env).await {
        Ok(c) => c,
        Err(e) => {
            console_error!("alc-cam: cron db {}", e.status);
            return;
        }
    };
    let tenants = match pg::recording_tenants(&mut pg).await {
        Ok(t) => t,
        Err(e) => {
            console_error!("alc-cam: cron tenants {}", alc_worker_db::kind(&e));
            return;
        }
    };
    // 鍵はカメラが在るときに 1 回だけ引く (取れなければ全カメラを飛ばす)
    let mut key: Option<Option<String>> = None;
    let mut cameras_done = 0usize;
    for tenant in tenants {
        let cameras = match pg::recording_cameras(&mut pg, tenant).await {
            Ok(c) => c,
            Err(e) => {
                console_error!("alc-cam: cron cameras {}", alc_worker_db::kind(&e));
                continue;
            }
        };
        for c in cameras {
            let t0 = js_sys::Date::now();
            if key.is_none() {
                key = Some(sso_key(env).await.ok());
            }
            let Some(k) = key.as_ref().and_then(|k| k.as_deref()) else {
                console_error!("alc-cam: cron camera {}: no encryption key", c.id);
                continue;
            };
            let mut rec = match recorder(env, tenant, &c, k) {
                Ok(r) => r,
                Err(reason) => {
                    console_error!("alc-cam: cron camera {}: skipped ({reason})", c.id);
                    continue;
                }
            };
            let cam = AutoCamera { id: c.id, scanned_until_ms: c.scanned_until_ms };
            match run_camera(&mut pg, tenant, &cam, &mut rec, js_sys::Date::now() as i64, max_groups).await {
                Ok(o) => console_log!("alc-cam: cron camera {} {} in {}ms", c.id, o.line(), (js_sys::Date::now() - t0) as i64),
                Err(e) => console_error!("alc-cam: cron camera {} db {}", c.id, alc_worker_db::kind(&e)),
            }
            cameras_done += 1;
        }
    }
    console_log!("alc-cam: cron {cameras_done} cameras in {}ms", (js_sys::Date::now() - started) as i64);
}
