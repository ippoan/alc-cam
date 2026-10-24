// 担当カメラを DB から引く (docs: alc-cam の拠点 PC の設定を DB 主体にする設計)。
//
//   点呼の PWA (https://alc.ippoan.org) ─chrome.runtime.sendMessage({type:'cam_host_token', token, exp})─▶ background
//   background ─GET ${apiUrl}/device-data-proxy/api/cam-clips/cameras/mine (Bearer 鍵)─▶ auth-worker ─▶ alc-cam ingest
//     → { cameras: [{ id, name, rtsp: { host, port, path, username, password } | null }] }
//
// 鍵は role=device-cam-host の device JWT (TTL 900 秒、PWA が期限の 60 秒前に取り直して渡し直す)。
// /device-data-proxy は Bearer の device JWT を直接検証する口なので、alc-app の /api/proxy (cookie を優先する) を通さない。

export const PWA_ORIGIN = 'https://alc.ippoan.org';
export const MINE_PATH = '/device-data-proxy/api/cam-clips/cameras/mine';

/** onMessageExternal で来た値を検める。正しければ {token, exp (ミリ秒)}、違えば null */
export function parseTokenMessage(msg, sender) {
  if (sender?.origin !== PWA_ORIGIN || msg?.type !== 'cam_host_token') return null;
  const { token, exp } = msg;
  if (typeof token !== 'string' || !/^[\w-]+\.[\w-]+\.[\w-]+$/.test(token) || token.length > 8192) return null;
  if (typeof exp !== 'number' || !Number.isFinite(exp)) return null;
  // JWT の exp (秒) でも Date.now() (ミリ秒) でも受ける
  return { token, exp: exp < 1e12 ? exp * 1000 : exp };
}

/** rtsp の接続先 → URL (資格情報は URL エンコード) */
export function rtspUrl({ host, port, path, username }, password) {
  const auth = username ? `${encodeURIComponent(username)}${password ? `:${encodeURIComponent(password)}` : ''}@` : '';
  const h = host.includes(':') ? `[${host}]` : host;
  const p = !path ? '' : path.startsWith('/') ? path : `/${path}`;
  return `rtsp://${auth}${h}${port ? `:${port}` : ''}${p}`;
}

/**
 * 担当カメラを引く。先頭の 1 台だけ使う (1 PC 1 カメラ)。
 * @returns {Promise<{camera: null, reason: string} | {camera: {id, name, rtsp: object}, password: string}>}
 * 通信の失敗・401/403 等は throw (呼び手は写しのまま配信を続ける)
 */
export async function fetchMine(apiUrl, token) {
  const res = await fetch(`${apiUrl}${MINE_PATH}`, {
    headers: { Authorization: `Bearer ${token}` },
    credentials: 'omit',
    cache: 'no-store',
  });
  if (!res.ok) throw new Error(`担当カメラの取得: HTTP ${res.status}`);
  const first = (await res.json())?.cameras?.[0];
  if (!first?.id) return { camera: null, reason: 'この端末の担当カメラがありません (録画の画面で端末を紐づけてください)' };
  const r = first.rtsp;
  if (!r?.host) return { camera: null, reason: `${first.name ?? first.id}: RTSP の接続先が未設定です (録画の画面で入れてください)` };
  const { password = '', ...rest } = r;
  const rtsp = { host: String(rest.host), port: Number(rest.port) || 0, path: String(rest.path ?? ''), username: String(rest.username ?? '') };
  return { camera: { id: String(first.id), name: String(first.name ?? first.id), rtsp }, password: String(password ?? '') };
}
