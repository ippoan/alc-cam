// 設定 (chrome.storage.local) と状態 (chrome.storage.session) の置き場。background と panel で共有する。
// 拠点 ID と RTSP の接続先は手で入れない: 点呼の PWA から受けた鍵で DB (担当カメラ) から引く (lib/config.js)。
// local に置くのは繋ぎ先の既定値の上書き (画面は無い。e2e と開発用)・パネルの「停止」・担当カメラの写し (パスワード以外)。
// RTSP のパスワードは session (メモリだけ、ディスクに書かない) に置き、service worker が止まっても Chrome が動いている間は残す。
// 鍵 (device JWT) はどこにも書かない (background のメモリだけ)。
// 状態は background が書き、パネルが読む (service worker が止まっても、パネルを開き直しても残る)。

export const DEFAULTS = {
  signalingUrl: 'wss://alc-signaling.ippoan.org',
  // auth-worker。担当カメラは `${apiUrl}/device-data-proxy/api/cam-clips/cameras/mine` (device JWT の口)
  apiUrl: 'https://auth.ippoan.org',
  stun: 'stun.l.google.com:19302',
  // パネルの「停止」で false。既定は配信する (担当カメラが決まっていれば Chrome の起動と同時に繋ぐ)
  enabled: true,
  /** @type {null | {id: string, name: string, rtsp: {host: string, port: number, path: string, username: string}}} */
  camera: null,
};

export const STATUS_KEY = 'status';
export const PASSWORD_KEY = 'rtspPassword';
// v0.3.0 までの手入力 (rtspUrl はパスワードを含むのでディスクに残さない)。DB から引けたら消す
export const LEGACY_KEYS = ['siteId', 'rtspUrl'];

export async function loadSettings() {
  return { ...DEFAULTS, ...(await chrome.storage.local.get(Object.keys(DEFAULTS))) };
}

/** 「停止」/「開始」(パネルと点呼の画面の両方から)。background は storage.onChanged で繋ぎ直す */
export const setEnabled = (enabled) => chrome.storage.local.set({ enabled });
