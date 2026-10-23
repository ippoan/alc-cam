# alc-cam

拠点のカメラ (Tapo C212 等の RTSP) を、**Chrome が動いている間ずっと** (拡張の background service worker が常時 cam-room に居て)
WebRTC (P2P) で見る側へ届ける。alc-gw (Go の常駐アプリ) を置かない拠点向けの、カメラ中継規約の 3 つ目の device 実装。

```
カメラ ─RTSP─▶ alc-cam-host (Rust, native messaging) ══WebRTC P2P (H.264 そのまま)══▶ 見る側
                     ▲ stdin/stdout                                                    (alc-app の TenkoCameraView)
                     │                                                                  ▲
      Chrome 拡張 (background service worker) ──WebSocket── cf-alc-signaling /cam-room/<カメラID> ──┘
                                              (SDP だけ。role=device&proto=2)
```

- 拡張は TCP を張れないので、RTSP と WebRTC は Rust の host が持つ。host は見る人が来たときに拡張が起動し、最後の 1 人が離れると終わる (サービス常駐しない)
- 配信の本体は拡張の background service worker (`extension/background.js`)。設定が保存済みなら Chrome の起動と同時に cam-room に繋ぎ、
  パネルを開いていなくても、閉じても配信は続く。サイドパネルは設定と状態の表示だけ (やり取りは `chrome.storage`)
- 常時接続の保ち方: 20 秒ごとに `{"type":"ping"}` ちょうどを送る (cam-room は `setWebSocketAutoResponse` で `{"type":"pong"}` を返し、DO は hibernate したまま)。
  この送受信が service worker も生かす。pong が 60 秒来なければ繋ぎ直し、切れたら 1s→60s のバックオフ。
  service worker が止まっても 30 秒ごとの alarm で起き直し、設定から繋ぎ直す (状態をメモリに頼らない)。Chrome 116 以降
- シグナリングは `cf-alc-signaling` の `/cam-room` を使う。部屋の名前は `alc_api.cameras.id` (拡張の「拠点 ID」に入れる)。見る側は既存の alc-app のまま
- **1 台のカメラを最大 4 人が同時に見られる** (cam-room v2。DO が見る人ごとに `viewer_id` を付ける)。
  RTSP はカメラにつき 1 本で、最初の 1 人で開き最後の 1 人が離れたら TEARDOWN、映像は見る人ごとの PeerConnection へ複製する。
  上り帯域は 人数 × stream2。v2 に対応していない cam-room (古い DO) でも 1 人なら今までどおり映る
- 映像は Cloudflare を通らない (DO が中継するのは SDP の数通だけで、hibernation が効く)
- 規約: ippoan/alc-gw の `docs/camera-relay-convention.md`。拡張と host の間は [docs/host-protocol.md](docs/host-protocol.md)

## 構成

| パス | 中身 |
|---|---|
| `host/` | `alc-cam-host` (str0m で WebRTC、暗号は pure Rust)。`hub.rs` が見る人への複製、`update.rs` が自己更新。`install.ps1` / `install-admin.ps1` は Windows への初回の登録 |
| `scripts/` | Release に付ける物を作る `pack-release.ps1` (CRX の署名・`update.xml`・host の版情報) と、CRX の拡張 ID を確かめる `crx-id.mjs` |
| `crates/cam-rtsp/` | RTSP (Digest・TCP interleaved) → H.264 Annex B。I/O を持たない |
| `extension/` | 拠点側の Chrome 拡張 (MV3)。配信は `background.js` (service worker)、`panel.*` は設定と状態表示。ID は `key` で `icmmpdjddlphkbokdalofdjpkcgfiggd` に固定 |
| `test/` | 偽カメラ・cam-room のローカル版・見る側のページと、それらを通す `e2e.mjs` |
| `crates/cam-sd/` | カメラの SD 録画の一覧・取り出し・時刻の扱い (I/O を持たない) |
| `crates/cam-mp4/` | MP4 を再エンコードせずに切ってつなぐ (moov を読み、出力の頭と写す区間を返す。I/O を持たない) |
| `crates/cam-ptz/` | 首振り (パン・チルト・プリセット) の命令の交通整理と CGI (HTTP Digest) の組み立て (I/O を持たない)。見る側とはデータチャネル `ptz` |
| `ingest/` | SD 録画の時間指定の切り出しを R2 に置く Worker `alc-cam-sd-ingest` (wasm32、別 workspace) |

## 拠点 PC への入れ方 (Windows)

**初回の 1 回だけ手で入れる。以後は GitHub の Release を出すだけで、拡張も host も拠点 PC で勝手に新しくなる。**

### 初回 (管理者の確認が 1 回出る)

0. 前提: Chrome が **管理された PC** であること (下の「管理されていない PC」を参照)。拠点ではふだん使う Windows のユーザーで行う
1. 最新 Release の `alc-cam-windows.zip` を展開する
2. 手で読み込んだ (パッケージ化されていない) alc-cam の拡張が入っている PC は、先に `chrome://extensions` で
   「alc-cam 拠点中継」を **削除** する (同じ ID のまま残るとポリシーの版と取り合いになる。install.ps1 も見つけたら止まって促す。
   設定 `chrome.storage.local` は削除で消えるので、拠点 ID と RTSP URL を控えておく)
3. `install.cmd` をダブルクリック (= `powershell -ExecutionPolicy Bypass -File install.ps1`)。中身は 2 段:
   - 今のユーザーの権限で host を `%LOCALAPPDATA%\alc-cam` に置き、`HKCU` に native messaging として登録する
   - 管理者の確認 (UAC) が出て、`install-admin.ps1` が Chrome のポリシーを `HKLM` (64bit) に書く:
     `SOFTWARE\Policies\Google\Chrome\ExtensionSettings\icmmpdjddlphkbokdalofdjpkcgfiggd` に
     `installation_mode = force_installed`・`update_url = https://github.com/ippoan/alc-cam/releases/latest/download/update.xml`
4. Chrome を完全に終了して起動し直す → 拡張が「管理者によってインストール」されて入る (`chrome://policy` で `ExtensionSettings` が OK になっていること)
5. ツールバーのアイコン → サイドパネル → 設定に拠点 ID (= `alc_api.cameras.id`)・カメラの RTSP URL (stream2) を入れて保存。
   保存した時点で配信が始まる (以後は Chrome を起動するだけで繋ぐ)。パネルは閉じてよい。止めるときはパネルの「停止」

見る側は alc-app の拠点カメラ画面で同じカメラを開く。Chrome に「組織によって管理されています」と出て、拡張は Chrome から削除できなくなる。

外すとき: `uninstall.cmd` (= `install.ps1 -Uninstall`。ポリシーと host の登録を消す。Chrome を起動し直すと拡張も消える)。

### 管理されていない PC (WORKGROUP の Windows)

Windows の Chrome が **Web Store 外の拡張を force_installed で入れるのは、AD / Entra に参加しているか
Chrome Enterprise Core (無料) に登録された PC だけ**。それ以外ではレジストリが正しくても `chrome://policy` に `[BLOCKED]` と出て入らない
(`install-admin.ps1` は `dsregcmd /status` を見て、そうなら警告する)。Google 管理コンソールで Chrome Enterprise Core の
登録トークンを発行し、初回に `install.ps1 -EnrollmentToken <トークン>` で入れると `CloudManagementEnrollmentToken` も書く。
手で読み込む開発用の入れ方は `install.ps1 -NoPolicy` (host だけ) → `chrome://extensions` → デベロッパーモード → `extension/` を読み込む (自動更新は host だけ)。

### 以後の更新 (Release を出すだけ)

- **拡張**: Chrome がポリシーの `update_url` (= 最新 Release の `update.xml`) を数時間ごとに見て、版が上がっていれば
  その版の Release の `alc-cam.crx` を入れ替える (`chrome://extensions` の「更新」で今すぐにもできる)
- **host**: 拡張が 6 時間ごと (Chrome の起動から 10 分後が最初) とパネルの「今すぐ確認」で host に `update_check` を送る。
  host は最新 Release の `alc-cam-host.json` (`{"version","sha256"}`) を見て、自分より新しければその版の
  `alc-cam-host.exe` を落とし、SHA256 が合えば実行中の exe を `.old` に退けて置き換える (新版は host の次の起動から。
  見る人が居なければ確認のためだけに起動して終わる)。どこで失敗しても今の版のまま動く。出る先は github.com と
  `*.githubusercontent.com` の https だけ。パネルの「host 更新」に今の版と結果が出る

版を上げるとき: `host/Cargo.toml` と `extension/manifest.json` の `version` を揃えて上げ、merge 後に `v<版>` のタグを打つ
(タグは手で打つ。GITHUB_TOKEN で打ったタグは workflow を起動しない)。タグの CI (`windows` job) が
`scripts/pack-release.ps1` で拡張を CRX に署名し (鍵は repo の Secret `ALC_CAM_EXTENSION_KEY`、manifest の `key` の秘密鍵。
ID が `icmmpdjddlphkbokdalofdjpkcgfiggd` でなければ止まる)、`alc-cam.crx`・`update.xml`・`alc-cam-host.exe`・
`alc-cam-host.json`・`alc-cam-windows.zip` を Release に付ける。拡張と host の版は別々に届くので、どちらが先に新しくなっても
動くようにメッセージは後方互換に保つ (知らない `type` は双方とも捨てる)。署名鍵を失うと拡張 ID が変わり、全拠点で入れ直しになる。

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

`e2e.mjs` が確かめること: パネルを開かずに設定だけで cam-room に繋ぐ / 見る人が来るまでカメラにも host にも触らない /
Digest 認証 / 見る側で H.264 がデコードされる / 2 人目が途中から入っても映り、そのあいだ RTSP は 1 本 /
片方が離れてももう片方は流れ続ける / 最後の 1 人が離れたらカメラへ TEARDOWN し host も止める / 入り直しで再び映る /
パネルの状態表示と、パネルを閉じても映り続けること / keepalive の ping が `{"type":"ping"}` ちょうど /
service worker を CDP で止めても起き直して繋ぎ直し、見ている人に再び映る / 設定を変えたら繋ぎ直す (v1 の部屋でも 1 人なら映る) /
見る人が居ないときの「今すぐ確認」で確認のためだけに host を起こし、答えがパネルに出て host が止まる (Linux の host は入れ替えないので unsupported) /
「停止」で cam-room から抜ける。`signaling.mjs` は cam-room の v2 (と `v1Sites` に入れた部屋の v1)、
keepalive の自動応答 (完全一致の ping にだけ pong) を写している。

## 未確認・未対応

- Windows の実機 (install.ps1・install-admin.ps1 と host.exe、ポリシーでの拡張の入り方と更新、host の exe の入れ替え)。
  CRX の梱包・ID の検査・update.xml は手元の Linux の Chrome で、GitHub の redirect の辿り方は
  `cargo test -p alc-cam-host -- --ignored real_github` で確かめた。ビルドは CI の windows-latest で行う (この repo の Linux 環境に MinGW が無い)
- 本物の C212 と、拠点の外 (NAT 越し) からの視聴。TURN は無いので、対称型 NAT 同士では繋がらない (規約 v1 と同じ)
- device の認証 (規約 v1 と同じく cam-room は device を検証しない)
- 音声
