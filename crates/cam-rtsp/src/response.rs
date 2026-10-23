//! TCP 上に交互に来る「RTSP の応答 (テキスト)」と「`$` で始まる interleaved フレーム」を切り出す。

use crate::{err, Error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn headers_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Item {
    Response(Response),
    /// カメラから来たリクエスト等、応答でないテキスト。読み捨てる
    Other,
    Interleaved { channel: u8, data: Vec<u8> },
}

/// 応答 1 つの上限 (SDP 込み)。超えたら壊れた入力とみなす
const MAX_TEXT: usize = 64 * 1024;

/// `buf` の先頭から 1 つ切り出す。足りなければ `Ok(None)` (続きを待つ)。
pub fn next(buf: &mut Vec<u8>) -> Result<Option<Item>, Error> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'$' {
        if buf.len() < 4 {
            return Ok(None);
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if buf.len() < 4 + len {
            return Ok(None);
        }
        let channel = buf[1];
        let data = buf[4..4 + len].to_vec();
        buf.drain(..4 + len);
        return Ok(Some(Item::Interleaved { channel, data }));
    }

    let Some(head_end) = find(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_TEXT {
            return err("RTSP の応答ヘッダが長すぎます");
        }
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_TEXT {
        return err("RTSP の応答本文が長すぎます");
    }
    let total = head_end + 4 + content_length;
    if buf.len() < total {
        return Ok(None);
    }
    let body = buf[head_end + 4..total].to_vec();
    buf.drain(..total);

    let Some(rest) = first.strip_prefix("RTSP/1.0 ") else {
        return Ok(Some(Item::Other));
    };
    let status = rest.get(..3).and_then(|s| s.parse().ok());
    let Some(status) = status else {
        return err("RTSP の応答行が読めません");
    };
    Ok(Some(Item::Response(Response { status, headers, body })))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_response_and_interleaved() {
        let mut buf = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 3\r\n\r\nabc$\x00\x00\x02hi".to_vec();
        let Some(Item::Response(r)) = next(&mut buf).unwrap() else { panic!() };
        assert_eq!(r.status, 200);
        assert_eq!(r.header("cseq"), Some("2"));
        assert_eq!(r.body, b"abc");
        assert_eq!(next(&mut buf).unwrap(), Some(Item::Interleaved { channel: 0, data: b"hi".to_vec() }));
        assert!(buf.is_empty());
    }

    #[test]
    fn waits_for_more() {
        let mut buf = b"RTSP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nab".to_vec();
        assert_eq!(next(&mut buf).unwrap(), None);
        let mut buf = b"$\x00\x00\x05ab".to_vec();
        assert_eq!(next(&mut buf).unwrap(), None);
        assert_eq!(buf.len(), 6);
    }

    #[test]
    fn requests_from_camera_are_other() {
        let mut buf = b"GET_PARAMETER rtsp://x RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec();
        assert_eq!(next(&mut buf).unwrap(), Some(Item::Other));
    }
}
