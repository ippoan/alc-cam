//! 拠点カメラ (I-O DATA TS-NS 系) の音声を WebRTC とつなぐ部品。I/O を持たない。
//!
//! - 聞く ([`Downlink`]): RTSP の音声 (AAC は 1 AU ずつ、G.711 はそのまま) → 8kHz モノラル → G.711 μ-law 20ms (160 バイト)。
//!   WebRTC 側は PCMU だけを出す (ブラウザは必ず PCMU を持つ。Opus のエンコーダを持ち込まないため)
//! - 話す ([`Uplink`]): 見る側の PCMU → 8kHz / 16kHz → ADTS の AAC-LC。カメラの `audio/transmit.cgi`
//!   (`transmit`) が受ける形式は応答の Content-Type で決まる (公開仕様では TS-NS310W は AAC のみ)
//! - 順番 ([`Floor`]): 同時に話せるのは 1 人。後から押した人が優先、押している間だけ (送り直しが途切れたら離したとみなす)

mod down;
mod floor;
pub mod g711;
mod resample;
pub mod transmit;
mod up;

pub use down::Downlink;
pub use floor::{Change, Floor, TALK_HOLD};
pub use resample::Resampler;
pub use up::Uplink;

/// WebRTC 側の音声 (PCMU) の標本化周波数と、1 パケットのサンプル数 (20ms)
pub const PCMU_RATE: u32 = 8000;
pub const PCMU_FRAME: usize = 160;
