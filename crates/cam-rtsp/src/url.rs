//! `rtsp://user:pass@host:port/path` を、Request-URI (資格情報を抜いたもの) と資格情報に分ける。

use crate::{err, Error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspUrl {
    /// 資格情報を抜いた URL。リクエスト行と Digest の uri に使う
    pub uri: String,
    pub user: String,
    pub pass: String,
}

pub fn parse(raw: &str) -> Result<RtspUrl, Error> {
    let Some(rest) = raw.strip_prefix("rtsp://") else {
        return err("RTSP URL は rtsp:// で始めてください");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (userinfo, host) = match authority.rfind('@') {
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
        None => (None, authority),
    };
    if host.is_empty() {
        return err("RTSP URL にホストがありません");
    }
    let (user, pass) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((a, b)) => (percent_decode(a)?, percent_decode(b)?),
            None => (percent_decode(u)?, String::new()),
        },
        None => (String::new(), String::new()),
    };
    Ok(RtspUrl { uri: format!("rtsp://{host}{path}"), user, pass })
}

/// TCP で繋ぐ先 (`host`, `port`)。port が無ければ RTSP の既定 554
pub fn target(raw: &str) -> Result<(String, u16), Error> {
    let u = parse(raw)?;
    let authority = u.uri["rtsp://".len()..].split('/').next().unwrap_or_default();
    // [v6]:port は使わない前提 (カメラは LAN の IPv4)
    match authority.rsplit_once(':') {
        Some((host, port)) => match port.parse() {
            Ok(p) => Ok((host.to_string(), p)),
            Err(_) => err("RTSP URL の port が数ではありません"),
        },
        None => Ok((authority.to_string(), 554)),
    }
}

/// SDP の `a=control` を基準 URL に対して解決する (RFC 2326 C.1.1)。
pub fn resolve_control(base: &str, control: &str) -> String {
    if control.is_empty() || control == "*" {
        return base.to_string();
    }
    if control.starts_with("rtsp://") || control.starts_with("rtsps://") {
        return control.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{control}")
    } else {
        format!("{base}/{control}")
    }
}

fn percent_decode(s: &str) -> Result<String, Error> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(b) = hex else {
                return err("RTSP URL の資格情報の %xx が壊れています");
            };
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).or_else(|_| err("RTSP URL の資格情報が UTF-8 ではありません"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_credentials() {
        let u = parse("rtsp://admin:p%40ss@192.0.2.1:554/stream2").unwrap();
        assert_eq!(u.uri, "rtsp://192.0.2.1:554/stream2");
        assert_eq!(u.user, "admin");
        assert_eq!(u.pass, "p@ss");
    }

    #[test]
    fn no_credentials_and_no_path() {
        let u = parse("rtsp://cam").unwrap();
        assert_eq!(u.uri, "rtsp://cam/");
        assert_eq!(u.user, "");
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(parse("http://cam/").is_err());
        assert!(parse("rtsp:///x").is_err());
    }

    #[test]
    fn target_host_port() {
        assert_eq!(target("rtsp://u:p@192.0.2.1:8554/s").unwrap(), ("192.0.2.1".into(), 8554));
        assert_eq!(target("rtsp://cam/s").unwrap(), ("cam".into(), 554));
        assert!(target("rtsp://cam:x/s").is_err());
    }

    #[test]
    fn resolves_control() {
        assert_eq!(resolve_control("rtsp://c/s2/", "track1"), "rtsp://c/s2/track1");
        assert_eq!(resolve_control("rtsp://c/s2", "track1"), "rtsp://c/s2/track1");
        assert_eq!(resolve_control("rtsp://c/s2", "*"), "rtsp://c/s2");
        assert_eq!(resolve_control("rtsp://c/s2", "rtsp://c/s2/t"), "rtsp://c/s2/t");
    }
}
