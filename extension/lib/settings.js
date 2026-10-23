// 設定 (chrome.storage.local) と状態 (chrome.storage.session) の置き場。background と panel で共有する。
// 設定はパネルが書き、background が storage.onChanged で拾って繋ぎ直す。
// 状態は background が書き、パネルが読む (service worker が止まっても、パネルを開き直しても残る)。

export const DEFAULTS = {
  signalingUrl: 'wss://alc-signaling.ippoan.org',
  siteId: '',
  rtspUrl: '',
  stun: 'stun.l.google.com:19302',
  // パネルの「停止」で false。既定は配信する (設定が揃っていれば Chrome の起動と同時に繋ぐ)
  enabled: true,
};

export const STATUS_KEY = 'status';

export async function loadSettings() {
  return { ...DEFAULTS, ...(await chrome.storage.local.get(Object.keys(DEFAULTS))) };
}

export const isComplete = (s) => !!(s.signalingUrl && s.siteId && s.rtspUrl);
