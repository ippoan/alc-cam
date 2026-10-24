//! 通話の順番。カメラのスピーカーは 1 つなので、同時に話せるのは 1 人。
//!
//! - 押した (`start`) 人が話す。後から押した人が優先 (前の人は外れる)
//! - 押している間、見る側は `start` を送り直す。[`TALK_HOLD`] 送り直しが無ければ離したとみなす
//! - 外された人・カメラ側で話せなかった人 (`refuse`) は、押し続けている間 (送り直し) は取り返さない。
//!   離して (`stop`、または送り直しが途切れて) から押し直すと、また話せる
//! - 離した (`stop`) のは今話している人のときだけ効く

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const TALK_HOLD: Duration = Duration::from_millis(1500);

/// 話す人が変わった
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Debug, Default)]
pub struct Floor {
    talker: Option<(String, Instant)>,
    /// 押したままで待たされている人 (外された・断られた) と、最後に送り直しが来た時刻
    waiting: BTreeMap<String, Instant>,
}

impl Floor {
    pub fn talker(&self) -> Option<&str> {
        self.talker.as_ref().map(|(id, _)| id.as_str())
    }

    pub fn start(&mut self, by: &str, now: Instant) -> Option<Change> {
        if let Some((id, at)) = &mut self.talker {
            if id == by {
                *at = now; // 送り直し
                return None;
            }
        }
        if let Some(last) = self.waiting.get_mut(by) {
            if now.duration_since(*last) < TALK_HOLD {
                *last = now; // まだ押したまま
                return None;
            }
        }
        self.waiting.remove(by);
        let from = self.talker.take().map(|(id, at)| {
            self.waiting.insert(id.clone(), at);
            id
        });
        self.talker = Some((by.to_string(), now));
        Some(Change { from, to: Some(by.to_string()) })
    }

    pub fn stop(&mut self, by: &str) -> Option<Change> {
        self.waiting.remove(by);
        self.release_if(by)
    }

    /// 送り直しが途切れた人を外す
    pub fn poll(&mut self, now: Instant) -> Option<Change> {
        self.waiting.retain(|_, last| now.duration_since(*last) < TALK_HOLD);
        let (id, at) = self.talker.as_ref()?;
        if now.duration_since(*at) < TALK_HOLD {
            return None;
        }
        let id = id.clone();
        self.release_if(&id)
    }

    /// 話せなかった (カメラが他の人と通話中など)。外して、押し直すまで受けない
    pub fn refuse(&mut self, by: &str, now: Instant) -> Option<Change> {
        self.waiting.insert(by.to_string(), now);
        self.release_if(by)
    }

    /// 見る人が居なくなった
    pub fn forget(&mut self, by: &str) -> Option<Change> {
        self.waiting.remove(by);
        self.release_if(by)
    }

    fn release_if(&mut self, by: &str) -> Option<Change> {
        if self.talker() != Some(by) {
            return None;
        }
        self.talker = None;
        Some(Change { from: Some(by.to_string()), to: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(from: Option<&str>, to: Option<&str>) -> Option<Change> {
        Some(Change { from: from.map(Into::into), to: to.map(Into::into) })
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn later_press_wins_and_only_talker_can_release() {
        let t = Instant::now();
        let mut f = Floor::default();
        assert_eq!(f.start("a", t), ch(None, Some("a")));
        assert_eq!(f.start("a", ms(t, 500)), None); // 送り直し
        assert_eq!(f.start("b", ms(t, 600)), ch(Some("a"), Some("b")));
        // 外された a が押したまま送り直しても取り返さない
        assert_eq!(f.start("a", ms(t, 1000)), None);
        assert_eq!(f.start("b", ms(t, 1100)), None);
        assert_eq!(f.start("a", ms(t, 1500)), None);
        assert_eq!(f.talker(), Some("b"));
        assert_eq!(f.stop("a"), None); // 外された人の離しは効かない
        assert_eq!(f.talker(), Some("b"));
        // a が離して押し直すと a
        assert_eq!(f.start("a", ms(t, 1600)), ch(Some("b"), Some("a")));
        assert_eq!(f.stop("a"), ch(Some("a"), None));
        assert_eq!(f.talker(), None);
    }

    #[test]
    fn released_when_resend_stops() {
        let t = Instant::now();
        let mut f = Floor::default();
        f.start("a", t);
        f.start("a", ms(t, 1000));
        assert_eq!(f.poll(ms(t, 2000)), None);
        assert_eq!(f.poll(ms(t, 2600)), ch(Some("a"), None));
        assert_eq!(f.poll(ms(t, 9000)), None);
    }

    #[test]
    fn waiting_expires_without_stop() {
        let t = Instant::now();
        let mut f = Floor::default();
        f.start("a", t);
        f.start("b", ms(t, 100));
        // a の送り直しが途切れた (stop は届かなかった) → 次に押したら取り返せる
        f.poll(ms(t, 2000));
        f.start("b", ms(t, 2000));
        assert_eq!(f.start("a", ms(t, 2100)), ch(Some("b"), Some("a")));
    }

    #[test]
    fn refused_until_pressed_again_and_forget() {
        let t = Instant::now();
        let mut f = Floor::default();
        f.start("a", t);
        assert_eq!(f.refuse("a", t), ch(Some("a"), None));
        assert_eq!(f.start("a", ms(t, 500)), None); // 押しっぱなしの送り直しは受けない
        assert_eq!(f.stop("a"), None);
        assert_eq!(f.start("a", ms(t, 600)), ch(None, Some("a")));
        assert_eq!(f.forget("b"), None);
        assert_eq!(f.forget("a"), ch(Some("a"), None));
    }
}
