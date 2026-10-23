// サイドパネル = 拠点側の中継の本体。開いている間だけ動く。
//
//   カメラ ─RTSP─▶ alc-cam-host (Rust, native messaging) ─WebRTC (P2P)─▶ 見る側 (alc-app の TenkoCameraView)
//                          ▲ stdin/stdout                                  ▲
//                          └── このページ ─WebSocket─ /cam-room/<拠点ID> ──┘ (SDP だけ)
//
// 拠点側 (device) の振る舞いは alc-gw の docs/camera-relay-convention.md のとおり:
// 見る側が入ってきたら (peer_joined) host が offer を作り (non-trickle)、出ていったら (peer_left) 畳む。
// cam-room には proto=2 で入り、見る人を複数 (DO が付ける viewer_id で区別) 同時に持つ。
// v1 の部屋 (viewer_id が来ない) では viewer_id = '' の 1 人として同じ道を通る。

import { Host } from './lib/host.js';
import { CamRoom } from './lib/signaling.js';

const DEFAULTS = {
  signalingUrl: 'wss://alc-signaling.ippoan.org',
  siteId: '',
  rtspUrl: '',
  stun: 'stun.l.google.com:19302',
  autoStart: false,
};

const $ = (id) => document.getElementById(id);
const form = $('settings');

function setStatus(id, text, cls = '') {
  const el = $(id);
  el.textContent = text;
  el.className = cls;
}

async function loadSettings() {
  return { ...DEFAULTS, ...(await chrome.storage.local.get(Object.keys(DEFAULTS))) };
}

function fillForm(s) {
  for (const [k, v] of Object.entries(s)) {
    const el = form.elements.namedItem(k);
    if (!el) continue;
    if (el.type === 'checkbox') el.checked = v;
    else el.value = v;
  }
}

function readForm() {
  const fd = new FormData(form);
  return {
    signalingUrl: fd.get('signalingUrl').trim().replace(/\/+$/, ''),
    siteId: fd.get('siteId').trim(),
    rtspUrl: fd.get('rtspUrl').trim(),
    stun: fd.get('stun').trim(),
    autoStart: fd.get('autoStart') === 'on',
  };
}

export function roomUrl(s) {
  const url = new URL(`${s.signalingUrl}/cam-room/${encodeURIComponent(s.siteId)}`);
  url.searchParams.set('role', 'device');
  url.searchParams.set('proto', '2');
  return url.toString();
}

// ---- 中継 -----------------------------------------------------------------

class Relay {
  constructor(s) {
    /** viewer_id → 映像の状態 {ok, message} */
    this.viewers = new Map();

    this.host = new Host({
      config: { rtsp_url: s.rtspUrl, stun: s.stun },
      onMessage: (msg) => this.onHost(msg),
      onDown: (reason) => {
        setStatus('st-host', `停止: ${reason}`, 'warn');
        for (const v of this.viewers.values()) Object.assign(v, { ok: false, message: 'host 停止' });
        this.renderViewers();
      },
    });

    this.room = new CamRoom({
      url: roomUrl(s),
      onState: (state, detail) => {
        const label = { connecting: '接続中…', open: '接続済み', closed: '切断' }[state];
        setStatus('st-signal', detail ? `${label} (${detail})` : label, state === 'open' ? 'ok' : state === 'closed' ? 'warn' : '');
      },
      onMessage: (msg) => this.onRoom(msg),
    });
  }

  start() {
    this.host.start();
    this.room.start();
  }

  onRoom(msg) {
    const viewerId = msg.viewer_id ?? '';
    switch (msg.type) {
      case 'peer_joined':
        if (msg.role !== 'admin') return;
        this.viewers.set(viewerId, { ok: false, message: '接続待ち' });
        this.renderViewers();
        this.host.send({ type: 'start', viewer_id: viewerId });
        break;
      case 'peer_left':
        if (msg.role !== 'admin' || !this.viewers.delete(viewerId)) return;
        this.renderViewers();
        this.host.send({ type: 'stop', viewer_id: viewerId });
        break;
      case 'room_closed':
        // cam-room の WebSocket が切れた = 見る人は全員居なくなったものとして畳む
        for (const id of this.viewers.keys()) this.host.send({ type: 'stop', viewer_id: id });
        this.viewers.clear();
        this.renderViewers();
        break;
      case 'sdp_answer':
        this.host.send({ type: 'sdp_answer', viewer_id: viewerId, sdp: msg.sdp });
        break;
      case 'ice_candidate':
        this.host.send({ type: 'ice_candidate', viewer_id: viewerId, candidate: msg.candidate });
        break;
      case 'error':
        console.warn('cam-room:', msg.message);
        break;
    }
  }

  onHost(msg) {
    switch (msg.type) {
      case 'ready':
        setStatus('st-host', `起動 (v${msg.version})`, 'ok');
        // host が落ちて起動し直したとき、見る人がまだ居れば offer を作り直させる
        for (const id of this.viewers.keys()) this.host.send({ type: 'start', viewer_id: id });
        break;
      case 'sdp_offer':
        // viewer_id '' は v1 の部屋 (DO が宛先を 1 人と決めている)
        this.room.send(msg.viewer_id ? { type: 'sdp_offer', viewer_id: msg.viewer_id, sdp: msg.sdp } : { type: 'sdp_offer', sdp: msg.sdp });
        break;
      case 'camera':
        setStatus('st-camera', msg.message, msg.ok ? 'ok' : 'warn');
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
    }
  }

  renderViewers() {
    const vs = [...this.viewers.values()];
    setStatus('st-viewer', vs.length ? `${vs.length} 人` : 'なし', vs.length ? 'ok' : '');
    if (!vs.length) return setStatus('st-peer', '-');
    const ok = vs.filter((v) => v.ok).length;
    const other = vs.filter((v) => !v.ok).map((v) => v.message);
    const text = [ok ? `接続 (P2P) ${ok} 人` : '', ...new Set(other)].filter(Boolean).join(' / ');
    setStatus('st-peer', text, ok === vs.length ? 'ok' : '');
  }

  stop() {
    this.room.stop();
    this.host.stop();
    setStatus('st-host', '停止');
    setStatus('st-viewer', 'なし');
    setStatus('st-camera', '-');
    setStatus('st-peer', '-');
  }
}

// ---- UI --------------------------------------------------------------------

let relay = null;
const toggle = $('toggle');

function renderToggle() {
  toggle.textContent = relay ? '停止' : '開始';
  toggle.classList.toggle('running', !!relay);
}

async function startRelay() {
  const s = await loadSettings();
  if (!s.signalingUrl || !s.siteId || !s.rtspUrl) {
    $('settings-box').open = true;
    setStatus('st-host', '設定を入れてください', 'warn');
    return;
  }
  relay = new Relay(s);
  relay.start();
  renderToggle();
}

function stopRelay() {
  relay?.stop();
  relay = null;
  renderToggle();
}

toggle.addEventListener('click', () => (relay ? stopRelay() : startRelay()));

form.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  await chrome.storage.local.set(readForm());
  $('saved').hidden = false;
  setTimeout(() => ($('saved').hidden = true), 1500);
  if (relay) {
    stopRelay();
    await startRelay();
  }
});

window.addEventListener('pagehide', () => relay?.stop());

const initial = await loadSettings();
fillForm(initial);
if (!initial.siteId || !initial.rtspUrl) $('settings-box').open = true;
renderToggle();
if (initial.autoStart) startRelay();
