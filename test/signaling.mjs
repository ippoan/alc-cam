// cf-alc-signaling の CameraSignalingRoom (/cam-room/:siteId) をそのまま写したローカル版。
// device/admin 各 1 本・同 role の 2 本目は 409・peer_joined/peer_left・sdp/ice の中継・ping→pong。
// 本物との違い: admin の token を検証しない (ローカル確認用)。
//   import { startSignaling } from './signaling.mjs'

import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const here = path.dirname(fileURLToPath(import.meta.url));

export function startSignaling({ port }) {
  /** siteId → { device, admin } */
  const rooms = new Map();
  const log = [];
  const send = (ws, msg) => ws?.readyState === 1 && ws.send(JSON.stringify(msg));

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
    const room = rooms.get(m[1]) ?? {};
    rooms.set(m[1], room);
    if (room[role]) {
      socket.end('HTTP/1.1 409 Conflict\r\n\r\n');
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => {
      const peer = role === 'device' ? 'admin' : 'device';
      room[role] = ws;
      log.push(`${role} joined`);
      send(room[peer], { type: 'peer_joined', role });
      if (room[peer]) send(ws, { type: 'peer_joined', role: peer });

      ws.on('message', (data) => {
        let msg;
        try {
          msg = JSON.parse(data.toString());
        } catch {
          return send(ws, { type: 'error', message: 'Invalid JSON' });
        }
        log.push(`${role}: ${msg.type}`);
        switch (msg.type) {
          case 'sdp_offer':
            if (role !== 'device') return send(ws, { type: 'error', message: 'Only device can send sdp_offer' });
            return send(room.admin, { type: 'sdp_offer', sdp: msg.sdp });
          case 'sdp_answer':
            if (role !== 'admin') return send(ws, { type: 'error', message: 'Only admin can send sdp_answer' });
            return send(room.device, { type: 'sdp_answer', sdp: msg.sdp });
          case 'ice_candidate':
            return send(room[peer], { type: 'ice_candidate', candidate: msg.candidate });
          case 'ping':
            return send(ws, { type: 'pong' });
          default:
            return send(ws, { type: 'error', message: `Unknown message type: ${msg.type}` });
        }
      });
      ws.on('close', () => {
        if (room[role] !== ws) return;
        room[role] = null;
        log.push(`${role} left`);
        send(room[peer], { type: 'peer_left', role });
      });
    });
  });

  server.listen(port, '127.0.0.1');
  return { log, close: () => server.close() };
}
