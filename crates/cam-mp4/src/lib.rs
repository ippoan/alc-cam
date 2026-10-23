//! MP4 を再エンコードせずに切ってつなぐ (カメラの約 4 分ごとの録画から、指定した時間帯の 1 本を作る)。
//!
//! 1. [`BoxHeader::parse`] で元ファイルの一番外の箱を辿り `moov` の場所を知る (カメラの録画は moov が末尾)
//! 2. [`parse_moov`] で目次を読み、トラックごとのサンプル (位置・大きさ・時刻・キーフレームか) に開く
//! 3. [`plan_clip`] に「各ファイルの開始時刻 + 目次」と時間帯を渡すと、出力の頭 (ftyp + moov + mdat の
//!    ヘッダ) と、元ファイルのどの区間をどの順で写せばよいか ([`CopyRange`]) が返る
//!
//! 出力は moov を前に置く (再生開始が速い)。映像は時間帯の頭の直前のキーフレームから始める
//! (そこから前は復号できないので)。元ファイルの区間はファイルごとに 1 つの連続した範囲で写し、
//! 目次の位置だけを付け替える (範囲の中に時間帯の外の音声が少し混ざっても、目次から外すので再生されない)。
//!
//! 想定外の形 (断片化 MP4・複数の sample description・編集リスト付き) は Err にする。

mod read;
mod write;

pub use read::{parse_moov, BoxHeader};

/// 読み取り・組み立ての失敗。文言はそのまま利用者に返してよい
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// 元ファイルの先頭からの位置
    pub offset: u64,
    pub size: u32,
    /// 復号時刻 (トラックの timescale)
    pub dts: u64,
    pub duration: u32,
    /// 表示時刻 - 復号時刻 (ctts。B フレームが無ければ 0)
    pub cto: i32,
    pub sync: bool,
    /// 元ファイルでのチャンク番号 (0 始まり)。出力でのチャンクの切れ目に使う
    pub chunk: u32,
}

/// 目次の 1 トラック。箱は作り直さずに写すもの (stsd 等) を生のまま持つ
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub kind: Kind,
    pub timescale: u32,
    pub samples: Vec<Sample>,
    pub(crate) tkhd: Vec<u8>,
    pub(crate) mdhd: Vec<u8>,
    pub(crate) hdlr: Vec<u8>,
    /// vmhd / smhd
    pub(crate) media_header: Vec<u8>,
    pub(crate) dinf: Vec<u8>,
    pub(crate) stsd: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moov {
    pub tracks: Vec<Track>,
}

impl Moov {
    pub fn track(&self, kind: Kind) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == kind)
    }
}

/// つなぐ元の 1 ファイル
pub struct Source<'a> {
    /// ファイルの開始時刻 (UNIX ms)。カメラのファイル名から出す
    pub start_ms: i64,
    pub moov: &'a Moov,
}

/// 元ファイル `source` (plan_clip に渡した順の番号) の `offset` から `len` バイトを写す
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyRange {
    pub source: usize,
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// 出力の頭 (ftyp + moov + mdat のヘッダ)。この直後に ranges を順に写せば 1 本になる
    pub head: Vec<u8>,
    pub ranges: Vec<CopyRange>,
    /// 出力全体の大きさ (head + ranges)
    pub total_len: u64,
    /// 実際に入った時間帯 (UNIX ms)。頭はキーフレームに寄るので、頼んだ from より少し前になりうる
    pub actual_from_ms: i64,
    pub actual_to_ms: i64,
}

/// 時間帯 `[from_ms, to_ms)` を `sources` (開始時刻の昇順) から切り出してつなぐ
pub fn plan_clip(sources: &[Source<'_>], from_ms: i64, to_ms: i64) -> Result<Clip, Error> {
    write::plan(sources, from_ms, to_ms)
}
