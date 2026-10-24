//! カメラのスピーカーへ送る `audio/transmit.cgi` (I-O DATA の公開仕様 QwatchAPI「通話」)。
//!
//! 1. 管理画面の port に TCP で繋ぎ、`GET /camera-cgi/audio/transmit.cgi` (Keep-Alive)。401 なら Digest を付けて **繋ぎ直して** 送り直す
//!    (仕様: TS-NS310W 1.00.46 等は切断して再接続しないと受けない)
//! 2. `200 OK` のあとに受ける形式が `Content-Type` で来る。他の人が通話中なら `Too many users`
//! 3. 接続を保ったまま音声を送り続け、終えるときは切断する
//!
//! 応答の判定だけを持つ (TCP は host)。

use cam_ptz::Response;

pub const PATH: &str = "/camera-cgi/audio/transmit.cgi";

/// カメラが受ける形式 (仕様の Content-Type)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `audio/aac-lc`: ADTS・AAC-LC・モノラル・8kHz (TS-NS410W 1.02.18 以降と TS-NA230WP は 16kHz)
    AacLc,
    /// `audio/aac`: ADTS・AAC-LC・モノラル・16kHz
    Aac,
    /// `audio/gsm`: GSM 06.10 (送れない)
    Gsm,
}

impl Format {
    /// AAC を作る周波数 (GSM は None)
    pub fn sample_rate(self) -> Option<u32> {
        match self {
            Format::AacLc => Some(8000),
            Format::Aac => Some(16000),
            Format::Gsm => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Format::AacLc => "AAC 8kHz",
            Format::Aac => "AAC 16kHz",
            Format::Gsm => "GSM",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// 続きを待つ
    Pending,
    /// 200 だが形式がまだ来ていない (しばらく待って来なければ `audio/aac` とみなす)
    OkWithoutType,
    Ready(Format),
    /// 他の人が通話中
    Busy,
    /// 401 (Digest を付けて繋ぎ直す)
    Unauthorized(Response),
    Failed(String),
}

/// ここまでに読めた応答を判定する
pub fn judge(raw: &[u8]) -> Reply {
    let Some(resp) = cam_ptz::parse_response(raw) else { return Reply::Pending };
    match resp.status {
        200 => {
            // 形式は応答のヘッダにも本文にも来うる (仕様の例は 200 の後ろに Content-Type を続けている)
            let text = String::from_utf8_lossy(raw).to_ascii_lowercase();
            if text.contains("too many users") {
                Reply::Busy
            } else if text.contains("audio/aac-lc") {
                Reply::Ready(Format::AacLc)
            } else if text.contains("audio/aac") {
                Reply::Ready(Format::Aac)
            } else if text.contains("audio/gsm") {
                Reply::Ready(Format::Gsm)
            } else {
                Reply::OkWithoutType
            }
        }
        401 => Reply::Unauthorized(resp),
        404 => Reply::Failed("このカメラは通話 (スピーカー) に対応していません".into()),
        s => Reply::Failed(format!("カメラが HTTP {s} を返しました")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judges_spec_examples() {
        assert_eq!(judge(b"HTTP/1.1 200 OK\r\n"), Reply::Pending);
        assert_eq!(judge(b"HTTP/1.1 200 OK\r\nConnection: Keep-Alive\r\n\r\nContent-Type: audio/aac\r\n\r\n"), Reply::Ready(Format::Aac));
        assert_eq!(judge(b"HTTP/1.1 200 OK\r\nContent-Type: audio/aac-lc\r\n\r\n"), Reply::Ready(Format::AacLc));
        assert_eq!(judge(b"HTTP/1.1 200 OK\r\n\r\nContent-Type: audio/gsm\r\n\r\n"), Reply::Ready(Format::Gsm));
        assert_eq!(judge(b"HTTP/1.1 200 OK\r\nX-CurrentUsers: Too many users\r\nContent-Length: 0\r\nContent-Type: text/plain\r\n\r\n"), Reply::Busy);
        assert_eq!(judge(b"HTTP/1.1 200 OK\r\n\r\n"), Reply::OkWithoutType);
        assert!(matches!(judge(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"x\", nonce=\"n\"\r\n\r\n"), Reply::Unauthorized(r) if r.www_authenticate.len() == 1));
        assert!(matches!(judge(b"HTTP/1.1 404 Not Found\r\n\r\n"), Reply::Failed(m) if m.contains("対応していません")));
        assert!(matches!(judge(b"HTTP/1.1 503 X\r\n\r\n"), Reply::Failed(m) if m.contains("503")));
        assert_eq!(Format::AacLc.sample_rate(), Some(8000));
        assert_eq!(Format::Aac.sample_rate(), Some(16000));
        assert_eq!(Format::Gsm.sample_rate(), None);
    }
}
