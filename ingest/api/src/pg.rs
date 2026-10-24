//! ingest の SQL (ここ 1 か所)。`PgClient::tenant_tx` + 型付きの名前なしの文 (`query_typed` 系) だけを使う
//! (Hyperdrive 経由では名前付き prepared statement で接続が切れる)。WHERE に tenant_id も明示する (RLS と二重)。
//!
//! 時刻は UNIX ミリ秒 (`int8`) で出し入れし、表示は呼び手が `cam_sd::jst_iso` で JST にする。

use std::collections::{BTreeSet, HashMap};

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
    /// 結び付けた端末。hub の ID は結び付いた機体の鍵の ID に畳んである ([`fold_device_ids`])
    pub device_ids: Vec<String>,
    /// `device_ids` のうち、結び付きの無い hub の ID (= `alarm:` で始まらない ID)。機体の鍵への置き換え待ち
    pub pending_device_ids: Vec<String>,
    /// 配信担当の端末 (先頭 = 1 位)。空なら `device_ids` の全員が 1 位 ([`my_cameras`])
    pub stream_device_ids: Vec<String>,
    pub rec_scanned_until_ms: Option<i64>,
    pub lan_ip: Option<String>,
    pub rtsp_port: i32,
    pub rtsp_path: Option<String>,
}
impl TxOutput for CameraRow {}

pub struct NewCamera {
    pub name: String,
    /// 検査済みの binding 名
    pub vpc_binding: Option<String>,
    pub rec_username: Option<String>,
    pub rec_password_encrypted: Option<String>,
    /// 検査済みの RTSP の接続先
    pub lan_ip: Option<String>,
    pub rtsp_port: i32,
    pub rtsp_path: Option<String>,
    pub device_ids: Vec<String>,
    /// 配信担当の端末 (先頭 = 1 位。検査済み = 重複なし・10 件まで)
    pub stream_device_ids: Vec<String>,
}

pub struct PatchCamera {
    pub name: Change<String>,
    /// 検査済みの binding 名。`Set(None)` = 取り込みをやめる
    pub vpc_binding: Change<Option<String>>,
    pub rec_username: Change<Option<String>>,
    pub rec_password_encrypted: Change<Option<String>>,
    pub lan_ip: Change<Option<String>>,
    pub rtsp_port: Change<i32>,
    pub rtsp_path: Change<Option<String>>,
    /// `Some` なら結び付ける端末を差し替える
    pub device_ids: Option<Vec<String>>,
    /// `Some` なら配信担当の端末 (順位) を差し替える
    pub stream_device_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    Updated(Box<CameraRow>),
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
    /// 開発端末の行 (RLS の is_dev 軸が true 側) から出した候補
    pub is_dev: bool,
}
impl TxOutput for Candidate {}

/// 拠点 PC の配信の鍵の端末が映すカメラの 1 行 (パスワードは暗号文のまま。復号は呼び手)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MyCamera {
    pub id: Uuid,
    pub name: String,
    pub lan_ip: Option<String>,
    pub rtsp_port: i32,
    pub rtsp_path: Option<String>,
    pub rec_username: Option<String>,
    pub rec_password_encrypted: Option<String>,
    /// 配信担当の順位 (1 = 主)。順位表が空のカメラは結び付けた端末の全員が 1
    pub rank: i32,
}
impl TxOutput for MyCamera {}

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
       (extract(epoch FROM c.rec_scanned_until) * 1000)::int8,
       c.lan_ip, c.rtsp_port, c.rtsp_path,
       ARRAY(SELECT s.device_id FROM alc_api.cam_stream_devices s
              WHERE s.tenant_id = c.tenant_id AND s.camera_id = c.id ORDER BY s.priority)
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
        pending_device_ids: Vec::new(),
        rec_scanned_until_ms: r.get(6),
        lan_ip: r.get(7),
        rtsp_port: r.get(8),
        rtsp_path: r.get(9),
        stream_device_ids: r.get(10),
    }
}

const REPLACE_DEVICES_DELETE: &str =
    "DELETE FROM alc_api.cam_camera_devices WHERE tenant_id = $1 AND camera_id = $2";
const REPLACE_DEVICES_INSERT: &str = "\
INSERT INTO alc_api.cam_camera_devices (tenant_id, camera_id, device_id)
SELECT $1, $2, x FROM unnest($3::text[]) AS x";
const REPLACE_STREAM_DELETE: &str =
    "DELETE FROM alc_api.cam_stream_devices WHERE tenant_id = $1 AND camera_id = $2";
/// 配列の並び = 順位 (1 始まり)
const REPLACE_STREAM_INSERT: &str = "\
INSERT INTO alc_api.cam_stream_devices (tenant_id, camera_id, device_id, priority)
SELECT $1, $2, x, n::int2 FROM unnest($3::text[]) WITH ORDINALITY AS t(x, n)";

/// そのテナントのカメラ全行 (`id` を渡すとその 1 行)
pub async fn list_cameras(
    pg: &mut PgClient,
    tenant: Uuid,
    id: Option<Uuid>,
) -> Result<Vec<CameraRow>, Error> {
    let mut rows: Vec<CameraRow> = pg
        .tenant_tx(tenant, move |tx| {
            Box::pin(async move {
                let rows = tx
                    .query_typed(SELECT_CAMERAS, &[(&tenant, Type::UUID), (&id, Type::UUID)])
                    .await?;
                Ok(rows.iter().map(camera_row).collect())
            })
        })
        .await?;
    fold_cameras(pg, tenant, &mut rows).await?;
    Ok(rows)
}

/// 作る (`active = false`: ONVIF の死活監視の対象にしない)。結び付ける端末も同じ transaction で入れる
pub async fn create_camera(
    pg: &mut PgClient,
    tenant: Uuid,
    c: NewCamera,
) -> Result<CameraRow, Error> {
    let mut row = pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let row = tx
                .query_typed_one(
                    "INSERT INTO alc_api.cameras (tenant_id, name, rec_ip, rec_username, rec_password_encrypted, lan_ip, rtsp_port, rtsp_path, active)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, false) RETURNING id",
                    &[
                        (&tenant, Type::UUID),
                        (&c.name, Type::TEXT),
                        (&c.vpc_binding, Type::TEXT),
                        (&c.rec_username, Type::TEXT),
                        (&c.rec_password_encrypted, Type::TEXT),
                        (&c.lan_ip, Type::TEXT),
                        (&c.rtsp_port, Type::INT4),
                        (&c.rtsp_path, Type::TEXT),
                    ],
                )
                .await?;
            let id: Option<Uuid> = Some(row.get(0));
            if !c.device_ids.is_empty() {
                tx.execute_typed(REPLACE_DEVICES_INSERT, &[(&tenant, Type::UUID), (&id, Type::UUID), (&c.device_ids, Type::TEXT_ARRAY)]).await?;
            }
            if !c.stream_device_ids.is_empty() {
                tx.execute_typed(REPLACE_STREAM_INSERT, &[(&tenant, Type::UUID), (&id, Type::UUID), (&c.stream_device_ids, Type::TEXT_ARRAY)]).await?;
            }
            let row = tx.query_typed_one(SELECT_CAMERAS, &[(&tenant, Type::UUID), (&id, Type::UUID)]).await?;
            Ok(camera_row(&row))
        })
    })
    .await?;
    fold_cameras(pg, tenant, std::slice::from_mut(&mut row)).await?;
    Ok(row)
}

/// 変える。宛先を今と違う値にする (null 化を含む) のは `developer` のときだけ ([`UpdateOutcome::Forbidden`])
pub async fn update_camera(
    pg: &mut PgClient,
    tenant: Uuid,
    id: Uuid,
    p: PatchCamera,
    developer: bool,
) -> Result<UpdateOutcome, Error> {
    let outcome = pg.tenant_tx(tenant, move |tx| {
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
            let (set_lan, lan) = (p.lan_ip.set(), p.lan_ip.value().cloned().flatten());
            let (set_port, port) = (p.rtsp_port.set(), p.rtsp_port.value().copied().unwrap_or(0));
            let (set_path, path) = (p.rtsp_path.set(), p.rtsp_path.value().cloned().flatten());
            tx.execute_typed(
                "UPDATE alc_api.cameras SET
                    name = CASE WHEN $3 THEN $4 ELSE name END,
                    rec_ip = CASE WHEN $5 THEN $6 ELSE rec_ip END,
                    rec_username = CASE WHEN $7 THEN $8 ELSE rec_username END,
                    rec_password_encrypted = CASE WHEN $9 THEN $10 ELSE rec_password_encrypted END,
                    lan_ip = CASE WHEN $11 THEN $12 ELSE lan_ip END,
                    rtsp_port = CASE WHEN $13 THEN $14 ELSE rtsp_port END,
                    rtsp_path = CASE WHEN $15 THEN $16 ELSE rtsp_path END,
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
                    (&set_lan, Type::BOOL),
                    (&lan, Type::TEXT),
                    (&set_port, Type::BOOL),
                    (&port, Type::INT4),
                    (&set_path, Type::BOOL),
                    (&path, Type::TEXT),
                ],
            )
            .await?;
            if let Some(ids) = &p.device_ids {
                tx.execute_typed(REPLACE_DEVICES_DELETE, &[(&tenant, Type::UUID), (&id, Type::UUID)]).await?;
                if !ids.is_empty() {
                    tx.execute_typed(REPLACE_DEVICES_INSERT, &[(&tenant, Type::UUID), (&id, Type::UUID), (ids, Type::TEXT_ARRAY)]).await?;
                }
            }
            if let Some(ids) = &p.stream_device_ids {
                tx.execute_typed(REPLACE_STREAM_DELETE, &[(&tenant, Type::UUID), (&id, Type::UUID)]).await?;
                if !ids.is_empty() {
                    tx.execute_typed(REPLACE_STREAM_INSERT, &[(&tenant, Type::UUID), (&id, Type::UUID), (ids, Type::TEXT_ARRAY)]).await?;
                }
            }
            let some_id = Some(id);
            let row = tx.query_typed_one(SELECT_CAMERAS, &[(&tenant, Type::UUID), (&some_id, Type::UUID)]).await?;
            Ok(UpdateOutcome::Updated(Box::new(camera_row(&row))))
        })
    })
    .await?;
    Ok(match outcome {
        UpdateOutcome::Updated(mut row) => {
            fold_cameras(pg, tenant, std::slice::from_mut(&mut *row)).await?;
            UpdateOutcome::Updated(row)
        }
        other => other,
    })
}

/// 端末 (`device`) が配信を受け持つカメラ (順位 → 名前の順)。RTSP の列が未設定の行も返す (呼び手が rtsp: null にする)。
/// 受け持ち = 配信担当の順位表 (`cam_stream_devices`) に在る。順位表が空のカメラだけは、結び付けた端末
/// (`cam_camera_devices`) の全員を 1 位とする (順位を入れる前の紐づけを止めないため。同順位は cam-room が先着にする)
pub async fn my_cameras(pg: &mut PgClient, tenant: Uuid, device: String) -> Result<Vec<MyCamera>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    "SELECT c.id, c.name, c.lan_ip, c.rtsp_port, c.rtsp_path, c.rec_username, c.rec_password_encrypted, r.rank
                       FROM alc_api.cameras c
                       JOIN LATERAL (
                            SELECT s.priority::int4 AS rank FROM alc_api.cam_stream_devices s
                             WHERE s.tenant_id = c.tenant_id AND s.camera_id = c.id AND s.device_id = $2
                            UNION ALL
                            SELECT 1 FROM alc_api.cam_camera_devices d
                             WHERE d.tenant_id = c.tenant_id AND d.camera_id = c.id AND d.device_id = $2
                               AND NOT EXISTS (SELECT 1 FROM alc_api.cam_stream_devices e
                                                WHERE e.tenant_id = c.tenant_id AND e.camera_id = c.id)
                       ) r ON true
                      WHERE c.tenant_id = $1
                      ORDER BY r.rank, c.name, c.id",
                    &[(&tenant, Type::UUID), (&device, Type::TEXT)],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| MyCamera {
                    id: r.get(0),
                    name: r.get(1),
                    lan_ip: r.get(2),
                    rtsp_port: r.get(3),
                    rtsp_path: r.get(4),
                    rec_username: r.get(5),
                    rec_password_encrypted: r.get(6),
                    rank: r.get(7),
                })
                .collect())
        })
    })
    .await
}

// ---- 鍵 ⇔ hub の結び付き (hub_measurements.via_device_id) ----

/// 結び付き (hub の ID → 機体の鍵の ID) の元。1 つの hub の ID に鍵が複数結び付いた (鍵の作り直し) ら最後に見えた方。
/// `$1` テナント。hub_measurements の RLS は is_dev 軸付きなので、見える行 = 今の transaction の軸の行だけ
macro_rules! link_select {
    () => {
        "SELECT DISTINCT ON (via_device_id) via_device_id AS hub_id, device_id AS key_id
      FROM alc_api.hub_measurements
     WHERE tenant_id = $1 AND via_device_id IS NOT NULL
     ORDER BY via_device_id, created_at DESC, id"
    };
}

/// 軸ごとの結び付きを合わせた対応表 (hub の ID → 鍵の ID)。本番の軸と開発端末の軸を別の transaction で引き、
/// 同じ hub の ID が両方に在れば本番の軸を採る
pub async fn device_links(pg: &mut PgClient, tenant: Uuid) -> Result<HashMap<String, String>, Error> {
    let mut links = HashMap::new();
    for is_dev in [false, true] {
        let rows: Vec<(String, String)> = pg
            .tenant_tx(tenant, move |tx| {
                Box::pin(async move {
                    if is_dev {
                        tx.query_typed(SET_DEVICE_DEV, &[]).await?;
                    }
                    let rows = tx.query_typed(link_select!(), &[(&tenant, Type::UUID)]).await?;
                    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
                })
            })
            .await?;
        for (hub, key) in rows {
            links.entry(hub).or_insert(key);
        }
    }
    Ok(links)
}

/// 結び付けた端末を機体の鍵の ID に畳む。(畳んだ ID (重複なし・並び替え済み), そのうち結び付きの無い hub の ID)。
/// 結び付きの無い ID はそのまま残り、`alarm:` で始まらないものが 2 つ目 (機体の鍵への置き換え待ち) に入る
pub fn fold_device_ids(ids: &[String], links: &HashMap<String, String>) -> (Vec<String>, Vec<String>) {
    let folded: BTreeSet<String> = ids.iter().map(|id| links.get(id).unwrap_or(id).clone()).collect();
    let pending = folded.iter().filter(|id| !id.starts_with("alarm:")).cloned().collect();
    (folded.into_iter().collect(), pending)
}

/// カメラの `device_ids` を畳む (結び付いた端末が 1 つも無ければ結び付きを引かない)
async fn fold_cameras(pg: &mut PgClient, tenant: Uuid, rows: &mut [CameraRow]) -> Result<(), Error> {
    if rows.iter().all(|r| r.device_ids.is_empty()) {
        return Ok(());
    }
    let links = device_links(pg, tenant).await?;
    for r in rows {
        (r.device_ids, r.pending_device_ids) = fold_device_ids(&r.device_ids, &links);
    }
    Ok(())
}

/// 直近 `days` 日にイベントを出した端末と、その種類ごとの件数 (新しい順)。
/// hub_measurements (alcohol / blood_pressure / timecard / license、時刻は COALESCE(recorded_at, created_at)) と
/// vein_enroll_events (registered_device_id)・measurements (alcohol / blood_pressure) を合わせる ([`events_cte!`])。
/// 端末は結び付きで機体の鍵の ID に畳み、`alarm:` で始まる ID (機体の鍵) の行だけを返す
/// (結び付きの無い hub の ID・`browser`・自己ペアの乱数 ID は出さない)。
/// 両表の RLS は is_dev 軸付きなので、同じ集計を軸ごとに 2 回流す: 本番の行 (`app.device_dev` 未設定) と、
/// 別の transaction で `set_config('app.device_dev', '1', true)` (transaction 単位) を立てた開発端末の行 (`is_dev = true`)。
/// 結び付きも軸ごとに引く (軸を跨いで畳まない)。並びは last_at の新しい順
pub async fn device_candidates(
    pg: &mut PgClient,
    tenant: Uuid,
    days: i32,
) -> Result<Vec<Candidate>, Error> {
    let mut all = candidates_in_axis(pg, tenant, days, false).await?;
    all.extend(candidates_in_axis(pg, tenant, days, true).await?);
    all.sort_by(|a, b| {
        b.last_at_ms
            .cmp(&a.last_at_ms)
            .then_with(|| a.device_id.cmp(&b.device_id))
            .then_with(|| a.is_dev.cmp(&b.is_dev))
    });
    Ok(all)
}

async fn candidates_in_axis(
    pg: &mut PgClient,
    tenant: Uuid,
    days: i32,
    is_dev: bool,
) -> Result<Vec<Candidate>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            if is_dev {
                // transaction 単位 (第 3 引数 true)。COMMIT で消えるので後の文に漏れない
                tx.query_typed("SELECT set_config('app.device_dev', '1', true)", &[])
                    .await?;
            }
            let rows = tx
                .query_typed(SELECT_CANDIDATES, &[(&tenant, Type::UUID), (&days, Type::INT4)])
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
                    is_dev,
                })
                .collect())
        })
    })
    .await
}

/// 端末の鍵の状態の 1 行 (畳んだ端末 × 軸。直近 days 日の件数)。`device_id` が `None` の行は端末 ID の無い測定
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStatus {
    pub device_id: Option<String>,
    /// 鍵の ID (`alarm:`) で入った hub_measurements の行数
    pub hub_via_key: i64,
    /// hub の ID で入った hub_measurements の行数 (鍵の行に畳まれた分と、結び付きの無い hub の分)
    pub hub_hub_id: i64,
    /// measurements (完了のアルコールと血圧のイベント) の行数
    pub measurements: i64,
    pub vein_enroll: i64,
    /// hub_measurements のうち kind が timecard の行数 (`hub_via_key` + `hub_hub_id` の内数)
    pub timecard: i64,
    pub last_at_ms: i64,
    pub is_dev: bool,
}
impl TxOutput for KeyStatus {}

/// 直近 `days` 日の端末ごとの記録の件数 (鍵の ID で入ったか hub の ID で入ったかを分けて、新しい順)。
/// 束ね方・結び付き・軸の流し方は [`device_candidates`] と同じ (本番の軸と開発端末の軸を別の transaction で)。
/// 候補と違い `alarm:` 以外の ID も、端末 ID の無い測定 (`device_id = None`) も返す
pub async fn device_key_status(pg: &mut PgClient, tenant: Uuid, days: i32) -> Result<Vec<KeyStatus>, Error> {
    let mut all = key_status_in_axis(pg, tenant, days, false).await?;
    all.extend(key_status_in_axis(pg, tenant, days, true).await?);
    all.sort_by(|a, b| {
        b.last_at_ms
            .cmp(&a.last_at_ms)
            .then_with(|| a.device_id.cmp(&b.device_id))
            .then_with(|| a.is_dev.cmp(&b.is_dev))
    });
    Ok(all)
}

async fn key_status_in_axis(pg: &mut PgClient, tenant: Uuid, days: i32, is_dev: bool) -> Result<Vec<KeyStatus>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            if is_dev {
                tx.query_typed(SET_DEVICE_DEV, &[]).await?;
            }
            let rows = tx
                .query_typed(SELECT_KEY_STATUS, &[(&tenant, Type::UUID), (&days, Type::INT4)])
                .await?;
            Ok(rows
                .iter()
                .map(|r| KeyStatus {
                    device_id: r.get(0),
                    hub_via_key: r.get(1),
                    hub_hub_id: r.get(2),
                    measurements: r.get(3),
                    vein_enroll: r.get(4),
                    timecard: r.get(5),
                    last_at_ms: r.get(6),
                    is_dev,
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

// ---- cron の自動切り出し (`crate::auto`) と、その日のイベントの口 ----

/// 録画を取り込むカメラを持つテナント。テナントの確定前なので nil のテナントの transaction で、SECURITY DEFINER の
/// 関数 (migration 187) だけを読む (RLS の表は 1 行も見えない)
pub async fn recording_tenants(pg: &mut PgClient) -> Result<Vec<Uuid>, Error> {
    pg.tenant_tx(Uuid::nil(), move |tx| {
        Box::pin(async move {
            let rows = tx.query_typed("SELECT alc_api.list_cam_recording_tenant_ids()", &[]).await?;
            Ok(rows.iter().map(|r| r.get::<_, Uuid>(0)).collect())
        })
    })
    .await
}

/// cron が切り出すカメラ (`rec_ip IS NOT NULL`) と、切り出しに要る値
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingCamera {
    pub id: Uuid,
    pub source: ClipSource,
    pub scanned_until_ms: Option<i64>,
}
impl TxOutput for RecordingCamera {}

pub async fn recording_cameras(pg: &mut PgClient, tenant: Uuid) -> Result<Vec<RecordingCamera>, Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            let rows = tx
                .query_typed(
                    "SELECT id, rec_ip, rec_username, rec_password_encrypted, (extract(epoch FROM rec_scanned_until) * 1000)::int8
                       FROM alc_api.cameras WHERE tenant_id = $1 AND rec_ip IS NOT NULL ORDER BY id",
                    &[(&tenant, Type::UUID)],
                )
                .await?;
            Ok(rows
                .iter()
                .map(|r| RecordingCamera {
                    id: r.get(0),
                    source: ClipSource { vpc_binding: r.get(1), rec_username: r.get(2), rec_password_encrypted: r.get(3) },
                    scanned_until_ms: r.get(4),
                })
                .collect())
        })
    })
    .await
}

/// イベントの元 (hub_measurements の 5 種 (試験記録 `test` を含む。MCP `create_test_record` が機体に作らせ、いつも開発の軸)・指静脈の登録・measurements のアルコールと血圧)。`$1` テナント。
/// 引数は期間の `[lo, hi)` (timestamptz の式。切り出しは UNIX ms の `$3`/`$4`、候補は直近 `$2` 日)。
/// 時刻の比較は ms を interval で足す (浮動小数の to_timestamp で境目がずれないように)。
/// `device_id` は結び付き (`link`: hub の ID → 機体の鍵の ID) で畳んだ値 — hub の ID の行は鍵の ID で出る。
/// measurements は `device_id IS NOT NULL` の行だけ (`measurements` = status completed のアルコール、時刻 measured_at /
/// `measurements_bp` = systolic と medical_measured_at のある血圧)。同じ (畳んだ) 端末・同じ kind の hub_measurements が
/// ±60 秒に在れば measurements 側は出さない (CoreS3 の PC で同じ測定が 2 回出るため)
///
/// 3 つ目の引数 `all` は端末の鍵の状態の口 ([`device_key_status`]) 用の形: measurements の重複落としをせず
/// (行数をそのまま数える)、`device_id` が NULL の行 (管理者ログイン・無認証の経路) も NULL の端末として出す。
/// `ev` には畳む前の ID (`raw_device_id`) も出す (鍵の ID で入った行と hub の ID で入った行を分けて数えるため)
macro_rules! events_cte {
    ($lo:literal, $hi:literal) => {
        events_cte!(
            @ $lo, $hi,
            "AND m.device_id IS NOT NULL",
            dedupe!("alcohol", "m.measured_at"),
            dedupe!("blood_pressure", "m.medical_measured_at")
        )
    };
    ($lo:literal, $hi:literal, all) => {
        events_cte!(@ $lo, $hi, "", "true", "true")
    };
    (@ $lo:literal, $hi:literal, $nn:literal, $dd_a:expr, $dd_b:expr) => {
        concat!(
            "WITH link AS (",
            link_select!(),
            "),
ev AS (
    SELECT 'hub_measurements'::text AS source_table, h.id, h.kind, COALESCE(h.recorded_at, h.created_at) AS at,
           COALESCE(l.key_id, h.device_id) AS device_id, h.device_id AS raw_device_id
      FROM alc_api.hub_measurements h
      LEFT JOIN link l ON l.hub_id = h.device_id
     WHERE h.tenant_id = $1
       AND h.kind IN ('alcohol', 'blood_pressure', 'timecard', 'license', 'test')
       AND COALESCE(h.recorded_at, h.created_at) >= ", $lo, "
       AND COALESCE(h.recorded_at, h.created_at) < ", $hi, "
    UNION ALL
    SELECT 'vein_enroll_events', v.id, 'vein_enroll', v.created_at, COALESCE(l.key_id, v.registered_device_id), v.registered_device_id
      FROM alc_api.vein_enroll_events v
      LEFT JOIN link l ON l.hub_id = v.registered_device_id
     WHERE v.tenant_id = $1 AND v.registered_device_id IS NOT NULL
       AND v.created_at >= ", $lo, "
       AND v.created_at < ", $hi, "
    UNION ALL
    SELECT 'measurements', m.id, 'alcohol', m.measured_at, COALESCE(l.key_id, m.device_id), m.device_id
      FROM alc_api.measurements m
      LEFT JOIN link l ON l.hub_id = m.device_id
     WHERE m.tenant_id = $1 AND m.status = 'completed' ", $nn, "
       AND m.measured_at >= ", $lo, "
       AND m.measured_at < ", $hi, "
       AND ", $dd_a, "
    UNION ALL
    SELECT 'measurements_bp', m.id, 'blood_pressure', m.medical_measured_at, COALESCE(l.key_id, m.device_id), m.device_id
      FROM alc_api.measurements m
      LEFT JOIN link l ON l.hub_id = m.device_id
     WHERE m.tenant_id = $1 AND m.systolic IS NOT NULL AND m.medical_measured_at IS NOT NULL ", $nn, "
       AND m.medical_measured_at >= ", $lo, "
       AND m.medical_measured_at < ", $hi, "
       AND ", $dd_b, "
)
"
        )
    };
}

/// measurements のイベントを落とす条件 (events_cte! の中で使う。`m` = measurements、`l` = 結び付き)。
/// 同じ (畳んだ) 端末・同じ kind の hub_measurements が時刻 `$at` の ±60 秒に無いこと
macro_rules! dedupe {
    ($kind:literal, $at:literal) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM alc_api.hub_measurements d LEFT JOIN link dl ON dl.hub_id = d.device_id
                    WHERE d.tenant_id = $1 AND d.kind = '", $kind, "'
                      AND COALESCE(dl.key_id, d.device_id) = COALESCE(l.key_id, m.device_id)
                      AND COALESCE(d.recorded_at, d.created_at) BETWEEN ", $at, " - interval '60 seconds' AND ", $at, " + interval '60 seconds')"
        )
    };
}

/// 結び付けたカメラの端末 (畳んだ ID)。カメラに hub の ID が結び付いていれば鍵の ID に、鍵の ID なら鍵のまま。
/// ev も畳んであるので、hub と鍵のどちらで結び付けていても両方の記録に当たる。`$1` テナント・`$2` カメラ
macro_rules! camera_devices {
    () => {
        "(SELECT COALESCE(cl.key_id, cd.device_id) FROM alc_api.cam_camera_devices cd
           LEFT JOIN link cl ON cl.hub_id = cd.device_id
          WHERE cd.tenant_id = $1 AND cd.camera_id = $2)"
    };
}

/// 候補の集計。イベントは [`events_cte!`] (直近 `days` 日)。畳んだ後で `alarm:` で始まる ID (機体の鍵) だけ。
/// 試験記録 (`test`) は数えない (候補に出す理由が無い。切り出し・その日の口・鍵の状態には出る)
const SELECT_CANDIDATES: &str = concat!(
    events_cte!("now() - make_interval(days => $2)", "'infinity'::timestamptz"),
    "SELECT ev.device_id,
       count(*) FILTER (WHERE ev.kind = 'alcohol'),
       count(*) FILTER (WHERE ev.kind = 'blood_pressure'),
       count(*) FILTER (WHERE ev.kind = 'timecard'),
       count(*) FILTER (WHERE ev.kind = 'license'),
       count(*) FILTER (WHERE ev.kind = 'vein_enroll'),
       (extract(epoch FROM max(ev.at)) * 1000)::int8
  FROM ev
 WHERE ev.device_id LIKE 'alarm:%' AND ev.kind <> 'test'
 GROUP BY ev.device_id
 ORDER BY max(ev.at) DESC, ev.device_id"
);

/// 端末の鍵の状態の集計 ([`device_key_status`])。イベントは [`events_cte!`] の `all` 形 (直近 `$2` 日)。
/// 畳んだ端末ごとに、hub_measurements を鍵の ID で入った行 (畳む前の ID が `alarm:`) と hub の ID で入った行
/// (畳まれた行と結び付きの無い行) に分け、measurements (アルコール + 血圧)・vein_enroll・timecard
/// (hub_measurements の内数) を数える。`device_id` が NULL の組は端末 ID の無い測定
const SELECT_KEY_STATUS: &str = concat!(
    events_cte!("now() - make_interval(days => $2)", "'infinity'::timestamptz", all),
    "SELECT ev.device_id,
       count(*) FILTER (WHERE ev.source_table = 'hub_measurements' AND ev.raw_device_id LIKE 'alarm:%'),
       count(*) FILTER (WHERE ev.source_table = 'hub_measurements' AND ev.raw_device_id NOT LIKE 'alarm:%'),
       count(*) FILTER (WHERE ev.source_table IN ('measurements', 'measurements_bp')),
       count(*) FILTER (WHERE ev.source_table = 'vein_enroll_events'),
       count(*) FILTER (WHERE ev.source_table = 'hub_measurements' AND ev.kind = 'timecard'),
       (extract(epoch FROM max(ev.at)) * 1000)::int8
  FROM ev
 GROUP BY ev.device_id
 ORDER BY max(ev.at) DESC, ev.device_id"
);

/// 結び付けた端末の、まだそのカメラで切っていないイベント
const SELECT_AUTO_EVENTS: &str = concat!(
    events_cte!(
        "'epoch'::timestamptz + $3::int8 * interval '1 millisecond'",
        "'epoch'::timestamptz + $4::int8 * interval '1 millisecond'"
    ),
    "SELECT ev.source_table, ev.id, ev.kind, (extract(epoch FROM ev.at) * 1000)::int8, ev.device_id
  FROM ev
 WHERE ev.device_id IN ",
    camera_devices!(),
    "
   AND NOT EXISTS (SELECT 1 FROM alc_api.cam_clip_events e
                    WHERE e.tenant_id = $1 AND e.camera_id = $2 AND e.source_table = ev.source_table AND e.source_id = ev.id)
 ORDER BY ev.at, ev.source_table, ev.id"
);

/// その日の口: 結び付けた端末のイベント (と、結び付けを外した後も切り出し済みのイベント) と、切り出し済みならその clip
const SELECT_DAY_EVENTS: &str = concat!(
    events_cte!(
        "'epoch'::timestamptz + $3::int8 * interval '1 millisecond'",
        "'epoch'::timestamptz + $4::int8 * interval '1 millisecond'"
    ),
    "SELECT ev.source_table, ev.id, ev.kind, (extract(epoch FROM ev.at) * 1000)::int8, ev.device_id,
       c.id, c.r2_key, c.status, c.error
  FROM ev
  LEFT JOIN alc_api.cam_clip_events e
         ON e.tenant_id = $1 AND e.camera_id = $2 AND e.source_table = ev.source_table AND e.source_id = ev.id
  LEFT JOIN alc_api.cam_clips c ON c.tenant_id = e.tenant_id AND c.id = e.clip_id
 WHERE ev.device_id IN ",
    camera_devices!(),
    "
    OR e.clip_id IS NOT NULL
 ORDER BY ev.at, ev.source_table, ev.id"
);

/// 開発端末の軸の transaction の頭で流す (transaction 単位。COMMIT で消える)
const SET_DEVICE_DEV: &str = "SELECT set_config('app.device_dev', '1', true)";

/// 拾ったイベント (`[from, to)`、本番の行と開発端末の行。軸ごとに別の transaction)
pub async fn auto_events(
    pg: &mut PgClient,
    tenant: Uuid,
    camera_id: Uuid,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<crate::auto::Event>, Error> {
    let mut all = Vec::new();
    for is_dev in [false, true] {
        let rows = pg
            .tenant_tx(tenant, move |tx| {
                Box::pin(async move {
                    if is_dev {
                        tx.query_typed(SET_DEVICE_DEV, &[]).await?;
                    }
                    let rows = tx
                        .query_typed(
                            SELECT_AUTO_EVENTS,
                            &[(&tenant, Type::UUID), (&camera_id, Type::UUID), (&from_ms, Type::INT8), (&to_ms, Type::INT8)],
                        )
                        .await?;
                    Ok(rows
                        .iter()
                        .map(|r| crate::auto::Event {
                            source_table: r.get(0),
                            source_id: r.get(1),
                            kind: r.get(2),
                            at_ms: r.get(3),
                            device_id: r.get(4),
                            is_dev,
                        })
                        .collect::<Vec<_>>())
                })
            })
            .await?;
        all.extend(rows);
    }
    Ok(all)
}

/// 切り出しに含めたイベントを残す (is_dev=false は通常の transaction、true は開発端末の軸の transaction)。
/// 同じカメラ・元の表・元の行が既に在れば何もしない
pub async fn record_clip_events(
    pg: &mut PgClient,
    tenant: Uuid,
    camera_id: Uuid,
    clip_id: Uuid,
    events: Vec<crate::auto::Event>,
) -> Result<(), Error> {
    for is_dev in [false, true] {
        let mine: Vec<&crate::auto::Event> = events.iter().filter(|e| e.is_dev == is_dev).collect();
        if mine.is_empty() {
            continue;
        }
        let tables: Vec<String> = mine.iter().map(|e| e.source_table.clone()).collect();
        let ids: Vec<Uuid> = mine.iter().map(|e| e.source_id).collect();
        let kinds: Vec<String> = mine.iter().map(|e| e.kind.clone()).collect();
        let ats: Vec<i64> = mine.iter().map(|e| e.at_ms).collect();
        let devices: Vec<String> = mine.iter().map(|e| e.device_id.clone()).collect();
        pg.tenant_tx(tenant, move |tx| {
            Box::pin(async move {
                if is_dev {
                    tx.query_typed(SET_DEVICE_DEV, &[]).await?;
                }
                tx.execute_typed(
                    "INSERT INTO alc_api.cam_clip_events (tenant_id, camera_id, clip_id, source_table, source_id, kind, occurred_at, device_id, is_dev)
                     SELECT $1, $2, $3, t, i, k, 'epoch'::timestamptz + a * interval '1 millisecond', d, $9
                       FROM unnest($4::text[], $5::uuid[], $6::text[], $7::int8[], $8::text[]) AS x(t, i, k, a, d)
                     ON CONFLICT (tenant_id, camera_id, source_table, source_id) DO NOTHING",
                    &[
                        (&tenant, Type::UUID),
                        (&camera_id, Type::UUID),
                        (&clip_id, Type::UUID),
                        (&tables, Type::TEXT_ARRAY),
                        (&ids, Type::UUID_ARRAY),
                        (&kinds, Type::TEXT_ARRAY),
                        (&ats, Type::INT8_ARRAY),
                        (&devices, Type::TEXT_ARRAY),
                        (&is_dev, Type::BOOL),
                    ],
                )
                .await?;
                Ok(())
            })
        })
        .await?;
    }
    Ok(())
}

/// 走査済み時刻を書く
pub async fn set_scanned_until(pg: &mut PgClient, tenant: Uuid, camera_id: Uuid, until_ms: i64) -> Result<(), Error> {
    pg.tenant_tx(tenant, move |tx| {
        Box::pin(async move {
            tx.execute_typed(
                "UPDATE alc_api.cameras SET rec_scanned_until = 'epoch'::timestamptz + $3::int8 * interval '1 millisecond'
                  WHERE tenant_id = $1 AND id = $2",
                &[(&tenant, Type::UUID), (&camera_id, Type::UUID), (&until_ms, Type::INT8)],
            )
            .await?;
            Ok(())
        })
    })
    .await
}

/// 切り出し済みの clip (その日の口)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipRef {
    pub clip_id: Uuid,
    pub key: String,
    pub status: String,
    pub error: Option<String>,
}

/// その日の口の 1 行
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayEvent {
    pub event: crate::auto::Event,
    pub clip: Option<ClipRef>,
}
impl TxOutput for DayEvent {}

/// カメラの `[from, to)` のイベント (本番と開発端末。時刻の順)
pub async fn day_events(
    pg: &mut PgClient,
    tenant: Uuid,
    camera_id: Uuid,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<DayEvent>, Error> {
    let mut all = Vec::new();
    for is_dev in [false, true] {
        let rows = pg
            .tenant_tx(tenant, move |tx| {
                Box::pin(async move {
                    if is_dev {
                        tx.query_typed(SET_DEVICE_DEV, &[]).await?;
                    }
                    let rows = tx
                        .query_typed(
                            SELECT_DAY_EVENTS,
                            &[(&tenant, Type::UUID), (&camera_id, Type::UUID), (&from_ms, Type::INT8), (&to_ms, Type::INT8)],
                        )
                        .await?;
                    Ok(rows
                        .iter()
                        .map(|r| DayEvent {
                            event: crate::auto::Event {
                                source_table: r.get(0),
                                source_id: r.get(1),
                                kind: r.get(2),
                                at_ms: r.get(3),
                                device_id: r.get(4),
                                is_dev,
                            },
                            clip: r.get::<_, Option<Uuid>>(5).map(|clip_id| ClipRef {
                                clip_id,
                                key: r.get(6),
                                status: r.get(7),
                                error: r.get(8),
                            }),
                        })
                        .collect::<Vec<_>>())
                })
            })
            .await?;
        all.extend(rows);
    }
    all.sort_by(|a, b| {
        a.event
            .at_ms
            .cmp(&b.event.at_ms)
            .then_with(|| a.event.source_table.cmp(&b.event.source_table))
            .then_with(|| a.event.source_id.cmp(&b.event.source_id))
    });
    Ok(all)
}
