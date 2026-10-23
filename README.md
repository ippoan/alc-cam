# alc-cam

拠点のカメラ (Tapo C212 等の RTSP) を、**Chrome 拡張のサイドパネルを開いている間だけ** WebRTC (P2P) で
見る側へ届ける。alc-gw (Go の常駐アプリ) を置かない拠点向けの、カメラ中継規約の 3 つ目の device 実装。

```
カメラ ─RTSP─▶ alc-cam-host (Rust, native messaging) ══WebRTC P2P (H.264 そのまま)══▶ 見る側
                     ▲ stdin/stdout                                                    (alc-app の TenkoCameraView)
                     │                                                                  ▲
               Chrome 拡張 (サイドパネル) ──WebSocket── cf-alc-signaling /cam-room/<拠点ID> ──┘
                                              (SDP だけ。role=device)
```

- 拡張は TCP を張れないので、RTSP と WebRTC は Rust の host が持つ。host は拡張が起動し、パネルを閉じると終わる (サービス常駐しない)
- シグナリングは既存の `cf-alc-signaling` の `/cam-room` をそのまま使う。**クラウド側の変更は無い**。見る側も既存の alc-app のまま
- 映像は Cloudflare を通らない (DO が中継するのは SDP の数通だけで、hibernation が効く)
- 規約: ippoan/alc-gw の `docs/camera-relay-convention.md`。拡張と host の間は [docs/host-protocol.md](docs/host-protocol.md)

## 構成

| パス | 中身 |
|---|---|
| `host/` | `alc-cam-host` (str0m で WebRTC、暗号は pure Rust)。`install.ps1` は Windows への登録 |
| `crates/cam-rtsp/` | RTSP (Digest・TCP interleaved) → H.264 Annex B。I/O を持たない |
| `extension/` | 拠点側の Chrome 拡張 (MV3・サイドパネル)。ID は `key` で `icmmpdjddlphkbokdalofdjpkcgfiggd` に固定 |
| `test/` | 偽カメラ・cam-room のローカル版・見る側のページと、それらを通す `e2e.mjs` |
| `crates/cam-sd/` | カメラの SD 録画の一覧と取り出しの手順 (I/O を持たない) |
| `ingest/` | SD 録画を R2 へ取り込む Worker `alc-cam-sd-ingest` (wasm32、別 workspace) |

## 拠点 PC への入れ方 (Windows)

1. Actions の成果物 (またはタグの Release) の `alc-cam-windows.zip` を展開
2. `powershell -ExecutionPolicy Bypass -File install.ps1` (管理者不要。`%LOCALAPPDATA%\alc-cam` と HKCU に登録)
3. `chrome://extensions` → デベロッパーモード → 「パッケージ化されていない拡張機能を読み込む」→ `extension/`
4. ツールバーのアイコン → サイドパネル → 設定に拠点 ID・カメラの RTSP URL (stream2) を入れて保存 → 開始

見る側は alc-app の拠点カメラ画面で同じ拠点 ID を開く。

## SD 録画の取り込み (ingest/)

カメラの SD に録られた録画を、Worker `alc-cam-sd-ingest` が 15 分ごとに R2 (`alc-cam-recordings`) へ取り込む。

```
cron ─▶ alc-cam-sd-ingest ─Workers VPC (HTTP Service, ohishi-data の Tunnel)─▶ カメラの管理画面 (Digest)
              └─▶ R2 alc-cam-recordings/<カメラ名>/<種類>/<YYYYMMDD>/<HH>/<ファイル名>.mp4
```

- 一覧は `camera-cgi/admin/sdcard.cgi?action=generate`、本体は `POST /action/stream?subject=recdownload`
  (パスの頭に `/mnt/mmc`)。手順の細部と実機での確認は `crates/cam-sd/src/lib.rs` の先頭
- 昨日と今日 (JST) を古い順に見て、R2 に同じ大きさで在るものは飛ばす。1 回 12 本 (約 1.2GB)・600 秒で打ち切り、残りは次へ
- 本体は wasm を通さず `FixedLengthStream` で R2 へ流す。VPC 越しで約 11MB/s (100MB を 9 秒)
- 資格情報は Worker の secret `CAM_AUTH` (`user:pass`)。repo には無い

確認 (VPC は実物のカメラ、R2 は手元の模擬):

```bash
cd ingest
printf 'CAM_AUTH="user:pass"\n' > .dev.vars   # 終わったら消す
npx wrangler@4.152.0 dev --test-scheduled --var MAX_FILES_PER_RUN:1 --var SCAN_DAYS:1
curl 'http://localhost:8787/cdn-cgi/handler/scheduled?cron=*/15+*+*+*+*'
```

`--var DRAIN_ONLY:1` を付けると R2 に書かず読み捨て、届いたバイト数と時間だけログに出す (経路の切り分け用)。
wrangler 4.58 では手元の dev から VPC に繋がらない (preview セッションの作成が 400)。

## 開発

```bash
cargo test --workspace
cargo build -p alc-cam-host
cd test && npm install && npx playwright-core install chromium
node make-fixture.mjs   # 偽カメラの H.264 を作る (branded Chrome の WebCodecs を使う)
node e2e.mjs            # 偽カメラ → host → 拡張 → cam-room → 見る側 を通す
```

`e2e.mjs` が確かめること: 見る人が来るまでカメラに触らない / Digest 認証 / 見る側で H.264 がデコードされる /
見る人が離れたらカメラへ TEARDOWN / 入り直しで再び映る / パネルを閉じたら host が終わる。

## 未確認・未対応

- Windows の実機 (install.ps1 と host.exe)。ビルドは CI の windows-latest で行う (この repo の Linux 環境に MinGW が無い)
- 本物の C212 と、拠点の外 (NAT 越し) からの視聴。TURN は無いので、対称型 NAT 同士では繋がらない (規約 v1 と同じ)
- device の認証 (規約 v1 と同じく cam-room は device を検証しない)
- 音声
