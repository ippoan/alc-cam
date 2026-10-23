// 偽カメラの管理画面 (HTTP)。首振りの CGI (ptz.cgi / ptzconfig.cgi) だけを持ち、Digest (MD5, qop=auth) を求める。
// 応答は少し遅らせ、同時に何本来たか (host が 1 本ずつ送っているか) を数える。
//   import { startFakeCgi } from './fake-cgi.mjs'

import crypto from 'node:crypto';
import http from 'node:http';

const REALM = 'fake-camera-cgi';
const md5 = (s) => crypto.createHash('md5').update(s).digest('hex');

export function startFakeCgi({ host, port, user = 'admin', pass = 'secret', delayMs = 80 }) {
  const nonce = crypto.randomBytes(8).toString('hex');
  // calls: 認証が通った要求 (path)。unauthorized: 資格情報付きなのに落ちた数
  const stats = { calls: [], challenges: 0, unauthorized: 0, inFlight: 0, maxInFlight: 0 };

  const authorized = (req) => {
    const a = req.headers.authorization;
    if (!a?.startsWith('Digest ')) return false;
    const p = Object.fromEntries([...a.matchAll(/(\w+)="?([^",]*)"?/g)].map((m) => [m[1], m[2]]));
    const ha1 = md5(`${user}:${REALM}:${pass}`);
    const ha2 = md5(`${req.method}:${p.uri}`);
    return p.username === user && p.nonce === nonce && p.uri === req.url && p.response === md5(`${ha1}:${nonce}:${p.nc}:${p.cnonce}:auth:${ha2}`);
  };

  const server = http.createServer((req, res) => {
    stats.inFlight++;
    stats.maxInFlight = Math.max(stats.maxInFlight, stats.inFlight);
    setTimeout(() => {
      stats.inFlight--;
      if (!authorized(req)) {
        if (req.headers.authorization) stats.unauthorized++;
        else stats.challenges++;
        res.writeHead(401, { 'WWW-Authenticate': `Digest realm="${REALM}", nonce="${nonce}", qop="auth"` });
        return res.end();
      }
      const url = new URL(req.url, 'http://x');
      const ok =
        req.method === 'POST' &&
        ((url.pathname === '/camera-cgi/com/ptz.cgi' && /^(up|down|left|right|home)$/.test(url.searchParams.get('move'))) ||
          (url.pathname === '/camera-cgi/com/ptzconfig.cgi' && /^[1-4]$/.test(url.searchParams.get('movepresetpoint'))));
      if (!ok) {
        res.writeHead(404);
        return res.end();
      }
      stats.calls.push(url.search.slice(1));
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      res.end('OK\n');
    }, delayMs);
  });
  server.listen(port, host);
  return { stats, close: () => server.close() };
}
