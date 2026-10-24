// 通しの動作確認:
//   偽カメラ (RTSP, Digest) → alc-cam-host (Rust, native messaging) ⇄ 拡張 (background service worker) ⇄ cam-room (ローカル版)
//   → 見る側 (viewer.html = alc-app の admin と同じ振る舞い) が WebRTC で受けてデコードする
// 拠点とカメラは手で入れない: 偽の点呼の PWA (https://alc.ippoan.org を Playwright の route で差し替えたページ) が
// 鍵を chrome.runtime.sendMessage で拡張に渡し、拡張が偽の /cameras/mine (signaling.mjs) から担当カメラを引く。
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
import { KEEPALIVE_PING } from '../extension/lib/signaling.js';

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

// URL に入れると壊れる文字を含むパスワード (拡張が URL エンコードし、host がデコードすること)
const CAM_USER = 'admin';
const CAM_PASS = 'p@ss:w/rd%';
const camera = startFakeRtsp({ host: lanIp, port: 8554, user: CAM_USER, pass: CAM_PASS });
// 首振りの CGI (本物は 80 番。host は ALC_CAM_HTTP_PORT を見る。Chromium から起動される host へ環境変数で渡る)
const cgi = startFakeCgi({ host: lanIp, port: CGI_PORT, user: CAM_USER, pass: CAM_PASS });
process.env.ALC_CAM_HTTP_PORT = String(CGI_PORT);
// 担当カメラ (DB の写し)。rtsp は ingest の /cameras/mine と同じ形
const camEntry = (id) => ({ id, name: `カメラ ${id}`, rtsp: { host: lanIp, port: 8554, path: 'stream2', username: CAM_USER, password: CAM_PASS } });
const mine = { tokens: [], cameras: [] };
const signaling = startSignaling({ port: SIG_PORT, v1Sites: ['v1-site'], mine });
// 鍵の見た目だけ JWT (拡張は形を確かめる。sub は点呼の画面の照合の表示用に読む)
const TOKEN_SUB = 'alarm:e2e';
const fakePayload = Buffer.from(JSON.stringify({ role: 'device-cam-host', sub: TOKEN_SUB })).toString('base64url');
const fakeToken = (n) => `eyJhbGciOiJIUzI1NiJ9.${fakePayload}.sig${n}`;

// native messaging host の登録 (Linux の Chromium は <user-data-dir>/NativeMessagingHosts も見る)
const userDataDir = await fs.mkdtemp(path.join(os.tmpdir(), 'alc-cam-e2e-'));
await fs.mkdir(path.join(userDataDir, 'NativeMessagingHosts'));
// 拠点と同じくデベロッパー モードを ON に (OFF だと chrome.runtime.reload() で手で読み込んだ拡張が無効になる)
await fs.mkdir(path.join(userDataDir, 'Default'));
await fs.writeFile(path.join(userDataDir, 'Default', 'Preferences'), JSON.stringify({ extensions: { ui: { developer_mode: true } } }));
await fs.writeFile(
  path.join(userDataDir, 'NativeMessagingHosts', `${HOST_NAME}.json`),
  JSON.stringify({ name: HOST_NAME, description: 'alc-cam host (e2e)', path: HOST_BIN, type: 'stdio', allowed_origins: [`chrome-extension://${EXT_ID}/`] }),
);

// 拡張は写しを読み込む (自己更新の確認で manifest.json の版を書き換えるため)
const extDir = await fs.mkdtemp(path.join(os.tmpdir(), 'alc-cam-e2e-ext-'));
await fs.cp(path.join(here, '../extension'), extDir, { recursive: true });
const ctx = await chromium.launchPersistentContext(userDataDir, {
  channel: 'chromium',
  headless: false,
  args: ['--headless=new', `--disable-extensions-except=${extDir}`, `--load-extension=${extDir}`],
});
// 偽の点呼の PWA と、許されていない origin のページ (どちらも中身は空。chrome.runtime が生えるかを見る)
await ctx.route(/^https:\/\/(alc\.ippoan\.org|evil\.example)\//, (route) =>
  route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>pwa</title>' }));
const viewerBrowser = await chromium.launch({ executablePath: process.env.CHROME ?? '/usr/bin/google-chrome', headless: true });

const results = {};
const deviceJoins = () => signaling.log.filter((l) => l.startsWith('device joined')).length;
try {
  const sw = ctx.serviceWorkers()[0] ?? (await ctx.waitForEvent('serviceworker'));
  results.extId = new URL(sw.url()).host;
  if (results.extId !== EXT_ID) throw new Error(`拡張 ID が manifest の key と合わない: ${results.extId}`);
  sw.on('console', (m) => console.log('[sw]', m.text()));

  // 繋ぎ先だけローカルに向ける (画面は無い上書き)。これだけでは拠点もカメラも決まらないので繋がない
  await waitFor(() => sw.evaluate(() => !!globalThis.chrome?.storage), 10_000, 'service worker の chrome.storage');
  await sw.evaluate((s) => chrome.storage.local.set(s), { signalingUrl: `ws://127.0.0.1:${SIG_PORT}`, apiUrl: `http://127.0.0.1:${SIG_PORT}` });
  await sleep(1500);
  results.noConfig = { joins: deviceJoins(), mineCalls: mine.calls.length };

  // v0.3.0 からの入れ替え: 手入力の設定 (storage の siteId / rtspUrl) が残っていれば、DB から引けるまではそれで配る
  await sw.evaluate((s) => chrome.storage.local.set(s), { siteId: 'legacy-site', rtspUrl: camera.url });
  await waitFor(() => signaling.deviceTokens.some((d) => d.site === 'legacy-site'), 10_000, '手入力の設定で cam-room 接続');

  // 点呼の PWA が鍵を渡す (PWA と同じく sendMessage(拡張 ID, ...))
  const pwa = await ctx.newPage();
  await pwa.goto('https://alc.ippoan.org/tenko');
  const sendToken = (n) =>
    pwa.evaluate(([id, token]) => chrome.runtime.sendMessage(id, { type: 'cam_host_token', token, exp: Math.floor(Date.now() / 1000) + 900 }), [EXT_ID, fakeToken(n)]);
  // 点呼の画面の「カメラ配信 (拡張)」と同じ呼び方
  const pwaSend = (msg) => pwa.evaluate(([id, m]) => chrome.runtime.sendMessage(id, m), [EXT_ID, msg]);
  const camStatus = async () => (await pwaSend({ type: 'cam_status' })).status;
  // 許されていない origin には chrome.runtime.sendMessage が生えない
  const evil = await ctx.newPage();
  await evil.goto('https://evil.example/');
  results.evilHasSendMessage = await evil.evaluate(() => typeof globalThis.chrome?.runtime?.sendMessage);
  await evil.close();

  // 担当カメラが 0 台: 理由を出して待つ (手入力の設定はまだ消さない)
  mine.tokens.push(fakeToken(1));
  results.token1Reply = await sendToken(1);
  await waitFor(() => mine.calls.includes(fakeToken(1)), 10_000, '/cameras/mine (0 台)');
  const swStatus = () => sw.evaluate(async () => (await chrome.storage.session.get('status')).status);
  results.zeroCameras = await waitFor(async () => ((await swStatus())?.config?.[0].includes('担当カメラがありません') ? (await swStatus()).config : null), 5_000, '0 台の理由');

  // 担当カメラが入った: 鍵を渡し直すと DB の設定に切り替わり、token 付きで cam-room に入る
  mine.cameras = [camEntry('test-site')];
  mine.tokens.push(fakeToken(2));
  await sendToken(2);
  await waitFor(() => signaling.deviceTokens.some((d) => d.site === 'test-site'), 10_000, '担当カメラの cam-room 接続');
  results.joinToken = signaling.deviceTokens.find((d) => d.site === 'test-site').token;
  results.stored = await sw.evaluate(async () => ({ local: await chrome.storage.local.get(null), session: Object.keys(await chrome.storage.session.get(null)) }));

  // 鍵の更新では今の接続を切らない
  const joinsAtRefresh = deviceJoins();
  const leftAtRefresh = signaling.log.filter((l) => l === 'device left').length;
  mine.tokens.push(fakeToken(3));
  await sendToken(3);
  await waitFor(() => mine.calls.includes(fakeToken(3)), 10_000, '/cameras/mine (鍵の更新)');
  await sleep(1500);
  results.refresh = { joins: deviceJoins() - joinsAtRefresh, left: signaling.log.filter((l) => l === 'device left').length - leftAtRefresh };

  // 点呼の画面から: 状態を読む (パネルと同じ中身。鍵の中身・パスワードは入らない)
  results.extStatus = await camStatus();
  // 担当カメラの取り直し (今の鍵で /cameras/mine を呼び直す。同じなら繋ぎ直さない)
  const mineBefore = mine.calls.length;
  results.extRefetch = await pwaSend({ type: 'cam_control', action: 'refetch' });
  results.extRefetchCalls = mine.calls.length - mineBefore;
  // 停止 → 開始
  const leftBeforeStop = signaling.log.filter((l) => l === 'device left').length;
  results.extStop = await pwaSend({ type: 'cam_control', action: 'stop' });
  await waitFor(() => signaling.log.filter((l) => l === 'device left').length > leftBeforeStop, 10_000, '点呼の画面の停止で cam-room から抜ける');
  results.extStoppedStatus = await camStatus();
  const joinsBeforeStart = deviceJoins();
  results.extStart = await pwaSend({ type: 'cam_control', action: 'start' });
  await waitFor(() => deviceJoins() > joinsBeforeStart, 10_000, '点呼の画面の開始で cam-room に入り直す');
  results.extUnknown = await pwaSend({ type: 'cam_control', action: 'rm -rf' });
  // パネル (拡張のページ) は開いていない
  results.beforeViewer = { ...camera.stats, hostRunning: hostRunning(), extPages: ctx.pages().filter((p) => p.url().startsWith('chrome-extension://')).length };

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

  // service worker を止めても (CDP)、alarm で起き直して担当カメラの写しから繋ぎ直し、C に再び映る
  // (鍵はメモリにしか無いので、繋ぎ直しは鍵無し。パスワードは storage.session に残っている)
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
  results.swRestartToken = signaling.deviceTokens.at(-1).token;
  await waitFor(async () => (await c.evaluate(() => window.camState.offers)) > offersBefore, 20_000, 'C へ offer し直し');
  await sleep(3000);
  results.afterSwRestart = { cDelta: await stillFlowing(c), camState: await c.evaluate(() => window.camState) };
  await c.close();
  await waitFor(() => !hostRunning(), 15_000, 'host 終了 (SW 再起動後)');

  // DB の担当カメラが変わったら (次に鍵が届いたとき) 繋ぎ直す: v1 の部屋 (proto を知らない古い DO) でも 1 人なら映る
  mine.cameras = [camEntry('v1-site')];
  mine.tokens.push(fakeToken(4));
  await sendToken(4);
  await waitFor(() => signaling.log.includes('device joined (v1)'), 10_000, 'cam-room (v1) 接続');
  results.v1Token = signaling.deviceTokens.at(-1).token;
  const d = await openViewer('v1-site');
  results.v1 = await decodes(d, 15, 'v1 の部屋のデコード');
  await d.close();
  await waitFor(() => !hostRunning(), 15_000, 'host 終了 (v1)');

  // 拡張のページから storage を書く (service worker は起き直して別物になっている)
  const setStorage = async (obj) => {
    const page = await ctx.newPage();
    await page.goto(`chrome-extension://${EXT_ID}/panel.html`);
    await page.evaluate((o) => chrome.storage.local.set(o), obj);
    await page.close();
  };

  // host の自己更新: 見る人が居なくても「今すぐ確認」で確認のためだけに host を起こし、答えが来たら止める
  // (Linux の host は入れ替えないので unsupported が返る。GitHub には出ない)
  const upd = await ctx.newPage();
  await upd.goto(`chrome-extension://${EXT_ID}/panel.html`);
  await upd.click('#update-check');
  results.update = await waitFor(async () => {
    const t = await upd.evaluate(() => document.getElementById('st-update').textContent);
    return t.includes('Windows だけ') ? t : null;
  }, 10_000, 'パネルに host の更新状況');
  await upd.close();
  await waitFor(() => !hostRunning(), 10_000, '確認のためだけの host が終わる');
  results.afterUpdateCheck = { hostRunning: hostRunning() };
  // 点呼の画面の「更新確認」も同じ確認 (結果と時刻が状態に出る)
  const updAt = (await camStatus()).updateAt;
  results.extUpdate = await pwaSend({ type: 'cam_control', action: 'update_check' });
  results.extUpdateStatus = await waitFor(async () => {
    const st = await camStatus();
    return st.updateAt > updAt && st.updateState !== 'checking' ? st : null;
  }, 10_000, '点呼の画面の更新確認');
  await waitFor(() => !hostRunning(), 10_000, '確認のためだけの host が終わる (点呼の画面から)');

  // host が拡張のフォルダを入れ替えた後の形: ディスクの manifest.json の版が上がっていれば、確認の答えで読み込み直す
  const manifestPath = path.join(extDir, 'manifest.json');
  const m = JSON.parse(await fs.readFile(manifestPath, 'utf8'));
  await fs.writeFile(manifestPath, JSON.stringify({ ...m, version: '99.0.0' }));
  const upd2 = await ctx.newPage();
  await upd2.goto(`chrome-extension://${EXT_ID}/panel.html`);
  const joins = deviceJoins();
  await upd2.click('#update-check');
  // 読み込み直すと拡張のページは閉じる。開き直して動いている版を見る
  results.reloadedVersion = await waitFor(async () => {
    const p = await ctx.newPage();
    try {
      await p.goto(`chrome-extension://${EXT_ID}/panel.html`);
      const v = await p.evaluate(() => chrome.runtime.getManifest().version);
      return v === '99.0.0' ? v : null;
    } catch {
      return null;
    } finally {
      await p.close().catch(() => {});
    }
  }, 15_000, '拡張の読み込み直し');
  // 読み込み直すと storage.session (パスワード) も鍵も消える: 次の鍵が届くまでパスワード待ちで止まり、届いたら繋ぎ直す
  await sleep(2000);
  results.afterReload = { joins: deviceJoins() - joins, stillHasCamera: await (async () => {
    const p = await ctx.newPage();
    await p.goto(`chrome-extension://${EXT_ID}/panel.html`);
    const st = await p.evaluate(async () => ({ camera: (await chrome.storage.local.get('camera')).camera?.id, config: document.getElementById('st-config').textContent }));
    await p.close();
    return st;
  })() };
  mine.tokens.push(fakeToken(5));
  await sendToken(5);
  await waitFor(() => deviceJoins() > joins, 15_000, '読み込み直し後、鍵が届いて再接続');
  results.afterReloadToken = signaling.deviceTokens.at(-1).token;
  await pwa.close();

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
  cgi.close();
  signaling.close();
}

console.log(JSON.stringify(results, null, 1));
const checks = results.error ? {} : {
  // 拠点もカメラも決まらないうちは繋がない
  waitsWithoutConfig: results.noConfig.joins === 0 && results.noConfig.mineCalls === 0,
  // 点呼の画面から状態が読め、版・担当カメラ・鍵の sub が出る。鍵の中身・パスワードは出ない
  extStatus: (() => {
    const st = results.extStatus;
    const json = JSON.stringify(st);
    return st?.version === '0.4.3' && st.enabled === true && st.tokenLive === true && st.tokenSub === TOKEN_SUB &&
      st.assigned?.id === 'test-site' && st.signal?.[0] === '接続済み' && !json.includes(CAM_PASS) && !json.includes('eyJ') && !json.includes('rtsp://');
  })(),
  // 取り直しは /cameras/mine を 1 回呼び、同じカメラなら繋ぎ直さない
  extRefetch: results.extRefetch?.ok === true && results.extRefetchCalls === 1,
  extStopStart: results.extStop?.ok === true && results.extStoppedStatus?.enabled === false && results.extStart?.ok === true &&
    results.extStart.status?.enabled === true,
  extUnknownRejected: results.extUnknown?.ok === false,
  extUpdateCheck: results.extUpdate?.ok === true && results.extUpdateStatus?.updateState === 'unsupported' && results.extUpdateStatus.hostVersion === '0.4.3',
  // 鍵は alc.ippoan.org からだけ受ける (他の origin には chrome.runtime.sendMessage 自体が生えない = 状態も操作も届かない)
  pwaOnly: results.evilHasSendMessage === 'undefined' && results.token1Reply?.ok === true,
  zeroCamerasReason: !!results.zeroCameras,
  // 担当カメラの id の部屋に、受けた鍵を付けて入る
  joinsWithToken: results.joinToken === fakeToken(2),
  // ディスク (local) にはパスワードも鍵も手入力の rtspUrl も残らない。パスワードは session (メモリ) だけ
  noSecretOnDisk: (() => {
    const disk = JSON.stringify(results.stored.local);
    return !disk.includes(CAM_PASS) && !disk.includes('eyJ') && !('rtspUrl' in results.stored.local) && !('siteId' in results.stored.local) &&
      results.stored.local.camera?.id === 'test-site' && results.stored.session.includes('rtspPassword');
  })(),
  // 鍵の更新では繋ぎ直さない
  keepsConnectionOnRefresh: results.refresh.joins === 0 && results.refresh.left === 0,
  // 鍵が無くても (service worker の起き直し) 写しで繋ぎ直す。次の鍵では新しい鍵で入る
  reconnectsWithoutToken: results.swRestartToken === '' && results.v1Token === fakeToken(4),
  // パネルを開かずに cam-room に繋ぎ、見る人が来るまでカメラにも host にも触らない
  startsWithoutPanel: results.beforeViewer.extPages === 0 && results.beforeViewer.sessions === 0 && !results.beforeViewer.hostRunning,
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
  // 更新の確認は host の版を添えてパネルに出て、確認のためだけの host は残らない
  updateCheck: results.update?.includes('host v0.4.3') && results.afterUpdateCheck?.hostRunning === false,
  // ディスクの拡張が新しくなっていたら読み込み直し、配信に戻る
  reloadsWhenUpdatedOnDisk: results.reloadedVersion === '99.0.0',
  // 読み込み直しの後はパスワード待ち (写しのカメラ id は残る)、鍵が届けば繋ぎ直す
  waitsPasswordAfterReload: results.afterReload.joins === 0 && results.afterReload.stillHasCamera.camera === 'v1-site' &&
    results.afterReload.stillHasCamera.config.includes('パスワード待ち') && results.afterReloadToken === fakeToken(5),
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
