//! カメラ (Tapo C212 等) の RTSP を TCP interleaved で受け、H.264 を Annex B のアクセスユニットにして返す。
//! 音声のトラック (AAC / G.711) があれば一緒に SETUP し、音声のまとまりも返す (`audio`)。
//!
//! I/O を持たない: [`Client::feed`] に socket から読んだバイトを渡すとイベントが返り、
//! [`Client::take_output`] で socket へ書くバイトを取り出す。手順は alc-gw-p4 の
//! `components/rtsp_client` (C 実装) と同じ: DESCRIBE → SETUP (interleaved=0-1) → PLAY。

pub mod audio;
mod client;
pub mod digest;
pub mod h264;
mod response;
mod sdp;
mod url;
mod clock;

pub use audio::{AudioCodec, AudioTrack};
pub use client::{Client, Event};
pub use url::{parse as parse_url, target, RtspUrl};

/// 受信を続けられない失敗。文言に資格情報・宛先は含めない (ログにそのまま出してよい)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) fn err<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}
