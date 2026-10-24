//! cron (`#[event(scheduled)]`、1 時間ごと) の自動切り出し。テナント → 録画を取り込むカメラの順に巡り、
//! 1 カメラ分の流れ (`cam_ingest_api::auto::run_camera`) にカメラと R2 ([`CamRecorder`]) を渡す。
//! 手動 (`POST /auto-run`) も同じ [`run_tenant`] を呼び手のテナントだけに使う。ログは件数・カメラ id・所要時間だけ (宛先・資格情報を出さない)。

use alc_worker_db::PgClient;
use cam_ingest_api::auto::{run_camera, AutoCamera, ListedFile, Outcome, Recorder, DEFAULT_MAX_GROUPS};
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

/// 1 カメラ分の結果。`result` の Err は飛ばした理由か DB の失敗の kind (固定の語)
pub struct CameraRun {
    pub camera_id: Uuid,
    pub result: Result<Outcome, String>,
    /// 流れに入った (飛ばしていない)。cron の「N cameras」はこれを数える
    pub ran: bool,
}

/// `AUTO_CLIP_MAX_GROUPS` (cron と手動で共有)
pub fn max_groups(env: &Env) -> usize {
    env.var("AUTO_CLIP_MAX_GROUPS")
        .ok()
        .and_then(|v| v.to_string().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_GROUPS)
}

/// テナント 1 件ぶん。`only_camera` があればそのカメラだけ。鍵はカメラが在るときに 1 回だけ引く (`key` に残し、
/// 取れなければ全カメラを飛ばす)。カメラの一覧が引けなければ Err (ログは出した後)。`label` はログの先頭 (cron / manual)
pub async fn run_tenant(
    env: &Env,
    pg: &mut PgClient,
    tenant: Uuid,
    only_camera: Option<Uuid>,
    key: &mut Option<Option<String>>,
    max_groups: usize,
    label: &str,
) -> Result<Vec<CameraRun>, tokio_postgres::Error> {
    let cameras = match pg::recording_cameras(pg, tenant).await {
        Ok(c) => c,
        Err(e) => {
            console_error!("alc-cam: {label} cameras {}", alc_worker_db::kind(&e));
            return Err(e);
        }
    };
    let mut runs = Vec::new();
    for c in cameras.into_iter().filter(|c| only_camera.is_none_or(|id| id == c.id)) {
        let t0 = js_sys::Date::now();
        if key.is_none() {
            *key = Some(sso_key(env).await.ok());
        }
        let Some(k) = key.as_ref().and_then(|k| k.as_deref()) else {
            console_error!("alc-cam: {label} camera {}: no encryption key", c.id);
            runs.push(CameraRun { camera_id: c.id, result: Err("no encryption key".into()), ran: false });
            continue;
        };
        let mut rec = match recorder(env, tenant, &c, k) {
            Ok(r) => r,
            Err(reason) => {
                console_error!("alc-cam: {label} camera {}: skipped ({reason})", c.id);
                runs.push(CameraRun { camera_id: c.id, result: Err(format!("skipped ({reason})")), ran: false });
                continue;
            }
        };
        let cam = AutoCamera { id: c.id, scanned_until_ms: c.scanned_until_ms };
        let result = match run_camera(pg, tenant, &cam, &mut rec, js_sys::Date::now() as i64, max_groups).await {
            Ok(o) => {
                console_log!("alc-cam: {label} camera {} {} in {}ms", c.id, o.line(), (js_sys::Date::now() - t0) as i64);
                Ok(o)
            }
            Err(e) => {
                let kind = alc_worker_db::kind(&e);
                console_error!("alc-cam: {label} camera {} db {kind}", c.id);
                Err(format!("db {kind}"))
            }
        };
        runs.push(CameraRun { camera_id: c.id, result, ran: true });
    }
    Ok(runs)
}

pub async fn run(env: &Env) {
    let started = js_sys::Date::now();
    let max_groups = max_groups(env);
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
    let mut key: Option<Option<String>> = None;
    let mut cameras_done = 0usize;
    for tenant in tenants {
        // 失敗のログは run_tenant が出す
        if let Ok(runs) = run_tenant(env, &mut pg, tenant, None, &mut key, max_groups, "cron").await {
            cameras_done += runs.iter().filter(|r| r.ran).count();
        }
    }
    console_log!("alc-cam: cron {cameras_done} cameras in {}ms", (js_sys::Date::now() - started) as i64);
}
