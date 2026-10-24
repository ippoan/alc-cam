//! 1 台のカメラを複数の見る人へ配る束ね役。RTSP は 1 本で、届いたフレームを viewer ごとに複製する。
//!
//! - viewer は cam-room の DO が付けた `viewer_id` で引く (上限 [`MAX_VIEWERS`])
//! - 直近の GOP (キーフレームから) を 1 つ持ち、後から繋がった viewer にはそれを先に流す。
//!   キーフレームの AU は先頭に SPS/PPS を必ず含む (`cam_rtsp` の h264) ので、途中参加でもデコーダが組める
//! - viewer ごとに「キーフレームを渡したか」を持ち、渡す前の差分フレームは捨てる (次の IDR から流す)
//! - 1 人の送信の失敗はその人だけ外す。他の人とカメラには響かない
//! - 音声 (PCMU 20ms) は映像を流し始めた人へそのまま配る。送れなくても外さない (映像は続ける)
//!
//! I/O を持たない (WebRTC は [`Sink`] の向こう)。カメラを開く/畳むのは呼び手が `is_empty` を見て決める。

use std::collections::BTreeMap;
use std::sync::Arc;

/// 1 部屋の見る人の上限 (cam-room の DO と同じ)。上り帯域 = 人数 × stream2 のため
pub const MAX_VIEWERS: usize = 4;
/// 溜めておく直近の GOP の上限 (フレーム数)。超えたら捨てて次のキーフレームを待つ
const MAX_GOP: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub key: bool,
    /// 90kHz
    pub ts: u64,
    pub data: Arc<[u8]>,
}

/// 見る人 1 人ぶんの送り先 (本物は `peer::Viewer`)
pub trait Sink {
    /// ICE/DTLS が繋がって映像を書ける
    fn is_connected(&self) -> bool;
    fn write(&mut self, f: &Frame) -> Result<(), String>;
    /// カメラの音 (PCMU 20ms ぶん)
    fn write_audio(&mut self, pcmu: &[u8]) -> Result<(), String>;
}

struct Slot<V> {
    v: V,
    /// キーフレームを渡した (= 以後の差分フレームを渡してよい)
    started: bool,
    /// 繋がったのを見て GOP を流し終えた
    flushed: bool,
}

pub struct Hub<V> {
    viewers: BTreeMap<String, Slot<V>>,
    gop: Vec<Frame>,
}

#[derive(Debug, PartialEq)]
pub enum InsertError {
    Full,
}

impl<V: Sink> Default for Hub<V> {
    fn default() -> Self {
        Self { viewers: BTreeMap::new(), gop: Vec::new() }
    }
}

impl<V: Sink> Hub<V> {
    pub fn len(&self) -> usize {
        self.viewers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.viewers.is_empty()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.viewers.contains_key(id)
    }

    /// 同じ id が居れば入れ替える (入り直し)。上限を超える新顔は断る
    pub fn insert(&mut self, id: &str, v: V) -> Result<(), InsertError> {
        if !self.viewers.contains_key(id) && self.viewers.len() >= MAX_VIEWERS {
            return Err(InsertError::Full);
        }
        self.viewers.insert(id.to_string(), Slot { v, started: false, flushed: false });
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<V> {
        let v = self.viewers.remove(id).map(|s| s.v);
        if self.viewers.is_empty() {
            // 次にカメラを開き直したときに古い GOP を流さない
            self.gop.clear();
        }
        v
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut V> {
        self.viewers.get_mut(id).map(|s| &mut s.v)
    }

    pub fn ids(&self) -> Vec<String> {
        self.viewers.keys().cloned().collect()
    }

    /// カメラからの 1 フレーム。GOP に足し、繋がっている全員へ配る。送れなかった人は外して返す
    pub fn on_frame(&mut self, f: Frame) -> Vec<(String, V, String)> {
        if f.key {
            self.gop.clear();
        }
        if !self.gop.is_empty() || f.key {
            if self.gop.len() >= MAX_GOP {
                self.gop.clear();
            } else {
                self.gop.push(f.clone());
            }
        }
        let mut failed = Vec::new();
        for (id, slot) in &mut self.viewers {
            if !slot.flushed {
                continue; // 繋がった時に GOP ごと受け取る
            }
            if let Err(e) = deliver(slot, &f) {
                failed.push((id.clone(), e));
            }
        }
        self.take_failed(failed)
    }

    /// カメラの音 (PCMU 20ms) を、映像を流し始めた全員へ。送れない人は飛ばす (音が出ないだけ)
    pub fn on_audio(&mut self, pcmu: &[u8]) {
        for slot in self.viewers.values_mut().filter(|s| s.flushed) {
            let _ = slot.v.write_audio(pcmu);
        }
    }

    /// viewer の状態が進んだあとに呼ぶ: 繋がったばかりの人へ GOP を流す。送れなかった人は外して返す
    pub fn flush_connected(&mut self) -> Vec<(String, V, String)> {
        let mut failed = Vec::new();
        for (id, slot) in &mut self.viewers {
            if slot.flushed || !slot.v.is_connected() {
                continue;
            }
            slot.flushed = true;
            slot.started = false;
            for f in &self.gop {
                if let Err(e) = deliver(slot, f) {
                    failed.push((id.clone(), e));
                    break;
                }
            }
        }
        self.take_failed(failed)
    }

    fn take_failed(&mut self, failed: Vec<(String, String)>) -> Vec<(String, V, String)> {
        failed
            .into_iter()
            .filter_map(|(id, e)| self.remove(&id).map(|v| (id, v, e)))
            .collect()
    }
}

fn deliver<V: Sink>(slot: &mut Slot<V>, f: &Frame) -> Result<(), String> {
    if !slot.started && !f.key {
        return Ok(());
    }
    slot.v.write(f)?;
    slot.started = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        connected: bool,
        got: Vec<u64>,
        audio: usize,
        fail: bool,
    }

    impl Sink for Fake {
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn write(&mut self, f: &Frame) -> Result<(), String> {
            if self.fail {
                return Err("boom".into());
            }
            self.got.push(f.ts);
            Ok(())
        }
        fn write_audio(&mut self, _: &[u8]) -> Result<(), String> {
            if self.fail {
                return Err("boom".into());
            }
            self.audio += 1;
            Ok(())
        }
    }

    fn frame(ts: u64, key: bool) -> Frame {
        Frame { key, ts, data: Arc::from(&[0u8][..]) }
    }

    fn connect(hub: &mut Hub<Fake>, id: &str) {
        hub.get_mut(id).unwrap().connected = true;
        assert!(hub.flush_connected().is_empty());
    }

    #[test]
    fn two_viewers_get_the_same_frames() {
        let mut hub = Hub::default();
        hub.insert("a", Fake::default()).unwrap();
        hub.insert("b", Fake::default()).unwrap();
        connect(&mut hub, "a");
        connect(&mut hub, "b");
        for (ts, key) in [(1, true), (2, false), (3, false)] {
            assert!(hub.on_frame(frame(ts, key)).is_empty());
        }
        assert_eq!(hub.get_mut("a").unwrap().got, [1, 2, 3]);
        assert_eq!(hub.get_mut("b").unwrap().got, [1, 2, 3]);
    }

    #[test]
    fn late_joiner_gets_gop_from_last_key_first() {
        let mut hub = Hub::default();
        hub.insert("a", Fake::default()).unwrap();
        connect(&mut hub, "a");
        for (ts, key) in [(1, true), (2, false), (3, true), (4, false)] {
            hub.on_frame(frame(ts, key));
        }
        hub.insert("b", Fake::default()).unwrap();
        // 繋がるまでは何も渡さない
        hub.on_frame(frame(5, false));
        assert!(hub.get_mut("b").unwrap().got.is_empty());
        connect(&mut hub, "b");
        hub.on_frame(frame(6, false));
        assert_eq!(hub.get_mut("b").unwrap().got, [3, 4, 5, 6]);
        assert_eq!(hub.get_mut("a").unwrap().got, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn joiner_without_gop_waits_for_next_key() {
        let mut hub = Hub::default();
        hub.insert("a", Fake::default()).unwrap();
        connect(&mut hub, "a");
        hub.on_frame(frame(1, false)); // まだキーフレームが来ていない
        hub.on_frame(frame(2, true));
        hub.on_frame(frame(3, false));
        assert_eq!(hub.get_mut("a").unwrap().got, [2, 3]);
    }

    #[test]
    fn one_viewer_failing_or_leaving_does_not_affect_the_other() {
        let mut hub = Hub::default();
        hub.insert("a", Fake::default()).unwrap();
        hub.insert("b", Fake::default()).unwrap();
        connect(&mut hub, "a");
        connect(&mut hub, "b");
        hub.on_frame(frame(1, true));
        hub.get_mut("b").unwrap().fail = true;
        let failed = hub.on_frame(frame(2, false));
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "b");
        hub.on_frame(frame(3, false));
        assert_eq!(hub.get_mut("a").unwrap().got, [1, 2, 3]);

        hub.insert("c", Fake::default()).unwrap();
        connect(&mut hub, "c");
        assert!(hub.remove("c").is_some());
        hub.on_frame(frame(4, false));
        assert_eq!(hub.get_mut("a").unwrap().got, [1, 2, 3, 4]);
        assert_eq!(hub.len(), 1);
    }

    #[test]
    fn audio_goes_to_connected_viewers_and_failures_do_not_drop() {
        let mut hub = Hub::default();
        hub.insert("a", Fake::default()).unwrap();
        hub.insert("b", Fake::default()).unwrap();
        hub.insert("c", Fake::default()).unwrap();
        connect(&mut hub, "a");
        connect(&mut hub, "b");
        hub.get_mut("b").unwrap().fail = true;
        hub.on_audio(&[0xff; 160]);
        hub.on_audio(&[0xff; 160]);
        assert_eq!(hub.get_mut("a").unwrap().audio, 2);
        assert_eq!(hub.get_mut("c").unwrap().audio, 0); // まだ繋がっていない
        assert_eq!(hub.len(), 3);
    }

    #[test]
    fn limit_and_reinsert() {
        let mut hub = Hub::default();
        for id in ["a", "b", "c", "d"] {
            hub.insert(id, Fake::default()).unwrap();
        }
        assert_eq!(hub.insert("e", Fake::default()), Err(InsertError::Full));
        // 居る人の入り直しは上限に数えない
        hub.insert("a", Fake::default()).unwrap();
        assert_eq!(hub.len(), MAX_VIEWERS);
        assert!(hub.remove("zzz").is_none());
    }

    #[test]
    fn last_leave_empties_and_drops_gop() {
        let mut hub = Hub::default();
        hub.insert("a", Fake::default()).unwrap();
        connect(&mut hub, "a");
        hub.on_frame(frame(1, true));
        hub.remove("a");
        assert!(hub.is_empty());
        hub.insert("b", Fake::default()).unwrap();
        connect(&mut hub, "b");
        assert!(hub.get_mut("b").unwrap().got.is_empty());
    }
}
