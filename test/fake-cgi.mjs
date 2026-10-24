// 偽カメラの管理画面 (HTTP)。首振りの CGI (ptz.cgi / ptzconfig.cgi) と通話 (audio/transmit.cgi) を持ち、Digest (MD5, qop=auth) を求める。
// 首振りの応答は少し遅らせ、同時に何本来たか (host が 1 本ずつ送っているか) を数える。
// 通話は公開仕様 (QwatchAPI「通話」) の形: Keep-Alive の GET → 200 の後ろに Content-Type: audio/aac → 本文として ADTS を受け続ける。
// 401 の後は接続を閉じる (仕様: 一部の版は繋ぎ直さないと受けない)。2 本目と `busy` の間は Too many users を返す。
//   import { startFakeCgi } from './fake-cgi.mjs'

import crypto from 'node:crypto';
import http from 'node:http';
import net from 'node:net';

const REALM = 'fake-camera-cgi';
const TRANSMIT = '/camera-cgi/audio/transmit.cgi';
const md5 = (s) => crypto.createHash('md5').update(s).digest('hex');

export function startFakeCgi({ host, port, user = 'admin', pass = 'secret', delayMs = 80 }) {
  const nonce = crypto.randomBytes(8).toString('hex');
  // calls: 認証が通った要求 (path)。unauthorized: 資格情報付きなのに落ちた数
  const stats = { calls: [], challenges: 0, unauthorized: 0, inFlight: 0, maxInFlight: 0 };
  // 通話: sessions (受け付けた数) / busyReplies / open (今の本数) / frames (ADTS) / rates (ADTS の周波数 index) / closed
  const talk = { sessions: 0, busyReplies: 0, challenges: 0, open: 0, frames: 0, bytes: 0, rates: new Set(), closed: 0, busy: false };

  const authorized = (method, url, authorization) => {
    if (!authorization?.startsWith('Digest ')) return false;
    const p = Object.fromEntries([...authorization.matchAll(/(\w+)="?([^",]*)"?/g)].map((m) => [m[1], m[2]]));
    const ha1 = md5(`${user}:${REALM}:${pass}`);
    const ha2 = md5(`${method}:${p.uri}`);
    return p.username === user && p.nonce === nonce && p.uri === url && p.response === md5(`${ha1}:${nonce}:${p.nc}:${p.cnonce}:auth:${ha2}`);
  };

  const server = http.createServer((req, res) => {
    stats.inFlight++;
    stats.maxInFlight = Math.max(stats.maxInFlight, stats.inFlight);
    setTimeout(() => {
      stats.inFlight--;
      if (!authorized(req.method, req.url, req.headers.authorization)) {
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

  // 通話の接続: 要求のヘッダの後ろは音声 (長さの無い本文) なので http では読めない。生の TCP で受ける
  const handleTransmit = (sock, first) => {
    let buf = first;
    let head = null;
    let accepted = false;
    const onData = (d) => {
      buf = Buffer.concat([buf, d]);
      if (!head) {
        const end = buf.indexOf('\r\n\r\n');
        if (end < 0) return;
        head = buf.subarray(0, end).toString();
        buf = buf.subarray(end + 4);
        const lines = head.split('\r\n');
        const [method, url] = lines[0].split(' ');
        const headers = Object.fromEntries(lines.slice(1).map((l) => [l.slice(0, l.indexOf(':')).toLowerCase(), l.slice(l.indexOf(':') + 1).trim()]));
        if (!authorized(method, url, headers.authorization) || method !== 'GET') {
          if (headers.authorization) stats.unauthorized++;
          else talk.challenges++;
          sock.end(`HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm="${REALM}", nonce="${nonce}", qop="auth"\r\nContent-Length: 0\r\n\r\n`);
          return;
        }
        if (talk.busy || talk.open > 0) {
          talk.busyReplies++;
          sock.end('HTTP/1.1 200 OK\r\nX-CurrentUsers: Too many users\r\nContent-Length: 0\r\nContent-Type: text/plain\r\n\r\n');
          return;
        }
        accepted = true;
        talk.sessions++;
        talk.open++;
        sock.write('HTTP/1.1 200 OK\r\nConnection: Keep-Alive\r\n\r\nContent-Type: audio/aac\r\n\r\n');
      }
      // ADTS を数える (同期語 0xFFF・長さ 13 bit)
      for (;;) {
        if (buf.length < 7) return;
        if (buf[0] !== 0xff || (buf[1] & 0xf0) !== 0xf0) {
          buf = buf.subarray(1);
          continue;
        }
        const len = ((buf[3] & 0x03) << 11) | (buf[4] << 3) | (buf[5] >> 5);
        if (len < 7 || buf.length < len) return;
        talk.frames++;
        talk.bytes += len;
        talk.rates.add((buf[2] >> 2) & 0x0f);
        buf = buf.subarray(len);
      }
    };
    sock.on('data', onData);
    sock.on('close', () => {
      if (!accepted) return;
      talk.open--;
      talk.closed++;
    });
    sock.on('error', () => {});
    onData(Buffer.alloc(0));
  };

  const front = net.createServer((sock) => {
    sock.once('data', (first) => {
      if (first.subarray(0, 64).toString().startsWith(`GET ${TRANSMIT}`)) return handleTransmit(sock, first);
      sock.pause();
      sock.unshift(first);
      server.emit('connection', sock);
      process.nextTick(() => sock.resume());
    });
    sock.on('error', () => {});
  });
  front.listen(port, host);
  return { stats, talk, close: () => front.close() };
}
