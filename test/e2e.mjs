// 通しの動作確認:
//   偽カメラ (RTSP, Digest) → alc-cam-host (Rust, native messaging) ⇄ 拡張 (background service worker) ⇄ cam-room (ローカル版)
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
import { KEEPALIVE_PING } from '../extension/lib/signaling.js';

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
const signaling = startSignaling({ port: SIG_PORT, v1Sites: ['v1-site'] });

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
const deviceJoins = () => signaling.log.filter((l) => l.startsWith('device joined')).length;
try {
  const sw = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent('serviceworker'));
  results.extId = new URL(sw.url()).host;
  if (results.extId !== EXT_ID) throw new Error(`拡張 ID が manifest の key と合わない: ${results.extId}`);
  sw.on('console', (m) => console.log('[sw]', m.text()));

  // パネルは開かない。設定を保存するだけで background が cam-room に繋ぐ
  const settings = (siteId) => ({ signalingUrl: `ws://127.0.0.1:${SIG_PORT}`, siteId, rtspUrl: camera.url, stun: 'stun.l.google.com:19302' });
  // 起動直後の worker は chrome.* がまだ生えていないことがある
  await waitFor(() => sw.evaluate(() => !!globalThis.chrome?.storage), 10_000, 'service worker の chrome.storage');
  await sw.evaluate((s) => chrome.storage.local.set(s), settings('test-site'));
  await waitFor(() => deviceJoins() >= 1, 10_000, 'パネル無しで cam-room 接続');
  await sleep(1000);
  results.beforeViewer = { ...camera.stats, hostRunning: hostRunning(), pages: ctx.pages().length };

  const openViewer = async (site = 'test-site') => {
    const page = await viewerBrowser.newPage();
    await page.goto(`http://localhost:${SIG_PORT}/viewer.html?site=${site}`);
    return page;
  };
  const decoded = (page) => page.evaluate(() => window.camState.framesDecoded ?? 0);
  const decodes = (page, n, label) => waitFor(async () => ((await decoded(page)) > n ? page.evaluate(() => window.camState) : null), 30_000, label);
  // 2 秒の間に増えたか (= まだ映像が流れているか)
  const stillFlowing = async (page) => {
    const before = await decoded(page);
    await sleep(2000);
    return (await decoded(page)) - before;
  };

  // A が見ている途中で B が入る (同時に 2 人)
  const a = await openViewer();
  results.a = await decodes(a, 30, 'A のデコード');
  const b = await openViewer();
  results.b = await decodes(b, 15, 'B (途中参加) のデコード');
  results.bothWatching = { aDelta: await stillFlowing(a), camera: { ...camera.stats } };
  await a.screenshot({ path: path.join(here, 'e2e-viewer.png') });

  // A が離れても B は流れ続け、カメラは畳まない
  await a.close();
  await sleep(1000);
  results.afterALeft = { bDelta: await stillFlowing(b), camera: { ...camera.stats } };

  // 最後の 1 人 (B) が離れたらカメラを畳み、host も止める
  await b.close();
  await waitFor(() => camera.stats.closed >= 1, 10_000, 'カメラの切断');
  await waitFor(() => !hostRunning(), 15_000, '見る人が居なくなって host 終了');
  results.afterAllLeft = { ...camera.stats, hostRunning: hostRunning() };

  // 入り直しで再び映る。パネルを開いて状態を見て、閉じても映り続ける
  const c = await openViewer();
  results.c = await decodes(c, 15, 'C (入り直し) のデコード');
  const panel = await ctx.newPage();
  await panel.goto(`chrome-extension://${EXT_ID}/panel.html`);
  const status = () => panel.evaluate(() =>
    Object.fromEntries([...document.querySelectorAll('.status dt')].map((dt) => [dt.textContent, dt.nextElementSibling.textContent])));
  results.panel = await waitFor(async () => {
    const st = await status();
    return st['見る人'] === '1 人' && st.host.startsWith('起動') ? st : null;
  }, 10_000, 'パネルの状態表示');
  await panel.close();
  await sleep(1000);
  results.afterPanelClosed = { cDelta: await stillFlowing(c), hostRunning: hostRunning() };

  // keepalive: 20 秒ごとに完全一致の ping (= モックの自動応答に当たり、従来の ping 分岐に来ない)
  await waitFor(() => signaling.keepalive.device >= 1, 25_000, 'keepalive の ping');
  results.keepalive = { ...signaling.keepalive, constant: KEEPALIVE_PING, legacyPings: signaling.log.filter((l) => l === 'device: ping').length };

  // service worker を止めても (CDP)、alarm で起き直して設定から繋ぎ直し、C に再び映る
  const joinsBefore = deviceJoins();
  const offersBefore = await c.evaluate(() => window.camState.offers);
  const cdpPage = await ctx.newPage();
  await cdpPage.goto(`chrome-extension://${EXT_ID}/panel.html`);
  const cdp = await ctx.newCDPSession(cdpPage);
  await cdp.send('ServiceWorker.enable');
  await cdp.send('ServiceWorker.stopAllWorkers');
  await cdpPage.close();
  await waitFor(() => signaling.log.slice(-50).includes('device left'), 10_000, 'service worker 停止で device 切断');
  const stoppedAt = Date.now();
  await waitFor(() => deviceJoins() > joinsBefore, 70_000, 'service worker の起き直しで再接続');
  results.swRestartSec = (Date.now() - stoppedAt) / 1000;
  await waitFor(async () => (await c.evaluate(() => window.camState.offers)) > offersBefore, 20_000, 'C へ offer し直し');
  await sleep(3000);
  results.afterSwRestart = { cDelta: await stillFlowing(c), camState: await c.evaluate(() => window.camState) };
  await c.close();
  await waitFor(() => !hostRunning(), 15_000, 'host 終了 (SW 再起動後)');

  // 設定を変えたら繋ぎ直す: v1 の部屋 (proto を知らない古い DO) でも 1 人なら映る
  // パネルの「保存」と同じく、拡張のページから storage を書く (service worker は起き直して別物になっている)
  const setStorage = async (obj) => {
    const page = await ctx.newPage();
    await page.goto(`chrome-extension://${EXT_ID}/panel.html`);
    await page.evaluate((o) => chrome.storage.local.set(o), obj);
    await page.close();
  };
  await setStorage(settings('v1-site'));
  await waitFor(() => signaling.log.includes('device joined (v1)'), 10_000, 'cam-room (v1) 接続');
  const d = await openViewer('v1-site');
  results.v1 = await decodes(d, 15, 'v1 の部屋のデコード');
  await d.close();
  await waitFor(() => !hostRunning(), 15_000, 'host 終了 (v1)');

  // 「停止」(enabled=false) で cam-room から抜ける
  const leftBefore = signaling.log.filter((l) => l === 'device left').length;
  await setStorage({ enabled: false });
  await waitFor(() => signaling.log.filter((l) => l === 'device left').length > leftBefore, 10_000, '停止で cam-room から抜ける');
  results.stoppedByToggle = true;
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
const checks = results.error ? {} : {
  // パネルを開かずに cam-room に繋ぎ、見る人が来るまでカメラにも host にも触らない
  startsWithoutPanel: results.beforeViewer.pages <= 1 && results.beforeViewer.sessions === 0 && !results.beforeViewer.hostRunning,
  // 2 人同時でも RTSP は 1 本、A も流れ続けている
  oneRtspForTwo: results.bothWatching.camera.sessions === 1 && results.bothWatching.aDelta > 0,
  // 片方が離れてももう片方は流れ、カメラは畳まない
  otherKeepsFlowing: results.afterALeft.bDelta > 0 && results.afterALeft.camera.teardowns === 0,
  // 最後の 1 人で TEARDOWN、host も止まる
  teardownOnLastLeave: results.afterAllLeft.teardowns >= 1 && results.afterAllLeft.sessions === 1 && !results.afterAllLeft.hostRunning,
  // パネルを閉じても映り続ける
  keepsFlowingAfterPanelClose: results.afterPanelClosed.cDelta > 0 && results.afterPanelClosed.hostRunning,
  // ping は仕様の文字列ちょうど (DO を起こす従来の分岐に 1 回も来ない)
  keepalivePingExact: results.keepalive.constant === '{"type":"ping"}' && results.keepalive.device >= 1 && results.keepalive.legacyPings === 0,
  // service worker を止めても繋ぎ直して映る
  reconnectsAfterSwStop: results.afterSwRestart.cDelta > 0,
  stoppedByToggle: !!results.stoppedByToggle,
};
console.log(JSON.stringify(checks, null, 1));
const ok = !results.error && Object.values(checks).every(Boolean);
console.log(ok ? 'E2E OK' : 'E2E NG');
process.exit(ok ? 0 : 1);
