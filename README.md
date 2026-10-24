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
- 配信の本体は拡張の background service worker (`extension/background.js`)。担当カメラが決まっていれば Chrome の起動と同時に cam-room に繋ぎ、
  パネルを開いていなくても、閉じても配信は続く。サイドパネルは状態の表示と「停止」だけ (やり取りは `chrome.storage`)
- 点呼の画面 (同じ PWA のメンテナンス) から、その端末の拡張の状態を読み、停止/開始・更新確認・担当カメラの取り直しができる (v0.4.2〜)。
  `{type:'cam_status'}` / `{type:'cam_control', action}` をパネルと同じ実装で受ける。形は [docs/extension-messages.md](docs/extension-messages.md)
- 拠点とカメラは手で入れない (v0.4.0〜)。点呼の PWA (`https://alc.ippoan.org`) が CoreS3 の署名で取った鍵
  (role `device-cam-host`、TTL 900 秒) を `chrome.runtime.sendMessage(拡張 ID, {type:'cam_host_token', token, exp})` で渡し
  (`externally_connectable`、送り主の origin も確かめる)、拡張がその鍵で auth-worker の
  `GET /device-data-proxy/api/cam-clips/cameras/mine` を呼んで先頭のカメラの `id` (= 部屋の名前) と RTSP の接続先を受ける。
  鍵はメモリだけ (cam-room に `?token=` で付けて入る。鍵の更新では今の接続を切らず、次に繋ぐときに使う)。
  カメラの写し (パスワード以外) は `storage.local`、パスワードは `storage.session` (メモリ) に置くので、鍵が切れても・
  service worker が止まっても配信は続く。Chrome の再起動と拡張の読み込み直し (自己更新) では `storage.session` が消えるので、
  次に鍵が届くまで「パスワード待ち」で止まる。v0.3.0 の手入力の設定が残っていれば DB から引けるまではそれで配り、引けたら消す
- 常時接続の保ち方: 20 秒ごとに `{"type":"ping"}` ちょうどを送る (cam-room は `setWebSocketAutoResponse` で `{"type":"pong"}` を返し、DO は hibernate したまま)。
  この送受信が service worker も生かす。pong が 60 秒来なければ繋ぎ直し、切れたら 1s→60s のバックオフ。
  service worker が止まっても 30 秒ごとの alarm で起き直し、設定から繋ぎ直す (状態をメモリに頼らない)。Chrome 116 以降
- 配信担当の順位 (v0.4.3〜): 同じカメラを複数の拠点 PC が担当するとき、`/cameras/mine` の `rank` (1 = 主。録画の画面の「配信担当 (順位)」) を
  cam-room に `?rank=N` で付けて入る。上位か同位の端末が配信中なら cam-room が受け入れてすぐ 4011 で閉じ、上位の端末が来れば今の接続を 4010 で切る。
  どちらも「待機中」になり、30〜60 秒 (乱数) おきに入り直す (上位が落ちていれば入れる = 引き継ぐ)。パネルと点呼の画面の状態に順位と待機が出る
- シグナリングは `cf-alc-signaling` の `/cam-room` を使う。部屋の名前は `alc_api.cameras.id` (担当カメラの id)。見る側は既存の alc-app のまま
- **1 台のカメラを最大 4 人が同時に見られる** (cam-room v2。DO が見る人ごとに `viewer_id` を付ける)。
  RTSP はカメラにつき 1 本で、最初の 1 人で開き最後の 1 人が離れたら TEARDOWN、映像は見る人ごとの PeerConnection へ複製する。
  上り帯域は 人数 × stream2。v2 に対応していない cam-room (古い DO) でも 1 人なら今までどおり映る
- 映像は Cloudflare を通らない (DO が中継するのは SDP の数通だけで、hibernation が効く)
- 規約: ippoan/alc-gw の `docs/camera-relay-convention.md`。拡張と host の間は [docs/host-protocol.md](docs/host-protocol.md)

## 構成

| パス | 中身 |
|---|---|
| `host/` | `alc-cam-host` (str0m で WebRTC、暗号は pure Rust)。`hub.rs` が見る人への複製、`update.rs` が host と拡張の自己更新 |
| `scripts/` | Release に付ける物 (host・拡張の zip・`alc-cam.json`・`alc-cam.msi`) を作る `pack-release.ps1`。MSI は `alc-cam.wxs` + `build-msi.ps1`、検査は `check-msi.ps1` |
| `crates/cam-rtsp/` | RTSP (Digest・TCP interleaved) → H.264 Annex B と音声 (AAC / G.711)。I/O を持たない |
| `extension/` | 拠点側の Chrome 拡張 (MV3)。配信は `background.js` (service worker)、`lib/config.js` が鍵の受け取りと担当カメラの取得、`panel.*` は状態表示。ID は `key` で `icmmpdjddlphkbokdalofdjpkcgfiggd` に固定 |
| `test/` | 偽カメラ・cam-room のローカル版・見る側のページと、それらを通す `e2e.mjs` |
| `crates/cam-sd/` | カメラの SD 録画の一覧・取り出し・時刻の扱い (I/O を持たない) |
| `crates/cam-mp4/` | MP4 を再エンコードせずに切ってつなぐ (moov を読み、出力の頭と写す区間を返す。I/O を持たない) |
| `crates/cam-ptz/` | 首振り (パン・チルト・プリセット) の命令の交通整理と CGI (HTTP Digest) の組み立て (I/O を持たない)。見る側とはデータチャネル `ptz` |
| `crates/cam-audio/` | 音声: カメラの AAC → PCMU (聞く)、見る側の PCMU → AAC (`audio/transmit.cgi` へ話す)、通話の順番 (1 人だけ・後から押した人が優先)。I/O を持たない。`docs/host-protocol.md` の「音声」 |
| `ingest/` | SD 録画の時間指定の切り出しを R2 に置く Worker `alc-cam-sd-ingest` (wasm32、別 workspace) |

## 拠点 PC への入れ方 (Windows)

**初回の 1 回だけ手で入れる (管理者権限は要らない)。以後は GitHub の Release を出すだけで、host も拡張も拠点 PC で勝手に新しくなる。**

### 初回

拠点で Chrome を使う Windows のユーザーで行う。

1. 最新 Release の `alc-cam.msi` をダブルクリックして入れる (Chrome は閉じておく)。host を `%LOCALAPPDATA%\alc-cam\alc-cam-host.exe` に置いて
   `HKCU` に native messaging として登録し、拡張を `%LOCALAPPDATA%\alc-cam\extension` に置く。最後の画面に次の 2・3 の手順と
   拡張のフォルダの実際のパス (欄から選んでコピーできる) が出て、「フォルダを開く」(既定 ON) のまま [完了] を押すとそのフォルダが開く
2. 以前に別の場所 (zip を展開したフォルダ等) から拡張を読み込んでいた PC は、`chrome://extensions` で「alc-cam 拠点中継」を
   **削除** する (そこは自動で更新されない)
3. `chrome://extensions` → 右上の「デベロッパー モード」を ON → 開いたフォルダ (`extension`) を `chrome://extensions` の画面へ
   ドラッグ&ドロップ (または「パッケージ化されていない拡張機能を読み込む」で最後の画面のパスを貼る)。ID が `icmmpdjddlphkbokdalofdjpkcgfiggd` であること。
   **デベロッパー モードは以後も OFF に戻さない** (OFF のまま拡張が読み込み直すと、Chrome が拡張を無効にする)
4. 録画の画面でこの拠点のカメラ (RTSP の接続先 = stream2) にこの PC の CoreS3 の鍵を紐づけておき、この Chrome で点呼の画面
   (alc.ippoan.org) を開く。鍵が届くと配信が始まり、サイドパネルの「担当カメラ」にカメラ名が出る (以後は Chrome と点呼の画面を
   開くだけで繋ぐ)。パネルは閉じてよい。止めるときはパネルの「停止」

見る側は alc-app の拠点カメラ画面で同じカメラを開く。外すとき: 「アプリ」の一覧から「alc-cam 拠点中継」をアンインストール (host の登録と `%LOCALAPPDATA%\alc-cam` を丸ごと消す) のあと `chrome://extensions` で削除。

### MSI と自己更新の両立

MSI (`scripts/alc-cam.wxs`、WiX 5.0.2 で perUser) は入れた後のファイルを host の自己更新に任せる。Windows Installer の修復で古い版に戻さないために:

- exe と拡張の部品は `NeverOverwrite`。KeyPath は「その版の MSI で入れた」印の `HKCU\Software\ippoan\alc-cam\Installed` の値
  (名前に版を含む)。修復 (`msiexec /f`、.msi の右クリックの「修復」) は印があるので入れ直さない = host が書き換えた版のまま。
  ファイルが KeyPath ではないので、中身が変わっても Windows Installer は壊れたと見なさない
- 自動修復の入口を作らない: ショートカット・広告 (advertise) なし、「アプリと機能」に修復・変更を出さない (`ARPNOREPAIR`・`ARPNOMODIFY`)。
  native messaging の登録と manifest の JSON の部品だけは普通の部品 (自己更新は触らないので修復で直ってよい)
- 新しい版の MSI は `MajorUpgrade` で前の版を丸ごと外してから入れる (印の名前が変わるので NeverOverwrite でも入る)。
  host が MSI より新しい版に自己更新していても MSI の版に戻るが、次の更新確認で Release の版に上がり直す。
  古い版の MSI は「新しい版が入っています」で止まる。同じ版の MSI をもう一度入れても何もしない
- アンインストールは `util:RemoveFolderEx` で `%LOCALAPPDATA%\alc-cam` を丸ごと消す (自己更新の `.old`・`.new-*`・`extension.old-*` も)

CI (`windows` job) の `scripts/check-msi.ps1` が、表 (ファイル・HKCU・perUser・NeverOverwrite・ショートカットなし) を読んだあと
runner のユーザーに入れ、自己更新の書き換えを真似て `msiexec /fa` しても戻らない・版を 1 つ上げた MSI で上がって残りが消える・
古い MSI では下がらない・外すと全部消える、を確かめる。

### 以後の更新 (Release を出すだけ)

拡張が 6 時間ごと (Chrome の起動から 10 分後が最初) とパネルの「今すぐ確認」で host に `update_check` を送る
(host が動いていなければ確認のためだけに起動して、答えが来たら止める)。host は最新 Release の `alc-cam.json`
(`{"version","host_sha256","extension_sha256"}`) を見て、

- **host**: 自分より新しければその版の `alc-cam-host.exe` を落とし、SHA256 が合えば実行中の exe を `.old` に退けて置き換える (新版は host の次の起動から)
- **拡張**: `%LOCALAPPDATA%\alc-cam\extension\manifest.json` より新しければ `alc-cam-extension.zip` を落として SHA256 を確かめ、
  隣の一時フォルダに展開して manifest の版と key (= 拡張 ID) を確かめてから、フォルダごと rename で入れ替える。
  **見る人が居る間は入れ替えない** (次の確認に回す)。拡張はディスクの manifest.json の版が動いている版と違えば `chrome.runtime.reload()` で読み込み直す

どこで失敗しても今の版のまま動く (2 つ目の rename が失敗したら 1 つ目を戻す)。出る先は github.com と `*.githubusercontent.com` の https だけ
(redirect 先も確かめる)。パネルの「host 更新」に拡張と host の版と結果が出る。

版を上げるとき: `host/Cargo.toml` と `extension/manifest.json` の `version` を揃えて上げ、merge 後に `v<版>` のタグを手で打つ
(GITHUB_TOKEN で打ったタグは workflow を起動しない)。タグの CI (`windows` job) が `scripts/pack-release.ps1` で
`alc-cam-host.exe`・`alc-cam-extension.zip`・`alc-cam.json` と初回用の `alc-cam.msi` を作り、Release に付ける。
拡張と host は別々の時点で入れ替わることがあるので、メッセージは後方互換に保つ (知らない `type` は双方とも捨てる)。

Chrome のポリシー (`ExtensionSettings` の force_installed + CRX) で配る方式は採らない: Windows の Chrome が Web Store 外の拡張を
force_installed するのは AD / Entra 参加か Chrome Enterprise Core 登録の PC だけで、拠点の PC では `[BLOCKED]` になるため。

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
- カメラの宛先 (VPC Service の binding 名)・ユーザー名・パスワード (暗号化) は DB `alc_api.cameras` の `rec_*` 列に持ち、
  暗号の鍵は Secrets Store に置く。repo には無い
- 外から直接は届かない (workers_dev / route なし)。auth-worker の振り分け表 `ALC_BINDING_ROUTES` から届き、
  JWT の検証は auth-worker が済ませる。Worker は付け直された `X-User-Role` が `admin` か、`X-Tenant-ID` が
  `CAM_TENANT_ID` (カメラの持ち主。空なら確かめない) と同じかだけを見て、外れたら 403

手元での確認は `ingest/README.md` の `wrangler dev --env local` の節を見る。

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
node e2e-priority.mjs   # 拠点 PC 2 台 (rank 1 / 2) で待機・引き継ぎ・1 位への戻り
```

`e2e-priority.mjs` が確かめること: 1 位が配信し 2 位は待機 (状態に順位と待機) / 待機は入り直しても見る人の接続を揺らさない /
1 位を停止すると 2 位が入り直して引き継ぎ、見る人に映る / 1 位を開始すると 2 位を 4010 で切って 1 位に戻り、見る人に再び映り、2 位は待機に戻る。

`e2e.mjs` が確かめること: 拠点もカメラも決まらないうちは繋がない / v0.3.0 の手入力の設定が残っていればそれで繋ぐ /
偽の点呼の PWA (`https://alc.ippoan.org` を route で差し替え) が渡す鍵で偽の `/cameras/mine` を引き、担当カメラの部屋に `?token=` 付きで入る
(他の origin には `sendMessage` が生えない・0 台なら理由を出して待つ・ディスクにパスワードも鍵も手入力の URL も残らない・
URL に入れると壊れる文字のパスワードが通る) / 鍵の更新では繋ぎ直さない / パネルを開かずに cam-room に繋ぐ / 見る人が来るまでカメラにも host にも触らない /
Digest 認証 / 見る側で H.264 がデコードされる / 2 人目が途中から入っても映り、そのあいだ RTSP は 1 本 /
片方が離れてももう片方は流れ続ける / 最後の 1 人が離れたらカメラへ TEARDOWN し host も止める / 入り直しで再び映る /
パネルの状態表示と、パネルを閉じても映り続けること / keepalive の ping が `{"type":"ping"}` ちょうど /
service worker を CDP で止めても起き直して (鍵無しで) 写しから繋ぎ直し、見ている人に再び映る /
DB の担当カメラが変わったら次の鍵で繋ぎ直す (v1 の部屋でも 1 人なら映る) /
見る人が居ないときの「今すぐ確認」で確認のためだけに host を起こし、答えがパネルに出て host が止まる (Linux の host は入れ替えないので unsupported) /
ディスクの拡張の manifest.json の版が上がっていたら確認の答えで読み込み直し、パスワード待ちを経て次の鍵で cam-room に繋ぎ直す (拡張は写しを読み込み、デベロッパー モードは ON) /
「停止」で cam-room から抜ける。`signaling.mjs` は cam-room の v2 (と `v1Sites` に入れた部屋の v1)、配信担当の順位 (4010 / 4011)、
keepalive の自動応答 (完全一致の ping にだけ pong) を写している。

## 未確認・未対応

- Windows の実機 (alc-cam.msi の画面と Chrome を開いたままの上書き、host.exe、host の exe と拡張のフォルダの入れ替え。Chrome が読み込んでいるフォルダを rename できるか)。
  入れ替えの手順は `host/src/update.rs` の単体テスト、拡張の読み込み直しは e2e、GitHub の redirect の辿り方は
  `cargo test -p alc-cam-host -- --ignored real_github` で確かめたビルドは CI の windows-latest で行う (この repo の Linux 環境に MinGW が無い)
- 本物の C212 と、拠点の外 (NAT 越し) からの視聴。TURN は無いので、対称型 NAT 同士では繋がらない (規約 v1 と同じ)
- device の認証: 拡張は `?token=` を付けるが、検証 (role `device-cam-host` の introspect) は cf-alc-signaling 側で移行期間は無しも通す。
  必須化の後は、鍵の無い繋ぎ直し (service worker の起き直し) は次の鍵が届くまで入れない
- 本物の `/cameras/mine` と点呼の PWA からの鍵 (e2e は偽物)。v0.3.0 → v0.4.0 の自己更新で `host_permissions` /
  `externally_connectable` が増えた manifest を、パッケージ化されていない拡張の読み込み直しが黙って受け入れるか (Windows の実機)
- 音声
