//! RTP (RFC 3550) + H.264 payload (RFC 6184) → Annex B のアクセスユニット。
//! 単一 NAL / STAP-A / FU-A に対応。alc-gw-p4 の `h264_depacket.c` と同じ方針で、
//! パケットロスを見たら組み立て中のものを捨て、次の IDR まで待つ (崩れた絵を出さない)。

const START: [u8; 4] = [0, 0, 0, 1];
/// 1 アクセスユニットの上限。超えたら壊れた入力とみなして捨てる
const MAX_AU: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// 開始コード付き NAL の連結。キーフレームには SPS/PPS を必ず先頭に含める
    pub data: Vec<u8>,
    pub key: bool,
    pub rtp_timestamp: u32,
}

#[derive(Debug, Default)]
pub struct Depacketizer {
    au: Vec<u8>,
    au_ts: Option<u32>,
    au_idr: bool,
    au_sps: bool,
    in_fu: bool,
    last_seq: Option<u16>,
    /// 最初とロスの後は IDR まで捨てる
    synced: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl Depacketizer {
    pub fn new(sps: Option<Vec<u8>>, pps: Option<Vec<u8>>) -> Self {
        Self { sps, pps, ..Self::default() }
    }

    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    /// RTP パケット 1 つ (ヘッダ込み) を渡す。完成したアクセスユニットを返す (0〜2 個)。
    pub fn push(&mut self, rtp: &[u8], payload_type: u8) -> Vec<AccessUnit> {
        let mut out = Vec::new();
        let Some(pkt) = Rtp::parse(rtp) else { return out };
        if pkt.payload_type != payload_type {
            return out;
        }

        if let Some(last) = self.last_seq {
            if pkt.seq != last.wrapping_add(1) {
                self.reset_au();
                self.synced = false;
            }
        }
        self.last_seq = Some(pkt.seq);

        // マーカーを取りこぼしても、時刻が変わったら前のものは閉じる
        if self.au_ts.is_some_and(|ts| ts != pkt.timestamp) {
            out.extend(self.flush());
        }
        self.au_ts = Some(pkt.timestamp);

        let p = pkt.payload;
        if p.is_empty() {
            return out;
        }
        match p[0] & 0x1f {
            1..=23 => self.add_nal(p),
            24 => {
                // STAP-A: [16bit size][NAL]...
                let mut rest = &p[1..];
                while rest.len() >= 2 {
                    let n = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    if rest.len() < 2 + n {
                        break;
                    }
                    self.add_nal(&rest[2..2 + n]);
                    rest = &rest[2 + n..];
                }
            }
            28 if p.len() >= 2 => {
                // FU-A
                let (ind, hdr) = (p[0], p[1]);
                let start = hdr & 0x80 != 0;
                let end = hdr & 0x40 != 0;
                if start {
                    let header = (ind & 0xe0) | (hdr & 0x1f);
                    self.begin_nal(header);
                    self.au.extend_from_slice(&p[2..]);
                    self.in_fu = true;
                } else if self.in_fu {
                    self.au.extend_from_slice(&p[2..]);
                }
                // 開始を見ていない断片は捨てる
                if end {
                    self.in_fu = false;
                }
            }
            _ => {} // STAP-B / MTAP / FU-B は使うカメラが稀なので無視
        }

        if self.au.len() > MAX_AU {
            self.reset_au();
            self.synced = false;
        }
        if pkt.marker {
            out.extend(self.flush());
        }
        out
    }

    fn begin_nal(&mut self, header: u8) {
        match header & 0x1f {
            5 => self.au_idr = true,
            7 => self.au_sps = true,
            _ => {}
        }
        self.au.extend_from_slice(&START);
        self.au.push(header);
    }

    fn add_nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        match nal[0] & 0x1f {
            7 => self.sps = Some(nal.to_vec()),
            8 => self.pps = Some(nal.to_vec()),
            _ => {}
        }
        self.begin_nal(nal[0]);
        self.au.extend_from_slice(&nal[1..]);
    }

    fn reset_au(&mut self) {
        self.au.clear();
        self.au_idr = false;
        self.au_sps = false;
        self.in_fu = false;
    }

    fn flush(&mut self) -> Option<AccessUnit> {
        let ts = self.au_ts?;
        if self.au.is_empty() {
            return None;
        }
        let mut data = std::mem::take(&mut self.au);
        let key = self.au_idr;
        let has_sps = self.au_sps;
        self.reset_au();

        if key {
            // SPS/PPS が分からないとデコーダが組めない。分かるまでは出さない
            let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else { return None };
            if !has_sps {
                let mut with = Vec::with_capacity(data.len() + sps.len() + pps.len() + 8);
                for nal in [sps, pps] {
                    with.extend_from_slice(&START);
                    with.extend_from_slice(nal);
                }
                with.extend_from_slice(&data);
                data = with;
            }
            self.synced = true;
        } else if !self.synced {
            return None;
        }
        Some(AccessUnit { data, key, rtp_timestamp: ts })
    }
}

/// WebCodecs の codec 文字列 (`avc1.PPCCLL`) を SPS から作る
pub fn codec_string(sps: &[u8]) -> Option<String> {
    let b = sps.get(1..4)?;
    Some(format!("avc1.{:02x}{:02x}{:02x}", b[0], b[1], b[2]))
}

struct Rtp<'a> {
    marker: bool,
    payload_type: u8,
    seq: u16,
    timestamp: u32,
    payload: &'a [u8],
}

impl<'a> Rtp<'a> {
    fn parse(b: &'a [u8]) -> Option<Self> {
        if b.len() < 12 || b[0] >> 6 != 2 {
            return None;
        }
        let cc = (b[0] & 0x0f) as usize;
        let mut off = 12 + cc * 4;
        if b[0] & 0x10 != 0 {
            let ext = b.get(off + 2..off + 4)?;
            off += 4 + u16::from_be_bytes([ext[0], ext[1]]) as usize * 4;
        }
        let mut end = b.len();
        if b[0] & 0x20 != 0 {
            let pad = *b.last()? as usize;
            end = end.checked_sub(pad)?;
        }
        Some(Self {
            marker: b[1] & 0x80 != 0,
            payload_type: b[1] & 0x7f,
            seq: u16::from_be_bytes([b[2], b[3]]),
            timestamp: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            payload: b.get(off..end)?,
        })
    }
}

/// テスト用の RTP 組み立て (偽カメラも使う)
pub fn build_rtp(seq: u16, ts: u32, marker: bool, payload_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + payload.len());
    v.push(0x80);
    v.push(if marker { 0x80 } else { 0 } | payload_type);
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(&ts.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 1]);
    v.extend_from_slice(payload);
    v
}

/// NAL 1 つを RTP ペイロードに分ける (MTU を超えたら FU-A)。偽カメラとテスト用
pub fn packetize_nal(nal: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    if nal.len() <= mtu {
        return vec![nal.to_vec()];
    }
    let ind = (nal[0] & 0xe0) | 28;
    let typ = nal[0] & 0x1f;
    let chunks: Vec<&[u8]> = nal[1..].chunks(mtu - 2).collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut hdr = typ;
            if i == 0 {
                hdr |= 0x80;
            }
            if i == last {
                hdr |= 0x40;
            }
            let mut v = vec![ind, hdr];
            v.extend_from_slice(c);
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 0x42, 0xc0, 0x1e];
    const PPS: [u8; 2] = [0x68, 0xce];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| START.iter().chain(n.iter()).copied()).collect()
    }

    #[test]
    fn single_nal_idr_gets_sps_pps_prepended() {
        let mut d = Depacketizer::new(Some(SPS.to_vec()), Some(PPS.to_vec()));
        let idr = [0x65, 1, 2, 3];
        let out = d.push(&build_rtp(1, 900, true, 96, &idr), 96);
        assert_eq!(out, vec![AccessUnit { data: annexb(&[&SPS, &PPS, &idr]), key: true, rtp_timestamp: 900 }]);
    }

    #[test]
    fn waits_for_idr_and_for_sps() {
        let mut d = Depacketizer::new(None, None);
        // 差分は同期前なので捨てる
        assert!(d.push(&build_rtp(1, 0, true, 96, &[0x41, 9]), 96).is_empty());
        // SPS/PPS 不明の IDR も出さない
        assert!(d.push(&build_rtp(2, 3000, true, 96, &[0x65, 9]), 96).is_empty());
        // in-band の SPS/PPS + IDR (STAP-A) なら出る。前置きは足さない (既にある)
        let mut stap = vec![24];
        for n in [&SPS[..], &PPS[..], &[0x65, 7][..]] {
            stap.extend_from_slice(&(n.len() as u16).to_be_bytes());
            stap.extend_from_slice(n);
        }
        let out = d.push(&build_rtp(3, 6000, true, 96, &stap), 96);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, annexb(&[&SPS, &PPS, &[0x65, 7]]));
        // 以後の差分は出る
        assert_eq!(d.push(&build_rtp(4, 9000, true, 96, &[0x41, 1]), 96).len(), 1);
    }

    #[test]
    fn fu_a_roundtrip() {
        let mut d = Depacketizer::new(Some(SPS.to_vec()), Some(PPS.to_vec()));
        let idr: Vec<u8> = std::iter::once(0x65).chain((0..3000u32).map(|i| i as u8)).collect();
        let parts = packetize_nal(&idr, 1200);
        assert_eq!(parts.len(), 3);
        let mut out = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            out.extend(d.push(&build_rtp(10 + i as u16, 1, i == parts.len() - 1, 96, p), 96));
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, annexb(&[&SPS, &PPS, &idr]));
    }

    #[test]
    fn loss_discards_until_next_idr() {
        let mut d = Depacketizer::new(Some(SPS.to_vec()), Some(PPS.to_vec()));
        assert_eq!(d.push(&build_rtp(1, 0, true, 96, &[0x65, 1]), 96).len(), 1);
        // seq 2 が抜けた
        assert!(d.push(&build_rtp(3, 3000, true, 96, &[0x41, 1]), 96).is_empty());
        assert!(d.push(&build_rtp(4, 6000, true, 96, &[0x41, 1]), 96).is_empty());
        assert_eq!(d.push(&build_rtp(5, 9000, true, 96, &[0x65, 1]), 96).len(), 1);
    }

    #[test]
    fn timestamp_change_closes_unmarked_au() {
        let mut d = Depacketizer::new(Some(SPS.to_vec()), Some(PPS.to_vec()));
        assert!(d.push(&build_rtp(1, 0, false, 96, &[0x65, 1]), 96).is_empty());
        let out = d.push(&build_rtp(2, 3000, true, 96, &[0x41, 2]), 96);
        assert_eq!(out.iter().map(|a| a.key).collect::<Vec<_>>(), vec![true, false]);
    }

    #[test]
    fn other_payload_types_are_ignored() {
        let mut d = Depacketizer::new(Some(SPS.to_vec()), Some(PPS.to_vec()));
        assert!(d.push(&build_rtp(1, 0, true, 8, &[0x65, 1]), 96).is_empty());
    }

    #[test]
    fn codec_from_sps() {
        assert_eq!(codec_string(&[0x67, 0x4d, 0x00, 0x1e]).as_deref(), Some("avc1.4d001e"));
        assert_eq!(codec_string(&[0x67]), None);
    }
}
