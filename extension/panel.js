// サイドパネル = 状態の表示と「開始/停止」だけ (拠点とカメラは DB から決まるので入力欄は無い)。
// 配信の本体は background.js (service worker) にあり、パネルを閉じても止まらない。やり取りは chrome.storage だけ:
//   「停止」(local の enabled) をここで書く → background が storage.onChanged で拾って繋ぎ直す
//   状態 (session) を background が書く → ここで storage.onChanged を見て描く

import { DEFAULTS, STATUS_KEY, loadSettings, setEnabled } from './lib/settings.js';

const $ = (id) => document.getElementById(id);
const toggle = $('toggle');

function setStatus(id, [text, cls]) {
  const el = $(id);
  el.textContent = text;
  el.className = cls;
}

/** 鍵の期限 → 表示 (切れても配信は写しで続く) */
function tokenLabel(exp, now = Date.now()) {
  if (!exp) return ['なし (点呼の画面を開くと届きます)', 'warn'];
  const at = new Date(exp).toLocaleTimeString('ja-JP', { hour: '2-digit', minute: '2-digit' });
  return exp > now ? [`あり (${at} まで)`, 'ok'] : [`期限切れ (${at})。配信は続けます`, ''];
}

/** 配信の順位と待機 (順位を知らなければ '-') */
function rankLabel(st) {
  if (!st.rank) return ['-', ''];
  const head = `${st.rank} 位${st.rank === 1 ? ' (主)' : ''}`;
  return st.standby ? [`${head}・待機中 (上位の端末が配信中)`, ''] : [head, 'ok'];
}

let lastStatus = null;
function renderStatus(st) {
  if (!st) return;
  lastStatus = st;
  setStatus('st-token', tokenLabel(st.tokenExp));
  if (st.config) setStatus('st-config', st.config);
  setStatus('st-rank', rankLabel(st));
  setStatus('st-host', st.host);
  setStatus('st-signal', st.signal);
  setStatus('st-camera', st.camera);
  if (st.update) setStatus('st-update', st.update);
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
  await setEnabled(!enabled);
});

$('update-check').addEventListener('click', () => chrome.runtime.sendMessage({ type: 'update_check' }));

// 期限切れの表示を進める
setInterval(() => lastStatus && setStatus('st-token', tokenLabel(lastStatus.tokenExp)), 30_000);

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'session' && changes[STATUS_KEY]) renderStatus(changes[STATUS_KEY].newValue);
  if (area === 'local' && changes.enabled) renderToggle(changes.enabled.newValue ?? DEFAULTS.enabled);
});

const initial = await loadSettings();
renderToggle(initial.enabled);
renderStatus((await chrome.storage.session.get(STATUS_KEY))[STATUS_KEY]);
