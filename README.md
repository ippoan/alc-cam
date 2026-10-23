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
| `crates/cam-sd/` | カメラの SD 録画の一覧・取り出し・時刻の扱い (I/O を持たない) |
| `crates/cam-mp4/` | MP4 を再エンコードせずに切ってつなぐ (moov を読み、出力の頭と写す区間を返す。I/O を持たない) |
| `ingest/` | SD 録画の時間指定の切り出しを R2 に置く Worker `alc-cam-sd-ingest` (wasm32、別 workspace) |

## 拠点 PC への入れ方 (Windows)

1. Actions の成果物 (またはタグの Release) の `alc-cam-windows.zip` を展開
2. `powershell -ExecutionPolicy Bypass -File install.ps1` (管理者不要。`%LOCALAPPDATA%\alc-cam` と HKCU に登録)
3. `chrome://extensions` → デベロッパーモード → 「パッケージ化されていない拡張機能を読み込む」→ `extension/`
4. ツールバーのアイコン → サイドパネル → 設定に拠点 ID・カメラの RTSP URL (stream2) を入れて保存 → 開始

見る側は alc-app の拠点カメラ画面で同じ拠点 ID を開く。

## SD 録画の時間指定の切り出し (ingest/)

カメラの SD の録画 (約 4 分・約 100MB ごとの MP4) から、指定した時間帯を 1 本の MP4 にして R2 に置く
Worker `alc-cam-sd-ingest`。再エンコードはしない。

```
auth-worker (/api/cam-clips/) ─Service Binding─▶ alc-cam-sd-ingest ─Workers VPC (HTTP, ohishi-data の Tunnel)─▶ カメラの管理画面 (Digest)
                                     ├▶ R2 alc-cam-recordings/<カメラ名>/<種類>/<YYYYMMDD>/<HH>/<元ファイル>.mp4  (元ファイルの置き場)
                                     └▶ R2 alc-cam-recordings/clips/<カメラ名>/<開始>_<終了>.mp4             (切り出した 1 本)
```

```
POST /api/cam-clips/clips   {"from": "2026-10-23T12:58:00+09:00", "to": "2026-10-23T12:59:30+09:00"}
→ 201 {"key": "clips/TS-NS320W-55/20261023T125800_20261023T125930.mp4", "size": …,
       "actual_from": "2026-10-23T12:57:58.257+09:00", "actual_to": "…", "sources": [元ファイル…], "reused": false}
GET  /api/cam-clips/clips/TS-NS320W-55/20261023T125800_20261023T125930.mp4   (Range 対応 = シークできる)
```

- 時刻は ISO 8601。時差が無ければ JST。1 回 120 分まで (`MAX_CLIP_SECS`)。同じ時間帯は作り直さず 200 で返す
- 映像の頭は、時間帯の頭の直前のキーフレーム (約 2 秒おき) に寄る → `actual_from` が少し前になる
- 元ファイルの映像は 240 秒の区切りに対して約 238 秒しか無く、つなぎ目の約 1.7 秒の抜けは詰まる
- 手順: 重なる元ファイルを SD の一覧 (`camera-cgi/admin/sdcard.cgi`) で探し、R2 に無ければカメラから
  (`/action/stream?subject=recdownload`、パスに `/mnt/mmc`) 丸ごと取って置く → R2 の範囲読み出しで末尾の
  moov を読む → `crates/cam-mp4` で切ってつなぐ計画を立て → 16MiB ずつ R2 のマルチパートに写す
- 資格情報は Worker の secret `CAM_AUTH` (`user:pass`)。repo には無い
- 外から直接は届かない (workers_dev / route なし)。auth-worker の振り分け表 `ALC_BINDING_ROUTES` から届き、
  JWT の検証は auth-worker が済ませる。Worker は付け直された `X-User-Role` が `admin` か、`X-Tenant-ID` が
  `CAM_TENANT_ID` (カメラの持ち主。空なら確かめない) と同じかだけを見て、外れたら 403

確認 (VPC は実物のカメラ、R2 は手元の模擬):

```bash
cd ingest
printf 'CAM_AUTH="user:pass"\n' > .dev.vars   # 終わったら消す
npx wrangler@4.152.0 dev
curl -X POST localhost:8787/api/cam-clips/clips -H 'X-User-Role: admin' -d '{"from":"2026-10-23T12:58:00","to":"2026-10-23T12:59:30"}'
```

wrangler 4.58 では手元の dev から VPC に繋がらない (preview セッションの作成が 400)。
手元の dev から本物の R2 (`remote = true`) へ 100MB を流すと途中で切れるので、R2 は手元の模擬で確かめる。
手元のファイルだけで切り出しを試すなら `cargo run -p cam-mp4 --example clip -- 出力 from_ms to_ms start_ms:入力…`。

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
