//! DESCRIBE の SDP から映像 (H.264) のトラックを 1 本選ぶ。音声は `audio` が選ぶ。

use base64::Engine;

use crate::{err, Error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrack {
    pub control: String,
    pub payload_type: u8,
    pub clock_rate: u32,
    /// `sprop-parameter-sets` の SPS / PPS (NAL ヘッダ込み、開始コード無し)
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

pub fn parse(sdp: &str) -> Result<VideoTrack, Error> {
    let mut in_video = false;
    let mut pt: Option<u8> = None;
    let mut control = String::new();
    let mut codec_ok = false;
    let mut clock_rate = 90_000;
    let mut sps = None;
    let mut pps = None;

    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            if in_video && pt.is_some() {
                break; // 最初の映像トラックだけ使う
            }
            let mut f = m.split_whitespace();
            in_video = f.next() == Some("video");
            if in_video {
                pt = f.nth(2).and_then(|p| p.parse().ok());
            }
            continue;
        }
        if !in_video {
            continue;
        }
        let Some(pt) = pt else { continue };
        if let Some(c) = line.strip_prefix("a=control:") {
            control = c.trim().to_string();
        } else if let Some(r) = line.strip_prefix("a=rtpmap:") {
            let (num, enc) = r.split_once(' ').unwrap_or((r, ""));
            if num.parse() == Ok(pt) {
                let mut e = enc.split('/');
                codec_ok = e.next().is_some_and(|n| n.eq_ignore_ascii_case("H264"));
                if let Some(rate) = e.next().and_then(|r| r.parse().ok()) {
                    clock_rate = rate;
                }
            }
        } else if let Some(f) = line.strip_prefix("a=fmtp:") {
            let (num, params) = f.split_once(' ').unwrap_or((f, ""));
            if num.parse() != Ok(pt) {
                continue;
            }
            for p in params.split(';') {
                if let Some(sets) = p.trim().strip_prefix("sprop-parameter-sets=") {
                    for nal in sets.split(',') {
                        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(nal.trim()) else {
                            continue;
                        };
                        match bytes.first().map(|h| h & 0x1f) {
                            Some(7) => sps = Some(bytes),
                            Some(8) => pps = Some(bytes),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    let Some(payload_type) = pt else {
        return err("SDP に映像トラックがありません");
    };
    if !codec_ok {
        return err("映像が H.264 ではありません");
    }
    Ok(VideoTrack { control, payload_type, clock_rate, sps, pps })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tapo C212 の stream2 に近い形 (値は作り物)
    const SDP: &str = "v=0\r\n\
o=- 1 1 IN IP4 0.0.0.0\r\n\
s=Session streamed by \"TP-LINK RTSP Server\"\r\n\
t=0 0\r\n\
a=control:*\r\n\
m=video 0 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1; profile-level-id=4D001E; sprop-parameter-sets=Z00AHpWoKAv+WbgICAgQ,aO48gA==\r\n\
a=control:track1\r\n\
m=audio 0 RTP/AVP 8\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=control:track2\r\n";

    #[test]
    fn picks_h264_video() {
        let t = parse(SDP).unwrap();
        assert_eq!(t.control, "track1");
        assert_eq!(t.payload_type, 96);
        assert_eq!(t.clock_rate, 90_000);
        assert_eq!(t.sps.as_ref().unwrap()[..4], [0x67, 0x4d, 0x00, 0x1e]);
        assert_eq!(t.pps.as_ref().unwrap()[0], 0x68);
    }

    #[test]
    fn rejects_non_h264() {
        let sdp = "m=video 0 RTP/AVP 97\r\na=rtpmap:97 H265/90000\r\n";
        assert!(parse(sdp).is_err());
        assert!(parse("m=audio 0 RTP/AVP 8\r\n").is_err());
    }
}
