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
import { startFakeCgi } from './fake-cgi.mjs';
import { startFakeRtsp } from './fake-rtsp.mjs';
import { startSignaling } from './signaling.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const EXT_ID = 'icmmpdjddlphkbokdalofdjpkcgfiggd';
const HOST_NAME = 'org.ippoan.alc_cam_host';
const HOST_BIN = path.resolve(here, '../target/debug/alc-cam-host');
const SIG_PORT = 8791;
const CGI_PORT = 8780;
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
// 首振りの CGI (本物は 80 番。host は ALC_CAM_HTTP_PORT を見る。Chromium から起動される host へ環境変数で渡る)
const cgi = startFakeCgi({ host: lanIp, port: CGI_PORT });
process.env.ALC_CAM_HTTP_PORT = String(CGI_PORT);
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
try {
  const sw = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent('serviceworker'));
  results.extId = new URL(sw.url()).host;
  if (results.extId !== EXT_ID) throw new Error(`拡張 ID が manifest の key と合わない: ${results.extId}`);

  const panel = await ctx.newPage();
  panel.on('console', (m) => console.log('[panel]', m.text()));
  await panel.goto(`chrome-extension://${EXT_ID}/panel.html`);
  const settings = (siteId) => ({ signalingUrl: `ws://127.0.0.1:${SIG_PORT}`, siteId, rtspUrl: camera.url, stun: 'stun.l.google.com:19302' });
  await panel.evaluate((s) => chrome.storage.local.set(s), settings('test-site'));
  await panel.reload();
  await panel.click('#toggle');
  const status = () => panel.evaluate(() =>
    Object.fromEntries([...document.querySelectorAll('.status dt')].map((dt) => [dt.textContent, dt.nextElementSibling.textContent])));
  await waitFor(async () => (await status()).host.startsWith('起動'), 10_000, 'host 起動');
  await waitFor(async () => (await status())['シグナリング'] === '接続済み', 10_000, 'cam-room 接続');
  results.beforeViewer = { ...camera.stats };

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
  results.bothWatching = { aDelta: await stillFlowing(a), camera: { ...camera.stats }, panel: await status() };
  await a.screenshot({ path: path.join(here, 'e2e-viewer.png') });

  // 首振り: 映像と同じ PeerConnection のデータチャネル ptz で host へ。A と B が同時に押す
  results.ptzCaps = await waitFor(() => a.evaluate(() => window.camState.ptz.caps), 10_000, 'ptz の caps');
  // 押している間は 300ms ごとに再送する (alc-app と同じ)。A が左を押している途中で B が上を押す (最後の命令優先)
  const hold = async (page, dir, times) => {
    for (let i = 0; i < times; i++) {
      await page.evaluate((d) => window.ptzSend({ action: 'move', dir: d }), dir);
      await sleep(300);
    }
  };
  await hold(a, 'left', 3);
  await hold(b, 'up', 3);
  await a.evaluate(() => window.ptzSend({ action: 'stop' })); // A の離し: 後から動かしている B は止めない
  const upBeforeAStop = cgi.stats.calls.filter((c) => c.startsWith('move=up')).length;
  await sleep(400);
  const upAfterAStop = cgi.stats.calls.filter((c) => c.startsWith('move=up')).length;
  await b.evaluate(() => window.ptzSend({ action: 'stop' }));
  await sleep(500);
  const stepsAfterStop = cgi.stats.calls.length;
  // 離したのに stop が届かない (閉じた等) → 自動で止まる
  await a.evaluate(() => window.ptzSend({ action: 'move', dir: 'right' }));
  await sleep(2000);
  const stepsAfterAuto = cgi.stats.calls.length;
  await sleep(800);
  await a.evaluate(() => window.ptzSend({ action: 'preset', preset: 2 }));
  await a.evaluate(() => window.ptzSend({ action: 'zoom', dir: 'in' }));
  await a.evaluate(() => window.ptzSend({ action: 'preset', preset: 9 }));
  await sleep(800);
  results.ptz = {
    stats: { ...cgi.stats, calls: [...cgi.stats.calls] },
    upBeforeAStop,
    upAfterAStop,
    stepsAfterStop,
    stepsAfterAuto,
    stepsAtEnd: cgi.stats.calls.length,
    a: await a.evaluate(() => window.camState.ptz.results),
    b: await b.evaluate(() => window.camState.ptz.results),
  };

  // A が離れても B は流れ続け、カメラは畳まない
  await a.close();
  await sleep(1000);
  results.afterALeft = { bDelta: await stillFlowing(b), camera: { ...camera.stats }, panel: await status() };

  // 最後の 1 人 (B) が離れたらカメラを畳む
  await b.close();
  await waitFor(() => camera.stats.closed >= 1, 10_000, 'カメラの切断');
  await sleep(300);
  results.afterAllLeft = { ...camera.stats, panel: await status() };

  // 入り直しで再び映る
  const c = await openViewer();
  results.c = await decodes(c, 15, 'C (入り直し) のデコード');
  await c.close();
  await waitFor(() => camera.stats.closed >= 2, 10_000, 'カメラの切断 (2 回目)');

  // v1 の部屋 (proto を知らない古い DO) でも 1 人なら映る
  await panel.click('#toggle');
  await panel.evaluate((s) => chrome.storage.local.set(s), settings('v1-site'));
  await panel.reload();
  await panel.click('#toggle');
  await waitFor(async () => (await status())['シグナリング'] === '接続済み', 10_000, 'cam-room (v1) 接続');
  const d = await openViewer('v1-site');
  results.v1 = await decodes(d, 15, 'v1 の部屋のデコード');
  await d.close();
  await waitFor(() => camera.stats.closed >= 3, 10_000, 'カメラの切断 (v1)');

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
  cgi.close();
  signaling.close();
}

console.log(JSON.stringify(results, null, 1));
const checks = results.error ? {} : {
  // 見る人が来るまでカメラに触らない
  noCameraBeforeViewer: results.beforeViewer.sessions === 0,
  // 2 人同時でも RTSP は 1 本、A も流れ続けている
  oneRtspForTwo: results.bothWatching.camera.sessions === 1 && results.bothWatching.aDelta > 0,
  // 片方が離れてももう片方は流れ、カメラは畳まない
  otherKeepsFlowing: results.afterALeft.bDelta > 0 && results.afterALeft.camera.teardowns === 0,
  // 最後の 1 人で TEARDOWN
  teardownOnLastLeave: results.afterAllLeft.teardowns >= 1 && results.afterAllLeft.sessions === 1,
  hostExitedOnPanelClose: !!results.hostExitedOnPanelClose,
  // データチャネルで caps が届き、押している間ステップが続き (左も上も)、要求は常に 1 本・Digest が通る
  ptzCaps: results.ptzCaps?.dirs?.length === 4 && results.ptzCaps.zoom === false,
  ptzSteps: ['move=left', 'move=up'].every((m) => results.ptz.stats.calls.filter((c) => c.startsWith(m)).length >= 2),
  ptzOneAtATime: results.ptz.stats.maxInFlight === 1 && results.ptz.stats.unauthorized === 0,
  // A の stop で B は止まらない・B の stop の後は送らない・stop が無くても 1 秒で止まる
  ptzStops: results.ptz.upAfterAStop > results.ptz.upBeforeAStop &&
    results.ptz.stats.calls[results.ptz.stepsAfterStop]?.startsWith('move=right') &&
    results.ptz.stepsAfterAuto === results.ptz.stats.calls.filter((c) => !c.startsWith('movepresetpoint')).length,
  ptzPreset: results.ptz.stats.calls.at(-1) === 'movepresetpoint=2',
  ptzResults: results.ptz.a.some((r) => r.action === 'move' && r.ok) && results.ptz.b.some((r) => r.action === 'move' && r.ok) &&
    results.ptz.a.some((r) => r.action === 'preset' && r.ok) && results.ptz.a.some((r) => r.action === 'zoom' && r.unsupported) &&
    results.ptz.a.some((r) => r.action === 'preset' && !r.ok && !r.unsupported),
};
console.log(JSON.stringify(checks, null, 1));
const ok = !results.error && Object.values(checks).every(Boolean);
console.log(ok ? 'E2E OK' : 'E2E NG');
process.exit(ok ? 0 : 1);
