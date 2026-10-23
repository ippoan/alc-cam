//! 手元のファイルで切り出しを試す (Worker と同じ plan_clip を、R2 の代わりにファイルの読み書きで回す)。
//!   cargo run -p cam-mp4 --example clip -- <出力.mp4> <from_ms> <to_ms> <start_ms>:<入力.mp4> ...
//! 時刻はすべて UNIX ms。入力は開始時刻の順に並べる。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use cam_mp4::{parse_moov, plan_clip, BoxHeader, Moov, Source};

fn read_moov(path: &str) -> Moov {
    let mut f = File::open(path).expect("入力を開けません");
    let len = f.metadata().unwrap().len();
    let mut pos = 0u64;
    // 一番外の箱を辿って moov を探す (Worker では R2 の範囲読み出しで同じことをする)
    while pos < len {
        let mut h = [0u8; 16];
        f.seek(SeekFrom::Start(pos)).unwrap();
        let n = f.read(&mut h).unwrap();
        let bh = BoxHeader::parse(&h[..n]).expect("箱が読めません");
        let size = if bh.size == 0 { len - pos } else { bh.size };
        if &bh.kind == b"moov" {
            let mut buf = vec![0u8; size as usize];
            f.seek(SeekFrom::Start(pos)).unwrap();
            f.read_exact(&mut buf).unwrap();
            return parse_moov(&buf).expect("moov が読めません");
        }
        pos += size;
    }
    panic!("{path} に moov がありません");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (out, from, to) = (&args[0], args[1].parse().unwrap(), args[2].parse().unwrap());
    let inputs: Vec<(i64, &str)> = args[3..].iter().map(|a| {
        let (s, p) = a.split_once(':').unwrap();
        (s.parse().unwrap(), p)
    }).collect();
    let moovs: Vec<Moov> = inputs.iter().map(|(_, p)| read_moov(p)).collect();
    let sources: Vec<Source> = inputs.iter().zip(&moovs).map(|((s, _), m)| Source { start_ms: *s, moov: m }).collect();
    let clip = plan_clip(&sources, from, to).expect("切り出せません");

    let mut o = File::create(out).unwrap();
    o.write_all(&clip.head).unwrap();
    for r in &clip.ranges {
        let mut f = File::open(inputs[r.source].1).unwrap();
        f.seek(SeekFrom::Start(r.offset)).unwrap();
        std::io::copy(&mut f.take(r.len), &mut o).unwrap();
    }
    eprintln!(
        "wrote {} bytes (expected {}), actual {}..{} ms, ranges={:?}",
        o.metadata().unwrap().len(),
        clip.total_len,
        clip.actual_from_ms,
        clip.actual_to_ms,
        clip.ranges
    );
}
