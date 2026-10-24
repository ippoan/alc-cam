// e2e の偽カメラ (test/fake-rtsp.mjs) が RTSP の音声として流す AAC の AU を作る。
// TS-NS 系と同じ AAC-LC 32kHz モノラル (AudioSpecificConfig = 12 88)。中身は 440Hz と 880Hz を 0.5 秒ずつ、2 秒。
//   cargo run -p cam-audio --example aac-fixture -- test/fixture.aacbin
// 形式: [AU の長さ u16 big endian][AU] の繰り返し
use filmcraft_aac::{Encoder, EncoderConfig};

fn main() {
    let path = std::env::args().nth(1).expect("出力先");
    let mut enc = Encoder::new(EncoderConfig::cbr(32000, 1, 32000)).unwrap();
    assert_eq!(enc.audio_specific_config(), [0x12, 0x88]);
    let pcm: Vec<f32> = (0..64_000)
        .map(|i| {
            let f = if (i / 16_000) % 2 == 0 { 440.0 } else { 880.0 };
            0.3 * (2.0 * std::f32::consts::PI * f * i as f32 / 32000.0).sin()
        })
        .collect();
    let mut aus = enc.encode(&[&pcm]);
    aus.extend(enc.flush());
    let mut out = Vec::new();
    for au in &aus {
        out.extend_from_slice(&(au.len() as u16).to_be_bytes());
        out.extend_from_slice(au);
    }
    std::fs::write(&path, out).unwrap();
    println!("{} AU を {path} へ", aus.len());
}
