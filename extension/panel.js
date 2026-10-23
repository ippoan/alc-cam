// サイドパネル = 設定の入力・保存と状態の表示だけ。配信の本体は background.js (service worker) にあり、
// パネルを閉じても止まらない。やり取りは chrome.storage だけ:
//   設定 (local) をここで書く → background が storage.onChanged で拾って繋ぎ直す
//   状態 (session) を background が書く → ここで storage.onChanged を見て描く

import { DEFAULTS, STATUS_KEY, isComplete, loadSettings } from './lib/settings.js';

const $ = (id) => document.getElementById(id);
const form = $('settings');
const toggle = $('toggle');

function setStatus(id, [text, cls]) {
  const el = $(id);
  el.textContent = text;
  el.className = cls;
}

function fillForm(s) {
  for (const [k, v] of Object.entries(s)) {
    const el = form.elements.namedItem(k);
    if (el) el.value = v;
  }
}

function readForm() {
  const fd = new FormData(form);
  return {
    signalingUrl: fd.get('signalingUrl').trim().replace(/\/+$/, ''),
    siteId: fd.get('siteId').trim(),
    rtspUrl: fd.get('rtspUrl').trim(),
    stun: fd.get('stun').trim(),
  };
}

function renderStatus(st) {
  if (!st) return;
  setStatus('st-host', st.host);
  setStatus('st-signal', st.signal);
  setStatus('st-camera', st.camera);
  const vs = st.viewers;
  setStatus('st-viewer', [vs.length ? `${vs.length} 人` : 'なし', vs.length ? 'ok' : '']);
  if (!vs.length) return setStatus('st-peer', ['-', '']);
  const ok = vs.filter((v) => v.ok).length;
  const other = vs.filter((v) => !v.ok).map((v) => v.message);
  const text = [ok ? `接続 (P2P) ${ok} 人` : '', ...new Set(other)].filter(Boolean).join(' / ');
  setStatus('st-peer', [text, ok === vs.length ? 'ok' : '']);
}

function renderToggle(enabled) {
  toggle.textContent = enabled ? '停止' : '開始';
  toggle.classList.toggle('running', enabled);
}

toggle.addEventListener('click', async () => {
  const { enabled } = await loadSettings();
  await chrome.storage.local.set({ enabled: !enabled });
});

form.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  await chrome.storage.local.set(readForm());
  $('saved').hidden = false;
  setTimeout(() => ($('saved').hidden = true), 1500);
});

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'session' && changes[STATUS_KEY]) renderStatus(changes[STATUS_KEY].newValue);
  if (area === 'local' && changes.enabled) renderToggle(changes.enabled.newValue ?? DEFAULTS.enabled);
});

const initial = await loadSettings();
fillForm({ signalingUrl: initial.signalingUrl, siteId: initial.siteId, rtspUrl: initial.rtspUrl, stun: initial.stun });
if (!isComplete(initial)) $('settings-box').open = true;
renderToggle(initial.enabled);
renderStatus((await chrome.storage.session.get(STATUS_KEY))[STATUS_KEY]);
