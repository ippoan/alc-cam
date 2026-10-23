// 偽カメラが流す H.264 (Annex B) を作る。branded Chrome の WebCodecs (H.264 エンコーダあり) で
// canvas の動く絵を 360p・15fps・2 秒ごとにキーフレームで 6 秒ぶん符号化し、fixture.h264bin に書く。
//   形式: [u32 BE 長さ][u8 key][Annex B] の繰り返し
//   node make-fixture.mjs   (CHROME= で branded Chrome の場所を変えられる)

import fs from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const here = path.dirname(fileURLToPath(import.meta.url));
const browser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome', headless: true });
const page = await browser.newPage();
// WebCodecs は secure context 専用。localhost は secure 扱いなので空のページを localhost で出す
const server = http.createServer((_, res) => res.end('<!doctype html>')).listen(0, '127.0.0.1');
await new Promise((r) => server.once('listening', r));
await page.goto(`http://localhost:${server.address().port}/`);
const frames = await page.evaluate(async () => {
  const W = 640, H = 360, FPS = 15, N = FPS * 6;
  const canvas = new OffscreenCanvas(W, H);
  const g = canvas.getContext('2d');
  const out = [];
  const enc = new VideoEncoder({
    output: (chunk) => {
      const b = new Uint8Array(chunk.byteLength);
      chunk.copyTo(b);
      out.push({ key: chunk.type === 'key', data: btoa(String.fromCharCode(...b)) });
    },
    error: (e) => { throw e; },
  });
  // Main profile (C212 の stream2 と同じ系統)
  enc.configure({ codec: 'avc1.4d001e', width: W, height: H, bitrate: 500_000, framerate: FPS, avc: { format: 'annexb' } });
  for (let i = 0; i < N; i++) {
    g.fillStyle = `hsl(${(i * 4) % 360} 45% 35%)`;
    g.fillRect(0, 0, W, H);
    g.fillStyle = '#fff';
    g.fillRect((i * 9) % W, 150, 80, 80);
    g.font = '40px sans-serif';
    g.fillText(`frame ${i}`, 20, 60);
    const f = new VideoFrame(canvas, { timestamp: (i * 1e6) / FPS });
    enc.encode(f, { keyFrame: i % (FPS * 2) === 0 });
    f.close();
  }
  await enc.flush();
  return out;
});
await browser.close();
server.close();

const parts = [];
for (const f of frames) {
  const data = Buffer.from(f.data, 'base64');
  const head = Buffer.alloc(5);
  head.writeUInt32BE(data.length);
  head[4] = f.key ? 1 : 0;
  parts.push(head, data);
}
await fs.writeFile(path.join(here, 'fixture.h264bin'), Buffer.concat(parts));
console.log(`${frames.length} frames (${frames.filter((f) => f.key).length} key) → fixture.h264bin`);
