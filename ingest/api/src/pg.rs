//! ingest の SQL (ここ 1 か所)。`PgClient::tenant_tx` + 型付きの名前なしの文 (`query_typed` 系) だけを使う
//! (Hyperdrive 経由では名前付き prepared statement で接続が切れる)。WHERE に tenant_id も明示する (RLS と二重)。
//!
//! 時刻は UNIX ミリ秒 (`int8`) で出し入れし、表示は呼び手が `cam_sd::jst_iso` で JST にする。

use alc_worker_db::{PgClient, TxOutput};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{Error, Row};
use uuid::Uuid;

/// 省略 = 変えない / 値 = その値にする
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change<T> {
    Keep,
    Set(T),
}

impl<T> Change<T> {
    pub(crate) fn set(&self) -> bool {
        matches!(self, Change::Set(_))
    }

    pub(crate) fn value(&self) -> Option<&T> {
        match self {
            Change::Keep => None,
            Change::Set(v) => Some(v),
        }
    }
}

/// カメラの 1 行 (パスワードは「在るか」だけ)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRow {
    pub id: Uuid,
    pub name: String,
    pub vpc_binding: Option<String>,
    pub rec_username: Option<String>,
    pub has_password: bool,
    pub device_ids: Vec<String>,
    pub rec_scanned_until_ms: Option<i64>,
}
impl TxOutput for CameraRow {}

pub struct NewCamera {
    pub name: String,
    /// 検査済みの binding 名
    pub vpc_binding: Option<String>,
    pub rec_username: Option<String>,
    pub rec_password_encrypted: Option<String>,
    pub device_ids: Vec<String>,
}

pub struct PatchCamera {
    pub name: Change<String>,
    /// 検査済みの binding 名。`Set(None)` = 取り込みをやめる
    pub vpc_binding: Change<Option<String>>,
    pub rec_username: Change<Option<String>>,
    pub rec_password_encrypted: Change<Option<String>>,
    /// `Some` なら結び付ける端末を差し替える
    pub device_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated(CameraRow),
    NotFound,
    /// developer でないのに宛先を変えようとした (何も変えていない)
    Forbidden,
}
impl TxOutput for UpdateOutcome {}

/// 端末の候補の 1 行 (直近 days 日の件数)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub device_id: String,
    pub alcohol: i64,
    pub blood_pressure: i64,
    pub timecard: i64,
    pub license: i64,
    pub vein_enroll: i64,
    pub last_at_ms: i64,
}
impl TxOutput for Candidate {}

/// 切り出しに要るカメラの値 (宛先は fetch の直前にもう一度検査する)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipSource {
    pub vpc_binding: Option<String>,
    pub rec_username: Option<String>,
    pub rec_password_encrypted: Option<String>,
}
impl TxOutput for ClipSource {}

/// `cam_clips` に残す 1 本
pub struct ClipRecord {
    pub camera_id: Uuid,
    pub from_ms: i64,
    pub to_ms: i64,
    pub r2_key: String,
    pub size_bytes: Option<i64>,
    /// `ready` / `failed`
    pub status: &'static str,
    pub error: Option<String>,
}

/// 他テナントを含めて同じ binding が既に在る (部分 unique `cameras_rec_ip_key`。同じカメラを 2 テナントに置けない)
pub fn is_vpc_binding_conflict(e: &Error) -> bool {
    e.as_db_error().is_some_and(|d| {
        d.code() == &SqlState::UNIQUE_VIOLATION && d.constraint() == Some("cameras_rec_ip_key")
    })
}

// ★ 列 `rec_ip` に入っているのは IP ではなく VPC Service の binding 名 (API の `vpc_binding`。migration はしていない)
/// 一覧の文。`$2` が NULL なら全行、UUID ならその 1 行
const SELECT_CAMERAS: &str = "\
SELECT c.id, c.name, c.rec_ip, c.rec_username, c.rec_password_encrypted IS NOT NULL,
       COALESCE(array_agg(d.device_id ORDER BY d.device_id) FILTER (WHERE d.device_id IS NOT NULL), '{}'::text[]),
       (extract(epoch FROM c.rec_scanned_until) * 1000)::int8
  FROM alc_api.cameras c
  LEFT JOIN alc_api.cam_camera_devices d ON d.tenant_id = c.tenant_id AND d.camera_id = c.id
 WHERE c.tenant_id = $1 AND ($2::uuid IS NULL OR c.id = $2)
 GROUP BY c.id
 ORDER BY c.name, c.id";

fn camera_row(r: &Row) -> CameraRow {
    CameraRow {
        id: r.get(0),
        name: r.get(1),
        vpc_binding: r.get(2),
        rec_username: r.get(3),
        has_password: r.get(4),
        device_ids: r.get(5),
        rec_scanned_until_ms: r.get(6),
    }
}

const REPLACE_DEVICES_DELETE: &str =
    "DELETE FROM alc_api.cam_camera_devices WHERE tenant_id = $1 AND camera_id = $2";
const REPLACE_DEVICES_INSERT: &str = "\
INSERT INTO alc_api.cam_camera_devices (tenant_id, camera_id, device_id)
SELECT $1, $2, x FROM unnest($3::text[]) AS x";

/// そのテナントのカメラ全行 (`id` を渡すとその 1 行)
pub async fn list_cameras(
    pg: &mut PgClient,
    tenant: Uuid,
    id: Option<Uuid>,
) -> Result<Vec<CameraRow>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(SELECT_CAMERAS, &[(&tenant, Type::UUID), (&id, Type::UUID)])
                .await?;
            Ok(rows.iter().map(camera_row).collect())
        })
    })
    .await
}

/// 作る (`active = false`: ONVIF の死活監視の対象にしない)。結び付ける端末も同じ transaction で入れる
pub async fn create_camera(
    pg: &mut PgClient,
    tenant: Uuid,
    c: NewCamera,
) -> Result<CameraRow, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_one(
                    "INSERT INTO alc_api.cameras (tenant_id, name, rec_ip, rec_username, rec_password_encrypted, active)
                     VALUES ($1, $2, $3, $4, $5, false) RETURNING id",
                    &[
                        (&tenant, Type::UUID),
                        (&c.name, Type::TEXT),
                        (&c.vpc_binding, Type::TEXT),
                        (&c.rec_username, Type::TEXT),
                        (&c.rec_password_encrypted, Type::TEXT),
                    ],
                )
                .await?;
            let id: Option<Uuid> = Some(row.get(0));
            if !c.device_ids.is_empty() {
                tx.execute_typed(REPLACE_DEVICES_INSERT, &[(&tenant, Type::UUID), (&id, Type::UUID), (&c.device_ids, Type::TEXT_ARRAY)]).await?;
            }
            let row = tx.query_typed_one(SELECT_CAMERAS, &[(&tenant, Type::UUID), (&id, Type::UUID)]).await?;
            Ok(camera_row(&row))
        })
    })
    .await
}

/// 変える。宛先を今と違う値にする (null 化を含む) のは `developer` のときだけ ([`UpdateOutcome::Forbidden`])
pub async fn update_camera(
    pg: &mut PgClient,
    tenant: Uuid,
    id: Uuid,
    p: PatchCamera,
    developer: bool,
) -> Result<UpdateOutcome, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let Some(current) = tx
                .query_typed_opt("SELECT rec_ip FROM alc_api.cameras WHERE tenant_id = $1 AND id = $2 FOR UPDATE", &[(&tenant, Type::UUID), (&id, Type::UUID)])
                .await?
            else {
                return Ok(UpdateOutcome::NotFound);
            };
            let current: Option<String> = current.get(0);
            if p.vpc_binding.value().is_some_and(|new| *new != current) && !developer {
                return Ok(UpdateOutcome::Forbidden);
            }
            let (set_name, name) = (p.name.set(), p.name.value().cloned());
            let (set_ip, ip) = (p.vpc_binding.set(), p.vpc_binding.value().cloned().flatten());
            let (set_user, user) = (p.rec_username.set(), p.rec_username.value().cloned().flatten());
            let (set_pass, pass) = (p.rec_password_encrypted.set(), p.rec_password_encrypted.value().cloned().flatten());
            tx.execute_typed(
                "UPDATE alc_api.cameras SET
                    name = CASE WHEN $3 THEN $4 ELSE name END,
                    rec_ip = CASE WHEN $5 THEN $6 ELSE rec_ip END,
                    rec_username = CASE WHEN $7 THEN $8 ELSE rec_username END,
                    rec_password_encrypted = CASE WHEN $9 THEN $10 ELSE rec_password_encrypted END,
                    updated_at = now()
                  WHERE tenant_id = $1 AND id = $2",
                &[
                    (&tenant, Type::UUID),
                    (&id, Type::UUID),
                    (&set_name, Type::BOOL),
                    (&name, Type::TEXT),
                    (&set_ip, Type::BOOL),
                    (&ip, Type::TEXT),
                    (&set_user, Type::BOOL),
                    (&user, Type::TEXT),
                    (&set_pass, Type::BOOL),
                    (&pass, Type::TEXT),
                ],
            )
            .await?;
            if let Some(ids) = &p.device_ids {
                tx.execute_typed(REPLACE_DEVICES_DELETE, &[(&tenant, Type::UUID), (&id, Type::UUID)]).await?;
                if !ids.is_empty() {
                    tx.execute_typed(REPLACE_DEVICES_INSERT, &[(&tenant, Type::UUID), (&id, Type::UUID), (ids, Type::TEXT_ARRAY)]).await?;
                }
            }
            let some_id = Some(id);
            let row = tx.query_typed_one(SELECT_CAMERAS, &[(&tenant, Type::UUID), (&some_id, Type::UUID)]).await?;
            Ok(UpdateOutcome::Updated(camera_row(&row)))
        })
    })
    .await
}

/// 直近 `days` 日にイベントを出した端末と、その種類ごとの件数 (新しい順)。
/// hub_measurements (alcohol / blood_pressure / timecard / license、時刻は COALESCE(recorded_at, created_at)) と
/// vein_enroll_events (registered_device_id) を合わせる。is_dev の行は RLS で見えない (app.device_dev を設定しない)
pub async fn device_candidates(
    pg: &mut PgClient,
    tenant: Uuid,
    days: i32,
) -> Result<Vec<Candidate>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    "WITH ev AS (
                        SELECT device_id, kind, COALESCE(recorded_at, created_at) AS at
                          FROM alc_api.hub_measurements
                         WHERE tenant_id = $1 AND device_id IS NOT NULL
                           AND kind IN ('alcohol', 'blood_pressure', 'timecard', 'license')
                           AND COALESCE(recorded_at, created_at) >= now() - make_interval(days => $2)
                        UNION ALL
                        SELECT registered_device_id, 'vein_enroll', created_at
                          FROM alc_api.vein_enroll_events
                         WHERE tenant_id = $1 AND registered_device_id IS NOT NULL
                           AND created_at >= now() - make_interval(days => $2)
                    )
                    SELECT device_id,
                           count(*) FILTER (WHERE kind = 'alcohol'),
                           count(*) FILTER (WHERE kind = 'blood_pressure'),
                           count(*) FILTER (WHERE kind = 'timecard'),
                           count(*) FILTER (WHERE kind = 'license'),
                           count(*) FILTER (WHERE kind = 'vein_enroll'),
                           (extract(epoch FROM max(at)) * 1000)::int8
                      FROM ev
                     GROUP BY device_id
                     ORDER BY max(at) DESC, device_id",
                    &[(&tenant, Type::UUID), (&days, Type::INT4)],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| Candidate {
                    device_id: r.get(0),
                    alcohol: r.get(1),
                    blood_pressure: r.get(2),
                    timecard: r.get(3),
                    license: r.get(4),
                    vein_enroll: r.get(5),
                    last_at_ms: r.get(6),
                })
                .collect())
        })
    })
    .await
}

/// 切り出しに要るカメラの値。無ければ `None`
pub async fn clip_source(
    pg: &mut PgClient,
    tenant: Uuid,
    camera_id: Uuid,
) -> Result<Option<ClipSource>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_opt(
                    "SELECT rec_ip, rec_username, rec_password_encrypted FROM alc_api.cameras WHERE tenant_id = $1 AND id = $2",
                    &[(&tenant, Type::UUID), (&camera_id, Type::UUID)],
                )
                .await?;
            Ok(row.map(|r| ClipSource { vpc_binding: r.get(0), rec_username: r.get(1), rec_password_encrypted: r.get(2) }))
        })
    })
    .await
}

/// 切り出しの結果を残す。同じカメラ・同じ開始の行は上書きする (作り直し・失敗のやり直し)
pub async fn record_clip(pg: &mut PgClient, tenant: Uuid, c: ClipRecord) -> Result<Uuid, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let status = c.status.to_owned();
            let row = tx
                .query_typed_one(
                    "INSERT INTO alc_api.cam_clips (tenant_id, camera_id, started_at, ended_at, r2_key, size_bytes, status, error)
                     VALUES ($1, $2, to_timestamp($3::int8 / 1000.0), to_timestamp($4::int8 / 1000.0), $5, $6, $7, $8)
                     ON CONFLICT (tenant_id, camera_id, started_at) DO UPDATE SET
                        ended_at = EXCLUDED.ended_at, r2_key = EXCLUDED.r2_key, size_bytes = EXCLUDED.size_bytes,
                        status = EXCLUDED.status, error = EXCLUDED.error, created_at = now()
                     RETURNING id",
                    &[
                        (&tenant, Type::UUID),
                        (&c.camera_id, Type::UUID),
                        (&c.from_ms, Type::INT8),
                        (&c.to_ms, Type::INT8),
                        (&c.r2_key, Type::TEXT),
                        (&c.size_bytes, Type::INT8),
                        (&status, Type::TEXT),
                        (&c.error, Type::TEXT),
                    ],
                )
                .await?;
            Ok(row.get::<_, Uuid>(0))
        })
    })
    .await
}
