// 配信担当の順位の通しの確認 (e2e.mjs と同じ道具立てで、拠点 PC を 2 台にする):
//   同じカメラを 2 台の拡張 (+ それぞれの alc-cam-host) が担当し、/cameras/mine の rank が A = 1 (主)・B = 2。
//   1. A が配信し B は待機 (cam-room が 4011 で閉じる) 2. A を止めると B が入り直して引き継ぎ、見る人に映る
//   3. A を戻すと A が B を 4010 で切って配信に戻り、B は待機に回る (見る人には A から offer が来て映る)
//
//   cargo build -p alc-cam-host && cd test && npm install && npx playwright-core install chromium && node e2e-priority.mjs
//
// 待機からの入り直しは storage.local の standbyRetryMs で短くする (本番は 30〜60 秒)。

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
const SIG_PORT = 8792;
const SITE = 'prio-site';
const STANDBY_RETRY_MS = 1_500;
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

const CAM_USER = 'admin';
const CAM_PASS = 'prio-pass';
const camera = startFakeRtsp({ host: lanIp, port: 8555, user: CAM_USER, pass: CAM_PASS });
const entry = (rank) => ({ id: SITE, name: '入口', rank, rtsp: { host: lanIp, port: 8555, path: 'stream2', username: CAM_USER, password: CAM_PASS } });
const token = (sub) => `eyJhbGciOiJIUzI1NiJ9.${Buffer.from(JSON.stringify({ role: 'device-cam-host', sub })).toString('base64url')}.sig`;
const TOKEN_A = token('alarm:pc-a');
const TOKEN_B = token('alarm:pc-b');
const mine = { tokens: [TOKEN_A, TOKEN_B], cameras: [], byToken: { [TOKEN_A]: [entry(1)], [TOKEN_B]: [entry(2)] } };
const signaling = startSignaling({ port: SIG_PORT, mine });

/** 拡張 + host の 1 台ぶん (Chromium の profile と拡張の写しを分ける) */
async function launchPc(label, pcToken) {
  const userDataDir = await fs.mkdtemp(path.join(os.tmpdir(), `alc-cam-prio-${label}-`));
  await fs.mkdir(path.join(userDataDir, 'NativeMessagingHosts'));
  await fs.writeFile(
    path.join(userDataDir, 'NativeMessagingHosts', `${HOST_NAME}.json`),
    JSON.stringify({ name: HOST_NAME, description: 'alc-cam host (e2e)', path: HOST_BIN, type: 'stdio', allowed_origins: [`chrome-extension://${EXT_ID}/`] }),
  );
  const extDir = await fs.mkdtemp(path.join(os.tmpdir(), `alc-cam-prio-ext-${label}-`));
  await fs.cp(path.join(here, '../extension'), extDir, { recursive: true });
  const ctx = await chromium.launchPersistentContext(userDataDir, {
    channel: 'chromium',
    headless: false,
    args: ['--headless=new', `--disable-extensions-except=${extDir}`, `--load-extension=${extDir}`],
  });
  await ctx.route(/^https:\/\/alc\.ippoan\.org\//, (route) => route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>pwa</title>' }));
  const sw = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent('serviceworker'));
  sw.on('console', (m) => console.log(`[sw ${label}]`, m.text()));
  await waitFor(() => sw.evaluate(() => !!globalThis.chrome?.storage), 10_000, `${label}: chrome.storage`);
  await sw.evaluate((s) => chrome.storage.local.set(s), {
    signalingUrl: `ws://127.0.0.1:${SIG_PORT}`,
    apiUrl: `http://127.0.0.1:${SIG_PORT}`,
    standbyRetryMs: STANDBY_RETRY_MS,
  });
  const pwa = await ctx.newPage();
  await pwa.goto('https://alc.ippoan.org/tenko');
  const send = (msg) => pwa.evaluate(([id, m]) => chrome.runtime.sendMessage(id, m), [EXT_ID, msg]);
  return {
    ctx,
    send,
    giveToken: () => send({ type: 'cam_host_token', token: pcToken, exp: Math.floor(Date.now() / 1000) + 900 }),
    status: async () => (await send({ type: 'cam_status' })).status,
  };
}

const results = {};
const viewerBrowser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome', headless: true });
let a;
let b;
try {
  a = await launchPc('a', TOKEN_A);
  b = await launchPc('b', TOKEN_B);
  // A が先に鍵を受けて入り、B は後から来て待機になる
  await a.giveToken();
  await waitFor(() => signaling.deviceTokens.some((d) => d.token === TOKEN_A && d.rank === 1), 15_000, 'A (1 位) が入る');
  await b.giveToken();
  results.standby = await waitFor(async () => {
    const st = await b.status();
    return st?.standby ? st : null;
  }, 15_000, 'B (2 位) が待機');
  results.primary = await a.status();

  // 見る人 (A から映る)
  const viewer = await viewerBrowser.newPage();
  await viewer.goto(`http://localhost:${SIG_PORT}/viewer.html?site=${SITE}`);
  const offers = () => viewer.evaluate(() => window.camState.offers);
  const decoded = () => viewer.evaluate(() => window.camState.framesDecoded ?? 0);
  // offer が n 本以上届き、その PeerConnection で frames 本以上デコードした
  const flowsAfter = (n, frames, label) =>
    waitFor(async () => ((await offers()) >= n && (await decoded()) > frames ? viewer.evaluate(() => ({ ...window.camState })) : null), 30_000, label);
  results.viewA = await flowsAfter(1, 15, 'A の映像');
  // 待機の B は入り直しても弾かれ続け、見る人の接続は揺れない
  await sleep(STANDBY_RETRY_MS * 3);
  results.standbyRetries = signaling.log.filter((l) => l === 'device standby (rank 2)').length;
  results.offersWhileStandby = await offers();

  // 1 位が落ちる (停止) → 2 位が入り直して引き継ぐ
  await a.send({ type: 'cam_control', action: 'stop' });
  await waitFor(() => signaling.deviceTokens.some((d) => d.token === TOKEN_B && d.rank === 2), 15_000, 'B が引き継いで入る');
  results.viewB = await flowsAfter(2, 15, 'B の映像');
  results.takeover = await waitFor(async () => {
    const st = await b.status();
    return !st.standby && st.signal[0] === '接続済み' ? st : null;
  }, 10_000, 'B の状態が配信中');

  // 1 位が戻る → B を 4010 で切って A に戻る
  const joinsA = signaling.deviceTokens.filter((d) => d.token === TOKEN_A).length;
  await a.send({ type: 'cam_control', action: 'start' });
  await waitFor(() => signaling.deviceTokens.filter((d) => d.token === TOKEN_A).length > joinsA, 15_000, 'A が戻る');
  results.viewBack = await flowsAfter(3, 15, 'A に戻った映像');
  results.backToStandby = await waitFor(async () => {
    const st = await b.status();
    return st?.standby && st.signal[0].includes('交代') ? st : null;
  }, 10_000, 'B が待機に戻る');
  results.primaryAgain = await a.status();
  results.signaling = signaling.log;
} catch (e) {
  results.error = String(e);
} finally {
  await a?.ctx.close();
  await b?.ctx.close();
  await viewerBrowser.close();
  camera.close();
  signaling.close();
}

console.log(JSON.stringify(results, null, 1));
const checks = results.error ? {} : {
  // 1 位が配信、2 位は待機 (状態に順位と待機が出る)
  primaryStreams: results.primary.rank === 1 && results.primary.standby === false && results.primary.signal[0] === '接続済み',
  secondStandsBy: results.standby.rank === 2 && results.standby.standby === true && results.standby.signal[0].startsWith('待機中 (上位の端末が配信中)'),
  viewsPrimary: results.viewA.framesDecoded > 15,
  // 待機は間を空けて入り直すが、見る人の接続は揺らさない
  standbyRetriesQuietly: results.standbyRetries >= 2 && results.offersWhileStandby === 1,
  // 1 位が落ちると 2 位が引き継ぎ、見る人に映る
  secondTakesOver: results.takeover.rank === 2 && results.viewB.offers >= 2 && results.viewB.framesDecoded > 15,
  // 1 位が戻ると 1 位に戻り、2 位は待機 (交代)
  backToPrimary: results.signaling.includes('device preempted') && results.viewBack.offers >= 3 && results.viewBack.framesDecoded > 15 &&
    results.primaryAgain.standby === false && results.backToStandby.rank === 2,
};
console.log(JSON.stringify(checks, null, 1));
const ok = !results.error && Object.values(checks).every(Boolean);
console.log(ok ? 'E2E PRIORITY OK' : 'E2E PRIORITY NG');
process.exit(ok ? 0 : 1);
