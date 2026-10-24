//! host と拡張の自己更新。拡張が `update_check` を送った時だけ動く (見る人が居ない時は拡張が確認のためだけに起動する)。
//!
//!   1. GitHub の最新 Release の `alc-cam.json` ({"version","host_sha256","extension_sha256"}) を読む
//!   2. host: 自分の版より新しければ、その版の Release の `alc-cam-host.exe` を落として SHA256 を確かめ、
//!      同じフォルダに `.new-<pid>` で書き、実行中の exe を `.old` へ rename してから `.new-<pid>` を exe へ rename する
//!      (Windows は実行中の exe を消せないが rename はできる)。新版は次の起動から。`.old` は次の起動で消す
//!   3. 拡張: exe の隣の `extension\` (拠点で chrome://extensions から読み込んだフォルダ) の manifest.json より
//!      新しければ、`alc-cam-extension.zip` を落として SHA256 を確かめ、`extension.new-<pid>` に展開し、
//!      manifest の版と key (= 拡張 ID) を確かめてから `extension` と rename で入れ替える。
//!      見る人が居る間は入れ替えない (拡張の再読み込みで配信が切れるので、次の確認に回す)。
//!      入れ替えたら拡張がディスクの manifest の版の違いに気づいて `chrome.runtime.reload()` する
//!
//! どこで失敗しても今の版のまま (2 つ目の rename が失敗したら 1 つ目を戻す)。
//! 出る先は github.com と GitHub のファイル置き場 (*.githubusercontent.com) の https だけ (redirect 先も確かめる)。

use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const LATEST_URL: &str = "https://github.com/ippoan/alc-cam/releases/latest/download/alc-cam.json";
const RELEASE_BASE: &str = "https://github.com/ippoan/alc-cam/releases/download";
/// extension/manifest.json の key から決まる ID。違う拡張には入れ替えない
pub const EXT_ID: &str = "icmmpdjddlphkbokdalofdjpkcgfiggd";
const MAX_JSON: u64 = 64 * 1024;
const MAX_EXE: u64 = 64 * 1024 * 1024;
const MAX_ZIP: u64 = 16 * 1024 * 1024;
/// 展開後の合計 (zip 爆弾よけ)
const MAX_UNZIPPED: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

pub fn asset_url(version: &str, name: &str) -> String {
    format!("{RELEASE_BASE}/v{version}/{name}")
}

/// Release に置く `alc-cam.json`。host と拡張は同じ版で出す
#[derive(Debug, Deserialize)]
pub struct Release {
    pub version: String,
    pub host_sha256: String,
    pub extension_sha256: String,
}

#[derive(Debug, PartialEq)]
pub enum Part {
    /// 今の版が最新 (または新しい)
    UpToDate,
    /// 入れ替えた (host は次の起動から、拡張は再読み込みで)
    Installed,
    /// 見る人が居るので後で (拡張だけ)
    Deferred,
    /// 拡張が決まった場所に無い (別の場所から読み込んでいる)。触らない
    Missing,
    /// 今の版のまま
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub latest: String,
    pub host: Part,
    pub extension: Part,
}

/// `1.2.3` / `v1.2.3` だけ受ける (pre-release は付けない運用)
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.strip_prefix('v').unwrap_or(s).split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// 読めない版は「新しくない」(入れ替えない)
pub fn is_newer(current: &str, latest: &str) -> bool {
    matches!((parse_version(current), parse_version(latest)), (Some(c), Some(l)) if l > c)
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// manifest の key (公開鍵 DER の base64) → 拡張 ID (SHA256 の先頭 16 バイトを 16 進にし 0-f を a-p に)
pub fn extension_id(key: &str) -> Option<String> {
    let der = base64::engine::general_purpose::STANDARD.decode(key.trim()).ok()?;
    let hex = sha256_hex(&der);
    Some(hex[..32].bytes().map(|c| (b'a' + (c as char).to_digit(16).unwrap() as u8) as char).collect())
}

/// https で、github.com か *.githubusercontent.com だけ
pub fn allowed_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    !host.contains(['@', ':']) && (host == "github.com" || host.ends_with(".githubusercontent.com"))
}

pub trait Fetch {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String>;
}

type Rename<'a> = &'a dyn Fn(&Path, &Path) -> io::Result<()>;

fn rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// exe の隣の、拠点で読み込む拡張のフォルダ
pub fn ext_dir(exe: &Path) -> PathBuf {
    exe.with_file_name("extension")
}

/// 確認して、新しければ host と拡張を入れ替える。`busy` は見る人が居るか (拡張の入れ替えを延ばす)
pub fn check(fetch: &impl Fetch, current: &str, exe: &Path, busy: &dyn Fn() -> bool) -> Result<Outcome, String> {
    check_with(fetch, current, exe, busy, &rename)
}

fn check_with(fetch: &impl Fetch, current: &str, exe: &Path, busy: &dyn Fn() -> bool, rename: Rename) -> Result<Outcome, String> {
    let body = fetch.get(LATEST_URL, MAX_JSON)?;
    let rel: Release = serde_json::from_slice(&body).map_err(|e| format!("版の情報が読めません: {e}"))?;
    if parse_version(&rel.version).is_none() {
        return Err(format!("版の形が違います: {}", rel.version));
    }
    let host = if is_newer(current, &rel.version) {
        update_host(fetch, &rel, exe, rename).map_or_else(Part::Failed, |()| Part::Installed)
    } else {
        Part::UpToDate
    };
    let extension = update_extension(fetch, &rel, &ext_dir(exe), busy, rename).unwrap_or_else(Part::Failed);
    Ok(Outcome { latest: rel.version, host, extension })
}

fn download(fetch: &impl Fetch, rel: &Release, name: &str, sha: &str, limit: u64) -> Result<Vec<u8>, String> {
    let bin = fetch.get(&asset_url(&rel.version, name), limit)?;
    if !sha256_hex(&bin).eq_ignore_ascii_case(sha.trim()) {
        return Err(format!("v{} の {name} の SHA256 が合わないので入れ替えません", rel.version));
    }
    Ok(bin)
}

fn update_host(fetch: &impl Fetch, rel: &Release, exe: &Path, rename: Rename) -> Result<(), String> {
    // 前の確認で入れ替え済み (動いているのは古い版のまま、次の起動から): 落とし直さない
    if fs::read(exe).is_ok_and(|b| sha256_hex(&b).eq_ignore_ascii_case(rel.host_sha256.trim())) {
        return Ok(());
    }
    let bin = download(fetch, rel, "alc-cam-host.exe", &rel.host_sha256, MAX_EXE)?;
    replace_with(exe, &bin, rename).map_err(|e| format!("host の入れ替えに失敗 (今の版のまま): {e}"))
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    key: String,
}

/// 入っている拡張 (exe の隣の `extension\`) の版。読めなければ None
pub fn extension_version(exe: &Path) -> Option<String> {
    read_manifest(&ext_dir(exe)).ok().map(|m| m.version)
}

fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let body = fs::read(dir.join("manifest.json")).map_err(|e| format!("manifest.json が読めません: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("manifest.json が読めません: {e}"))
}

fn update_extension(fetch: &impl Fetch, rel: &Release, dir: &Path, busy: &dyn Fn() -> bool, rename: Rename) -> Result<Part, String> {
    let Ok(cur) = read_manifest(dir) else { return Ok(Part::Missing) };
    if !is_newer(&cur.version, &rel.version) {
        return Ok(Part::UpToDate);
    }
    if busy() {
        return Ok(Part::Deferred);
    }
    let zip = download(fetch, rel, "alc-cam-extension.zip", &rel.extension_sha256, MAX_ZIP)?;
    let staged = sibling(dir, &format!(".new-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staged);
    let res = (|| {
        unzip(&zip, &staged).map_err(|e| format!("拡張の zip を展開できません: {e}"))?;
        let m = read_manifest(&staged)?;
        if m.version != rel.version {
            return Err(format!("拡張の zip の版が {} です (v{} のはず)", m.version, rel.version));
        }
        if extension_id(&m.key).as_deref() != Some(EXT_ID) {
            return Err("拡張の zip の ID が違うので入れ替えません".into());
        }
        // 落としている間に見る人が来た
        if busy() {
            return Ok(Part::Deferred);
        }
        swap(dir, &staged, rename).map_err(|e| format!("拡張の入れ替えに失敗 (今の版のまま): {e}"))?;
        Ok(Part::Installed)
    })();
    let _ = fs::remove_dir_all(&staged);
    res
}

/// zip を `to` に展開する。フォルダの外を指す名前・大きすぎる中身は断る
fn unzip(zip: &[u8], to: &Path) -> io::Result<()> {
    let mut ar = zip::ZipArchive::new(Cursor::new(zip)).map_err(io::Error::other)?;
    let mut total = 0u64;
    for i in 0..ar.len() {
        let mut f = ar.by_index(i).map_err(io::Error::other)?;
        let Some(rel) = f.enclosed_name() else { return Err(io::Error::other("フォルダの外を指す名前があります")) };
        let path = to.join(rel);
        if f.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let mut out = fs::File::create(&path)?;
        total += io::copy(&mut (&mut f).take(MAX_UNZIPPED + 1 - total), &mut out)?;
        if total > MAX_UNZIPPED {
            return Err(io::Error::other("展開すると大きすぎます"));
        }
    }
    Ok(())
}

/// `dir` を `staged` に入れ替える (`dir` → `.old-<pid>`、`staged` → `dir`、失敗したら戻す)
fn swap(dir: &Path, staged: &Path, rename: Rename) -> io::Result<()> {
    let old = sibling(dir, &format!(".old-{}", std::process::id()));
    let _ = fs::remove_dir_all(&old);
    rename(dir, &old)?;
    if let Err(e) = rename(staged, dir) {
        let _ = rename(&old, dir);
        return Err(e);
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// `exe` を `bytes` に入れ替える。実行中の exe でもよい (Windows でも rename はできる)
fn replace_with(exe: &Path, bytes: &[u8], rename: Rename) -> io::Result<()> {
    let new = sibling(exe, &format!(".new-{}", std::process::id()));
    let res = (|| {
        let mut f = fs::File::create(&new)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        // 前回の .old がまだ動いている (消せない) なら別の名前に逃がす
        let mut old = sibling(exe, ".old");
        if old.exists() && fs::remove_file(&old).is_err() {
            old = sibling(exe, &format!(".old-{}", std::process::id()));
        }
        rename(exe, &old)?;
        if let Err(e) = rename(&new, exe) {
            let _ = rename(&old, exe);
            return Err(e);
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&new);
    }
    res
}

/// 前回の入れ替えの残り (exe の `.old*` / `.new-*`、拡張の `extension.old-*` / `extension.new-*`) を消す。
/// 動いている物は消えないので放っておく
pub fn cleanup(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else { return };
    let Ok(entries) = fs::read_dir(dir) else { return };
    let prefixes = [format!("{name}.old"), format!("{name}.new-"), "extension.old-".into(), "extension.new-".into()];
    for e in entries.flatten() {
        let n = e.file_name();
        let Some(n) = n.to_str() else { continue };
        if prefixes.iter().any(|p| n.starts_with(p.as_str())) {
            let p = e.path();
            let _ = if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
        }
    }
}

/// 拡張へ返す `update` の state と文言
pub fn summarize(o: &Outcome) -> (&'static str, String) {
    let v = &o.latest;
    let mut notes = Vec::new();
    for (what, part) in [("host", &o.host), ("拡張", &o.extension)] {
        match part {
            Part::Failed(e) => notes.push(e.clone()),
            Part::Installed if what == "host" => notes.push(format!("host を v{v} にしました (次の起動から)")),
            Part::Installed => notes.push(format!("拡張を v{v} にしました (再読み込みします)")),
            Part::Deferred => notes.push(format!("拡張の v{v} は見る人が居なくなってから入れます")),
            Part::Missing => notes.push("拡張が host の隣の extension フォルダに無いので、拡張は更新しません".into()),
            Part::UpToDate => {}
        }
    }
    let state = if matches!(o.host, Part::Failed(_)) || matches!(o.extension, Part::Failed(_)) {
        "failed"
    } else if o.host == Part::Installed || o.extension == Part::Installed {
        "installed"
    } else if o.extension == Part::Deferred {
        "deferred"
    } else {
        "latest"
    };
    (state, notes.join(" / "))
}

/// 本物の GitHub。redirect は自分で辿り、行き先ごとに `allowed_url` を確かめる
pub struct GitHub {
    agent: ureq::Agent,
}

impl Default for GitHub {
    fn default() -> Self {
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(concat!("alc-cam-host/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }
}

impl Fetch for GitHub {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        let mut url = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            if !allowed_url(&url) {
                return Err("GitHub 以外へは繋ぎません".into());
            }
            let mut res = self.agent.get(&url).call().map_err(|e| format!("GitHub に繋がりません: {e}"))?;
            let status = res.status().as_u16();
            if (300..400).contains(&status) {
                url = res
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or("redirect 先がありません")?
                    .to_string();
                continue;
            }
            if status != 200 {
                return Err(format!("GitHub が {status} を返しました"));
            }
            let mut buf = Vec::new();
            res.body_mut()
                .as_reader()
                .take(limit + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("受信に失敗: {e}"))?;
            if buf.len() as u64 > limit {
                return Err("大きすぎます".into());
            }
            return Ok(buf);
        }
        Err("redirect が多すぎます".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// extension/manifest.json の key
    fn real_key() -> String {
        let m: serde_json::Value = serde_json::from_str(include_str!("../../extension/manifest.json")).unwrap();
        m["key"].as_str().unwrap().to_string()
    }

    fn manifest(version: &str, key: &str) -> String {
        format!(r#"{{"manifest_version":3,"name":"alc-cam","version":"{version}","key":"{key}"}}"#)
    }

    fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    struct Fake {
        files: HashMap<String, Vec<u8>>,
        asked: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(version: &str, exe: &[u8], ext: &[u8]) -> Self {
            Self::with_sha(version, exe, &sha256_hex(exe), ext, &sha256_hex(ext))
        }

        fn with_sha(version: &str, exe: &[u8], exe_sha: &str, ext: &[u8], ext_sha: &str) -> Self {
            let json = format!(r#"{{"version":"{version}","host_sha256":"{exe_sha}","extension_sha256":"{ext_sha}"}}"#);
            let files = HashMap::from([
                (LATEST_URL.to_string(), json.into_bytes()),
                (asset_url(version, "alc-cam-host.exe"), exe.to_vec()),
                (asset_url(version, "alc-cam-extension.zip"), ext.to_vec()),
            ]);
            Self { files, asked: RefCell::default() }
        }
    }

    impl Fetch for Fake {
        fn get(&self, url: &str, _limit: u64) -> Result<Vec<u8>, String> {
            self.asked.borrow_mut().push(url.to_string());
            self.files.get(url).cloned().ok_or_else(|| "404".into())
        }
    }

    /// `alc-cam-host.exe` (中身 v0.3.0) と `extension/` (manifest は v0.3.0) の入ったフォルダ
    fn site(name: &str) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("alc-cam-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("extension")).unwrap();
        fs::write(d.join("extension/manifest.json"), manifest("0.3.0", &real_key())).unwrap();
        fs::write(d.join("extension/background.js"), "old").unwrap();
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v0.3.0").unwrap();
        (d, exe)
    }

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        v.sort();
        v
    }

    fn new_ext(version: &str) -> Vec<u8> {
        make_zip(&[("manifest.json", &manifest(version, &real_key())), ("background.js", "new"), ("lib/host.js", "x")])
    }

    const IDLE: &dyn Fn() -> bool = &|| false;
    const BUSY: &dyn Fn() -> bool = &|| true;

    #[test]
    fn compares_versions() {
        assert_eq!(parse_version("v0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_version("0.10.2"), Some((0, 10, 2)));
        assert_eq!(parse_version("0.3"), None);
        assert_eq!(parse_version("0.3.0.1"), None);
        assert_eq!(parse_version("0.3.0-rc1"), None);
        assert!(is_newer("0.3.0", "0.3.1"));
        assert!(is_newer("0.3.0", "v0.10.0")); // 文字列比較なら負ける
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.3.1", "0.3.0"));
        assert!(!is_newer("0.3.0", "abc"));
    }

    #[test]
    fn extension_id_matches_manifest_key() {
        assert_eq!(extension_id(&real_key()).as_deref(), Some(EXT_ID));
        assert_eq!(extension_id("!!"), None);
    }

    #[test]
    fn allows_only_github_https() {
        assert!(allowed_url(LATEST_URL));
        assert!(allowed_url("https://objects.githubusercontent.com/x?y=1"));
        assert!(allowed_url("https://release-assets.githubusercontent.com/x"));
        assert!(!allowed_url("http://github.com/x"));
        assert!(!allowed_url("https://github.com.evil.example/x"));
        assert!(!allowed_url("https://evil.example/github.com"));
        assert!(!allowed_url("https://github.com@evil.example/x"));
        assert!(!allowed_url("https://github.com:8443/x"));
        assert!(!allowed_url("https://githubusercontent.com.evil.example/"));
    }

    #[test]
    fn installs_both_and_cleans_up() {
        let (d, exe) = site("install");
        let fake = Fake::new("0.3.1", b"v0.3.1", &new_ext("0.3.1"));
        let o = check(&fake, "0.3.0", &exe, IDLE).unwrap();
        assert_eq!(o, Outcome { latest: "0.3.1".into(), host: Part::Installed, extension: Part::Installed });
        assert_eq!(summarize(&o).0, "installed");
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.1");
        assert_eq!(fs::read_to_string(d.join("extension/background.js")).unwrap(), "new");
        assert_eq!(fs::read_to_string(d.join("extension/lib/host.js")).unwrap(), "x");
        // 拡張の作業フォルダは残らず、exe の .old だけ残る (動いている間は消せないので次の起動で消す)
        assert_eq!(names(&d), ["alc-cam-host.exe", "alc-cam-host.exe.old", "extension"]);
        cleanup(&exe);
        assert_eq!(names(&d), ["alc-cam-host.exe", "extension"]);
    }

    #[test]
    fn up_to_date_does_not_download() {
        let (_, exe) = site("same");
        let fake = Fake::new("0.3.0", b"x", b"x");
        let o = check(&fake, "0.3.0", &exe, IDLE).unwrap();
        assert_eq!((o.host, o.extension), (Part::UpToDate, Part::UpToDate));
        assert_eq!(*fake.asked.borrow(), vec![LATEST_URL.to_string()]);
    }

    #[test]
    fn extension_waits_while_watched() {
        let (d, exe) = site("busy");
        let fake = Fake::new("0.3.1", b"v0.3.1", &new_ext("0.3.1"));
        let o = check(&fake, "0.3.0", &exe, BUSY).unwrap();
        // host は入れ替える (次の起動から)。拡張は落としもしない
        assert_eq!((&o.host, &o.extension), (&Part::Installed, &Part::Deferred));
        assert_eq!(summarize(&o).0, "installed");
        assert!(!fake.asked.borrow().iter().any(|u| u.ends_with(".zip")));
        assert_eq!(fs::read_to_string(d.join("extension/background.js")).unwrap(), "old");

        // 見る人が居るまま確認し直す: 入れ替え済みの exe は落とし直さない
        fake.asked.borrow_mut().clear();
        let o = check(&fake, "0.3.0", &exe, BUSY).unwrap();
        assert_eq!((&o.host, &o.extension), (&Part::Installed, &Part::Deferred));
        assert_eq!(*fake.asked.borrow(), vec![LATEST_URL.to_string()]);

        // host が既に新しく、拡張だけ待ち
        let o = check(&fake, "0.3.1", &exe, BUSY).unwrap();
        assert_eq!(summarize(&o).0, "deferred");
    }

    #[test]
    fn sha_mismatch_keeps_current() {
        let (d, exe) = site("sha");
        let fake = Fake::with_sha("0.3.1", b"tampered", &sha256_hex(b"other"), &new_ext("0.3.1"), &sha256_hex(b"other"));
        let o = check(&fake, "0.3.0", &exe, IDLE).unwrap();
        assert!(matches!(&o.host, Part::Failed(e) if e.contains("SHA256")));
        assert!(matches!(&o.extension, Part::Failed(e) if e.contains("SHA256")));
        assert_eq!(summarize(&o).0, "failed");
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
        assert_eq!(fs::read_to_string(d.join("extension/background.js")).unwrap(), "old");
        assert_eq!(names(&d), ["alc-cam-host.exe", "extension"]);
    }

    #[test]
    fn wrong_extension_id_or_version_is_refused() {
        let (d, exe) = site("id");
        let other_key = base64::engine::general_purpose::STANDARD.encode(b"another key");
        for (zip, what) in [
            (make_zip(&[("manifest.json", &manifest("0.3.1", &other_key))]), "ID"),
            (make_zip(&[("manifest.json", &manifest("0.3.1", ""))]), "ID"),
            (make_zip(&[("manifest.json", &manifest("0.3.2", &real_key()))]), "版"),
            (make_zip(&[("background.js", "no manifest")]), "manifest"),
            (make_zip(&[("../escape.js", "x")]), "展開"),
        ] {
            let fake = Fake::new("0.3.1", b"v0.3.1", &zip);
            let o = check(&fake, "0.3.1", &exe, IDLE).unwrap();
            assert!(matches!(&o.extension, Part::Failed(e) if e.contains(what)), "{what}: {o:?}");
            assert_eq!(fs::read_to_string(d.join("extension/background.js")).unwrap(), "old");
            assert_eq!(names(&d), ["alc-cam-host.exe", "extension"]);
        }
        assert!(!d.parent().unwrap().join("escape.js").exists());
    }

    #[test]
    fn missing_extension_dir_is_left_alone() {
        let (d, exe) = site("missing-ext");
        fs::remove_dir_all(d.join("extension")).unwrap();
        let fake = Fake::new("0.3.0", b"x", &new_ext("0.3.0"));
        let o = check(&fake, "0.3.0", &exe, IDLE).unwrap();
        assert_eq!(o.extension, Part::Missing);
        assert_eq!(summarize(&o).0, "latest");
        assert!(!d.join("extension").exists());
    }

    #[test]
    fn broken_release_keeps_current() {
        let (d, exe) = site("broken");
        let mut fake = Fake::new("0.3.1", b"", b"");
        fake.files.insert(LATEST_URL.into(), b"<html>".to_vec());
        assert!(check(&fake, "0.3.0", &exe, IDLE).is_err());
        fake.files.insert(LATEST_URL.into(), br#"{"version":"latest","host_sha256":"","extension_sha256":""}"#.to_vec());
        assert!(check(&fake, "0.3.0", &exe, IDLE).is_err());
        fake.files.clear();
        assert!(check(&fake, "0.3.0", &exe, IDLE).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
        assert_eq!(names(&d), ["alc-cam-host.exe", "extension"]);
    }

    /// 1 回目 (今の物 → .old) は通し、2 回目 (新しい物 → 今の場所) だけ落とす rename
    fn fail_second(calls: &RefCell<u32>) -> impl Fn(&Path, &Path) -> io::Result<()> + '_ {
        move |from, to| {
            *calls.borrow_mut() += 1;
            if *calls.borrow() % 3 == 2 {
                return Err(io::Error::other("busy"));
            }
            fs::rename(from, to)
        }
    }

    #[test]
    fn failed_second_rename_restores_both() {
        let (d, exe) = site("rollback");
        let fake = Fake::new("0.3.1", b"v0.3.1", &new_ext("0.3.1"));
        let calls = RefCell::new(0);
        let o = check_with(&fake, "0.3.0", &exe, IDLE, &fail_second(&calls)).unwrap();
        assert!(matches!(o.host, Part::Failed(_)) && matches!(o.extension, Part::Failed(_)), "{o:?}");
        // host・拡張それぞれ 3 回 (退ける・置く (失敗)・戻す)
        assert_eq!(*calls.borrow(), 6);
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
        assert_eq!(fs::read_to_string(d.join("extension/background.js")).unwrap(), "old");
        assert_eq!(names(&d), ["alc-cam-host.exe", "extension"]);
    }

    #[test]
    fn busy_old_exe_is_sidestepped() {
        let (d, exe) = site("busy-old");
        // 消せない .old (中身のあるフォルダ) が居ても .old-<pid> に逃がして入れ替える
        fs::create_dir_all(d.join("alc-cam-host.exe.old/x")).unwrap();
        replace_with(&exe, b"v3", &rename).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"v3");
        assert_eq!(fs::read(d.join(format!("alc-cam-host.exe.old-{}", std::process::id()))).unwrap(), b"v0.3.0");
    }

    #[test]
    fn missing_exe_leaves_nothing() {
        let (d, exe) = site("missing-exe");
        fs::remove_file(&exe).unwrap();
        assert!(replace_with(&exe, b"new", &rename).is_err());
        assert_eq!(names(&d), ["extension"]);
    }

    /// 本物の GitHub (github.com → *.githubusercontent.com の redirect) を辿れること。
    /// ネットワークが要るので既定では走らない: cargo test -p alc-cam-host -- --ignored real_github
    #[test]
    #[ignore]
    fn real_github_follows_redirects() {
        let gh = GitHub::default();
        let zip = gh.get(&asset_url("0.2.0", "alc-cam-windows.zip"), MAX_EXE).unwrap();
        assert_eq!(&zip[..2], b"PK");
        assert!(gh.get(&asset_url("0.2.0", "no-such-file"), MAX_JSON).unwrap_err().contains("404"));
        assert!(gh.get(&asset_url("0.2.0", "alc-cam-windows.zip"), 16).unwrap_err().contains("大きすぎ"));
    }
}
