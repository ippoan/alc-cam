//! 時間指定の切り出し: 重なる録画を R2 にそろえ、目次 (moov) を読み、範囲を切ってつないだ 1 本を
//! R2 `clips/<tenant>/<camera_id>/<開始>_<終了>.mp4` に置く。同じ時間帯をもう一度頼まれたら作り直さない。

use std::collections::HashMap;

use cam_mp4::{parse_moov, plan_clip, BoxHeader, Moov, Source};
use cam_sd::{file_start_ms, hours_to_scan, list_path, overlapping, parse_listing, safe_segment, wanted, Recording};
use futures_util::StreamExt;
use worker::{console_log, Bucket, Data, HttpMetadata, MultipartUpload, Range, Result, UploadedPart};

use crate::camera::Camera;
use crate::raw;

/// SD の録画の種類 (カメラ名の下のフォルダ)。スケジュール録画だけを見る
const KINDS: [&str; 1] = ["Schedule"];

/// R2 のマルチパートの 1 パートの大きさ。最後以外は同じ大きさ・5MiB 以上でないといけない
const PART: usize = 16 * 1024 * 1024;
/// 頭のファイルは時間帯より前に始まる。1 本 (約 4 分) より長めに前から探す
const LEAD_MS: i64 = 10 * 60_000;
/// 最後のファイルの長さの見込み (次のファイルが無いとき)
const LAST_LEN_MS: i64 = 5 * 60_000;

pub struct Made {
    pub key: String,
    pub size: u64,
    pub actual_from_ms: i64,
    pub actual_to_ms: i64,
    pub sources: Vec<String>,
    pub reused: bool,
}

/// 利用者に返せる失敗 (HTTP の status と文言)
pub struct Fail(pub u16, pub String);

impl From<worker::Error> for Fail {
    fn from(e: worker::Error) -> Self {
        Fail(502, e.to_string())
    }
}

impl From<cam_mp4::Error> for Fail {
    fn from(e: cam_mp4::Error) -> Self {
        Fail(422, e.0)
    }
}

struct Found {
    start_ms: i64,
    kind: String,
    date: String,
    hour: String,
    name: String,
    size: u64,
}

/// 切り出しの置き場
pub struct Target {
    /// SD のいちばん上のフォルダ (カメラ名)
    pub sd_root: String,
    /// 切り出しの R2 のキー (`cam_ingest_api::clip_r2_key`)
    pub key: String,
    /// 元ファイルの R2 のキーの頭 (`cam_ingest_api::raw_r2_prefix`)
    pub raw_prefix: String,
}

pub async fn make(target: &Target, cam: &mut Camera, bucket: &Bucket, from_ms: i64, to_ms: i64) -> std::result::Result<Made, Fail> {
    let key = target.key.clone();
    if let Some(o) = bucket.head(key.clone()).await? {
        let meta = o.custom_metadata().unwrap_or_default();
        let get = |k: &str| meta.get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
        let sources = meta.get("sources").map(|s| s.split(',').map(String::from).collect()).unwrap_or_default();
        return Ok(Made { key, size: o.size(), actual_from_ms: get("actual_from_ms"), actual_to_ms: get("actual_to_ms"), sources, reused: true });
    }

    // 1. 重なる録画を探す
    let mut found = Vec::new();
    for (date, hour) in hours_to_scan(from_ms, to_ms, LEAD_MS) {
        for kind in KINDS {
            let dir = format!("{}/{kind}/{date}/{hour}", target.sd_root);
            for f in parse_listing(&cam.get_text(&list_path(&dir)).await?).files {
                if !(wanted(&f.name) && f.name.ends_with(".mp4") && safe_segment(&f.name)) {
                    continue;
                }
                if let Some(start_ms) = file_start_ms(&f.name) {
                    found.push(Found { start_ms, kind: kind.to_string(), date: date.clone(), hour: hour.clone(), name: f.name, size: f.size });
                }
            }
        }
    }
    found.sort_by_key(|f| f.start_ms);
    let starts: Vec<i64> = found.iter().map(|f| f.start_ms).collect();
    let picked: Vec<&Found> = overlapping(&starts, from_ms, to_ms, LAST_LEN_MS).into_iter().map(|i| &found[i]).collect();
    if picked.is_empty() {
        return Err(Fail(404, "その時間帯の録画が SD にありません".into()));
    }

    // 2. 元ファイルを R2 にそろえ、目次を読む
    let mut moovs = Vec::with_capacity(picked.len());
    let mut keys = Vec::with_capacity(picked.len());
    for f in &picked {
        let rec = Recording { camera: &target.sd_root, kind: &f.kind, date: &f.date, hour: &f.hour, name: &f.name };
        raw::ensure(cam, bucket, &target.raw_prefix, &rec, f.size).await?;
        let raw_key = raw::key(&target.raw_prefix, &rec);
        moovs.push(read_moov(bucket, &raw_key, f.size).await?);
        keys.push(raw_key);
    }

    // 3. 切ってつなぐ計画を立て、R2 に書く
    let sources: Vec<Source> = picked.iter().zip(&moovs).map(|(f, m)| Source { start_ms: f.start_ms, moov: m }).collect();
    let clip = plan_clip(&sources, from_ms, to_ms)?;
    console_log!("alc-cam: clip {key} {} bytes from {} files", clip.total_len, picked.len());
    let meta = HashMap::from([
        ("actual_from_ms".to_string(), clip.actual_from_ms.to_string()),
        ("actual_to_ms".to_string(), clip.actual_to_ms.to_string()),
        ("sources".to_string(), keys.join(",")),
    ]);
    let upload = bucket
        .create_multipart_upload(key.clone())
        .http_metadata(HttpMetadata { content_type: Some("video/mp4".into()), ..Default::default() })
        .custom_metadata(meta)
        .execute()
        .await?;
    let mut w = PartWriter { upload: &upload, buf: Vec::with_capacity(PART * 2), parts: Vec::new() };
    let written = async {
        w.push(&clip.head).await?;
        for r in &clip.ranges {
            let obj = bucket
                .get(keys[r.source].clone())
                .range(Range::OffsetWithLength { offset: r.offset, length: r.len })
                .execute()
                .await?
                .ok_or("元ファイルが R2 から消えました")?;
            let mut stream = obj.body().ok_or("元ファイルの本文が読めません")?.stream()?;
            while let Some(chunk) = stream.next().await {
                w.push(&chunk?).await?;
            }
        }
        w.flush_last().await?;
        Ok::<_, worker::Error>(())
    }
    .await;
    if let Err(e) = written {
        let _ = upload.abort().await;
        return Err(e.into());
    }
    let parts = std::mem::take(&mut w.parts);
    let obj = upload.complete(parts).await?;
    Ok(Made { key, size: obj.size(), actual_from_ms: clip.actual_from_ms, actual_to_ms: clip.actual_to_ms, sources: keys, reused: false })
}

/// 一番外の箱を R2 の範囲読み出しで辿り、moov を読む (カメラの録画は moov が末尾)
async fn read_moov(bucket: &Bucket, key: &str, size: u64) -> Result<Moov> {
    let read = |offset: u64, length: u64| async move {
        let obj = bucket.get(key.to_string()).range(Range::OffsetWithLength { offset, length }).execute().await?.ok_or("元ファイルが R2 にありません")?;
        obj.body().ok_or("本文が読めません")?.bytes().await
    };
    let mut pos = 0u64;
    for _ in 0..64 {
        if pos + 8 > size {
            break;
        }
        let head = read(pos, 16.min(size - pos)).await?;
        let h = BoxHeader::parse(&head).map_err(|e| e.0)?;
        let box_size = if h.size == 0 { size - pos } else { h.size };
        if &h.kind == b"moov" {
            return parse_moov(&read(pos, box_size).await?).map_err(|e| e.0.into());
        }
        if box_size < 8 {
            break;
        }
        pos += box_size;
    }
    Err(format!("{key} に moov が見つかりません (録画が閉じていない可能性)").into())
}

struct PartWriter<'a> {
    upload: &'a MultipartUpload,
    buf: Vec<u8>,
    parts: Vec<UploadedPart>,
}

impl PartWriter<'_> {
    async fn push(&mut self, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= PART {
            let rest = self.buf.split_off(PART);
            let part = std::mem::replace(&mut self.buf, rest);
            self.send(part).await?;
        }
        Ok(())
    }

    async fn flush_last(&mut self) -> Result<()> {
        if !self.buf.is_empty() || self.parts.is_empty() {
            let part = std::mem::take(&mut self.buf);
            self.send(part).await?;
        }
        Ok(())
    }

    async fn send(&mut self, part: Vec<u8>) -> Result<()> {
        let n = self.parts.len() as u16 + 1;
        self.parts.push(self.upload.upload_part(n, Data::Bytes(part)).await?);
        Ok(())
    }
}
