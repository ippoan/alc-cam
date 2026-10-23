//! host の自己更新。拡張が `update_check` を送った時だけ動く (見る人が居ない時は拡張が確認のためだけに起動する)。
//!
//!   1. GitHub の最新 Release の `alc-cam-host.json` ({"version","sha256"}) を読む
//!   2. 自分の版より新しければ、その版の Release の `alc-cam-host.exe` を落とし、SHA256 を確かめる
//!   3. 同じフォルダに `.new-<pid>` で書き、実行中の exe を `.old` へ rename してから `.new-<pid>` を exe へ rename する
//!      (Windows は実行中の exe を消せないが rename はできる)。新版は次の起動から。`.old` は次の起動で消す
//!
//! どこで失敗しても exe は今の版のまま (2 つ目の rename が失敗したら `.old` を戻す)。
//! 出る先は github.com と GitHub のファイル置き場 (*.githubusercontent.com) の https だけ (redirect 先も確かめる)。

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const LATEST_URL: &str = "https://github.com/ippoan/alc-cam/releases/latest/download/alc-cam-host.json";
const RELEASE_BASE: &str = "https://github.com/ippoan/alc-cam/releases/download";
const MAX_JSON: u64 = 64 * 1024;
const MAX_EXE: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

pub fn exe_url(version: &str) -> String {
    format!("{RELEASE_BASE}/v{version}/alc-cam-host.exe")
}

/// Release に置く `alc-cam-host.json`
#[derive(Debug, Deserialize)]
pub struct Latest {
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// 今の版が最新 (または新しい)
    UpToDate { latest: String },
    /// 入れ替えた。次の起動から新版
    Installed { version: String },
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

/// https で、github.com か *.githubusercontent.com だけ
pub fn allowed_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    !host.contains(['@', ':']) && (host == "github.com" || host.ends_with(".githubusercontent.com"))
}

pub trait Fetch {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String>;
}

/// 確認して、新しければ入れ替える
pub fn check(fetch: &impl Fetch, current: &str, exe: &Path) -> Result<Outcome, String> {
    let body = fetch.get(LATEST_URL, MAX_JSON)?;
    let latest: Latest = serde_json::from_slice(&body).map_err(|e| format!("版の情報が読めません: {e}"))?;
    if parse_version(&latest.version).is_none() {
        return Err(format!("版の形が違います: {}", latest.version));
    }
    if !is_newer(current, &latest.version) {
        return Ok(Outcome::UpToDate { latest: latest.version });
    }
    let bin = fetch.get(&exe_url(&latest.version), MAX_EXE)?;
    let got = sha256_hex(&bin);
    if !got.eq_ignore_ascii_case(latest.sha256.trim()) {
        return Err(format!("v{} の SHA256 が合わないので入れ替えません", latest.version));
    }
    replace(exe, &bin).map_err(|e| format!("入れ替えに失敗 (今の版のまま): {e}"))?;
    Ok(Outcome::Installed { version: latest.version })
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// `exe` を `bytes` に入れ替える。実行中の exe でもよい (Windows でも rename はできる)
pub fn replace(exe: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(exe, bytes, |from, to| fs::rename(from, to))
}

/// `replace` の rename を差し替えられる形 (テストで 2 回目の rename を落とす)
fn replace_with(exe: &Path, bytes: &[u8], rename: impl Fn(&Path, &Path) -> io::Result<()>) -> io::Result<()> {
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

/// 前回の入れ替えの残り (`.old*` / `.new-*`) を消す。動いている物は消えないので放っておく
pub fn cleanup(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else { return };
    let Ok(entries) = fs::read_dir(dir) else { return };
    let (old, new) = (format!("{name}.old"), format!("{name}.new-"));
    for e in entries.flatten() {
        let n = e.file_name();
        let Some(n) = n.to_str() else { continue };
        if n.starts_with(&old) || n.starts_with(&new) {
            let _ = fs::remove_file(e.path());
        }
    }
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

    struct Fake {
        files: HashMap<String, Vec<u8>>,
        asked: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(version: &str, sha: &str, exe: &[u8]) -> Self {
            let json = format!(r#"{{"version":"{version}","sha256":"{sha}"}}"#).into_bytes();
            let files = HashMap::from([(LATEST_URL.to_string(), json), (exe_url(version), exe.to_vec())]);
            Self { files, asked: RefCell::default() }
        }
    }

    impl Fetch for Fake {
        fn get(&self, url: &str, _limit: u64) -> Result<Vec<u8>, String> {
            self.asked.borrow_mut().push(url.to_string());
            self.files.get(url).cloned().ok_or_else(|| "404".into())
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("alc-cam-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn installs_newer_and_keeps_old_aside() {
        let d = tmp("install");
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v0.3.0").unwrap();
        let fake = Fake::new("0.3.1", &sha256_hex(b"v0.3.1"), b"v0.3.1");
        assert_eq!(check(&fake, "0.3.0", &exe), Ok(Outcome::Installed { version: "0.3.1".into() }));
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.1");
        assert_eq!(fs::read(d.join("alc-cam-host.exe.old")).unwrap(), b"v0.3.0");
        cleanup(&exe);
        let left: Vec<_> = fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("alc-cam-host.exe")]);
    }

    #[test]
    fn up_to_date_does_not_download() {
        let d = tmp("same");
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v0.3.0").unwrap();
        let fake = Fake::new("0.3.0", &sha256_hex(b"x"), b"x");
        assert_eq!(check(&fake, "0.3.0", &exe), Ok(Outcome::UpToDate { latest: "0.3.0".into() }));
        assert_eq!(*fake.asked.borrow(), vec![LATEST_URL.to_string()]);
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
    }

    #[test]
    fn sha_mismatch_keeps_current() {
        let d = tmp("sha");
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v0.3.0").unwrap();
        let fake = Fake::new("0.3.1", &sha256_hex(b"something else"), b"tampered");
        assert!(check(&fake, "0.3.0", &exe).unwrap_err().contains("SHA256"));
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1);
    }

    #[test]
    fn broken_latest_keeps_current() {
        let d = tmp("broken");
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v0.3.0").unwrap();
        let mut fake = Fake::new("0.3.1", "", b"");
        fake.files.insert(LATEST_URL.into(), b"<html>".to_vec());
        assert!(check(&fake, "0.3.0", &exe).is_err());
        fake.files.insert(LATEST_URL.into(), br#"{"version":"latest","sha256":""}"#.to_vec());
        assert!(check(&fake, "0.3.0", &exe).is_err());
        fake.files.clear();
        assert!(check(&fake, "0.3.0", &exe).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
    }

    #[test]
    fn failed_second_rename_restores_exe() {
        let d = tmp("rollback");
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v0.3.0").unwrap();
        // 1 回目 (exe → .old) は通し、2 回目 (.new → exe) だけ落とす。3 回目 (.old → exe) で戻る
        let calls = RefCell::new(0);
        let res = replace_with(&exe, b"v0.3.1", |from, to| {
            *calls.borrow_mut() += 1;
            if *calls.borrow() == 2 {
                return Err(io::Error::other("busy"));
            }
            fs::rename(from, to)
        });
        assert!(res.is_err());
        assert_eq!(*calls.borrow(), 3);
        assert_eq!(fs::read(&exe).unwrap(), b"v0.3.0");
        // .new も .old も残らない
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1);
    }

    /// 本物の GitHub (github.com → *.githubusercontent.com の redirect) を辿れること。
    /// ネットワークが要るので既定では走らない: cargo test -p alc-cam-host -- --ignored real_github
    #[test]
    #[ignore]
    fn real_github_follows_redirects() {
        let gh = GitHub::default();
        let zip = gh.get(&format!("{RELEASE_BASE}/v0.2.0/alc-cam-windows.zip"), MAX_EXE).unwrap();
        assert_eq!(&zip[..2], b"PK");
        assert!(gh.get(&format!("{RELEASE_BASE}/v0.2.0/no-such-file"), MAX_JSON).unwrap_err().contains("404"));
        assert!(gh.get(&format!("{RELEASE_BASE}/v0.2.0/alc-cam-windows.zip"), 16).unwrap_err().contains("大きすぎ"));
    }

    #[test]
    fn missing_exe_leaves_nothing() {
        let d = tmp("missing");
        assert!(replace(&d.join("alc-cam-host.exe"), b"new").is_err());
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0);
    }

    #[test]
    fn busy_old_is_sidestepped() {
        let d = tmp("busy");
        let exe = d.join("alc-cam-host.exe");
        fs::write(&exe, b"v2").unwrap();
        // 消せない .old (中身のあるフォルダ) が居ても .old-<pid> に逃がして入れ替える
        let old = d.join("alc-cam-host.exe.old");
        fs::create_dir_all(old.join("x")).unwrap();
        replace(&exe, b"v3").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"v3");
        assert_eq!(fs::read(d.join(format!("alc-cam-host.exe.old-{}", std::process::id()))).unwrap(), b"v2");
    }
}
