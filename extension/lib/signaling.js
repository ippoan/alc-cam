// cf-alc-signaling の /cam-room/<拠点ID>?role=device&proto=2 への WebSocket (alc-gw の
// docs/camera-relay-convention.md が一次の規約、複数人視聴は v2)。切れたら規約どおり 1s→60s の指数バックオフで繋ぎ直す。
// 映像は通らない: SDP の offer/answer と peer_joined / peer_left だけが行き来する (v2 では viewer_id 付き)。
//
// 配信担当の順位: 上位か同位の端末が配信中なら cam-room は受け入れてすぐ 4011 で閉じ、上位の端末が来れば今の接続を 4010 で切る。
// どちらも「待機」: backoff ではなく standbyRetryMs〜その 2 倍 (乱数) の後に入り直す (上位が落ちていれば入れる)。
// 待機からの入り直しで開いても、すぐ 4011 が来ることが多いので、開いた表示は STANDBY_OPEN_GRACE_MS だけ待ってから出す。
//
// keepalive は 20 秒ごとに KEEPALIVE_PING をそのまま送る。cam-room は setWebSocketAutoResponse で
// runtime が KEEPALIVE_PONG を返すので DO は hibernate したまま。この送受信が MV3 の service worker も生かす。
// 1 文字でも違うと DO が起きるので、文字列は cf-alc-signaling src/keepalive.ts と完全に同じにする。

export const KEEPALIVE_PING = JSON.stringify({ type: 'ping' }); // '{"type":"ping"}'
export const KEEPALIVE_PONG = JSON.stringify({ type: 'pong' }); // '{"type":"pong"}'
export const PING_INTERVAL_MS = 20_000;
// pong がこれだけ来なければ切れたとみなす (DO 側は期限を持たない)
const PONG_TIMEOUT_MS = 60_000;
const BACKOFF_MIN_MS = 1_000;
const BACKOFF_MAX_MS = 60_000;
/** cam-room の close code (cf-alc-signaling camera-signaling-room.ts の CLOSE_PREEMPTED / CLOSE_STANDBY と同じ) */
export const CLOSE_PREEMPTED = 4010;
export const CLOSE_STANDBY = 4011;
export const STANDBY_RETRY_MS = 30_000;
const STANDBY_OPEN_GRACE_MS = 1_500;

/** 待機の close code なら表示の文言、違えば null */
export function standbyReason(code) {
  if (code === CLOSE_STANDBY) return '待機中 (上位の端末が配信中)';
  if (code === CLOSE_PREEMPTED) return '待機中 (上位の端末に交代)';
  return null;
}

export class CamRoom {
  /**
   * @param {{url: () => string,
   *          onState: (state: 'connecting'|'open'|'closed'|'standby', detail?: string) => void,
   *          onMessage: (msg: {type: string, [k: string]: unknown}) => void,
   *          standbyRetryMs?: number}} opts
   */
  constructor(opts) {
    this.opts = opts;
    this.ws = null;
    this.stopped = false;
    this.backoff = BACKOFF_MIN_MS;
    this.retryTimer = null;
    this.pingTimer = null;
    this.lastPong = 0;
    this.standby = false;
    this.openTimer = null;
  }

  start() {
    this.stopped = false;
    this.connect();
  }

  connect() {
    if (this.stopped) return;
    if (!this.standby) this.opts.onState('connecting');
    let ws;
    try {
      // 繋ぐたびに作る (鍵が更新されていれば新しい鍵で入る。開いている接続はそのまま)
      ws = new WebSocket(this.opts.url());
    } catch (e) {
      this.scheduleRetry(String(e));
      return;
    }
    this.ws = ws;
    ws.onopen = () => {
      this.backoff = BACKOFF_MIN_MS;
      this.lastPong = Date.now();
      // 待機からの入り直しは、閉じられなければ (= 上位が居ない) 配信に戻ったと出す
      const opened = () => {
        this.standby = false;
        this.opts.onState('open');
      };
      if (this.standby) this.openTimer = setTimeout(opened, STANDBY_OPEN_GRACE_MS);
      else opened();
      this.pingTimer = setInterval(() => this.keepalive(ws), PING_INTERVAL_MS);
    };
    ws.onmessage = (ev) => {
      if (ev.data === KEEPALIVE_PONG) {
        this.lastPong = Date.now();
        return;
      }
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      // 完全一致でない pong (DO が起きて返した従来の分岐) も生存の印として受ける
      if (msg.type === 'pong') this.lastPong = Date.now();
      else this.opts.onMessage(msg);
    };
    ws.onclose = (ev) => this.dropped(ws, `close ${ev.code}${ev.reason ? ' ' + ev.reason : ''}`, ev.code);
  }

  keepalive(ws) {
    if (this.ws !== ws) return;
    if (Date.now() - this.lastPong > PONG_TIMEOUT_MS) {
      // 黙って切れた回線は onclose がいつ来るか分からないので、こちらから捨てて繋ぎ直す
      ws.close(4000, 'pong timeout');
      this.dropped(ws, 'pong が来ない');
      return;
    }
    if (ws.readyState === WebSocket.OPEN) ws.send(KEEPALIVE_PING);
  }

  dropped(ws, detail, code) {
    if (this.ws !== ws) return;
    this.ws = null;
    clearInterval(this.pingTimer);
    clearTimeout(this.openTimer);
    // 見る人 (admin) も全員いなくなったものとして扱う
    this.opts.onMessage({ type: 'room_closed' });
    const reason = standbyReason(code);
    if (reason) return this.waitStandby(reason);
    this.standby = false;
    // 409 = 同じ拠点の device が既に居る (alc-gw 等)。間隔を空けて繋ぎ直すだけ
    this.scheduleRetry(detail);
  }

  waitStandby(reason) {
    if (this.stopped) return;
    this.standby = true;
    this.backoff = BACKOFF_MIN_MS;
    const base = this.opts.standbyRetryMs ?? STANDBY_RETRY_MS;
    const wait = Math.round(base + Math.random() * base);
    this.opts.onState('standby', `${reason} — ${Math.round(wait / 1000)}s 後に入り直す`);
    this.retryTimer = setTimeout(() => this.connect(), wait);
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
    this.standby = false;
    clearTimeout(this.retryTimer);
    clearTimeout(this.openTimer);
    clearInterval(this.pingTimer);
    const ws = this.ws;
    this.ws = null;
    ws?.close(1000, 'stopped');
    this.opts.onState('closed', '停止');
  }
}
