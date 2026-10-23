//! 箱 (box) の読み取りと、moov からサンプル一覧への展開。

use crate::{err, Error, Kind, Moov, Sample, Track};

/// 箱のヘッダ。`size` は箱全体 (ヘッダ込み) の大きさ。0 (= ファイルの終わりまで) は呼び手が解く
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    pub size: u64,
    pub header_len: u8,
}

impl BoxHeader {
    /// 箱の先頭 (8 か 16 バイト) を読む。足りなければ Err。一番外の箱を辿るときは、
    /// 位置 0 から `size` ずつ進めながら各位置の 16 バイトを読ませればよい (moov の場所を知るため)
    pub fn parse(b: &[u8]) -> Result<Self, Error> {
        if b.len() < 8 {
            return err("箱のヘッダが短すぎます");
        }
        let size32 = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let kind = [b[4], b[5], b[6], b[7]];
        if size32 == 1 {
            if b.len() < 16 {
                return err("64bit の箱のヘッダが短すぎます");
            }
            let size = u64::from_be_bytes(b[8..16].try_into().unwrap());
            return Ok(Self { kind, size, header_len: 16 });
        }
        Ok(Self { kind, size: u64::from(size32), header_len: 8 })
    }
}

/// 箱 1 つ: (種類, 箱全体, 中身)
type Child<'a> = ([u8; 4], &'a [u8], &'a [u8]);
/// 箱 1 つ: (箱全体, 中身)
type Found<'a> = Option<(&'a [u8], &'a [u8])>;

/// `buf` の中の箱を並べる
pub(crate) fn children(buf: &[u8]) -> Result<Vec<Child<'_>>, Error> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let h = BoxHeader::parse(&buf[pos..])?;
        let size = if h.size == 0 { (buf.len() - pos) as u64 } else { h.size };
        let end = pos.checked_add(usize::try_from(size).map_err(|_| Error("箱が大きすぎます".into()))?).ok_or(Error("箱の大きさが壊れています".into()))?;
        if size < u64::from(h.header_len) || end > buf.len() {
            return err(format!("箱 {} の大きさが壊れています", String::from_utf8_lossy(&h.kind)));
        }
        out.push((h.kind, &buf[pos..end], &buf[pos + h.header_len as usize..end]));
        pos = end;
    }
    Ok(out)
}

fn child<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Result<Found<'a>, Error> {
    Ok(children(buf)?.into_iter().find(|(k, _, _)| k == kind).map(|(_, whole, body)| (whole, body)))
}

fn need<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Result<(&'a [u8], &'a [u8]), Error> {
    child(buf, kind)?.ok_or_else(|| Error(format!("{} が見つかりません", String::from_utf8_lossy(kind))))
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let s = self.b.get(self.p..self.p + n).ok_or(Error("表が途中で切れています".into()))?;
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// full box の version (flags は読み捨て)
    fn version(&mut self) -> Result<u8, Error> {
        let v = self.u8()?;
        self.take(3)?;
        Ok(v)
    }
}

/// `moov` の箱 (ヘッダ込み) を読む。映像・音声以外のトラックは捨てる
pub fn parse_moov(moov: &[u8]) -> Result<Moov, Error> {
    let h = BoxHeader::parse(moov)?;
    if &h.kind != b"moov" {
        return err("moov ではありません");
    }
    let body = &moov[h.header_len as usize..];
    if child(body, b"mvex")?.is_some() {
        return err("断片化 MP4 には対応していません");
    }
    let mut tracks = Vec::new();
    for (kind, _, trak) in children(body)? {
        if &kind != b"trak" {
            continue;
        }
        if let Some(t) = parse_trak(trak)? {
            tracks.push(t);
        }
    }
    if !tracks.iter().any(|t| t.kind == Kind::Video) {
        return err("映像のトラックがありません");
    }
    Ok(Moov { tracks })
}

fn parse_trak(trak: &[u8]) -> Result<Option<Track>, Error> {
    if child(trak, b"edts")?.is_some() {
        return err("編集リスト (edts) 付きのトラックには対応していません");
    }
    let (tkhd, _) = need(trak, b"tkhd")?;
    let (_, mdia) = need(trak, b"mdia")?;
    let (mdhd, mdhd_body) = need(mdia, b"mdhd")?;
    let (hdlr, hdlr_body) = need(mdia, b"hdlr")?;
    let kind = match hdlr_body.get(8..12) {
        Some(b"vide") => Kind::Video,
        Some(b"soun") => Kind::Audio,
        _ => return Ok(None),
    };
    let mut r = Rd::new(mdhd_body);
    let timescale = if r.version()? == 1 {
        r.take(16)?;
        r.u32()?
    } else {
        r.take(8)?;
        r.u32()?
    };
    if timescale == 0 {
        return err("timescale が 0 です");
    }
    let (_, minf) = need(mdia, b"minf")?;
    let media_header = match kind {
        Kind::Video => need(minf, b"vmhd")?.0,
        Kind::Audio => need(minf, b"smhd")?.0,
    };
    let (dinf, _) = need(minf, b"dinf")?;
    let (_, stbl) = need(minf, b"stbl")?;
    let (stsd, stsd_body) = need(stbl, b"stsd")?;
    let mut r = Rd::new(stsd_body);
    r.version()?;
    if r.u32()? != 1 {
        return err("sample description が 1 つでないトラックには対応していません");
    }
    let samples = build_samples(stbl)?;
    Ok(Some(Track {
        kind,
        timescale,
        samples,
        tkhd: tkhd.to_vec(),
        mdhd: mdhd.to_vec(),
        hdlr: hdlr.to_vec(),
        media_header: media_header.to_vec(),
        dinf: dinf.to_vec(),
        stsd: stsd.to_vec(),
    }))
}

fn build_samples(stbl: &[u8]) -> Result<Vec<Sample>, Error> {
    // 大きさ
    let (_, b) = need(stbl, b"stsz")?;
    let mut r = Rd::new(b);
    r.version()?;
    let fixed = r.u32()?;
    let count = r.u32()? as usize;
    let mut sizes = Vec::with_capacity(count);
    for _ in 0..count {
        sizes.push(if fixed != 0 { fixed } else { r.u32()? });
    }

    // 時刻
    let (_, b) = need(stbl, b"stts")?;
    let mut r = Rd::new(b);
    r.version()?;
    let mut durations = Vec::with_capacity(count);
    for _ in 0..r.u32()? {
        let (n, d) = (r.u32()?, r.u32()?);
        durations.extend(std::iter::repeat_n(d, n as usize));
    }
    if durations.len() != count {
        return err("stts と stsz のサンプル数が合いません");
    }

    let mut ctos = vec![0i32; count];
    if let Some((_, b)) = child(stbl, b"ctts")? {
        let mut r = Rd::new(b);
        let v = r.version()?;
        let mut i = 0usize;
        for _ in 0..r.u32()? {
            let n = r.u32()? as usize;
            let raw = r.u32()?;
            let off = if v == 0 { i32::try_from(raw).unwrap_or(i32::MAX) } else { raw as i32 };
            for c in ctos.iter_mut().skip(i).take(n) {
                *c = off;
            }
            i += n;
        }
    }

    // キーフレーム (stss が無ければ全部キー)
    let mut sync = vec![true; count];
    if let Some((_, b)) = child(stbl, b"stss")? {
        sync.iter_mut().for_each(|s| *s = false);
        let mut r = Rd::new(b);
        r.version()?;
        for _ in 0..r.u32()? {
            let i = r.u32()? as usize;
            if let Some(s) = sync.get_mut(i.wrapping_sub(1)) {
                *s = true;
            }
        }
    }

    // チャンクの位置
    let chunk_offsets: Vec<u64> = if let Some((_, b)) = child(stbl, b"stco")? {
        let mut r = Rd::new(b);
        r.version()?;
        (0..r.u32()?).map(|_| r.u32().map(u64::from)).collect::<Result<_, _>>()?
    } else {
        let (_, b) = need(stbl, b"co64")?;
        let mut r = Rd::new(b);
        r.version()?;
        (0..r.u32()?).map(|_| r.u64()).collect::<Result<_, _>>()?
    };

    // チャンクごとのサンプル数 (stsc は「この番号のチャンクからは n 個ずつ」の並び)
    let (_, b) = need(stbl, b"stsc")?;
    let mut r = Rd::new(b);
    r.version()?;
    let mut runs = Vec::new();
    for _ in 0..r.u32()? {
        let (first, per, _desc) = (r.u32()?, r.u32()?, r.u32()?);
        runs.push((first, per));
    }
    let mut samples = Vec::with_capacity(count);
    let mut idx = 0usize;
    let mut dts = 0u64;
    for (ci, &base) in chunk_offsets.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let per = runs.iter().rev().find(|(first, _)| *first <= chunk_no).map(|r| r.1).unwrap_or(0);
        let mut off = base;
        for _ in 0..per {
            if idx >= count {
                return err("stsc がサンプル数より多くのサンプルを指しています");
            }
            samples.push(Sample {
                offset: off,
                size: sizes[idx],
                dts,
                duration: durations[idx],
                cto: ctos[idx],
                sync: sync[idx],
                chunk: ci as u32,
            });
            off += u64::from(sizes[idx]);
            dts += u64::from(durations[idx]);
            idx += 1;
        }
    }
    if idx != count {
        return err("チャンクに入っているサンプル数が stsz と合いません");
    }
    Ok(samples)
}
