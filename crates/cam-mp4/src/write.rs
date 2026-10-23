//! 時間帯の選び出しと、出力の頭 (ftyp + moov + mdat ヘッダ) の組み立て。

use std::ops::Range;

use crate::{err, Clip, CopyRange, Error, Kind, Sample, Source, Track};

/// 1 ファイルぶんの選び出し
struct Pick {
    source: usize,
    /// トラックごと (先頭ファイルの tracks と同じ並び) のサンプル番号の範囲
    ranges: Vec<Range<usize>>,
    /// 写す区間 (元ファイル内)
    region: Range<u64>,
    from_ms: i64,
    to_ms: i64,
}

fn to_ts(ms: i64, timescale: u32) -> u64 {
    if ms <= 0 {
        0
    } else {
        (ms as u128 * u128::from(timescale) / 1000) as u64
    }
}

fn ts_to_ms(ts: u64, timescale: u32) -> i64 {
    (u128::from(ts) * 1000 / u128::from(timescale)) as i64
}

pub(crate) fn plan(sources: &[Source<'_>], from_ms: i64, to_ms: i64) -> Result<Clip, Error> {
    if from_ms >= to_ms {
        return err("終わりが始まりより前です");
    }
    let Some(first) = sources.first() else { return err("元のファイルがありません") };
    if sources.windows(2).any(|w| w[0].start_ms > w[1].start_ms) {
        return err("元のファイルが開始時刻の順に並んでいません");
    }
    let layout: Vec<(Kind, &[u8])> = first.moov.tracks.iter().map(|t| (t.kind, t.stsd.as_slice())).collect();
    for s in sources {
        let mine: Vec<(Kind, &[u8])> = s.moov.tracks.iter().map(|t| (t.kind, t.stsd.as_slice())).collect();
        if mine != layout {
            return err("ファイルによって映像・音声の形式が違うのでつなげません");
        }
    }
    let video = first.moov.tracks.iter().position(|t| t.kind == Kind::Video).expect("parse_moov が保証");

    let mut picks = Vec::new();
    for (si, s) in sources.iter().enumerate() {
        if let Some(p) = pick(si, s, video, from_ms, to_ms) {
            picks.push(p);
        }
    }
    let (Some(head_pick), Some(tail_pick)) = (picks.first(), picks.last()) else {
        return err("その時間帯の録画がありません");
    };
    let (actual_from_ms, actual_to_ms) = (head_pick.from_ms, tail_pick.to_ms);
    let payload: u64 = picks.iter().map(|p| p.region.end - p.region.start).sum();

    // moov の大きさは位置の表の幅 (32/64bit) で変わり、位置は moov の大きさで変わる。
    // 先に 64bit が要るかを大きめに見積もって決め、決めた幅で 1 回組み立てて大きさを測ってから本番を組む
    let estimate = 1024 * 1024 * 64 + payload;
    let wide = estimate > u64::from(u32::MAX);
    let big_mdat = payload + 8 > u64::from(u32::MAX);
    let mdat_header_len = if big_mdat { 16 } else { 8 };
    let ftyp = ftyp();
    let probe = moov(sources, &picks, actual_to_ms - actual_from_ms, 0, wide);
    let base = (ftyp.len() + probe.len() + mdat_header_len) as u64;
    let moov = moov(sources, &picks, actual_to_ms - actual_from_ms, base, wide);
    debug_assert_eq!(moov.len(), probe.len());

    let mut head = ftyp;
    head.extend_from_slice(&moov);
    if big_mdat {
        head.extend_from_slice(&1u32.to_be_bytes());
        head.extend_from_slice(b"mdat");
        head.extend_from_slice(&(payload + 16).to_be_bytes());
    } else {
        head.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
        head.extend_from_slice(b"mdat");
    }
    let ranges = picks
        .iter()
        .map(|p| CopyRange { source: p.source, offset: p.region.start, len: p.region.end - p.region.start })
        .collect();
    Ok(Clip { total_len: head.len() as u64 + payload, head, ranges, actual_from_ms, actual_to_ms })
}

fn pick(si: usize, s: &Source<'_>, video: usize, from_ms: i64, to_ms: i64) -> Option<Pick> {
    let v = &s.moov.tracks[video];
    let rel_from = from_ms - s.start_ms;
    let rel_to = to_ms - s.start_ms;
    if rel_to <= 0 {
        return None;
    }
    let (from_v, to_v) = (to_ts(rel_from, v.timescale), to_ts(rel_to, v.timescale));
    // 時間帯がこのファイルの終わり以降なら選ばない (でないと最後の GOP を拾ってしまう)
    let file_end = v.samples.last().map(|x| x.dts + u64::from(x.duration)).unwrap_or(0);
    if from_v >= file_end {
        return None;
    }
    let v_end = v.samples.partition_point(|x| x.dts < to_v);
    // 頭は「時間帯の頭以前で最後のキーフレーム」(無ければファイルの先頭)
    let v_start = if rel_from <= 0 { 0 } else { v.samples[..v_end].iter().rposition(|x| x.sync && x.dts <= from_v).unwrap_or(0) };
    if v_start >= v_end {
        return None;
    }
    let start_ms_rel = ts_to_ms(v.samples[v_start].dts, v.timescale);
    let last = &v.samples[v_end - 1];
    let end_ms_rel = ts_to_ms(last.dts + u64::from(last.duration), v.timescale);

    let mut ranges = Vec::with_capacity(s.moov.tracks.len());
    for (ti, t) in s.moov.tracks.iter().enumerate() {
        if ti == video {
            ranges.push(v_start..v_end);
            continue;
        }
        // 映像以外は映像の頭から時間帯の終わりまで
        let a = to_ts(start_ms_rel, t.timescale);
        let b = to_ts(rel_to, t.timescale);
        let lo = t.samples.partition_point(|x| x.dts < a);
        let hi = t.samples.partition_point(|x| x.dts < b).max(lo);
        ranges.push(lo..hi);
    }
    let selected = || s.moov.tracks.iter().zip(&ranges).flat_map(|(t, r)| t.samples[r.clone()].iter());
    let lo = selected().map(|x| x.offset).min()?;
    let hi = selected().map(|x| x.offset + u64::from(x.size)).max()?;
    Some(Pick { source: si, ranges, region: lo..hi, from_ms: s.start_ms + start_ms_rel, to_ms: s.start_ms + end_ms_rel })
}

// ---- 箱を組む ----

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(body.len() + 8);
    v.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![version, 0, 0, 0];
    b.extend_from_slice(body);
    bx(kind, &b)
}

fn ftyp() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"isom");
    b.extend_from_slice(&512u32.to_be_bytes());
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        b.extend_from_slice(brand);
    }
    bx(b"ftyp", &b)
}

/// 生の箱の中身の `at` バイト目 (ヘッダの後ろから数える) を書き換える
fn patch(raw: &[u8], at: usize, bytes: &[u8]) -> Vec<u8> {
    let mut v = raw.to_vec();
    let hl = if u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) == 1 { 16 } else { 8 };
    v[hl + at..hl + at + bytes.len()].copy_from_slice(bytes);
    v
}

fn version_of(raw: &[u8]) -> u8 {
    let hl = if u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) == 1 { 16 } else { 8 };
    raw[hl]
}

fn mvhd(duration_ms: u64, next_track: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0; 8]); // 作成・変更時刻
    b.extend_from_slice(&1000u32.to_be_bytes());
    b.extend_from_slice(&(duration_ms.min(u64::from(u32::MAX)) as u32).to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    b.extend_from_slice(&[0; 10]);
    for m in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        b.extend_from_slice(&m.to_be_bytes());
    }
    b.extend_from_slice(&[0; 24]);
    b.extend_from_slice(&next_track.to_be_bytes());
    full(b"mvhd", 0, &b)
}

fn tkhd(raw: &[u8], track_id: u32, duration_ms: u64) -> Vec<u8> {
    if version_of(raw) == 1 {
        let v = patch(raw, 20, &track_id.to_be_bytes());
        patch(&v, 28, &duration_ms.to_be_bytes())
    } else {
        let v = patch(raw, 12, &track_id.to_be_bytes());
        patch(&v, 20, &(duration_ms.min(u64::from(u32::MAX)) as u32).to_be_bytes())
    }
}

fn mdhd(raw: &[u8], duration: u64) -> Vec<u8> {
    if version_of(raw) == 1 {
        patch(raw, 24, &duration.to_be_bytes())
    } else {
        patch(raw, 16, &(duration.min(u64::from(u32::MAX)) as u32).to_be_bytes())
    }
}

/// (値, 続く数) の並びに畳む
fn runs<T: PartialEq + Copy>(it: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for v in it {
        match out.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

fn moov(sources: &[Source<'_>], picks: &[Pick], duration_ms: i64, base: u64, wide: bool) -> Vec<u8> {
    // 出力の mdat の中で、各ファイルの区間が置かれる位置
    let mut region_out = Vec::with_capacity(picks.len());
    let mut at = base;
    for p in picks {
        region_out.push(at);
        at += p.region.end - p.region.start;
    }
    let layout = &sources[0].moov.tracks;
    let mut traks = Vec::new();
    for (ti, proto) in layout.iter().enumerate() {
        let samples: Vec<(usize, &Sample)> = picks
            .iter()
            .enumerate()
            .flat_map(|(pi, p)| sources[p.source].moov.tracks[ti].samples[p.ranges[ti].clone()].iter().map(move |s| (pi, s)))
            .collect();
        traks.push(trak(proto, ti as u32 + 1, &samples, picks, &region_out, wide));
    }
    let mut body = mvhd(duration_ms.max(0) as u64, layout.len() as u32 + 1);
    for t in traks {
        body.extend_from_slice(&t);
    }
    bx(b"moov", &body)
}

fn trak(proto: &Track, track_id: u32, samples: &[(usize, &Sample)], picks: &[Pick], region_out: &[u64], wide: bool) -> Vec<u8> {
    let media_duration: u64 = samples.iter().map(|(_, s)| u64::from(s.duration)).sum();
    let duration_ms = ts_to_ms(media_duration, proto.timescale).max(0) as u64;
    let out_offset = |pi: usize, s: &Sample| region_out[pi] + (s.offset - picks[pi].region.start);

    // stts / ctts / stss / stsz
    let mut stts = Vec::new();
    let r = runs(samples.iter().map(|(_, s)| s.duration));
    stts.extend_from_slice(&(r.len() as u32).to_be_bytes());
    for (n, d) in r {
        stts.extend_from_slice(&n.to_be_bytes());
        stts.extend_from_slice(&d.to_be_bytes());
    }
    let ctts = if samples.iter().any(|(_, s)| s.cto != 0) {
        let negative = samples.iter().any(|(_, s)| s.cto < 0);
        let r = runs(samples.iter().map(|(_, s)| s.cto));
        let mut b = (r.len() as u32).to_be_bytes().to_vec();
        for (n, c) in r {
            b.extend_from_slice(&n.to_be_bytes());
            b.extend_from_slice(&c.to_be_bytes());
        }
        Some(full(b"ctts", u8::from(negative), &b))
    } else {
        None
    };
    let stss = if proto.kind == Kind::Video {
        let keys: Vec<u32> = samples.iter().enumerate().filter(|(_, (_, s))| s.sync).map(|(i, _)| i as u32 + 1).collect();
        let mut b = (keys.len() as u32).to_be_bytes().to_vec();
        for k in keys {
            b.extend_from_slice(&k.to_be_bytes());
        }
        Some(full(b"stss", 0, &b))
    } else {
        None
    };
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for (_, s) in samples {
        stsz.extend_from_slice(&s.size.to_be_bytes());
    }

    // チャンク: 元のチャンクが同じで、出力でも隙間なく続くあいだは 1 チャンク
    let mut chunks: Vec<(u64, u32)> = Vec::new(); // (出力の位置, サンプル数)
    let mut prev: Option<(usize, u32, u64)> = None; // (ファイル, 元のチャンク, 次に来るはずの出力位置)
    for &(pi, s) in samples {
        let off = out_offset(pi, s);
        match prev {
            Some((ppi, pc, next)) if ppi == pi && pc == s.chunk && next == off => chunks.last_mut().unwrap().1 += 1,
            _ => chunks.push((off, 1)),
        }
        prev = Some((pi, s.chunk, off + u64::from(s.size)));
    }
    let mut stsc = Vec::new();
    let mut entries = 0u32;
    let mut last_per = None;
    for (i, (_, per)) in chunks.iter().enumerate() {
        if last_per != Some(*per) {
            stsc.extend_from_slice(&(i as u32 + 1).to_be_bytes());
            stsc.extend_from_slice(&per.to_be_bytes());
            stsc.extend_from_slice(&1u32.to_be_bytes());
            entries += 1;
            last_per = Some(*per);
        }
    }
    let mut stsc_body = entries.to_be_bytes().to_vec();
    stsc_body.extend_from_slice(&stsc);
    let mut co = (chunks.len() as u32).to_be_bytes().to_vec();
    for (off, _) in &chunks {
        if wide {
            co.extend_from_slice(&off.to_be_bytes());
        } else {
            co.extend_from_slice(&(*off as u32).to_be_bytes());
        }
    }

    let mut stbl = proto.stsd.clone();
    stbl.extend_from_slice(&full(b"stts", 0, &stts));
    if let Some(c) = ctts {
        stbl.extend_from_slice(&c);
    }
    if let Some(s) = stss {
        stbl.extend_from_slice(&s);
    }
    stbl.extend_from_slice(&full(b"stsc", 0, &stsc_body));
    stbl.extend_from_slice(&full(b"stsz", 0, &stsz));
    stbl.extend_from_slice(&full(if wide { b"co64" } else { b"stco" }, 0, &co));

    let mut minf = proto.media_header.clone();
    minf.extend_from_slice(&proto.dinf);
    minf.extend_from_slice(&bx(b"stbl", &stbl));
    let mut mdia = mdhd(&proto.mdhd, media_duration);
    mdia.extend_from_slice(&proto.hdlr);
    mdia.extend_from_slice(&bx(b"minf", &minf));
    let mut trak = tkhd(&proto.tkhd, track_id, duration_ms);
    trak.extend_from_slice(&bx(b"mdia", &mdia));
    bx(b"trak", &trak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_moov, BoxHeader, Moov};

    /// テスト用の最小の Track。tkhd/mdhd は v0 の実の形 (patch の位置を確かめるため)
    fn track(kind: Kind, timescale: u32, samples: Vec<Sample>) -> Track {
        let tkhd = full(b"tkhd", 0, &[0u8; 80]);
        let mut mdhd_body = vec![0u8; 20];
        mdhd_body[8..12].copy_from_slice(&timescale.to_be_bytes());
        let mdhd = full(b"mdhd", 0, &mdhd_body);
        let mut hdlr_body = vec![0u8; 21];
        hdlr_body[4..8].copy_from_slice(if kind == Kind::Video { b"vide" } else { b"soun" });
        let hdlr = full(b"hdlr", 0, &hdlr_body);
        let media_header = if kind == Kind::Video { full(b"vmhd", 0, &[0; 8]) } else { full(b"smhd", 0, &[0; 4]) };
        let dinf = bx(b"dinf", &full(b"dref", 0, &0u32.to_be_bytes()));
        let mut stsd_body = 1u32.to_be_bytes().to_vec();
        stsd_body.extend_from_slice(&bx(if kind == Kind::Video { b"avc1" } else { b"mp4a" }, &[0; 8]));
        let stsd = full(b"stsd", 0, &stsd_body);
        Track { kind, timescale, samples, tkhd, mdhd, hdlr, media_header, dinf, stsd }
    }

    /// 1 秒 10 フレーム・5 フレームごとにキー・映像と音声が交互に並ぶ 4 秒のファイル
    fn file(offset_base: u64) -> Moov {
        let mut v = Vec::new();
        let mut a = Vec::new();
        let mut off = offset_base;
        for i in 0..40u64 {
            v.push(Sample { offset: off, size: 100, dts: i * 100, duration: 100, cto: 0, sync: i % 5 == 0, chunk: i as u32 * 2 });
            off += 100;
            a.push(Sample { offset: off, size: 10, dts: i * 100, duration: 100, cto: 0, sync: true, chunk: i as u32 * 2 + 1 });
            off += 10;
        }
        Moov { tracks: vec![track(Kind::Video, 1000, v), track(Kind::Audio, 1000, a)] }
    }

    fn moov_of(head: &[u8]) -> Moov {
        let ftyp = BoxHeader::parse(head).unwrap();
        let moov_start = ftyp.size as usize;
        let h = BoxHeader::parse(&head[moov_start..]).unwrap();
        parse_moov(&head[moov_start..moov_start + h.size as usize]).unwrap()
    }

    #[test]
    fn cuts_within_one_file_from_the_previous_keyframe() {
        let m = file(24);
        let clip = plan(&[Source { start_ms: 10_000, moov: &m }], 10_000 + 1_250, 10_000 + 2_000).unwrap();
        // 1.25 秒の直前のキーは 1.0 秒 (フレーム 10)。終わりは 2.0 秒の手前のフレーム 19 まで
        assert_eq!(clip.actual_from_ms, 11_000);
        assert_eq!(clip.actual_to_ms, 12_000);
        let out = moov_of(&clip.head);
        let v = out.track(Kind::Video).unwrap();
        assert_eq!(v.samples.len(), 10);
        assert!(v.samples[0].sync && v.samples[5].sync && !v.samples[1].sync);
        assert_eq!(v.samples[0].dts, 0);
        assert_eq!(out.track(Kind::Audio).unwrap().samples.len(), 10);
        // 写す区間はフレーム 10 の頭から、音声 19 の終わりまで
        assert_eq!(clip.ranges, vec![CopyRange { source: 0, offset: 24 + 10 * 110, len: 10 * 110 }]);
        assert_eq!(clip.total_len, clip.head.len() as u64 + 1100);
        // 出力の位置は mdat の中身の頭から並び直っている
        assert_eq!(v.samples[0].offset, clip.head.len() as u64);
        assert_eq!(out.track(Kind::Audio).unwrap().samples[0].offset, clip.head.len() as u64 + 100);
    }

    #[test]
    fn joins_across_two_files() {
        let (m1, m2) = (file(24), file(24));
        let sources = [Source { start_ms: 0, moov: &m1 }, Source { start_ms: 4_000, moov: &m2 }];
        let clip = plan(&sources, 3_000, 5_000).unwrap();
        assert_eq!((clip.actual_from_ms, clip.actual_to_ms), (3_000, 5_000));
        assert_eq!(clip.ranges.len(), 2);
        assert_eq!(clip.ranges[0], CopyRange { source: 0, offset: 24 + 30 * 110, len: 10 * 110 });
        assert_eq!(clip.ranges[1], CopyRange { source: 1, offset: 24, len: 10 * 110 });
        let out = moov_of(&clip.head);
        let v = out.track(Kind::Video).unwrap();
        assert_eq!(v.samples.len(), 20);
        // 時刻は途切れずに続く
        assert_eq!(v.samples[10].dts, 1_000);
        // 2 本目の頭は 1 本目の区間の直後に置かれる
        assert_eq!(v.samples[10].offset, v.samples[0].offset + 1100);
    }

    #[test]
    fn empty_and_invalid_ranges() {
        let m = file(24);
        let s = [Source { start_ms: 10_000, moov: &m }];
        assert!(plan(&s, 0, 5_000).is_err()); // ファイルより前
        assert!(plan(&s, 20_000, 30_000).is_err()); // ファイルより後
        assert!(plan(&s, 12_000, 11_000).is_err());
        assert!(plan(&[], 0, 1).is_err());
    }

    #[test]
    fn rejects_mismatched_formats() {
        let m1 = file(24);
        let mut m2 = file(24);
        m2.tracks.pop();
        let s = [Source { start_ms: 0, moov: &m1 }, Source { start_ms: 4_000, moov: &m2 }];
        assert!(plan(&s, 0, 8_000).is_err());
    }
}
