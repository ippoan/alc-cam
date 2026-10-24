//! 聞く: カメラの音声 (RTSP) → 8kHz モノラル → G.711 μ-law 20ms ごと。

use cam_rtsp::{AudioCodec, AudioTrack};
use filmcraft_aac::Decoder;

use crate::resample::Resampler;
use crate::{g711, PCMU_FRAME, PCMU_RATE};

enum Source {
    Aac(Box<Decoder>),
    Pcmu,
    Pcma,
}

pub struct Downlink {
    source: Source,
    resampler: Resampler,
    /// 8kHz にした PCM の端数 (20ms に満たない分)
    pending: Vec<i16>,
}

impl Downlink {
    pub fn new(track: &AudioTrack) -> Result<Self, String> {
        let (source, rate) = match &track.codec {
            AudioCodec::Aac { config, .. } => {
                let dec = Decoder::new(config).map_err(|e| format!("カメラの AAC を扱えません ({e})"))?;
                if dec.sbr() || dec.channels() == 0 {
                    return Err("カメラの AAC (HE-AAC など) を扱えません".into());
                }
                let rate = dec.sample_rate();
                (Source::Aac(Box::new(dec)), rate)
            }
            AudioCodec::Pcmu => (Source::Pcmu, track.clock_rate),
            AudioCodec::Pcma => (Source::Pcma, track.clock_rate),
        };
        Ok(Self { source, resampler: Resampler::new(rate, PCMU_RATE), pending: Vec::new() })
    }

    /// 音声のまとまり 1 つ → 送れるだけの μ-law 20ms (各 160 バイト)。壊れた AU は捨てる
    pub fn push(&mut self, unit: &[u8]) -> Vec<Vec<u8>> {
        let mono: Vec<i16> = match &mut self.source {
            Source::Pcmu => unit.iter().map(|&b| g711::ulaw_decode(b)).collect(),
            Source::Pcma => unit.iter().map(|&b| g711::alaw_decode(b)).collect(),
            Source::Aac(dec) => {
                let Ok(planar) = dec.decode(unit) else { return Vec::new() };
                let Some(len) = planar.iter().map(Vec::len).min() else { return Vec::new() };
                let n = planar.len() as f32;
                (0..len).map(|i| to_i16(planar.iter().map(|c| c[i]).sum::<f32>() / n)).collect()
            }
        };
        self.resampler.push(&mono, &mut self.pending);
        let whole = self.pending.len() / PCMU_FRAME * PCMU_FRAME;
        let out = self.pending[..whole].chunks(PCMU_FRAME).map(|c| c.iter().map(|&s| g711::ulaw_encode(s)).collect()).collect();
        self.pending.drain(..whole);
        out
    }
}

fn to_i16(v: f32) -> i16 {
    (v * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resample::{amplitude, tone};
    use filmcraft_aac::{Encoder, EncoderConfig};

    fn track(codec: AudioCodec, rate: u32) -> AudioTrack {
        AudioTrack { control: String::new(), payload_type: 97, clock_rate: rate, channels: 1, codec }
    }

    fn decode(frames: &[Vec<u8>]) -> Vec<i16> {
        frames.iter().flatten().map(|&b| g711::ulaw_decode(b)).collect()
    }

    #[test]
    fn aac_32k_becomes_pcmu_8k() {
        // カメラの代わり: 32kHz モノラル 1kHz を AAC にし、ADTS を剥がして AU だけ渡す
        let mut enc = Encoder::new(EncoderConfig::cbr(32000, 1, 64000)).unwrap();
        assert_eq!(enc.audio_specific_config(), [0x12, 0x88]);
        let pcm: Vec<f32> = tone(32000, 1000.0, 6000.0, 32000).iter().map(|&s| f32::from(s) / 32767.0).collect();
        let mut d = Downlink::new(&track(AudioCodec::Aac { config: vec![0x12, 0x88], size_length: 13, index_length: 3, index_delta_length: 3 }, 32000)).unwrap();
        let mut frames = Vec::new();
        for hop in pcm.chunks(1024) {
            for au in enc.encode(&[hop]) {
                frames.extend(d.push(&au));
            }
        }
        assert!(frames.iter().all(|f| f.len() == PCMU_FRAME));
        // 1 秒ぶん (エンコーダの先読み・遅れ 2 AU ぶんと標本化変換の遅れを除く)
        assert!(frames.len() >= 40, "{}", frames.len());
        let pcm8 = decode(&frames);
        let body = &pcm8[2000..pcm8.len() - 200];
        let a = amplitude(body, 8000, 1000.0);
        assert!((a - 6000.0).abs() < 900.0, "{a}");
        // 壊れた AU は捨てる
        assert!(d.push(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn g711_passes_in_20ms_frames() {
        let mut d = Downlink::new(&track(AudioCodec::Pcma, 8000)).unwrap();
        assert!(d.push(&[0xd5; 100]).is_empty());
        let out = d.push(&[0xd5; 100]);
        assert_eq!(out.len(), 1);
        assert_eq!(decode(&out), vec![8; 160]);
        let mut d = Downlink::new(&track(AudioCodec::Pcmu, 8000)).unwrap();
        assert_eq!(d.push(&[0x80; 320]), vec![vec![0x80; 160]; 2]);
        // 読めない AudioSpecificConfig
        assert!(Downlink::new(&track(AudioCodec::Aac { config: vec![0xff, 0xff], size_length: 13, index_length: 3, index_delta_length: 3 }, 32000)).is_err());
    }
}
