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

## 拠点 PC への入れ方 (Windows)

1. Actions の成果物 (またはタグの Release) の `alc-cam-windows.zip` を展開
2. `powershell -ExecutionPolicy Bypass -File install.ps1` (管理者不要。`%LOCALAPPDATA%\alc-cam` と HKCU に登録)
3. `chrome://extensions` → デベロッパーモード → 「パッケージ化されていない拡張機能を読み込む」→ `extension/`
4. ツールバーのアイコン → サイドパネル → 設定に拠点 ID・カメラの RTSP URL (stream2) を入れて保存 → 開始

見る側は alc-app の拠点カメラ画面で同じ拠点 ID を開く。

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
