// cf-alc-signaling の CameraSignalingRoom (/cam-room/:siteId) を写したローカル版 (複数人視聴の v2 込み)。
// - device は 1 本 (2 本目は 409)。`?proto=2` で入った device の部屋は admin を 4 人まで、
//   proto 無しの device の部屋は v1 のまま admin 1 人 (2 本目は 409)
// - v2: admin ごとに viewer_id を振り、device へは peer_joined/peer_left/sdp_answer/ice_candidate に付ける。
//   device からの sdp_offer/ice_candidate は viewer_id の admin へだけ、viewer_id を外して渡す
// - 見る側 (admin) へのメッセージは v1 と同じ (viewer_id を知らない)
// - `v1Sites` に入れた部屋は device の proto を無視して v1 として扱う (古い DO の代わり)
// - keepalive: `{"type":"ping"}` ちょうどには `{"type":"pong"}` ちょうどを返し、log に残さない
//   (本物は setWebSocketAutoResponse で runtime が返し DO は起きない)。数は keepalive に数える。
//   1 文字でも違う ping は従来の `ping` 分岐 (log に `device: ping` が残る = 本物なら DO を起こす)
// 本物との違い: admin の token を検証しない (ローカル確認用)。
//   import { startSignaling } from './signaling.mjs'

import crypto from 'node:crypto';
import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const here = path.dirname(fileURLToPath(import.meta.url));
const MAX_ADMINS = 4;
// cf-alc-signaling src/keepalive.ts の KEEPALIVE_PING / KEEPALIVE_PONG と同じ
const KEEPALIVE_PING = '{"type":"ping"}';
const KEEPALIVE_PONG = '{"type":"pong"}';

export function startSignaling({ port, v1Sites = [] }) {
  /** siteId → { device, v2, admins: Map<viewer_id, ws> } */
  const rooms = new Map();
  const log = [];
  const keepalive = { device: 0, admin: 0 };
  // 完全一致の ping に runtime と同じく応じる (true = 応じた)
  const autoResponse = (ws, data, role) => {
    if (data.toString() !== KEEPALIVE_PING) return false;
    keepalive[role]++;
    ws.send(KEEPALIVE_PONG);
    return true;
  };
  const send = (ws, msg) => ws?.readyState === 1 && ws.send(JSON.stringify(msg));
  const withId = (room, msg, viewerId) => (room.v2 ? { ...msg, viewer_id: viewerId } : msg);

  const server = http.createServer(async (req, res) => {
    // viewer.html だけ配る (secure context にするため localhost で開く)
    if (req.url.startsWith('/viewer.html')) {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(await fs.readFile(path.join(here, 'viewer.html')));
      return;
    }
    res.writeHead(404).end();
  });
  const wss = new WebSocketServer({ noServer: true });

  server.on('upgrade', (req, socket, head) => {
    const url = new URL(req.url, 'http://x');
    const m = url.pathname.match(/^\/cam-room\/([a-zA-Z0-9_-]+)$/);
    const role = url.searchParams.get('role');
    if (!m || (role !== 'device' && role !== 'admin')) return socket.destroy();
    const site = m[1];
    const room = rooms.get(site) ?? { device: null, v2: false, admins: new Map() };
    rooms.set(site, room);
    if (role === 'device' && room.device) return socket.end('HTTP/1.1 409 Conflict\r\n\r\n');
    if (role === 'admin') {
      const limit = room.device && !room.v2 ? 1 : MAX_ADMINS;
      if (room.admins.size >= limit) return socket.end('HTTP/1.1 409 Conflict\r\n\r\nviewer limit');
    }
    wss.handleUpgrade(req, socket, head, (ws) => (role === 'device' ? onDevice(site, room, ws, url) : onAdmin(site, room, ws)));
  });

  function onDevice(site, room, ws, url) {
    room.device = ws;
    room.v2 = url.searchParams.get('proto') === '2' && !v1Sites.includes(site);
    log.push(`device joined (${room.v2 ? 'v2' : 'v1'})`);
    for (const [viewerId, admin] of room.admins) {
      send(admin, { type: 'peer_joined', role: 'device' });
      send(ws, withId(room, { type: 'peer_joined', role: 'admin' }, viewerId));
    }
    ws.on('message', (data) => {
      if (autoResponse(ws, data, 'device')) return;
      const msg = parse(ws, data);
      if (!msg) return;
      log.push(`device: ${msg.type}${msg.viewer_id ? ` → ${msg.viewer_id}` : ''}`);
      // v1 は宛先が 1 人しか居ない
      const target = room.v2 ? room.admins.get(msg.viewer_id) : room.admins.values().next().value;
      switch (msg.type) {
        case 'sdp_offer':
          return send(target, { type: 'sdp_offer', sdp: msg.sdp });
        case 'ice_candidate':
          return send(target, { type: 'ice_candidate', candidate: msg.candidate });
        case 'ping':
          return send(ws, { type: 'pong' });
        case 'sdp_answer':
          return send(ws, { type: 'error', message: 'Only admin can send sdp_answer' });
        default:
          return send(ws, { type: 'error', message: `Unknown message type: ${msg.type}` });
      }
    });
    ws.on('close', () => {
      if (room.device !== ws) return;
      room.device = null;
      log.push('device left');
      for (const admin of room.admins.values()) send(admin, { type: 'peer_left', role: 'device' });
    });
  }

  function onAdmin(site, room, ws) {
    const viewerId = crypto.randomBytes(6).toString('hex');
    room.admins.set(viewerId, ws);
    log.push(`admin joined ${viewerId}`);
    send(room.device, withId(room, { type: 'peer_joined', role: 'admin' }, viewerId));
    if (room.device) send(ws, { type: 'peer_joined', role: 'device' });
    ws.on('message', (data) => {
      if (autoResponse(ws, data, 'admin')) return;
      const msg = parse(ws, data);
      if (!msg) return;
      log.push(`admin ${viewerId}: ${msg.type}`);
      switch (msg.type) {
        case 'sdp_answer':
          return send(room.device, withId(room, { type: 'sdp_answer', sdp: msg.sdp }, viewerId));
        case 'ice_candidate':
          return send(room.device, withId(room, { type: 'ice_candidate', candidate: msg.candidate }, viewerId));
        case 'ping':
          return send(ws, { type: 'pong' });
        case 'sdp_offer':
          return send(ws, { type: 'error', message: 'Only device can send sdp_offer' });
        default:
          return send(ws, { type: 'error', message: `Unknown message type: ${msg.type}` });
      }
    });
    ws.on('close', () => {
      if (room.admins.get(viewerId) !== ws) return;
      room.admins.delete(viewerId);
      log.push(`admin left ${viewerId}`);
      send(room.device, withId(room, { type: 'peer_left', role: 'admin' }, viewerId));
    });
  }

  function parse(ws, data) {
    try {
      return JSON.parse(data.toString());
    } catch {
      send(ws, { type: 'error', message: 'Invalid JSON' });
      return null;
    }
  }

  server.listen(port, '127.0.0.1');
  return { log, keepalive, close: () => server.close() };
}
