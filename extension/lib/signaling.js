// cf-alc-signaling の /cam-room/<拠点ID>?role=device への WebSocket (alc-gw の
// docs/camera-relay-convention.md が一次の規約)。切れたら規約どおり 1s→60s の指数バックオフで繋ぎ直す。
// 映像は通らない: SDP の offer/answer と peer_joined / peer_left だけが行き来する。

const PING_INTERVAL_MS = 25_000;
const BACKOFF_MIN_MS = 1_000;
const BACKOFF_MAX_MS = 60_000;

export class CamRoom {
  /**
   * @param {{url: string,
   *          onState: (state: 'connecting'|'open'|'closed', detail?: string) => void,
   *          onMessage: (msg: {type: string, [k: string]: unknown}) => void}} opts
   */
  constructor(opts) {
    this.opts = opts;
    this.ws = null;
    this.stopped = false;
    this.backoff = BACKOFF_MIN_MS;
    this.retryTimer = null;
    this.pingTimer = null;
  }

  start() {
    this.stopped = false;
    this.connect();
  }

  connect() {
    if (this.stopped) return;
    this.opts.onState('connecting');
    let ws;
    try {
      ws = new WebSocket(this.opts.url);
    } catch (e) {
      this.scheduleRetry(String(e));
      return;
    }
    this.ws = ws;
    ws.onopen = () => {
      this.backoff = BACKOFF_MIN_MS;
      this.opts.onState('open');
      this.pingTimer = setInterval(() => this.send({ type: 'ping' }), PING_INTERVAL_MS);
    };
    ws.onmessage = (ev) => {
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (msg.type !== 'pong') this.opts.onMessage(msg);
    };
    ws.onclose = (ev) => {
      if (this.ws !== ws) return;
      this.ws = null;
      clearInterval(this.pingTimer);
      // 相手 (admin) もいなくなったものとして扱う
      this.opts.onMessage({ type: 'peer_left', role: 'admin' });
      // 409 = 同じ拠点の device が既に居る (alc-gw 等)。間隔を空けて繋ぎ直すだけ
      this.scheduleRetry(`close ${ev.code}${ev.reason ? ' ' + ev.reason : ''}`);
    };
  }

  scheduleRetry(detail) {
    if (this.stopped) return;
    const wait = this.backoff;
    this.backoff = Math.min(this.backoff * 2, BACKOFF_MAX_MS);
    this.opts.onState('closed', `${detail} — ${wait / 1000}s 後に再接続`);
    this.retryTimer = setTimeout(() => this.connect(), wait);
  }

  send(obj) {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(obj));
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.retryTimer);
    clearInterval(this.pingTimer);
    const ws = this.ws;
    this.ws = null;
    ws?.close(1000, 'stopped');
    this.opts.onState('closed', '停止');
  }
}
