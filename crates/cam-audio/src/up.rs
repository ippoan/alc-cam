//! 話す: 見る側の PCMU (8kHz) → カメラが受ける周波数 → ADTS の AAC-LC (1 フレーム 1024 サンプル)。

use filmcraft_aac::{Encoder, EncoderConfig};

use crate::resample::Resampler;
use crate::{g711, PCMU_RATE};

pub struct Uplink {
    enc: Encoder,
    resampler: Resampler,
    pending: Vec<i16>,
}

impl Uplink {
    /// `rate` はカメラが受ける周波数 (8000 / 16000)
    pub fn new(rate: u32) -> Result<Self, String> {
        // 仕様の上限は 64kbps・1 回 330 バイト程度。1 フレームが 300 バイトを超えない程度に抑える
        let bitrate = if rate <= 8000 { 16_000 } else { 24_000 };
        // TNS は声には要らないので省く (カメラ側のデコーダの対応も気にしなくて済む)
        let cfg = EncoderConfig { adts: true, tns: false, ..EncoderConfig::cbr(rate, 1, bitrate) };
        let enc = Encoder::new(cfg).map_err(|e| format!("AAC のエンコーダを作れません ({e})"))?;
        Ok(Self { enc, resampler: Resampler::new(PCMU_RATE, rate), pending: Vec::new() })
    }

    /// PCMU のペイロード 1 つ → 出来上がった ADTS フレーム (0 個以上)
    pub fn push_pcmu(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        let pcm: Vec<i16> = payload.iter().map(|&b| g711::ulaw_decode(b)).collect();
        self.resampler.push(&pcm, &mut self.pending);
        let planar: Vec<f32> = self.pending.drain(..).map(|s| f32::from(s) / 32768.0).collect();
        self.enc.encode(&[&planar])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resample::{amplitude, tone};
    use filmcraft_aac::{AdtsHeader, Decoder};

    /// 話す声の代わり: 8kHz の 600Hz を PCMU 20ms ずつ
    fn voice() -> Vec<Vec<u8>> {
        tone(8000, 600.0, 8000.0, 16000).chunks(160).map(|c| c.iter().map(|&s| g711::ulaw_encode(s)).collect()).collect()
    }

    fn encode(rate: u32) -> Vec<Vec<u8>> {
        let mut up = Uplink::new(rate).unwrap();
        voice().iter().flat_map(|p| up.push_pcmu(p)).collect()
    }

    /// oxideav 以外の実装 (symphonia) でも読めるか
    fn decode_with_symphonia(frames: &[Vec<u8>], rate: u32) -> Vec<i16> {
        use symphonia_codec_aac::AacDecoder;
        use symphonia_core::audio::layouts::CHANNEL_LAYOUT_MONO;
        use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
        use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
        use symphonia_core::packet::Packet;
        use symphonia_core::units::{Duration, Timestamp};

        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC).with_sample_rate(rate).with_channels(CHANNEL_LAYOUT_MONO);
        let mut dec = AacDecoder::try_new(&params, &AudioDecoderOptions::default()).unwrap();
        let mut out = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            let pkt = Packet::new(0, Timestamp::new(i as i64 * 1024), Duration::new(1024), &f[7..]);
            let buf = dec.decode(&pkt).unwrap();
            let mut s = vec![0i16; buf.frames()];
            buf.copy_to_slice_interleaved(&mut s);
            out.extend(s);
        }
        out
    }

    #[test]
    fn makes_adts_aac_for_both_camera_rates() {
        for rate in [8000, 16000] {
            let frames = encode(rate);
            // 2 秒ぶん (1 フレーム = 1024 サンプル)
            assert!(frames.len() as u32 >= 2 * rate / 1024 - 2, "{rate}: {}", frames.len());
            let mut dec = None;
            let mut pcm = Vec::new();
            for f in &frames {
                let h = AdtsHeader::parse(f).unwrap();
                assert_eq!(h.frame_length, f.len());
                let dec = dec.get_or_insert_with(|| Decoder::from_adts(&h).unwrap());
                assert_eq!((dec.sample_rate(), dec.channels()), (rate, 1));
                pcm.extend(dec.decode(&f[h.header_length..]).unwrap()[0].iter().map(|&v| (v * 32767.0) as i16));
            }
            let body = &pcm[pcm.len() / 4..pcm.len() * 3 / 4];
            let a = amplitude(body, rate, 600.0);
            assert!((a - 8000.0).abs() < 1500.0, "{rate}: {a}");

            let pcm = decode_with_symphonia(&frames, rate);
            let body = &pcm[pcm.len() / 4..pcm.len() * 3 / 4];
            let a = amplitude(body, rate, 600.0);
            assert!((a - 8000.0).abs() < 1500.0, "symphonia {rate}: {a}");
        }
        assert!(Uplink::new(12345).is_err());
        // 1 回に送る大きさの目安 (仕様: 330 バイト程度) を平均で超えない
        let frames = encode(16000);
        assert!(frames.iter().map(Vec::len).sum::<usize>() / frames.len() <= 330);
    }
}
