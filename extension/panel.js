// サイドパネル = 拠点側の中継の本体。開いている間だけ動く。
//
//   カメラ ─RTSP─▶ alc-cam-host (Rust, native messaging) ─WebRTC (P2P)─▶ 見る側 (alc-app の TenkoCameraView)
//                          ▲ stdin/stdout                                  ▲
//                          └── このページ ─WebSocket─ /cam-room/<拠点ID> ──┘ (SDP だけ)
//
// 拠点側 (device) の振る舞いは alc-gw の docs/camera-relay-convention.md のとおり:
// 見る側が入ってきたら (peer_joined) host が offer を作り (non-trickle)、出ていったら (peer_left) 畳む。

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
  return url.toString();
}

// ---- 中継 -----------------------------------------------------------------

class Relay {
  constructor(s) {
    this.viewerPresent = false;

    this.host = new Host({
      config: { rtsp_url: s.rtspUrl, stun: s.stun },
      onMessage: (msg) => this.onHost(msg),
      onDown: (reason) => {
        setStatus('st-host', `停止: ${reason}`, 'warn');
        setStatus('st-peer', '-');
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
    switch (msg.type) {
      case 'peer_joined':
        if (msg.role !== 'admin') return;
        this.viewerPresent = true;
        setStatus('st-viewer', 'あり', 'ok');
        this.host.send({ type: 'start' });
        break;
      case 'peer_left':
        if (msg.role !== 'admin' || !this.viewerPresent) return;
        this.viewerPresent = false;
        setStatus('st-viewer', 'なし');
        this.host.send({ type: 'stop' });
        break;
      case 'sdp_answer':
        this.host.send({ type: 'sdp_answer', sdp: msg.sdp });
        break;
      case 'ice_candidate':
        this.host.send({ type: 'ice_candidate', candidate: msg.candidate });
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
        if (this.viewerPresent) this.host.send({ type: 'start' });
        break;
      case 'sdp_offer':
        this.room.send({ type: 'sdp_offer', sdp: msg.sdp });
        break;
      case 'camera':
        setStatus('st-camera', msg.message, msg.ok ? 'ok' : 'warn');
        break;
      case 'peer':
        setStatus('st-peer', msg.message, msg.ok ? 'ok' : '');
        break;
      case 'log':
        console.log('[host]', msg.message);
        break;
    }
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
