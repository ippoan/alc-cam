//! ingest (`alc-cam-sd-ingest`) の口の中身。fetch・R2・Secrets Store を持たない部分をここに集め、
//! native のテスト (単体と tests/ の実 DB) で確かめる。直下の Worker は workers-rs に載せるだけ。
//!
//! 1 リクエストの流れ (Worker の `fetch`):
//! 1. [`caller`] — auth-worker が付け直したヘッダから呼び手を決める (role が admin・tenant が UUID)
//! 2. [`parse`] — method・path・本文から [`Op`] を作る。入力の検査と、宛先 (`rec_ip`) の検査 ([`dest`])、
//!    「宛先を書けるのは developer だけ」の検査 (POST の分) はここで終わる (DB に触らない)
//! 3. [`execute`] — カメラの台帳と端末の候補の口を DB で実行する (PATCH の「宛先を変えるか」は今の値と比べる)。
//!    切り出し (`CreateClip` / `GetClip`) は Worker が [`pg`] の関数とカメラ・R2 を組み合わせて行う
//!
//! 応答・ログに宛先 (`rec_ip`)・資格情報を出さない。一覧の応答は `rec_ip` を返すが、パスワードは返さない。

pub mod dest;
pub mod pg;

use alc_worker_db::PgClient;
use cam_sd::{jst_compact, jst_iso, parse_time, safe_segment};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::dest::Dest;
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

/// auth-worker が付け直したヘッダから決めた呼び手
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub tenant: Uuid,
    /// 開発者の Google ログイン (`X-User-Developer: 1`)。宛先 (`rec_ip`) を書けるのはこのときだけ
    pub developer: bool,
}

/// ヘッダから呼び手を決める。`header` は名前で引いて、無ければ `None`。
/// - `X-User-Role` が `admin` でない → 403
/// - `X-Tenant-ID` が無い (空を含む) → 401、UUID でない → 400
pub fn caller(header: impl Fn(&str) -> Option<String>) -> Result<Caller, ApiError> {
    if header("X-User-Role").as_deref() != Some("admin") {
        return Err(ApiError::new(403, "管理者だけが使えます"));
    }
    let tenant = header("X-Tenant-ID")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::new(401, "テナントがありません"))?;
    let tenant = Uuid::parse_str(&tenant)
        .map_err(|_| ApiError::new(400, "テナントの形が正しくありません"))?;
    Ok(Caller {
        tenant,
        developer: header("X-User-Developer").as_deref() == Some("1"),
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
}

/// `POST /cameras` の入力 (パスワードは平文のまま。暗号化は [`execute`] が鍵を受けて行う)
pub struct CameraInput {
    name: String,
    rec_ip: Option<Dest>,
    rec_username: Option<String>,
    rec_password: Option<String>,
    device_ids: Vec<String>,
}

/// `PATCH /cameras/{id}` の入力 (省略 = 変えない)
pub struct CameraPatchInput {
    name: Change<String>,
    rec_ip: Change<Option<Dest>>,
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
    rec_ip: Option<Option<String>>,
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

/// 宛先。null は「取り込まない」
fn rec_ip_of(v: Option<String>) -> Result<Option<Dest>, ApiError> {
    v.map(|s| Dest::parse(&s).ok_or_else(|| bad("rec_ip は私設帯 (10/8・172.16/12・192.168/16) の IPv4 にしてください (ポート・名前は不可)"))).transpose()
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
    let b: CameraBody = serde_json::from_str(body).map_err(|_| bad("本文は {\"name\", \"rec_ip\", \"rec_username\", \"rec_password\", \"device_ids\"} の JSON にしてください"))?;
    let rec_ip = rec_ip_of(b.rec_ip.flatten())?;
    if rec_ip.is_some() && !caller.developer {
        return Err(ApiError::new(
            403,
            "宛先 (rec_ip) を書けるのはシステム管理者だけです",
        ));
    }
    let rec_password = b.rec_password.flatten().filter(|p| !p.is_empty());
    if let Some(p) = &rec_password {
        password_ok(p)?;
    }
    Ok(CameraInput {
        name: name_of(b.name.flatten())?,
        rec_ip,
        rec_username: username_of(b.rec_username.flatten())?,
        rec_password,
        device_ids: device_ids_of(b.device_ids.unwrap_or_default())?,
    })
}

fn camera_patch(body: &str) -> Result<CameraPatchInput, ApiError> {
    let b: CameraBody = serde_json::from_str(body).map_err(|_| bad("本文は {\"name\", \"rec_ip\", \"rec_username\", \"rec_password\", \"device_ids\"} の JSON にしてください"))?;
    let name = match b.name {
        None => Change::Keep,
        Some(v) => Change::Set(name_of(v)?),
    };
    let rec_ip = match b.rec_ip {
        None => Change::Keep,
        Some(v) => Change::Set(rec_ip_of(v)?),
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
        rec_ip,
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

/// method・path (`PUBLIC_PATH_PREFIX` を剥がした後)・query (`?` の後)・本文から仕事を決める。DB には触らない
pub fn parse(
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    caller: &Caller,
    max_clip_ms: i64,
) -> Result<Op, ApiError> {
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

/// 失敗の文から宛先を消す (カメラへの fetch の失敗の文に URL が混ざっても出さない)
pub fn redact(message: &str, dest: &Dest) -> String {
    message.replace(&dest.canonical(), "<camera>")
}

fn camera_json(c: &CameraRow) -> Value {
    json!({
        "id": c.id,
        "name": c.name,
        "rec_ip": c.rec_ip,
        "rec_username": c.rec_username,
        "has_password": c.has_password,
        "recording": c.rec_ip.is_some(),
        "device_ids": c.device_ids,
        "rec_scanned_until": c.rec_scanned_until_ms.map(jst_iso),
    })
}

fn db_error(e: &tokio_postgres::Error) -> ApiError {
    if pg::is_rec_ip_conflict(e) {
        return ApiError::new(409, "その宛先は既に使われています");
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
            Ok(Reply {
                status: 200,
                body: json!({ "can_edit_destination": caller.developer, "cameras": cameras.iter().map(camera_json).collect::<Vec<_>>() }),
            })
        }
        Op::CreateCamera(c) => {
            let new = NewCamera {
                name: c.name,
                rec_ip: c.rec_ip.map(|d| d.canonical()),
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
                rec_ip: match p.rec_ip {
                    Change::Keep => Change::Keep,
                    Change::Set(d) => Change::Set(d.map(|d| d.canonical())),
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
                    "宛先 (rec_ip) を書けるのはシステム管理者だけです",
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
                    })
                })
                .collect();
            Ok(Reply {
                status: 200,
                body: Value::Array(list),
            })
        }
        Op::CreateClip { .. } | Op::GetClip { .. } => Err(ApiError::new(404, "Not Found")),
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
    fn writing_rec_ip_needs_developer() {
        let body = r#"{"name":"入口","rec_ip":"192.168.1.64"}"#;
        assert_eq!(
            status(parse("POST", "/cameras", "", body, &admin(false), 0)),
            403
        );
        assert_eq!(
            status(parse("POST", "/cameras", "", body, &admin(true), 0)),
            200
        );
        // rec_ip が null / 無しなら developer でなくても作れる
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"入口","rec_ip":null}"#,
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
        // 宛先の検査は developer でも通す
        assert_eq!(
            status(parse(
                "POST",
                "/cameras",
                "",
                r#"{"name":"x","rec_ip":"8.8.8.8"}"#,
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
                r#"{"name":"x","rec_ip":"192.168.1.64:80"}"#,
                &admin(true),
                0
            )),
            400
        );
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
    fn keys_and_redaction() {
        let t = Uuid::parse_str(TENANT).unwrap();
        let from = parse_time("2026-10-23T12:58:00+09:00").unwrap();
        let to = parse_time("2026-10-23T12:59:30+09:00").unwrap();
        assert_eq!(
            clip_r2_key(t, t, from, to),
            format!("clips/{TENANT}/{TENANT}/20261023T125800_20261023T125930.mp4")
        );
        assert_eq!(raw_r2_prefix(t, t), format!("{TENANT}/{TENANT}"));
        let d = Dest::parse("192.168.1.64").unwrap();
        assert_eq!(
            redact("fetch http://192.168.1.64/x failed", &d),
            "fetch http://<camera>/x failed"
        );
    }
}
