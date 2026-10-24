//! 標本化周波数の変換 (窓付き sinc)。音声通話の帯域 (〜4kHz) が保てればよいので素朴に計算する。

use std::f64::consts::PI;

/// 片側のタップ数 (出力側の周波数で数えた零点の数)
const ZEROS: f64 = 8.0;

pub struct Resampler {
    from: u32,
    to: u32,
    /// 低域通過の遮断 (入力の Nyquist に対する比)
    cutoff: f64,
    /// 片側の幅 (入力サンプル数)
    half: usize,
    /// 入力の保持分。`buf[0]` は入力の通し番号 `base`
    buf: Vec<f32>,
    base: i64,
    /// 次の出力の位置 (入力の通し番号で)
    next: f64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let cutoff = (f64::from(to) / f64::from(from)).min(1.0) * 0.92;
        let half = (ZEROS / cutoff).ceil() as usize;
        // 先頭は無音から始まったものとして扱う (遅れは half サンプルぶん)
        Self { from, to, cutoff, half, buf: vec![0.0; half], base: -(half as i64), next: 0.0 }
    }

    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if self.from == self.to {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend(input.iter().map(|&s| f32::from(s)));
        let step = f64::from(self.from) / f64::from(self.to);
        let end = self.base + self.buf.len() as i64;
        while self.next.floor() as i64 + (self.half as i64) < end {
            out.push(self.sample(self.next));
            self.next += step;
        }
        // もう使わない古い入力を捨てる
        let keep_from = self.next.floor() as i64 - self.half as i64;
        let drop = (keep_from - self.base).clamp(0, self.buf.len() as i64) as usize;
        self.buf.drain(..drop);
        self.base += drop as i64;
    }

    fn sample(&self, t: f64) -> i16 {
        let center = t.floor() as i64;
        let mut acc = 0.0;
        for k in center - self.half as i64 + 1..=center + self.half as i64 {
            let Some(&x) = usize::try_from(k - self.base).ok().and_then(|i| self.buf.get(i)) else { continue };
            let d = t - k as f64;
            let w = d / self.half as f64;
            if w.abs() >= 1.0 {
                continue;
            }
            let window = 0.5 + 0.5 * (PI * w).cos();
            let x_pi = PI * d * self.cutoff;
            let sinc = if x_pi.abs() < 1e-9 { 1.0 } else { x_pi.sin() / x_pi };
            acc += f64::from(x) * self.cutoff * sinc * window;
        }
        acc.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
    }
}

#[cfg(test)]
pub(crate) fn tone(rate: u32, freq: f64, amp: f64, n: usize) -> Vec<i16> {
    (0..n).map(|i| (amp * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin()) as i16).collect()
}

/// 周波数 `freq` の成分の振幅 (Goertzel)
#[cfg(test)]
pub(crate) fn amplitude(x: &[i16], rate: u32, freq: f64) -> f64 {
    let w = 2.0 * PI * freq / f64::from(rate);
    let (mut re, mut im) = (0.0, 0.0);
    for (i, &s) in x.iter().enumerate() {
        re += f64::from(s) * (w * i as f64).cos();
        im += f64::from(s) * (w * i as f64).sin();
    }
    2.0 * (re * re + im * im).sqrt() / x.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(from: u32, to: u32, input: &[i16], chunk: usize) -> Vec<i16> {
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            r.push(c, &mut out);
        }
        out
    }

    #[test]
    fn keeps_voice_band_and_rate() {
        // 32kHz → 8kHz: 1kHz は残り、6kHz (8kHz の Nyquist の外) は落ちる
        let input: Vec<i16> = tone(32000, 1000.0, 8000.0, 32000).iter().zip(tone(32000, 6000.0, 8000.0, 32000)).map(|(a, b)| a / 2 + b / 2).collect();
        let out = run(32000, 8000, &input, 1024);
        assert!((out.len() as i64 - 8000).abs() < 40, "{}", out.len());
        let body = &out[400..out.len() - 400];
        assert!((amplitude(body, 8000, 1000.0) - 4000.0).abs() < 300.0, "{}", amplitude(body, 8000, 1000.0));
        // 6kHz は 8kHz では 2kHz に折り返す。それが十分小さいこと
        assert!(amplitude(body, 8000, 2000.0) < 200.0, "{}", amplitude(body, 8000, 2000.0));
    }

    #[test]
    fn upsamples_and_passes_through() {
        let input = tone(8000, 500.0, 10000.0, 8000);
        let out = run(8000, 16000, &input, 160);
        assert!((out.len() as i64 - 16000).abs() < 40);
        assert!((amplitude(&out[800..15000], 16000, 500.0) - 10000.0).abs() < 600.0);
        assert_eq!(run(8000, 8000, &input, 160), input);
    }
}
