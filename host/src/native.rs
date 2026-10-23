//! Chrome native messaging の枠: 4 バイト (native endian = x86 では little endian) の長さ + UTF-8 JSON。
//! stdout にはこの枠以外を書かない (ログは stderr)。

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Chrome が host へ送れる 1 通の上限は 4GB だが、ここで受けるのは設定と SDP だけ
const MAX_IN: usize = 1024 * 1024;

/// 拡張 → host
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum In {
    Config { rtsp_url: String, stun: String },
    /// 見る人が入ってきた: offer を作る
    Start,
    /// 見る人が出ていった: 畳む
    Stop,
    SdpAnswer { sdp: String },
    IceCandidate { candidate: Value },
}

/// host → 拡張
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Out {
    Ready { version: &'static str },
    SdpOffer { sdp: String },
    Camera { ok: bool, message: String },
    Peer { ok: bool, message: String },
    Log { message: String },
}

/// 1 通読む。stdin が閉じたら `Ok(None)` (= 拡張が閉じた。host は終わる)
pub fn read(r: &mut impl Read) -> io::Result<Option<Result<In, String>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_IN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "native message too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).map_err(|e| e.to_string())))
}

pub fn write(w: &mut impl Write, msg: &Out) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    w.write_all(&(body.len() as u32).to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: &str) -> Vec<u8> {
        let mut v = (json.len() as u32).to_ne_bytes().to_vec();
        v.extend_from_slice(json.as_bytes());
        v
    }

    #[test]
    fn reads_messages_until_eof() {
        let mut data = frame(r#"{"type":"config","rtsp_url":"rtsp://c/s","stun":"s:1"}"#);
        data.extend(frame(r#"{"type":"start"}"#));
        data.extend(frame(r#"{"type":"nope"}"#));
        let mut r = data.as_slice();
        assert_eq!(read(&mut r).unwrap(), Some(Ok(In::Config { rtsp_url: "rtsp://c/s".into(), stun: "s:1".into() })));
        assert_eq!(read(&mut r).unwrap(), Some(Ok(In::Start)));
        assert!(matches!(read(&mut r).unwrap(), Some(Err(_))));
        assert_eq!(read(&mut r).unwrap(), None);
    }

    #[test]
    fn writes_framed_json() {
        let mut out = Vec::new();
        write(&mut out, &Out::SdpOffer { sdp: "v=0".into() }).unwrap();
        let len = u32::from_ne_bytes(out[..4].try_into().unwrap()) as usize;
        assert_eq!(&out[4..], br#"{"type":"sdp_offer","sdp":"v=0"}"#);
        assert_eq!(len, out.len() - 4);
    }
}
