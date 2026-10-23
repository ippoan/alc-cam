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
