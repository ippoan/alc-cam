// 偽カメラ (RTSP over TCP interleaved)。fixture.h264bin を 15fps でループ再生する。
// Tapo と同じく Digest 認証を求め、SDP の sprop-parameter-sets に SPS/PPS を載せる。
// 音声は TS-NS 系と同じ AAC 32kHz モノラル (RFC 3640 mpeg4-generic)。fixture.aacbin の AU を 32ms ごとに流す
// (track2 を SETUP された時だけ。interleaved は track ごとに 0-1 / 2-3)。
//   import { startFakeRtsp } from './fake-rtsp.mjs'

import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const FPS = 15;
const MTU = 1200;
const REALM = 'fake-camera';
const md5 = (s) => crypto.createHash('md5').update(s).digest('hex');

function loadFrames() {
  const buf = fs.readFileSync(path.join(here, 'fixture.h264bin'));
  const frames = [];
  for (let i = 0; i < buf.length; ) {
    const len = buf.readUInt32BE(i);
    frames.push({ key: buf[i + 4] === 1, data: buf.subarray(i + 5, i + 5 + len) });
    i += 5 + len;
  }
  return frames;
}

function loadAacUnits() {
  const buf = fs.readFileSync(path.join(here, 'fixture.aacbin'));
  const aus = [];
  for (let i = 0; i < buf.length; ) {
    const len = buf.readUInt16BE(i);
    aus.push(buf.subarray(i + 2, i + 2 + len));
    i += 2 + len;
  }
  return aus;
}

function splitNals(au) {
  const nals = [];
  let start = -1;
  for (let i = 0; i + 3 <= au.length; i++) {
    const sc3 = au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1;
    if (!sc3) continue;
    if (start >= 0) nals.push(au.subarray(start, i > 0 && au[i - 1] === 0 ? i - 1 : i));
    start = i + 3;
    i += 2;
  }
  if (start >= 0) nals.push(au.subarray(start));
  return nals.filter((n) => n.length);
}

function rtpPayloads(nal) {
  if (nal.length <= MTU) return [nal];
  const ind = (nal[0] & 0xe0) | 28;
  const typ = nal[0] & 0x1f;
  const out = [];
  for (let off = 1; off < nal.length; off += MTU - 2) {
    const chunk = nal.subarray(off, Math.min(off + MTU - 2, nal.length));
    let hdr = typ;
    if (off === 1) hdr |= 0x80;
    if (off + MTU - 2 >= nal.length) hdr |= 0x40;
    out.push(Buffer.concat([Buffer.from([ind, hdr]), chunk]));
  }
  return out;
}

export function startFakeRtsp({ host, port, user = 'admin', pass = 'secret' }) {
  const frames = loadFrames();
  const aacUnits = loadAacUnits();
  const first = splitNals(frames[0].data);
  const sps = first.find((n) => (n[0] & 0x1f) === 7);
  const pps = first.find((n) => (n[0] & 0x1f) === 8);
  // sessions: 映像の SETUP の数 (= RTSP のセッション数)。audioSetups: 音声の SETUP の数
  const stats = { sessions: 0, audioSetups: 0, playing: 0, teardowns: 0, closed: 0, authFailures: 0 };

  const server = net.createServer((sock) => {
    const nonce = crypto.randomBytes(8).toString('hex');
    let buf = Buffer.alloc(0);
    let timer = null;
    let audioTimer = null;
    let audio = false;
    let aseq = 0;
    let ats = 0;
    let aidx = 0;
    let seq = 0;
    let ts = 0;
    let idx = 0;
    const sessionId = crypto.randomBytes(4).toString('hex');

    const reply = (cseq, status, headers = {}, body = '') => {
      let s = `RTSP/1.0 ${status}\r\nCSeq: ${cseq}\r\n`;
      for (const [k, v] of Object.entries(headers)) s += `${k}: ${v}\r\n`;
      s += `Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`;
      sock.write(s);
    };

    const authorized = (method, headers) => {
      const a = headers['authorization'];
      if (!a?.startsWith('Digest ')) return false;
      const p = Object.fromEntries([...a.matchAll(/(\w+)="?([^",]*)"?/g)].map((m) => [m[1], m[2]]));
      const ha1 = md5(`${user}:${REALM}:${pass}`);
      const ha2 = md5(`${method}:${p.uri}`);
      return p.username === user && p.nonce === nonce && p.response === md5(`${ha1}:${nonce}:${ha2}`);
    };

    const sendFrame = () => {
      const f = frames[idx++ % frames.length];
      const nals = splitNals(f.data);
      nals.forEach((nal, ni) => {
        const parts = rtpPayloads(nal);
        parts.forEach((p, pi) => {
          const marker = ni === nals.length - 1 && pi === parts.length - 1;
          const h = Buffer.alloc(12);
          h[0] = 0x80;
          h[1] = (marker ? 0x80 : 0) | 96;
          h.writeUInt16BE(seq++ & 0xffff, 2);
          h.writeUInt32BE(ts >>> 0, 4);
          h.writeUInt32BE(0x12345678, 8);
          const rtp = Buffer.concat([h, p]);
          const il = Buffer.from([0x24, 0, rtp.length >> 8, rtp.length & 0xff]);
          sock.write(Buffer.concat([il, rtp]));
        });
      });
      ts += 90000 / FPS;
    };

    const sendAudio = () => {
      const au = aacUnits[aidx++ % aacUnits.length];
      // AU ヘッダ 1 つ (sizelength=13, indexlength=3)
      const payload = Buffer.concat([Buffer.from([0, 16, (au.length >> 5) & 0xff, (au.length << 3) & 0xff]), au]);
      const h = Buffer.alloc(12);
      h[0] = 0x80;
      h[1] = 0x80 | 97;
      h.writeUInt16BE(aseq++ & 0xffff, 2);
      h.writeUInt32BE(ats >>> 0, 4);
      h.writeUInt32BE(0x0a0b0c0d, 8);
      ats += 1024;
      const rtp = Buffer.concat([h, payload]);
      sock.write(Buffer.concat([Buffer.from([0x24, 2, rtp.length >> 8, rtp.length & 0xff]), rtp]));
    };

    const handle = (text) => {
      const [head] = text.split('\r\n\r\n');
      const lines = head.split('\r\n');
      const [method, uri] = lines[0].split(' ');
      const headers = Object.fromEntries(lines.slice(1).map((l) => [l.slice(0, l.indexOf(':')).toLowerCase(), l.slice(l.indexOf(':') + 1).trim()]));
      const cseq = headers['cseq'];
      if (method !== 'OPTIONS' && !authorized(method, headers)) {
        if (headers['authorization']) stats.authFailures++;
        return reply(cseq, '401 Unauthorized', { 'WWW-Authenticate': `Digest realm="${REALM}", nonce="${nonce}"` });
      }
      switch (method) {
        case 'OPTIONS':
          return reply(cseq, '200 OK', { Public: 'OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN' });
        case 'DESCRIBE': {
          const sprop = `${sps.toString('base64')},${pps.toString('base64')}`;
          const sdp = `v=0\r\no=- 0 0 IN IP4 ${host}\r\ns=fake\r\nt=0 0\r\na=control:*\r\n` +
            `m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n` +
            `a=fmtp:96 packetization-mode=1;profile-level-id=4d001e;sprop-parameter-sets=${sprop}\r\na=control:track1\r\n` +
            `m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/32000/1\r\n` +
            `a=fmtp:97 streamtype=5;profile-level-id=15;mode=AAC-hbr;config=1288;SizeLength=13;IndexLength=3;IndexDeltaLength=3\r\na=control:track2\r\n`;
          return reply(cseq, '200 OK', { 'Content-Type': 'application/sdp', 'Content-Base': `${uri}/` }, sdp);
        }
        case 'SETUP':
          if (uri.endsWith('track2')) {
            stats.audioSetups++;
            audio = true;
            return reply(cseq, '200 OK', { Session: `${sessionId};timeout=60`, Transport: 'RTP/AVP/TCP;unicast;interleaved=2-3' });
          }
          stats.sessions++;
          return reply(cseq, '200 OK', { Session: `${sessionId};timeout=60`, Transport: 'RTP/AVP/TCP;unicast;interleaved=0-1' });
        case 'PLAY':
          stats.playing++;
          reply(cseq, '200 OK', { Session: sessionId });
          timer = setInterval(sendFrame, 1000 / FPS);
          if (audio) audioTimer = setInterval(sendAudio, 1024 / 32);
          return;
        case 'TEARDOWN':
          stats.teardowns++;
          clearInterval(timer);
          clearInterval(audioTimer);
          return reply(cseq, '200 OK', { Session: sessionId });
        default:
          return reply(cseq, '200 OK', { Session: sessionId });
      }
    };

    sock.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      for (;;) {
        const end = buf.indexOf('\r\n\r\n');
        if (end < 0) return;
        handle(buf.subarray(0, end + 4).toString());
        buf = buf.subarray(end + 4);
      }
    });
    sock.on('error', () => {});
    sock.on('close', () => {
      clearInterval(timer);
      clearInterval(audioTimer);
      stats.closed++;
    });
  });

  server.listen(port, host);
  return { stats, url: `rtsp://${user}:${pass}@${host}:${port}/stream2`, close: () => server.close() };
}
