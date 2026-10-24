//! G.711 (ITU-T) の μ-law / A-law と 16bit PCM の相互変換。

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

pub fn ulaw_encode(sample: i16) -> u8 {
    let mut s = i32::from(sample);
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(CLIP) + BIAS;
    let exponent = (7 - (s.leading_zeros() as i32 - 17)).clamp(0, 7);
    let mantissa = (s >> (exponent + 3)) & 0x0f;
    !(sign | (exponent << 4) as u8 | mantissa as u8)
}

pub fn ulaw_decode(byte: u8) -> i16 {
    let b = !byte;
    let exponent = i32::from((b >> 4) & 0x07);
    let mantissa = i32::from(b & 0x0f);
    let s = (((mantissa << 3) + BIAS) << exponent) - BIAS;
    (if b & 0x80 != 0 { -s } else { s }) as i16
}

pub fn alaw_decode(byte: u8) -> i16 {
    let a = byte ^ 0x55;
    let exponent = i32::from((a >> 4) & 0x07);
    let mantissa = i32::from(a & 0x0f);
    let s = match exponent {
        0 => (mantissa << 4) + 8,
        e => ((mantissa << 4) + 0x108) << (e - 1),
    };
    (if a & 0x80 != 0 { s } else { -s }) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulaw_known_values() {
        assert_eq!(ulaw_encode(0), 0xff);
        assert_eq!(ulaw_encode(-1), 0x7f);
        assert_eq!(ulaw_encode(i16::MAX), 0x80);
        assert_eq!(ulaw_encode(i16::MIN), 0x00);
        assert_eq!(ulaw_decode(0xff), 0);
        assert_eq!(ulaw_decode(0x80), 32124);
        assert_eq!(ulaw_decode(0x00), -32124);
    }

    #[test]
    fn ulaw_roundtrip_is_close() {
        for s in (-32000i32..32000).step_by(37) {
            let back = i32::from(ulaw_decode(ulaw_encode(s as i16)));
            // 対数量子化: 誤差は振幅の 1/16 程度 + 小さい値の刻み
            assert!((back - s).abs() <= s.abs() / 16 + 8, "{s} -> {back}");
        }
        // 全 256 値は encode(decode(b)) で元に戻る (0x7f の -0 だけは +0 の 0xff になる)
        for b in 0..=255u8 {
            let e = ulaw_encode(ulaw_decode(b));
            assert!(e == b || (b == 0x7f && e == 0xff), "{b:#x} -> {e:#x}");
        }
    }

    #[test]
    fn alaw_known_values() {
        assert_eq!(alaw_decode(0xd5), 8);
        assert_eq!(alaw_decode(0x55), -8);
        assert_eq!(alaw_decode(0xaa), 32256);
        assert_eq!(alaw_decode(0x2a), -32256);
    }
}
