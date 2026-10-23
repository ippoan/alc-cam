// 通しの動作確認:
//   偽カメラ (RTSP, Digest) → alc-cam-host (Rust, native messaging) ⇄ 拡張 (サイドパネル) ⇄ cam-room (ローカル版)
//   → 見る側 (viewer.html = alc-app の admin と同じ振る舞い) が WebRTC で受けてデコードする
//
//   cargo build -p alc-cam-host && cd test && npm install && npx playwright-core install chromium && node e2e.mjs
//
// 拡張は Playwright の Chromium (branded Chrome 137+ は --load-extension を無視する)、
// 見る側は branded Chrome (H.264 のデコーダを確実に持つ) で開く。

import { execSync } from 'node:child_process';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';
import { startFakeRtsp } from './fake-rtsp.mjs';
import { startSignaling } from './signaling.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const EXT_ID = 'icmmpdjddlphkbokdalofdjpkcgfiggd';
const HOST_NAME = 'org.ippoan.alc_cam_host';
const HOST_BIN = path.resolve(here, '../target/debug/alc-cam-host');
const SIG_PORT = 8791;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const lanIp = Object.values(os.networkInterfaces()).flat().find((a) => a.family === 'IPv4' && !a.internal)?.address;

async function waitFor(fn, ms, label) {
  const until = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > until) throw new Error(`timeout: ${label}`);
    await sleep(250);
  }
}
// pgrep -f <パス> は execSync の sh -c 自身にも当たるので、プロセス名で完全一致を見る
const hostRunning = () => {
  try {
    return execSync('pgrep -x alc-cam-host').toString().trim().length > 0;
  } catch {
    return false;
  }
};

const camera = startFakeRtsp({ host: lanIp, port: 8554 });
const signaling = startSignaling({ port: SIG_PORT });

// native messaging host の登録 (Linux の Chromium は <user-data-dir>/NativeMessagingHosts も見る)
const userDataDir = await fs.mkdtemp(path.join(os.tmpdir(), 'alc-cam-e2e-'));
await fs.mkdir(path.join(userDataDir, 'NativeMessagingHosts'));
await fs.writeFile(
  path.join(userDataDir, 'NativeMessagingHosts', `${HOST_NAME}.json`),
  JSON.stringify({ name: HOST_NAME, description: 'alc-cam host (e2e)', path: HOST_BIN, type: 'stdio', allowed_origins: [`chrome-extension://${EXT_ID}/`] }),
);

const extDir = path.join(here, '../extension');
const ctx = await chromium.launchPersistentContext(userDataDir, {
  channel: 'chromium',
  headless: false,
  args: ['--headless=new', `--disable-extensions-except=${extDir}`, `--load-extension=${extDir}`],
});
const viewerBrowser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome', headless: true });

const results = {};
try {
  const sw = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent('serviceworker'));
  results.extId = new URL(sw.url()).host;
  if (results.extId !== EXT_ID) throw new Error(`拡張 ID が manifest の key と合わない: ${results.extId}`);

  const panel = await ctx.newPage();
  panel.on('console', (m) => console.log('[panel]', m.text()));
  await panel.goto(`chrome-extension://${EXT_ID}/panel.html`);
  await panel.evaluate((s) => chrome.storage.local.set(s), {
    signalingUrl: `ws://127.0.0.1:${SIG_PORT}`,
    siteId: 'test-site',
    rtspUrl: camera.url,
    stun: 'stun.l.google.com:19302',
  });
  await panel.reload();
  await panel.click('#toggle');
  const status = () => panel.evaluate(() =>
    Object.fromEntries([...document.querySelectorAll('.status dt')].map((dt) => [dt.textContent, dt.nextElementSibling.textContent])));
  await waitFor(async () => (await status()).host.startsWith('起動'), 10_000, 'host 起動');
  await waitFor(async () => (await status())['シグナリング'] === '接続済み', 10_000, 'cam-room 接続');
  results.beforeViewer = { ...camera.stats };

  // 1 人目
  const viewer = await viewerBrowser.newPage();
  await viewer.goto(`http://localhost:${SIG_PORT}/viewer.html?site=test-site`);
  const first = await waitFor(async () => {
    const s = await viewer.evaluate(() => window.camState);
    return s.framesDecoded > 30 ? s : null;
  }, 30_000, '1 人目のデコード');
  results.first = first;
  results.panelWhileViewing = await status();
  await viewer.screenshot({ path: path.join(here, 'e2e-viewer.png') });

  // 離れたらカメラを畳む
  await viewer.close();
  await waitFor(() => camera.stats.closed >= 1, 10_000, 'カメラの切断');
  await sleep(300);
  results.afterLeave = { ...camera.stats, panel: await status() };

  // 2 人目 (入り直し)
  const viewer2 = await viewerBrowser.newPage();
  await viewer2.goto(`http://localhost:${SIG_PORT}/viewer.html?site=test-site`);
  results.second = await waitFor(async () => {
    const s = await viewer2.evaluate(() => window.camState);
    return s.framesDecoded > 15 ? s : null;
  }, 30_000, '2 人目のデコード');
  await viewer2.close();

  // パネルを閉じたら host も終わる
  await panel.close();
  await waitFor(() => !hostRunning(), 10_000, 'host の終了');
  results.hostExitedOnPanelClose = true;
  results.signaling = signaling.log;
} catch (e) {
  results.error = String(e);
} finally {
  await ctx.close();
  await viewerBrowser.close();
  camera.close();
  signaling.close();
}

console.log(JSON.stringify(results, null, 1));
const ok = !results.error && results.beforeViewer.sessions === 0 && results.afterLeave.teardowns >= 1 && results.hostExitedOnPanelClose;
console.log(ok ? 'E2E OK' : 'E2E NG');
process.exit(ok ? 0 : 1);
