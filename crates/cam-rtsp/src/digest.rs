//! RTSP の認証 (RFC 2617 Digest / Basic)。Tapo は Digest を返す。

use base64::Engine;
use md5::{Digest as _, Md5};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Digest { realm: String, nonce: String, opaque: Option<String>, qop_auth: bool, stale: bool },
    Basic,
}

/// `WWW-Authenticate` の値を読む。複数あれば呼び手が Digest を優先して選ぶ。
pub fn parse(value: &str) -> Option<Challenge> {
    let value = value.trim();
    let (scheme, params) = value.split_once(' ').unwrap_or((value, ""));
    if scheme.eq_ignore_ascii_case("basic") {
        return Some(Challenge::Basic);
    }
    if !scheme.eq_ignore_ascii_case("digest") {
        return None;
    }
    let mut realm = None;
    let mut nonce = None;
    let mut opaque = None;
    let mut qop_auth = false;
    let mut stale = false;
    for (k, v) in params_iter(params) {
        match k.to_ascii_lowercase().as_str() {
            "realm" => realm = Some(v),
            "nonce" => nonce = Some(v),
            "opaque" => opaque = Some(v),
            "qop" => qop_auth = v.split(',').any(|q| q.trim() == "auth"),
            "stale" => stale = v.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    Some(Challenge::Digest { realm: realm?, nonce: nonce?, opaque, qop_auth, stale })
}

/// `key=value` / `key="value, with comma"` の並びを読む
fn params_iter(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_string();
        rest = rest[eq + 1..].trim_start();
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').unwrap_or(r.len());
            value = r[..end].to_string();
            rest = r.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        rest = rest.trim_start().trim_start_matches(',').trim_start();
        out.push((key, value));
    }
    out
}

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `Authorization` ヘッダの値を作る。`nc` はこの nonce で何回目か (1 始まり)
pub fn authorization(ch: &Challenge, method: &str, uri: &str, user: &str, pass: &str, nc: u32, cnonce: &str) -> String {
    match ch {
        Challenge::Basic => {
            let raw = format!("{user}:{pass}");
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
        }
        Challenge::Digest { realm, nonce, opaque, qop_auth, .. } => {
            let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
            let ha2 = md5_hex(&format!("{method}:{uri}"));
            let mut v = format!(r#"Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}""#);
            if *qop_auth {
                let nc = format!("{nc:08x}");
                let response = md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
                v += &format!(r#", response="{response}", qop=auth, nc={nc}, cnonce="{cnonce}""#);
            } else {
                let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
                v += &format!(r#", response="{response}""#);
            }
            if let Some(o) = opaque {
                v += &format!(r#", opaque="{o}""#);
            }
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_digest() {
        let c = parse(r#"Digest realm="TP-Link IP-Camera", nonce="abc,def", qop="auth,auth-int", stale=FALSE"#).unwrap();
        assert_eq!(
            c,
            Challenge::Digest {
                realm: "TP-Link IP-Camera".into(),
                nonce: "abc,def".into(),
                opaque: None,
                qop_auth: true,
                stale: false
            }
        );
        assert_eq!(parse("Basic realm=\"x\""), Some(Challenge::Basic));
        assert_eq!(parse("Bearer x"), None);
    }

    // RFC 2617 3.5 の例 (method と uri は RFC のまま)
    #[test]
    fn rfc2617_example() {
        let c = Challenge::Digest {
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".into()),
            qop_auth: true,
            stale: false,
        };
        let v = authorization(&c, "GET", "/dir/index.html", "Mufasa", "Circle Of Life", 1, "0a4f113b");
        assert!(v.contains(r#"response="6629fae49393a05397450978507c4ef1""#), "{v}");
        assert!(v.contains("nc=00000001"));
    }

    #[test]
    fn digest_without_qop() {
        let c = Challenge::Digest { realm: "r".into(), nonce: "n".into(), opaque: None, qop_auth: false, stale: false };
        let v = authorization(&c, "DESCRIBE", "rtsp://c/s", "u", "p", 1, "x");
        let expect = md5_hex(&format!("{}:n:{}", md5_hex("u:r:p"), md5_hex("DESCRIBE:rtsp://c/s")));
        assert!(v.contains(&format!(r#"response="{expect}""#)));
        assert!(!v.contains("qop"));
    }

    #[test]
    fn basic() {
        assert_eq!(authorization(&Challenge::Basic, "X", "u", "a", "b", 1, ""), "Basic YTpi");
    }
}
