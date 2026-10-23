//! cron (1 時間ごと) の自動切り出し: 結び付けた端末のイベントの前後 10 秒を、カメラの SD 録画から切って R2 に置く。
//!
//! 1 カメラ分の流れ ([`run_camera`]):
//! 1. 走査の範囲 `[from, upper)` — `from = rec_scanned_until` (NULL なら `now - 2 時間`)、`upper = now`
//! 2. その範囲のイベント (本番の行と開発端末の行。`cam_clip_events` に在るものは除く。[`pg::auto_events`])
//! 3. 各イベントの `[t - 10 秒, t + 10 秒]` を、重なる・接するものは 1 本にまとめる ([`group_events`])
//! 4. SD の一覧を取り、閉じたファイルが覆う終わり ([`closed_until`]) までのまとまりだけを切る。越えるまとまりと
//!    それ以降・上限 (カメラあたり `max_groups`) を超えた分は次の回へ ([`plan`])。固定の「N 分前まで」は使わない
//!    (サブストリームではファイルが閉じるまでが長い)
//! 5. まとまりごとに、かかる SD ファイルを R2 へ (1 回の中で 1 ファイル 1 回だけ取り寄せる) → 切る → `cam_clips` に
//!    1 行 (ready / failed) → `cam_clip_events` にイベントを 1 行ずつ。失敗しても再試行しない (failed の行とイベントが残る)
//! 6. `rec_scanned_until` を、次の回へ回したまとまりがあればその頭 (最初のイベント - 10 秒)、無ければ `upper` にする。
//!    SD の一覧が取れなかった回は進めない (次の回に同じ範囲から見直す)
//!
//! カメラと R2 は [`Recorder`] の向こう (Worker が実装し、tests/ は差し替える)。

use std::collections::HashMap;

use alc_worker_db::{PgClient, TxOutput};
use cam_sd::{file_start_ms, safe_segment, wanted, SdFile};
use uuid::Uuid;

use crate::{clip_r2_key, pg};

/// イベントの前後に残す長さ
pub const PAD_MS: i64 = 10_000;
/// 1 回で切るまとまりのカメラあたりの上限の既定 (vars `AUTO_CLIP_MAX_GROUPS`)
pub const DEFAULT_MAX_GROUPS: usize = 40;
/// `rec_scanned_until` が NULL (初回) のとき、どこから見るか
pub const FIRST_LOOKBACK_MS: i64 = 2 * 3_600_000;
/// SD の一覧を、最初のまとまりの頭よりどれだけ前から見るか (頭にかかるファイルはそれより前に始まる。
/// サブストリームの 1 本は長いので 1 時間のフォルダを 1 つ余分に見る)
pub const LIST_LEAD_MS: i64 = 3_600_000;

/// 拾うイベント (`cam_clip_events` の 1 行になる)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// `hub_measurements` / `vein_enroll_events`
    pub source_table: String,
    pub source_id: Uuid,
    /// hub_measurements の kind、指静脈の登録は `vein_enroll`
    pub kind: String,
    pub at_ms: i64,
    pub device_id: String,
    /// 開発端末の行 (RLS の is_dev 軸が true 側)
    pub is_dev: bool,
}
impl TxOutput for Event {}

/// 1 本に切る範囲と、そこに入るイベント
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub from_ms: i64,
    pub to_ms: i64,
    pub events: Vec<Event>,
}

/// イベントの前後 `pad_ms` を窓にし、重なる・接する窓を 1 本にまとめる (時刻の順)
pub fn group_events(mut events: Vec<Event>, pad_ms: i64) -> Vec<Group> {
    events.sort_by(|a, b| {
        a.at_ms
            .cmp(&b.at_ms)
            .then_with(|| a.source_table.cmp(&b.source_table))
            .then_with(|| a.source_id.cmp(&b.source_id))
    });
    let mut out: Vec<Group> = Vec::new();
    for e in events {
        let (from, to) = (e.at_ms - pad_ms, e.at_ms + pad_ms);
        match out.last_mut() {
            Some(g) if from <= g.to_ms => {
                g.to_ms = g.to_ms.max(to);
                g.events.push(e);
            }
            _ => out.push(Group { from_ms: from, to_ms: to, events: vec![e] }),
        }
    }
    out
}

/// SD の一覧の 1 本 (閉じていない書き込み中のファイルも含む)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFile {
    /// カメラ名の下のフォルダ (`Schedule`)
    pub kind: String,
    pub date: String,
    pub hour: String,
    pub name: String,
    pub size: u64,
    pub start_ms: i64,
    /// 閉じた (書き終わった) ファイルか。`_!` を含む名前は書き込み途中 ([`cam_sd::wanted`])
    pub closed: bool,
}

/// 時のフォルダごとの一覧 (`kind`, `date`, `hour`, ファイル) を、開始時刻の順の 1 列にする。MP4 で、名前が
/// フォルダとして安全で、開始時刻が読めるものだけ (書き込み途中は `_!` を外して時刻を読む)
pub fn listed_files(dirs: Vec<(String, String, String, Vec<SdFile>)>) -> Vec<ListedFile> {
    let mut out: Vec<ListedFile> = dirs
        .into_iter()
        .flat_map(|(kind, date, hour, files)| {
            files.into_iter().filter_map(move |f| {
                if !(f.name.ends_with(".mp4") && safe_segment(&f.name.replace("_!", "_x"))) {
                    return None;
                }
                let start_ms = file_start_ms(&f.name.replace("_!", ""))?;
                Some(ListedFile { kind: kind.clone(), date: date.clone(), hour: hour.clone(), closed: wanted(&f.name), name: f.name, size: f.size, start_ms })
            })
        })
        .collect();
    out.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then_with(|| a.name.cmp(&b.name)));
    out.dedup_by(|a, b| a.start_ms == b.start_ms && a.name == b.name);
    out
}

/// 各ファイルの終わり = 一覧で次のファイル (書き込み途中を含む) の開始。後ろにファイルが無い最後の 1 本は分からない
fn end_of(files: &[ListedFile], i: usize) -> Option<i64> {
    files.get(i + 1).map(|f| f.start_ms)
}

/// 閉じたファイルが覆う終わり (最後の閉じたファイルの終わり)。後ろにファイルが無い最後の閉じたファイルは
/// 長さが分からないので、その開始までとみなす (次の回へ回す側に倒す)。閉じたファイルが無ければ `None`
pub fn closed_until(files: &[ListedFile]) -> Option<i64> {
    (0..files.len())
        .filter(|&i| files[i].closed)
        .map(|i| end_of(files, i).unwrap_or(files[i].start_ms))
        .max()
}

/// `[from, to)` にかかる閉じたファイルの番号 (開始の順)
pub fn files_for(files: &[ListedFile], from_ms: i64, to_ms: i64) -> Vec<usize> {
    (0..files.len())
        .filter(|&i| {
            let f = &files[i];
            f.closed && f.start_ms < to_ms && end_of(files, i).unwrap_or(f.start_ms) > from_ms
        })
        .collect()
}

/// 今回切るまとまりと、次の `rec_scanned_until`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub cut: Vec<Group>,
    /// 次の回へ回したまとまりの数
    pub deferred: usize,
    pub scanned_until_ms: i64,
}

/// 頭から順に、終わりが `closed_until` 以内のまとまりを `max_groups` 本まで切る。最初に外れたまとまりと
/// それ以降は次の回へ回し、走査済み時刻はそのまとまりの頭 (最初のイベント - 10 秒) にする。全部切れたら `upper_ms`
pub fn plan(groups: Vec<Group>, closed_until: Option<i64>, max_groups: usize, upper_ms: i64) -> Plan {
    let total = groups.len();
    let mut cut = Vec::new();
    let mut scanned_until_ms = upper_ms;
    for g in groups {
        if cut.len() >= max_groups || closed_until.is_none_or(|c| g.to_ms > c) {
            scanned_until_ms = g.from_ms;
            break;
        }
        cut.push(g);
    }
    Plan { deferred: total - cut.len(), cut, scanned_until_ms }
}

/// 走査の範囲 `[from, upper)`
pub fn scan_range(scanned_until_ms: Option<i64>, now_ms: i64) -> (i64, i64) {
    (scanned_until_ms.unwrap_or(now_ms - FIRST_LOOKBACK_MS), now_ms)
}

/// カメラと R2 (Worker が実装する。wasm では JS の値を持つので `Send` を求めない)
#[allow(async_fn_in_trait)]
pub trait Recorder {
    /// `[from, to)` の時のフォルダの SD の一覧 ([`listed_files`] の形)
    async fn list(&mut self, from_ms: i64, to_ms: i64) -> Result<Vec<ListedFile>, String>;
    /// そのファイルを R2 にそろえる (同じ大きさで在れば取りに行かない)
    async fn fetch_raw(&mut self, file: &ListedFile) -> Result<(), String>;
    /// R2 にそろえたファイルから `[from, to)` を切って `key` に置く。大きさ (バイト) を返す
    async fn cut(&mut self, key: &str, files: &[&ListedFile], from_ms: i64, to_ms: i64) -> Result<u64, String>;
}

/// 切り出しの対象のカメラ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCamera {
    pub id: Uuid,
    pub scanned_until_ms: Option<i64>,
}

/// 1 カメラ分の結果 (ログ用。件数だけ)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub events: usize,
    pub groups: usize,
    pub ready: usize,
    pub failed: usize,
    pub deferred: usize,
    pub fetched: usize,
    /// SD の一覧が取れず、何も切らずに走査済み時刻も進めなかった
    pub listing_failed: bool,
}

impl Outcome {
    pub fn line(&self) -> String {
        format!(
            "events={} groups={} ready={} failed={} deferred={} fetched={}{}",
            self.events,
            self.groups,
            self.ready,
            self.failed,
            self.deferred,
            self.fetched,
            if self.listing_failed { " listing_failed" } else { "" }
        )
    }
}

/// 1 カメラ分を流す。DB のエラーだけを `Err` で返す (カメラ・R2 の失敗は `cam_clips` の failed か `listing_failed`)
pub async fn run_camera<R: Recorder>(
    pg: &mut PgClient,
    tenant: Uuid,
    cam: &AutoCamera,
    rec: &mut R,
    now_ms: i64,
    max_groups: usize,
) -> Result<Outcome, tokio_postgres::Error> {
    let mut out = Outcome::default();
    let (from, upper) = scan_range(cam.scanned_until_ms, now_ms);
    if from >= upper {
        return Ok(out);
    }
    let events = pg::auto_events(pg, tenant, cam.id, from, upper).await?;
    out.events = events.len();
    let groups = group_events(events, PAD_MS);
    out.groups = groups.len();
    let Some(first) = groups.first() else {
        pg::set_scanned_until(pg, tenant, cam.id, upper).await?;
        return Ok(out);
    };
    let files = match rec.list(first.from_ms - LIST_LEAD_MS, upper).await {
        Ok(f) => f,
        Err(_) => {
            out.listing_failed = true;
            out.deferred = groups.len();
            return Ok(out);
        }
    };
    let p = plan(groups, closed_until(&files), max_groups, upper);
    out.deferred = p.deferred;

    // 1 回の中では 1 ファイル 1 回だけ取り寄せる (失敗も覚えて、同じファイルにかかる次のまとまりも同じ失敗にする)
    let mut fetched: HashMap<usize, Result<(), String>> = HashMap::new();
    for g in p.cut {
        let idx = files_for(&files, g.from_ms, g.to_ms);
        let mut result: Result<u64, String> = if idx.is_empty() { Err("その時間帯の録画が SD にありません".into()) } else { Ok(0) };
        if result.is_ok() {
            for &i in &idx {
                let r = match fetched.get(&i) {
                    Some(r) => r.clone(),
                    None => {
                        let r = rec.fetch_raw(&files[i]).await;
                        out.fetched += 1;
                        fetched.insert(i, r.clone());
                        r
                    }
                };
                if let Err(e) = r {
                    result = Err(e);
                    break;
                }
            }
        }
        let key = clip_r2_key(tenant, cam.id, g.from_ms, g.to_ms);
        if result.is_ok() {
            let picked: Vec<&ListedFile> = idx.iter().map(|&i| &files[i]).collect();
            result = rec.cut(&key, &picked, g.from_ms, g.to_ms).await;
        }
        let record = match &result {
            Ok(size) => pg::ClipRecord { camera_id: cam.id, from_ms: g.from_ms, to_ms: g.to_ms, r2_key: key, size_bytes: i64::try_from(*size).ok(), status: "ready", error: None },
            Err(e) => pg::ClipRecord { camera_id: cam.id, from_ms: g.from_ms, to_ms: g.to_ms, r2_key: key, size_bytes: None, status: "failed", error: Some(e.clone()) },
        };
        match result {
            Ok(_) => out.ready += 1,
            Err(_) => out.failed += 1,
        }
        let clip_id = pg::record_clip(pg, tenant, record).await?;
        pg::record_clip_events(pg, tenant, cam.id, clip_id, g.events).await?;
    }
    pg::set_scanned_until(pg, tenant, cam.id, p.scanned_until_ms).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(n: u128, at_ms: i64) -> Event {
        Event { source_table: "hub_measurements".into(), source_id: Uuid::from_u128(n), kind: "alcohol".into(), at_ms, device_id: "d".into(), is_dev: false }
    }

    fn file(start_ms: i64, closed: bool) -> ListedFile {
        ListedFile { kind: "Schedule".into(), date: "20261023".into(), hour: "12".into(), name: format!("f{start_ms}"), size: 1, start_ms, closed }
    }

    fn spans(gs: &[Group]) -> Vec<(i64, i64, usize)> {
        gs.iter().map(|g| (g.from_ms, g.to_ms, g.events.len())).collect()
    }

    #[test]
    fn windows_merge_when_they_overlap_or_touch() {
        assert!(group_events(vec![], PAD_MS).is_empty());
        // 1 つだけ
        assert_eq!(spans(&group_events(vec![ev(1, 100_000)], PAD_MS)), [(90_000, 110_000, 1)]);
        // 接する (20 秒ちょうど離れている: 前の終わり = 次の頭) → 1 本
        assert_eq!(spans(&group_events(vec![ev(1, 100_000), ev(2, 120_000)], PAD_MS)), [(90_000, 130_000, 2)]);
        // 1ms 離れる → 2 本
        assert_eq!(spans(&group_events(vec![ev(1, 100_000), ev(2, 120_001)], PAD_MS)), [(90_000, 110_000, 1), (110_001, 130_001, 1)]);
        // 重なる・順不同・同じ時刻・連鎖
        let g = group_events(vec![ev(3, 130_000), ev(1, 100_000), ev(2, 115_000), ev(4, 115_000), ev(5, 300_000)], PAD_MS);
        assert_eq!(spans(&g), [(90_000, 140_000, 4), (290_000, 310_000, 1)]);
        let order: Vec<u128> = g[0].events.iter().map(|e| e.source_id.as_u128()).collect();
        assert_eq!(order, [1, 2, 4, 3]);
        // 中に含まれる窓で終わりが縮まない
        assert_eq!(spans(&group_events(vec![ev(1, 100_000), ev(2, 100_000)], PAD_MS)), [(90_000, 110_000, 2)]);
    }

    #[test]
    fn listing_reads_closed_and_open_files() {
        let sd = |name: &str| SdFile { name: name.into(), size: 10 };
        let got = listed_files(vec![
            ("Schedule".into(), "20261023".into(), "13".into(), vec![sd("Schedule20261023_130400_!.mp4"), sd("Schedule20261023_130000.mp4")]),
            ("Schedule".into(), "20261023".into(), "12".into(), vec![sd("Schedule20261023_125600.mp4"), sd("index.db"), sd("Schedule20261023_1256.mp4"), sd("Schedule20261023_125800.jpg"), sd("../x_20261023_125600.mp4")]),
        ]);
        let names: Vec<(&str, bool)> = got.iter().map(|f| (f.name.as_str(), f.closed)).collect();
        assert_eq!(names, [("Schedule20261023_125600.mp4", true), ("Schedule20261023_130000.mp4", true), ("Schedule20261023_130400_!.mp4", false)]);
        assert_eq!(got[2].start_ms - got[1].start_ms, 240_000);
        assert_eq!(got[0].hour, "12");
    }

    #[test]
    fn closed_until_is_the_end_of_the_last_closed_file() {
        assert_eq!(closed_until(&[]), None);
        // 書き込み途中だけ
        assert_eq!(closed_until(&[file(100, false)]), None);
        // 閉じた 2 本 + 書き込み途中: 最後の閉じたファイルの終わり = 書き込み途中の頭
        assert_eq!(closed_until(&[file(0, true), file(240, true), file(480, false)]), Some(480));
        // 後ろに何も無い閉じたファイルは長さが分からない → その頭まで
        assert_eq!(closed_until(&[file(0, true), file(240, true)]), Some(240));
        assert_eq!(closed_until(&[file(0, true)]), Some(0));
    }

    #[test]
    fn files_for_a_window() {
        let fs = [file(0, true), file(240, true), file(480, true), file(720, false)];
        assert_eq!(files_for(&fs, 250, 300), [1]);
        // ファイルの境目にかかる
        assert_eq!(files_for(&fs, 230, 250), [0, 1]);
        // 終わりちょうど = 次のファイルの頭 (半開区間なので次は入らない)
        assert_eq!(files_for(&fs, 200, 240), [0]);
        // 書き込み途中は入れない
        assert_eq!(files_for(&fs, 700, 760), [2]);
        assert!(files_for(&fs, 800, 900).is_empty());
        // 録画の前
        assert!(files_for(&[file(1000, true), file(1240, false)], 0, 20).is_empty());
    }

    fn g(from_ms: i64, to_ms: i64) -> Group {
        Group { from_ms, to_ms, events: vec![ev(from_ms as u128, from_ms + PAD_MS)] }
    }

    #[test]
    fn plan_defers_past_the_closed_end_and_over_the_cap() {
        // 空
        assert_eq!(plan(vec![], Some(10), 40, 999), Plan { cut: vec![], deferred: 0, scanned_until_ms: 999 });
        // 全部閉じている → upper まで
        let p = plan(vec![g(0, 20), g(100, 120)], Some(500), 40, 999);
        assert_eq!((p.cut.len(), p.deferred, p.scanned_until_ms), (2, 0, 999));
        // 終わりが閉じた終わりちょうど → 切る
        let p = plan(vec![g(0, 20), g(100, 120)], Some(120), 40, 999);
        assert_eq!((p.cut.len(), p.deferred, p.scanned_until_ms), (2, 0, 999));
        // 1ms 越える → そのまとまりから次の回 (走査済みはその頭)
        let p = plan(vec![g(0, 20), g(100, 121), g(200, 220)], Some(120), 40, 999);
        assert_eq!((p.cut.len(), p.deferred, p.scanned_until_ms), (1, 2, 100));
        // 閉じたファイルが無い → 全部次の回
        let p = plan(vec![g(50, 70)], None, 40, 999);
        assert_eq!((p.cut.len(), p.deferred, p.scanned_until_ms), (0, 1, 50));
        // 上限ちょうど → 全部切る
        let gs: Vec<Group> = (0..3).map(|i| g(i * 100, i * 100 + 20)).collect();
        let p = plan(gs.clone(), Some(10_000), 3, 999);
        assert_eq!((p.cut.len(), p.deferred, p.scanned_until_ms), (3, 0, 999));
        // 上限超え → 超えた分の頭から次の回
        let p = plan(gs, Some(10_000), 2, 999);
        assert_eq!((p.cut.len(), p.deferred, p.scanned_until_ms), (2, 1, 200));
    }

    #[test]
    fn scan_range_starts_two_hours_back_the_first_time() {
        assert_eq!(scan_range(None, 10_000_000), (10_000_000 - FIRST_LOOKBACK_MS, 10_000_000));
        assert_eq!(scan_range(Some(9_000_000), 10_000_000), (9_000_000, 10_000_000));
    }
}
