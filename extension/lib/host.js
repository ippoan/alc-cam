// native messaging の alc-cam-host (Rust)。このページが開いている間だけ Chrome が起動し、
// ページが閉じる (= port が切れる) と host の stdin が閉じて終了する。
// メッセージは docs/host-protocol.md の表のとおり。

export const HOST_NAME = 'org.ippoan.alc_cam_host';
const RETRY_MAX_MS = 30_000;

export class Host {
  /**
   * @param {{config: object,
   *          onMessage: (msg: {type: string, [k: string]: unknown}) => void,
   *          onDown: (reason: string) => void}} opts
   */
  constructor(opts) {
    this.opts = opts;
    this.port = null;
    this.stopped = false;
    this.retry = 1000;
    this.timer = null;
  }

  start() {
    this.stopped = false;
    this.connect();
  }

  connect() {
    if (this.stopped) return;
    const port = chrome.runtime.connectNative(HOST_NAME);
    this.port = port;
    port.onMessage.addListener((msg) => {
      this.retry = 1000;
      this.opts.onMessage(msg);
    });
    port.onDisconnect.addListener(() => {
      if (this.port !== port) return;
      this.port = null;
      // 未インストール・起動失敗・異常終了のどれもここに来る。文言は Chrome のものをそのまま出す
      const reason = chrome.runtime.lastError?.message ?? 'host が終了しました';
      this.opts.onDown(reason);
      if (this.stopped) return;
      const wait = this.retry;
      this.retry = Math.min(this.retry * 2, RETRY_MAX_MS);
      this.timer = setTimeout(() => this.connect(), wait);
    });
    port.postMessage({ type: 'config', ...this.opts.config });
  }

  send(msg) {
    this.port?.postMessage(msg);
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.timer);
    const port = this.port;
    this.port = null;
    port?.disconnect();
  }
}
