// 拠点側の中継の本体 (MV3 の service worker)。パネルを開いていなくても、Chrome が動いている間ずっと配信する。
//
//   カメラ ─RTSP─▶ alc-cam-host (Rust, native messaging) ─WebRTC (P2P)─▶ 見る側 (alc-app の TenkoCameraView)
//                          ▲ stdin/stdout                                  ▲
//                          └── この worker ─WebSocket─ /cam-room/<拠点ID> ──┘ (SDP だけ)
//
// 拠点側 (device) の振る舞いは alc-gw の docs/camera-relay-convention.md のとおり:
// 見る側が入ってきたら (peer_joined) host が offer を作り (non-trickle)、出ていったら (peer_left) 畳む。
// cam-room には proto=2 で入り、見る人を複数 (DO が付ける viewer_id で区別) 同時に持つ。
// v1 の部屋 (viewer_id が来ない) では viewer_id = '' の 1 人として同じ道を通る。
//
// 生かし方: cam-room への 20 秒ごとの keepalive (lib/signaling.js) が worker のアイドル期限を延ばす。
// それでも止まったとき (Chrome の都合・更新) は、状態をメモリに頼らず設定から繋ぎ直す:
// worker が起きるたびに top level で sync() し、起こす口は onStartup / onInstalled / 30 秒ごとの alarm / 設定の変更。
//
// 何をどこへ配るか (拠点 ID = 担当カメラの id、RTSP URL) は手で入れない: 点呼の PWA (alc.ippoan.org) が
// chrome.runtime.sendMessage で渡す鍵 (role=device-cam-host、TTL 900 秒) で担当カメラを引く (lib/config.js)。
// 鍵はメモリにだけ持ち、cam-room に入るときに ?token= で付ける (鍵の更新では今の接続を切らず、次に繋ぐときに使う)。
// 担当カメラの写し (パスワード以外) は storage.local、パスワードは storage.session (メモリ) に置くので、
// 鍵が切れても・service worker が止まっても配信は続く。v0.3.0 の手入力の設定は DB から引けるまでの間だけ使い、引けたら消す。
//
// 自己更新: 6 時間ごと (最初は 10 分後) とパネルの「今すぐ確認」で host に update_check を送る。
// host が動いていなければ (見る人なし) 確認のためだけに起動し、答えが来たら切る。host は自分の exe (次の起動から) と、
// この拡張のフォルダ (%LOCALAPPDATA%\alc-cam\extension、見る人が居ない時だけ) を Release の版に入れ替える。
// 拡張はディスクの manifest.json の版が動いている版と違えば chrome.runtime.reload() で読み込み直す。

import { HOST_NAME, Host } from './lib/host.js';
import { CamRoom } from './lib/signaling.js';
import { fetchMine, parseTokenMessage, rtspUrl } from './lib/config.js';
import { LEGACY_KEYS, PASSWORD_KEY, STATUS_KEY, loadSettings } from './lib/settings.js';

const ALARM = 'alc-cam-watchdog';
// 最後の見る人が離れてから host を止めるまで。stop を送った直後に port を切ると、
// host が TEARDOWN を書く前に stdout が閉じて終わることがあるので少し待つ (入り直しの起動し直しも省ける)
const HOST_IDLE_MS = 5_000;
const UPDATE_ALARM = 'alc-cam-host-update';
const UPDATE_FIRST_MIN = 10;
const UPDATE_PERIOD_MIN = 6 * 60;
// 確認のためだけに起動した host を諦めるまで (exe のダウンロード込み)
const UPDATE_TIMEOUT_MS = 10 * 60_000;
const UPDATE_LEGACY_MS = 30_000;

chrome.sidePanel
  .setPanelBehavior({ openPanelOnActionClick: true })
  .catch((e) => console.error('sidePanel.setPanelBehavior', e));

export function roomUrl(s, token) {
  const url = new URL(`${s.signalingUrl}/cam-room/${encodeURIComponent(s.siteId)}`);
  url.searchParams.set('role', 'device');
  url.searchParams.set('proto', '2');
  if (token) url.searchParams.set('token', token);
  return url.toString();
}

// ---- 鍵 (メモリだけ) --------------------------------------------------------

/** @type {null | {value: string, exp: number}} */
let token = null;
const liveToken = () => (token && token.exp > Date.now() ? token.value : '');

// ---- 状態 (パネルが storage.session を読む) -------------------------------

const status = {
  /** 鍵の期限 (ミリ秒)。鍵そのものは書かない */
  tokenExp: 0,
  config: ['-', ''],
  signal: ['停止', ''],
  host: ['停止', ''],
  camera: ['-', ''],
  /** @type {{ok: boolean, message: string}[]} */
  viewers: [],
  update: ['-', ''],
};

function report(patch) {
  Object.assign(status, patch);
  chrome.storage.session.set({ [STATUS_KEY]: status }).catch((e) => console.warn('storage.session', e));
}

// ---- 中継 -----------------------------------------------------------------

class Relay {
  constructor(s) {
    this.settings = s;
    /** viewer_id → 映像の状態 {ok, message} */
    this.viewers = new Map();
    this.host = null;
    this.hostReady = false;
    this.idleTimer = null;

    this.room = new CamRoom({
      url: () => roomUrl(s, liveToken()),
      onState: (state, detail) => {
        const label = { connecting: '接続中…', open: '接続済み', closed: '切断' }[state];
        report({ signal: [detail ? `${label} (${detail})` : label, state === 'open' ? 'ok' : state === 'closed' ? 'warn' : ''] });
      },
      onMessage: (msg) => this.onRoom(msg),
    });
  }

  start() {
    report({ host: ['停止 (見る人なし)', ''], camera: ['-', ''], viewers: [] });
    this.room.start();
  }

  // host は見る人が居る間だけ動かす
  ensureHost() {
    clearTimeout(this.idleTimer);
    if (this.host) return;
    this.hostReady = false;
    report({ host: ['起動中…', ''] });
    this.host = new Host({
      config: { rtsp_url: this.settings.rtspUrl, stun: this.settings.stun },
      onMessage: (msg) => this.onHost(msg),
      onDown: (reason) => {
        this.hostReady = false;
        for (const v of this.viewers.values()) Object.assign(v, { ok: false, message: 'host 停止' });
        report({ host: [`停止: ${reason}`, 'warn'] });
        this.renderViewers();
      },
    });
    this.host.start();
  }

  idleHost() {
    if (this.viewers.size) return;
    clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => {
      if (this.viewers.size) return;
      this.stopHost();
      report({ host: ['停止 (見る人なし)', ''], camera: ['-', ''] });
    }, HOST_IDLE_MS);
  }

  stopHost() {
    clearTimeout(this.idleTimer);
    this.host?.stop();
    this.host = null;
    this.hostReady = false;
  }

  toHost(msg) {
    if (this.hostReady) this.host.send(msg);
  }

  onRoom(msg) {
    const viewerId = msg.viewer_id ?? '';
    switch (msg.type) {
      case 'peer_joined':
        if (msg.role !== 'admin') return;
        this.viewers.set(viewerId, { ok: false, message: '接続待ち' });
        this.renderViewers();
        // 起動中なら ready を受けたところで居る全員ぶん start を送る
        this.ensureHost();
        this.toHost({ type: 'start', viewer_id: viewerId });
        break;
      case 'peer_left':
        if (msg.role !== 'admin' || !this.viewers.delete(viewerId)) return;
        this.renderViewers();
        this.toHost({ type: 'stop', viewer_id: viewerId });
        this.idleHost();
        break;
      case 'room_closed':
        // cam-room の WebSocket が切れた = 見る人は全員居なくなったものとして畳む
        for (const id of this.viewers.keys()) this.toHost({ type: 'stop', viewer_id: id });
        this.viewers.clear();
        this.renderViewers();
        this.idleHost();
        break;
      case 'sdp_answer':
        this.toHost({ type: 'sdp_answer', viewer_id: viewerId, sdp: msg.sdp });
        break;
      case 'ice_candidate':
        this.toHost({ type: 'ice_candidate', viewer_id: viewerId, candidate: msg.candidate });
        break;
      case 'error':
        console.warn('cam-room:', msg.message);
        break;
    }
  }

  onHost(msg) {
    switch (msg.type) {
      case 'ready':
        this.hostReady = true;
        report({ host: [`起動 (v${msg.version})`, 'ok'] });
        // 起動直後・host が落ちて起動し直したとき、見る人がまだ居れば offer を作らせる
        for (const id of this.viewers.keys()) this.host.send({ type: 'start', viewer_id: id });
        this.idleHost();
        break;
      case 'sdp_offer':
        // viewer_id '' は v1 の部屋 (DO が宛先を 1 人と決めている)
        this.room.send(msg.viewer_id ? { type: 'sdp_offer', viewer_id: msg.viewer_id, sdp: msg.sdp } : { type: 'sdp_offer', sdp: msg.sdp });
        break;
      case 'camera':
        report({ camera: [msg.message, msg.ok ? 'ok' : 'warn'] });
        break;
      case 'peer': {
        const v = this.viewers.get(msg.viewer_id ?? '');
        if (!v) return;
        Object.assign(v, { ok: msg.ok, message: msg.message });
        this.renderViewers();
        break;
      }
      case 'log':
        console.log('[host]', msg.message);
        break;
      case 'update':
        reportUpdate(msg);
        break;
    }
  }

  renderViewers() {
    report({ viewers: [...this.viewers.values()].map((v) => ({ ...v })) });
  }

  stop() {
    this.room.stop();
    this.stopHost();
    report({ host: ['停止', ''], camera: ['-', ''], viewers: [] });
  }
}

// ---- host の自己更新 -------------------------------------------------------

/** host の `update` を表示の [文言, class] にする */
export function updateLabel(msg, now = new Date()) {
  const v = `拡張 v${chrome.runtime.getManifest().version}・host v${msg.current}`;
  const at = now.toLocaleString('ja-JP', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  switch (msg.state) {
    case 'checking':
      return [`${v}: 確認中…`, ''];
    case 'latest':
      return [`${v} (最新、${at} 確認)${msg.message ? `。${msg.message}` : ''}`, 'ok'];
    case 'installed':
      return [`${v}: ${msg.message}`, 'ok'];
    case 'deferred':
    case 'unsupported':
      return [`${v}: ${msg.message}`, ''];
    default:
      return [`${v}: 更新に失敗 (${msg.message})。今の版で動きます`, 'warn'];
  }
}

/** @returns {boolean} 確認が終わった (checking 以外) */
function reportUpdate(msg) {
  report({ update: updateLabel(msg) });
  if (msg.state === 'checking') return false;
  reloadIfUpdatedOnDisk();
  return true;
}

// host が拡張のフォルダを入れ替えていたら (ディスクの manifest.json の版が動いている版と違う) 読み込み直す。
// 見る人が居る間は待つ (host も見る人が居る間は入れ替えない。次の確認か worker の起き直しで拾う)
async function reloadIfUpdatedOnDisk() {
  try {
    const disk = await (await fetch(chrome.runtime.getURL('manifest.json'), { cache: 'no-store' })).json();
    if (disk.version === chrome.runtime.getManifest().version || relay?.viewers.size) return;
    console.log(`拡張を v${disk.version} に読み込み直します`);
    chrome.runtime.reload();
  } catch (e) {
    console.warn('manifest.json', e);
  }
}

function checkHostUpdate() {
  if (relay?.hostReady) return relay.host.send({ type: 'update_check' });
  // 見る人のための host が起動中: 次の回に回す
  if (relay?.host) return;
  const port = chrome.runtime.connectNative(HOST_NAME);
  const timer = setTimeout(() => {
    report({ update: ['更新の確認が終わりません (中断)', 'warn'] });
    port.disconnect();
  }, UPDATE_TIMEOUT_MS);
  // 自己更新を知らない古い host は update を返さない
  let heard = false;
  const legacy = setTimeout(() => {
    if (heard) return;
    clearTimeout(timer);
    report({ update: ['この host は自己更新に対応していません (install.ps1 で入れ直してください)', 'warn'] });
    port.disconnect();
  }, UPDATE_LEGACY_MS);
  port.onMessage.addListener((msg) => {
    if (msg.type !== 'update') return;
    heard = true;
    if (reportUpdate(msg)) {
      clearTimeout(timer);
      port.disconnect();
    }
  });
  port.onDisconnect.addListener(() => {
    clearTimeout(timer);
    clearTimeout(legacy);
    report({ update: [`更新を確認できません: ${chrome.runtime.lastError?.message ?? 'host が終了しました'}`, 'warn'] });
  });
  port.postMessage({ type: 'update_check' });
}

// ---- 設定から繋ぐ (何度呼んでもよい) -------------------------------------

let relay = null;
let current = '';
let chain = Promise.resolve();

// 担当カメラの取得の失敗 (写しのまま配信を続けるときの添え書き)
let configNote = '';

/** 配る先を決める。決まらなければ wait に理由 */
async function resolve() {
  const s = await loadSettings();
  const base = { enabled: s.enabled, signalingUrl: s.signalingUrl, stun: s.stun };
  if (s.camera) {
    const pw = (await chrome.storage.session.get(PASSWORD_KEY))[PASSWORD_KEY];
    if (pw === undefined) return { ...base, wait: `${s.camera.name}: パスワード待ち (点呼の画面から鍵が届くまで)` };
    return { ...base, name: s.camera.name, siteId: s.camera.id, rtspUrl: rtspUrl(s.camera.rtsp, pw) };
  }
  const legacy = await chrome.storage.local.get(LEGACY_KEYS);
  if (legacy.siteId && legacy.rtspUrl) return { ...base, name: `${legacy.siteId} (v0.3.0 の手入力。DB から引けたら切り替わります)`, siteId: legacy.siteId, rtspUrl: legacy.rtspUrl };
  return { ...base, wait: '担当カメラ待ち (点呼の画面を開くと届きます)' };
}

async function sync() {
  const s = await resolve();
  report({ tokenExp: token?.exp ?? 0, config: s.wait ? [configNote || s.wait, 'warn'] : [configNote ? `${s.name} (${configNote})` : s.name, configNote ? 'warn' : 'ok'] });
  const key = JSON.stringify(s);
  if (key === current) return;
  current = key;
  relay?.stop();
  relay = null;
  if (!s.enabled) return report({ signal: ['停止 (パネルで開始)', ''] });
  if (s.wait) return report({ signal: ['担当カメラ待ち', 'warn'] });
  relay = new Relay(s);
  relay.start();
}

const resync = () => (chain = chain.then(sync).catch((e) => console.error('sync', e)));

// 鍵で担当カメラを引き直す。変わっていれば resync が繋ぎ直す (同じなら今の接続のまま)
async function refreshConfig() {
  const t = liveToken();
  if (!t) return;
  const s = await loadSettings();
  try {
    const r = await fetchMine(s.apiUrl, t);
    configNote = r.camera ? '' : r.reason;
    if (r.camera) {
      // パスワードを先に置く (local の変更で動く resync が新旧の混ざった設定を読まないように)
      await chrome.storage.session.set({ [PASSWORD_KEY]: r.password });
      await chrome.storage.local.set({ camera: r.camera });
      await chrome.storage.local.remove(LEGACY_KEYS);
    } else {
      await chrome.storage.session.remove(PASSWORD_KEY);
      await chrome.storage.local.set({ camera: null });
    }
  } catch (e) {
    configNote = `${e.message}。前の設定のまま`;
  }
  resync();
}

// 点呼の PWA から鍵を受ける (externally_connectable で alc.ippoan.org だけ。送り主の origin も確かめる)
chrome.runtime.onMessageExternal.addListener((msg, sender, sendResponse) => {
  const t = parseTokenMessage(msg, sender);
  if (!t) return sendResponse({ ok: false });
  token = { value: t.token, exp: t.exp };
  sendResponse({ ok: true });
  refreshConfig().catch((e) => console.error('refreshConfig', e));
});

chrome.runtime.onStartup.addListener(resync);
chrome.runtime.onInstalled.addListener(resync);
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === ALARM) resync();
reloadIfUpdatedOnDisk();
  if (a.name === UPDATE_ALARM) checkHostUpdate();
});
chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.type === 'update_check') checkHostUpdate();
});
// local = 担当カメラの写し・「停止」。session (状態・パスワード) は見ない (パスワードは refreshConfig が resync する)
chrome.storage.onChanged.addListener((changes, area) => area === 'local' && resync());

// keepalive で生きている間は何もしない。worker が止まっていたらこの alarm が起こし、下の resync で繋ぎ直す
chrome.alarms.get(ALARM).then((a) => a || chrome.alarms.create(ALARM, { periodInMinutes: 0.5 }));
chrome.alarms
  .get(UPDATE_ALARM)
  .then((a) => a || chrome.alarms.create(UPDATE_ALARM, { delayInMinutes: UPDATE_FIRST_MIN, periodInMinutes: UPDATE_PERIOD_MIN }));
resync();
