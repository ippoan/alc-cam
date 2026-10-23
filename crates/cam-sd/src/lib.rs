//! カメラ (I-O DATA TS-NS320W 系、fw 1.02.04 で確認) の SD 録画を取り出す手順。I/O を持たない。
//!
//! - 一覧: 公開仕様書の `GET /camera-cgi/admin/sdcard.cgi?action=generate&dir=…` (XML)。
//!   `<カメラ名>/<種類>/<YYYYMMDD>/<HH>/` の順に 1 段ずつ下りる。時のフォルダの `path` 属性は親と
//!   同じ値で返ってくるので、パスは `name` 属性から自分で組み立てる。
//!   管理画面の録画検索 (`/action/stream?subject=recsearch`) は録画があっても空を返すので使わない。
//! - 本体: 管理画面の `POST /action/stream?subject=recdownload` に XML で `/mnt/mmc` 付きのパスを渡す。
//!   `Range` は効かず、毎回ファイルを丸ごと返す (中身は H.264 + AAC の MP4、約 100MB ごとに区切り)。
//! - 認証はどちらも HTTP Digest (MD5, qop=auth)。

pub use cam_rtsp::digest;

/// 一覧の 1 ページの件数 (仕様の上限)。1 時間のフォルダに入るのは十数本なので 1 ページで足りる
const PAGE_SIZE: u32 = 1000;
/// `recdownload` に渡すパスの頭 (SD のマウント先)
const SD_MOUNT: &str = "/mnt/mmc";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    pub dirs: Vec<String>,
    pub files: Vec<SdFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdFile {
    pub name: String,
    pub size: u64,
}

/// `sdcard.cgi?action=generate` の XML を読む。形が固定なので正規表現も XML パーサーも使わない
pub fn parse_listing(xml: &str) -> Listing {
    let mut out = Listing::default();
    for tag in xml.split("<Dir ").skip(1) {
        if let Some(name) = attr(tag, "name") {
            out.dirs.push(name);
        }
    }
    for item in xml.split("<File").skip(1) {
        let name = between(item, "<Name>", "</Name>");
        let size = between(item, "<Size>", "</Size>").and_then(|s| s.trim().parse().ok());
        if let (Some(name), Some(size)) = (name, size) {
            out.files.push(SdFile { name: unescape(&name), size });
        }
    }
    out
}

fn attr(tag: &str, key: &str) -> Option<String> {
    let head = tag.split('>').next()?;
    let start = head.find(&format!("{key}=\""))? + key.len() + 2;
    let end = head[start..].find('"')? + start;
    Some(unescape(&head[start..end]))
}

fn between(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// 一覧の URL のパス部分 (`dir` は `TS-NS320W-55/Schedule/20261023/12` の形。頭の `/` は無し)
pub fn list_path(dir: &str) -> String {
    format!("/camera-cgi/admin/sdcard.cgi?action=generate&dir={dir}&pagesize={PAGE_SIZE}&pagenum=1")
}

pub const DOWNLOAD_PATH: &str = "/action/stream?subject=recdownload";

/// `recdownload` の本文。`sd_path` は `/TS-NS320W-55/Schedule/…/x.mp4` の形
pub fn download_body(sd_path: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="utf-8"?><request><recdownload><path>{SD_MOUNT}{sd_path}</path></recdownload></request>"#)
}

/// SD 上の 1 本のファイルの場所
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording<'a> {
    pub camera: &'a str,
    /// `Schedule` (スケジュール録画) / `Event` (検知録画) など、カメラ名の下のフォルダ
    pub kind: &'a str,
    pub date: &'a str,
    pub hour: &'a str,
    pub name: &'a str,
}

impl Recording<'_> {
    pub fn sd_dir(&self) -> String {
        format!("{}/{}/{}/{}", self.camera, self.kind, self.date, self.hour)
    }

    pub fn sd_path(&self) -> String {
        format!("/{}/{}", self.sd_dir(), self.name)
    }

    /// R2 のキー。SD と同じ並び (カメラ名/種類/日付/時/ファイル名) にして、どこから来たかを辿れるようにする
    pub fn r2_key(&self) -> String {
        format!("{}/{}", self.sd_dir(), self.name)
    }
}

/// 取り込む対象か。`_!` を含む名前はカメラの書き込み途中の一時ファイル (cf-flickr-cam-worker と同じ扱い)
pub fn wanted(name: &str) -> bool {
    !name.contains("_!") && (name.ends_with(".mp4") || name.ends_with(".jpg"))
}

/// フォルダ名として受ける文字 (一覧から来た値をそのまま URL に入れるので、外れた値は捨てる)
pub fn safe_segment(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// 見に行く日付 (`YYYYMMDD`、カメラの時計 = JST)。古い順。`days=2` なら昨日と今日
pub fn scan_dates(now_ms: f64, days: u32) -> Vec<String> {
    const JST_MS: f64 = 9.0 * 3600.0 * 1000.0;
    (0..days)
        .rev()
        .map(|back| ymd_from_epoch_days(((now_ms + JST_MS) / 86_400_000.0).floor() as i64 - i64::from(back)))
        .collect()
}

/// 1970-01-01 からの日数 → `YYYYMMDD` (proleptic Gregorian、Howard Hinnant の civil_from_days)
fn ymd_from_epoch_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

const JST_OFFSET_MS: i64 = 9 * 3600 * 1000;

/// `YYYY-MM-DD` → 1970-01-01 からの日数 (civil_from_days の逆)
fn epoch_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn num(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// 壁時計 (JST) の年月日時分秒 → UNIX ms
fn jst_ms(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<i64> {
    let ok = (1..=12).contains(&mo) && (1..=31).contains(&d) && (0..24).contains(&h) && (0..60).contains(&mi) && (0..60).contains(&s);
    ok.then(|| (epoch_days(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s) * 1000 - JST_OFFSET_MS)
}

/// 録画ファイルの開始時刻 (UNIX ms)。名前は `<種類>YYYYMMDD_HHMMSS.mp4` (カメラの時計 = JST)
pub fn file_start_ms(name: &str) -> Option<i64> {
    let stem = name.strip_suffix(".mp4")?;
    let (head, time) = stem.rsplit_once('_')?;
    let date = head.get(head.len().checked_sub(8)?..)?;
    if time.len() != 6 {
        return None;
    }
    jst_ms(num(&date[..4])?, num(&date[4..6])?, num(&date[6..])?, num(&time[..2])?, num(&time[2..4])?, num(&time[4..])?)
}

/// 指定時刻を読む: `YYYY-MM-DDTHH:MM:SS` に `Z` / `+09:00` などの時差。時差が無ければ JST とみなす
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (dt, offset_ms) = if let Some(dt) = s.strip_suffix('Z') {
        (dt, 0)
    } else if s.len() > 19 && matches!(s.as_bytes()[19], b'+' | b'-') {
        let (dt, off) = s.split_at(19);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (h, m) = off[1..].split_once(':')?;
        (dt, sign * (num(h)? * 3600 + num(m)? * 60) * 1000)
    } else {
        (s, JST_OFFSET_MS)
    };
    let b = dt.as_bytes();
    if dt.len() != 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let wall = jst_ms(num(&dt[..4])?, num(&dt[5..7])?, num(&dt[8..10])?, num(&dt[11..13])?, num(&dt[14..16])?, num(&dt[17..19])?)?;
    // jst_ms は JST として引いているので、実際の時差に置き直す
    Some(wall + JST_OFFSET_MS - offset_ms)
}

/// UNIX ms → JST の (`YYYYMMDD`, `HH`) (SD のフォルダ名と同じ形)
pub fn jst_date_hour(ms: i64) -> (String, String) {
    let local = ms + JST_OFFSET_MS;
    let day = local.div_euclid(86_400_000);
    let hour = local.rem_euclid(86_400_000) / 3_600_000;
    (ymd_from_epoch_days(day), format!("{hour:02}"))
}

/// UNIX ms → JST の `YYYYMMDDTHHMMSS` (切り出しの R2 キーに使う)
pub fn jst_compact(ms: i64) -> String {
    let local = ms + JST_OFFSET_MS;
    let (day, rem) = (local.div_euclid(86_400_000), local.rem_euclid(86_400_000) / 1000);
    format!("{}T{:02}{:02}{:02}", ymd_from_epoch_days(day), rem / 3600, rem / 60 % 60, rem % 60)
}

/// UNIX ms → `2026-10-23T12:57:58.257+09:00` (API の応答に使う)
pub fn jst_iso(ms: i64) -> String {
    let c = jst_compact(ms);
    format!("{}-{}-{}T{}:{}:{}.{:03}+09:00", &c[..4], &c[4..6], &c[6..8], &c[9..11], &c[11..13], &c[13..15], ms.rem_euclid(1000))
}

/// `[from, to)` の前後を含めて見に行く (`YYYYMMDD`, `HH`) の一覧。頭のファイルは時間帯より前に始まるので、
/// 1 本の長さ (約 4 分) より長めに `lead_ms` だけ前から見る
pub fn hours_to_scan(from_ms: i64, to_ms: i64, lead_ms: i64) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut t = (from_ms - lead_ms).div_euclid(3_600_000) * 3_600_000;
    while t < to_ms {
        let dh = jst_date_hour(t);
        if out.last() != Some(&dh) {
            out.push(dh);
        }
        t += 3_600_000;
    }
    out
}

/// 開始時刻の昇順に並んだファイルのうち、`[from, to)` に重なるものの番号。各ファイルの終わりは次の
/// ファイルの開始 (最後のファイルは `last_len_ms` だけ続くとみなす)
pub fn overlapping(starts: &[i64], from_ms: i64, to_ms: i64, last_len_ms: i64) -> Vec<usize> {
    (0..starts.len())
        .filter(|&i| {
            let end = starts.get(i + 1).copied().unwrap_or(starts[i] + last_len_ms);
            starts[i] < to_ms && end > from_ms
        })
        .collect()
}

/// secret の `user:pass` を分ける (パスワード側に `:` があっても最初の `:` で切る)
pub fn split_auth(s: &str) -> Option<(&str, &str)> {
    s.split_once(':').filter(|(u, _)| !u.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dirs_and_files_as_the_camera_returns_them() {
        // 実機 (2026-10-23) の応答そのまま
        let root = r#"<?xml version="1.0" encoding="utf-8"?><SDCard total="1"><Dir name="TS-NS320W-55" path="/TS-NS320W-55/" index="0"></Dir></SDCard>"#;
        assert_eq!(parse_listing(root).dirs, vec!["TS-NS320W-55"]);
        let hour = r#"<?xml version="1.0" encoding="utf-8"?><SDCard total="1"><Dir name="12" path="/TS-NS320W-55/Schedule/20261023" index="0"></Dir></SDCard>"#;
        assert_eq!(parse_listing(hour).dirs, vec!["12"]);
        let files = "<?xml version=\"1.0\" encoding=\"utf-8\"?><SDCard total=\"2\">\n<File index=\"0\"><Name>Schedule20261023_125448.mp4</Name><Size>104244153</Size></File>\n<File index=\"1\"><Name>Schedule20261023_125853.mp4</Name><Size>99</Size></File></SDCard>";
        let l = parse_listing(files);
        assert!(l.dirs.is_empty());
        assert_eq!(
            l.files,
            vec![
                SdFile { name: "Schedule20261023_125448.mp4".into(), size: 104_244_153 },
                SdFile { name: "Schedule20261023_125853.mp4".into(), size: 99 },
            ]
        );
        assert_eq!(parse_listing(r#"<SDCard total="0"></SDCard>"#), Listing::default());
    }

    #[test]
    fn paths_and_keys() {
        let r = Recording { camera: "TS-NS320W-55", kind: "Schedule", date: "20261023", hour: "12", name: "Schedule20261023_125448.mp4" };
        assert_eq!(list_path(&r.sd_dir()), "/camera-cgi/admin/sdcard.cgi?action=generate&dir=TS-NS320W-55/Schedule/20261023/12&pagesize=1000&pagenum=1");
        assert_eq!(r.sd_path(), "/TS-NS320W-55/Schedule/20261023/12/Schedule20261023_125448.mp4");
        assert_eq!(r.r2_key(), "TS-NS320W-55/Schedule/20261023/12/Schedule20261023_125448.mp4");
        assert!(download_body(&r.sd_path()).contains("<path>/mnt/mmc/TS-NS320W-55/Schedule/20261023/12/Schedule20261023_125448.mp4</path>"));
    }

    #[test]
    fn filters() {
        assert!(wanted("Schedule20261023_125448.mp4"));
        assert!(wanted("Event20261023_125448.jpg"));
        assert!(!wanted("Schedule20261023_125448_!.mp4"));
        assert!(!wanted("index.db"));
        assert!(safe_segment("20261023") && safe_segment("TS-NS320W-55"));
        assert!(!safe_segment("..") && !safe_segment("a/b") && !safe_segment("a&b") && !safe_segment(""));
    }

    #[test]
    fn dates_are_jst() {
        // 2026-10-22T15:30:00Z = 2026-10-23 00:30 JST
        let ms = 1_792_683_000_000.0;
        assert_eq!(scan_dates(ms, 2), vec!["20261022", "20261023"]);
        // 2026-10-22T14:30:00Z = 2026-10-22 23:30 JST
        assert_eq!(scan_dates(ms - 3_600_000.0, 1), vec!["20261022"]);
        // 月・年またぎ
        assert_eq!(ymd_from_epoch_days(0), "19700101");
        assert_eq!(ymd_from_epoch_days(20_454), "20260101");
        assert_eq!(ymd_from_epoch_days(20_453), "20251231");
    }

    #[test]
    fn times() {
        // 2026-10-23 12:54:48 JST = 03:54:48Z
        let t = file_start_ms("Schedule20261023_125448.mp4").unwrap();
        assert_eq!(t, parse_time("2026-10-23T03:54:48Z").unwrap());
        assert_eq!(t, parse_time("2026-10-23T12:54:48+09:00").unwrap());
        assert_eq!(t, parse_time("2026-10-23 12:54:48").unwrap()); // 時差なし = JST
        assert_eq!(t % 1000, 0);
        assert_eq!(jst_date_hour(t), ("20261023".into(), "12".into()));
        assert_eq!(jst_compact(t), "20261023T125448");
        assert_eq!(jst_iso(t + 257), "2026-10-23T12:54:48.257+09:00");
        // 日付またぎ (JST 0 時台は UTC では前日)
        let midnight = parse_time("2026-10-23T00:10:00+09:00").unwrap();
        assert_eq!(jst_date_hour(midnight), ("20261023".into(), "00".into()));
        assert_eq!(parse_time("2026-10-22T15:10:00Z"), Some(midnight));
        assert_eq!(epoch_days(1970, 1, 1), 0);
        assert_eq!(epoch_days(2026, 1, 1), 20_454);
        // 壊れた入力
        assert_eq!(file_start_ms("Schedule20261023_1254.mp4"), None);
        assert_eq!(file_start_ms("Schedule20261323_125448.mp4"), None);
        assert_eq!(parse_time("2026/10/23 12:00:00"), None);
        assert_eq!(parse_time("2026-10-23T25:00:00"), None);
    }

    #[test]
    fn scan_plan() {
        let from = parse_time("2026-10-23T13:02:00").unwrap();
        let to = parse_time("2026-10-23T14:10:00").unwrap();
        let h = hours_to_scan(from, to, 10 * 60_000);
        assert_eq!(h, vec![("20261023".into(), "12".into()), ("20261023".into(), "13".into()), ("20261023".into(), "14".into())]);
        // 日付をまたぐ
        let h = hours_to_scan(parse_time("2026-10-23T23:30:00").unwrap(), parse_time("2026-10-24T00:20:00").unwrap(), 0);
        assert_eq!(h, vec![("20261023".into(), "23".into()), ("20261024".into(), "00".into())]);

        let s = [0, 240, 480, 720];
        assert_eq!(overlapping(&s, 250, 500, 240), vec![1, 2]);
        assert_eq!(overlapping(&s, 240, 480, 240), vec![1]);
        assert_eq!(overlapping(&s, 900, 1000, 240), vec![3]);
        assert_eq!(overlapping(&s, 1000, 1100, 240), Vec::<usize>::new());
        assert_eq!(overlapping(&[], 0, 1, 240), Vec::<usize>::new());
    }

    #[test]
    fn auth_split() {
        assert_eq!(split_auth("admin:p:a:ss"), Some(("admin", "p:a:ss")));
        assert_eq!(split_auth(":x"), None);
        assert_eq!(split_auth("nocolon"), None);
    }

    #[test]
    fn digest_from_the_camera_header() {
        // 実機の WWW-Authenticate (区切りの空白が抜けている箇所がある)
        let h = r#"Digest realm="HD Network Camera", domain="0.0.0.0",qop="auth", nonce="61708bdd46ea26cecbad3c8b7379742f", opaque="5ccc069c403ebaf9f0171e9517f40e41",algorithm="MD5", stale="FALSE""#;
        let c = digest::parse(h).unwrap();
        let v = digest::authorization(&c, "POST", DOWNLOAD_PATH, "admin", "x", 1, "cn");
        assert!(v.contains(r#"realm="HD Network Camera""#) && v.contains("qop=auth") && v.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#), "{v}");
        assert!(v.contains(r#"uri="/action/stream?subject=recdownload""#));
    }
}
