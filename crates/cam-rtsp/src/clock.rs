//! RTP の時刻 (32bit・90kHz 等) を、巻き戻りのない µs に直す。

/// RTP の 32bit 時刻を巻き戻りなしの µs に直す (最初のフレームを 0 とする)
#[derive(Debug, Default)]
pub struct Clock {
    first: Option<u32>,
    last: u32,
    wraps: i64,
}

impl Clock {
    pub fn micros(&mut self, ts: u32, clock_rate: u32) -> f64 {
        let first = *self.first.get_or_insert(ts);
        if ts < self.last && self.last - ts > u32::MAX / 2 {
            self.wraps += 1;
        }
        self.last = ts;
        let ext = self.wraps * (1i64 << 32) + ts as i64 - first as i64;
        ext as f64 * 1e6 / clock_rate as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_unwraps() {
        let mut c = Clock::default();
        assert_eq!(c.micros(u32::MAX - 89_999, 90_000), 0.0);
        assert_eq!(c.micros(0, 90_000), 1e6 / 90_000.0 * 90_000.0);
    }
}
