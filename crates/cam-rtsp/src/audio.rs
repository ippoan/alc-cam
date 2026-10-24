//! DESCRIBE の SDP から音声のトラックを 1 本選び、RTP から音声のまとまり (AAC の AU / G.711 の塊) を取り出す。
//!
//! - AAC は RFC 3640 の `mpeg4-generic` (`mode=AAC-hbr`、AU ヘッダ付き)。I-O DATA の TS-NS 系は AAC 32kHz
//! - G.711 (PCMU / PCMA) は RTP のペイロードがそのまま音声
//! - それ以外 (MP4A-LATM 等) は使わない (映像だけで続ける)

use crate::h264::Rtp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioCodec {
    /// `config` は AudioSpecificConfig (SDP の `config=` の 16 進)
    Aac { config: Vec<u8>, size_length: u8, index_length: u8, index_delta_length: u8 },
    Pcmu,
    Pcma,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    pub control: String,
    pub payload_type: u8,
    pub clock_rate: u32,
    pub channels: u8,
    pub codec: AudioCodec,
}

impl AudioTrack {
    /// ログ・画面に出す短い名前
    pub fn describe(&self) -> String {
        let name = match self.codec {
            AudioCodec::Aac { .. } => "AAC",
            AudioCodec::Pcmu => "G.711 μ-law",
            AudioCodec::Pcma => "G.711 A-law",
        };
        format!("{name} {}Hz", self.clock_rate)
    }
}

/// 最初の音声トラック (使える形式のもの)。無ければ None
pub fn parse(sdp: &str) -> Option<AudioTrack> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            sections.push(vec![line]);
        } else if let Some(s) = sections.last_mut() {
            s.push(line);
        }
    }
    sections.iter().filter(|s| s[0].starts_with("m=audio ")).find_map(|s| track(s))
}

fn track(lines: &[&str]) -> Option<AudioTrack> {
    let pt: u8 = lines[0].split_whitespace().nth(3)?.parse().ok()?;
    let mut control = String::new();
    let mut rtpmap: Option<(String, u32, u8)> = None;
    let mut fmtp = "";
    for line in &lines[1..] {
        if let Some(c) = line.strip_prefix("a=control:") {
            control = c.trim().to_string();
        } else if let Some(r) = line.strip_prefix("a=rtpmap:") {
            let (num, enc) = r.split_once(' ').unwrap_or((r, ""));
            if num.parse() == Ok(pt) {
                let mut e = enc.trim().split('/');
                let name = e.next().unwrap_or_default().to_ascii_uppercase();
                let rate = e.next().and_then(|r| r.parse().ok()).unwrap_or(8000);
                let ch = e.next().and_then(|c| c.parse().ok()).unwrap_or(1);
                rtpmap = Some((name, rate, ch));
            }
        } else if let Some(f) = line.strip_prefix("a=fmtp:") {
            let (num, params) = f.split_once(' ').unwrap_or((f, ""));
            if num.parse() == Ok(pt) {
                fmtp = params;
            }
        }
    }
    // 静的 PT (0 = PCMU, 8 = PCMA) は rtpmap が無くてもよい
    let (name, clock_rate, channels) = rtpmap.unwrap_or_else(|| match pt {
        0 => ("PCMU".into(), 8000, 1),
        8 => ("PCMA".into(), 8000, 1),
        _ => (String::new(), 0, 0),
    });
    let codec = match name.as_str() {
        "PCMU" => AudioCodec::Pcmu,
        "PCMA" => AudioCodec::Pcma,
        "MPEG4-GENERIC" => aac(fmtp)?,
        _ => return None,
    };
    Some(AudioTrack { control, payload_type: pt, clock_rate, channels, codec })
}

fn aac(fmtp: &str) -> Option<AudioCodec> {
    let mut config = None;
    let (mut size_length, mut index_length, mut index_delta_length) = (0, 0, 0);
    let mut mode_ok = false;
    for p in fmtp.split(';') {
        let Some((k, v)) = p.trim().split_once('=') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "config" => config = hex(v),
            "sizelength" => size_length = v.parse().ok()?,
            "indexlength" => index_length = v.parse().ok()?,
            "indexdeltalength" => index_delta_length = v.parse().ok()?,
            "mode" => mode_ok = v.eq_ignore_ascii_case("AAC-hbr") || v.eq_ignore_ascii_case("AAC-lbr"),
            _ => {}
        }
    }
    let config = config.filter(|c| c.len() >= 2)?;
    if !mode_ok || size_length == 0 || size_length > 16 || index_length > 8 || index_delta_length > 8 {
        return None;
    }
    Some(AudioCodec::Aac { config, size_length, index_length, index_delta_length })
}

fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// 音声の RTP → 音声のまとまり (AAC は AU 1 つ、G.711 はペイロードそのまま)
#[derive(Debug, Default)]
pub struct AudioDepacketizer {
    /// 1 パケットに収まらない AU の組み立て中 (残りのバイト数, 中身)
    fragment: Option<(usize, Vec<u8>)>,
    last_seq: Option<u16>,
}

/// 音声のまとまり 1 つ。`rtp_timestamp` はそのまとまりの先頭 (クロックはトラックの `clock_rate`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioUnit {
    pub rtp_timestamp: u32,
    pub data: Vec<u8>,
}

impl AudioDepacketizer {
    pub fn push(&mut self, rtp: &[u8], track: &AudioTrack) -> Vec<AudioUnit> {
        let Some(pkt) = Rtp::parse(rtp) else { return Vec::new() };
        if pkt.payload_type != track.payload_type {
            return Vec::new();
        }
        if self.last_seq.is_some_and(|l| pkt.seq != l.wrapping_add(1)) {
            self.fragment = None; // 途中が抜けた AU は捨てる
        }
        self.last_seq = Some(pkt.seq);
        match &track.codec {
            AudioCodec::Pcmu | AudioCodec::Pcma => {
                if pkt.payload.is_empty() {
                    return Vec::new();
                }
                vec![AudioUnit { rtp_timestamp: pkt.timestamp, data: pkt.payload.to_vec() }]
            }
            AudioCodec::Aac { size_length, index_length, index_delta_length, .. } => {
                self.aac(&pkt, *size_length, *index_length, *index_delta_length)
            }
        }
    }

    fn aac(&mut self, pkt: &Rtp, size_len: u8, index_len: u8, delta_len: u8) -> Vec<AudioUnit> {
        let p = pkt.payload;
        if p.len() < 2 {
            return Vec::new();
        }
        let header_bits = u16::from_be_bytes([p[0], p[1]]) as usize;
        let header_bytes = header_bits.div_ceil(8);
        let Some(headers) = p.get(2..2 + header_bytes) else { return Vec::new() };
        let mut data = &p[2 + header_bytes..];

        // 続きのパケット (RFC 3640 3.2.3.1: 断片は AU ヘッダ 1 つ、サイズは AU 全体)
        if let Some((left, mut buf)) = self.fragment.take() {
            let take = left.min(data.len());
            buf.extend_from_slice(&data[..take]);
            let left = left - take;
            if left > 0 && !pkt.marker {
                self.fragment = Some((left, buf));
                return Vec::new();
            }
            // マーカーが来たのに足りない AU は捨てる
            return if left == 0 { vec![AudioUnit { rtp_timestamp: pkt.timestamp, data: buf }] } else { Vec::new() };
        }

        let mut bits = BitReader { buf: headers, pos: 0 };
        let mut out = Vec::new();
        let mut first = true;
        let mut i = 0u32;
        while bits.pos + size_len as usize <= header_bits {
            let Some(size) = bits.read(size_len) else { break };
            let skip = if first { index_len } else { delta_len };
            if bits.read(skip).is_none() {
                break;
            }
            first = false;
            let size = size as usize;
            // AAC の AU は 1024 サンプル。時刻はまとめて 1 つずつ進める
            let ts = pkt.timestamp.wrapping_add(i * 1024);
            i += 1;
            if size <= data.len() {
                out.push(AudioUnit { rtp_timestamp: ts, data: data[..size].to_vec() });
                data = &data[size..];
            } else {
                // AU が次のパケットへ続く
                if !pkt.marker {
                    self.fragment = Some((size - data.len(), data.to_vec()));
                }
                break;
            }
        }
        out
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn read(&mut self, n: u8) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.buf.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }
}

/// テスト用: AAC の AU を RFC 3640 (AAC-hbr: sizelength=13, indexlength=3, indexdeltalength=3) の RTP ペイロードにする
pub fn packetize_aac(aus: &[&[u8]]) -> Vec<u8> {
    let bits = aus.len() * 16;
    let mut v = (bits as u16).to_be_bytes().to_vec();
    for au in aus {
        v.extend_from_slice(&((au.len() as u16) << 3).to_be_bytes());
    }
    for au in aus {
        v.extend_from_slice(au);
    }
    v
}

/// SDP の `config=` に書く 16 進
pub fn config_hex(config: &[u8]) -> String {
    config.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::build_rtp;

    // TS-NS 系に近い形 (値は作り物): 映像 + AAC 32kHz モノラル
    const SDP: &str = "v=0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:track1\r\n\
m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/32000/1\r\n\
a=fmtp:97 streamtype=5; profile-level-id=15; mode=AAC-hbr; config=1288; SizeLength=13; IndexLength=3; IndexDeltaLength=3\r\n\
a=control:track2\r\n";

    fn aac_track() -> AudioTrack {
        parse(SDP).unwrap()
    }

    #[test]
    fn picks_aac_track() {
        let t = aac_track();
        assert_eq!(t.control, "track2");
        assert_eq!(t.payload_type, 97);
        assert_eq!(t.clock_rate, 32000);
        assert_eq!(t.channels, 1);
        assert_eq!(t.codec, AudioCodec::Aac { config: vec![0x12, 0x88], size_length: 13, index_length: 3, index_delta_length: 3 });
        assert_eq!(t.describe(), "AAC 32000Hz");
    }

    #[test]
    fn picks_g711_and_skips_unknown() {
        let t = parse("m=video 0 RTP/AVP 96\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=control:track2\r\n").unwrap();
        assert_eq!((t.codec, t.clock_rate, t.control.as_str()), (AudioCodec::Pcma, 8000, "track2"));
        // rtpmap の無い静的 PT
        assert_eq!(parse("m=audio 0 RTP/AVP 0\r\n").unwrap().codec, AudioCodec::Pcmu);
        // LATM は使わない
        assert_eq!(parse("m=audio 0 RTP/AVP 98\r\na=rtpmap:98 MP4A-LATM/32000\r\n"), None);
        // 音声が無い
        assert_eq!(parse("m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n"), None);
        // config の無い AAC
        assert_eq!(parse("m=audio 0 RTP/AVP 97\r\na=rtpmap:97 mpeg4-generic/16000\r\na=fmtp:97 mode=AAC-hbr;sizelength=13\r\n"), None);
    }

    #[test]
    fn splits_aac_hbr_packets() {
        let t = aac_track();
        let mut d = AudioDepacketizer::default();
        let rtp = build_rtp(1, 1000, true, 97, &packetize_aac(&[&[1, 2, 3], &[4, 5]]));
        assert_eq!(
            d.push(&rtp, &t),
            vec![AudioUnit { rtp_timestamp: 1000, data: vec![1, 2, 3] }, AudioUnit { rtp_timestamp: 2024, data: vec![4, 5] }]
        );
        // 他の PT は無視
        assert!(d.push(&build_rtp(2, 0, true, 96, &[0x65]), &t).is_empty());
    }

    #[test]
    fn reassembles_fragmented_au() {
        let t = aac_track();
        let mut d = AudioDepacketizer::default();
        let au: Vec<u8> = (0..10).collect();
        // 1 枚目: ヘッダは AU 全体のサイズ、中身は前半だけ
        let mut first = packetize_aac(&[&au]);
        first.truncate(4 + 6);
        let mut second = vec![0, 16];
        second.extend_from_slice(&((au.len() as u16) << 3).to_be_bytes());
        second.extend_from_slice(&au[6..]);
        assert!(d.push(&build_rtp(10, 500, false, 97, &first), &t).is_empty());
        assert_eq!(d.push(&build_rtp(11, 500, true, 97, &second), &t), vec![AudioUnit { rtp_timestamp: 500, data: au.clone() }]);

        // 途中が抜けたら組み立て中のものは捨てる
        assert!(d.push(&build_rtp(20, 900, false, 97, &first), &t).is_empty());
        let next = build_rtp(22, 900, true, 97, &packetize_aac(&[&[9]]));
        assert_eq!(d.push(&next, &t), vec![AudioUnit { rtp_timestamp: 900, data: vec![9] }]);
    }

    #[test]
    fn g711_payload_is_passed_through() {
        let t = parse("m=audio 0 RTP/AVP 0\r\na=control:a\r\n").unwrap();
        let mut d = AudioDepacketizer::default();
        assert_eq!(d.push(&build_rtp(1, 160, false, 0, &[0xff; 160]), &t), vec![AudioUnit { rtp_timestamp: 160, data: vec![0xff; 160] }]);
        assert_eq!(config_hex(&[0x12, 0x88]), "1288");
    }
}
