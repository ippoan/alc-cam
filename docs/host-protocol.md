# 拡張 ⇄ alc-cam-host のメッセージ

Chrome の native messaging (4 バイトの長さ + UTF-8 JSON、`host/src/native.rs`)。
host は拡張の background service worker が、見る人が来た時に `connectNative('org.ippoan.alc_cam_host')` して起動し、
最後の見る人が離れて約 5 秒後に port を切る (stdin が閉じる) と終わる。service worker が止まった時も stdin が閉じて終わり、
起き直した service worker が見る人の居る部屋に繋ぎ直せば、また起動する。stdout にはこの枠以外を書かない (ログは stderr)。

1 台のカメラを複数人 (上限 4) が同時に見られる。見る人は cam-room の DO が付ける `viewer_id` で区別する
(cam-room の v2。拡張は `?role=device&proto=2` で入る)。`viewer_id` が無い (古い拡張・v1 の部屋) ときは
`""` を 1 人ぶんの id として同じ道を通る。

## 拡張 → host

| type | 中身 | いつ |
|---|---|---|
| `config` | `rtsp_url`, `stun` (`host:port`) | 起動直後に 1 回 |
| `start` | `viewer_id` | cam-room で `peer_joined {role: admin, viewer_id}` を受けた時 (見る人が来た)。同じ id なら作り直し |
| `stop` | `viewer_id` | `peer_left {role: admin, viewer_id}` の時。cam-room の WebSocket が切れた時は居た全員ぶん |
| `sdp_answer` | `viewer_id`, `sdp` | cam-room から answer が来た時 |
| `ice_candidate` | `viewer_id`, `candidate` (RTCIceCandidateInit) | 見る側が trickle で送ってきた時 (任意。読めなければ捨てる) |
| `update_check` | — | 6 時間ごととパネルの「今すぐ確認」。host が動いていなければ拡張はこれだけのために起動し (`config` は送らない)、`update` の答えが来たら切る |

上限を超える `start`、知らない `viewer_id` の `sdp_answer` / `ice_candidate` は無視して `log` を返す。

## host → 拡張

| type | 中身 | 意味 |
|---|---|---|
| `ready` | `version` | 起動した。拡張は居る見る人ごとに `start` を送り直す (host が落ちて起動し直した時のため) |
| `sdp_offer` | `viewer_id`, `sdp` | 拡張は cam-room へ `sdp_offer {viewer_id, sdp}` として送る (`viewer_id` が `""` なら付けない)。**候補は全部入っている (non-trickle)** |
| `camera` | `ok`, `message` | カメラ (RTSP は 1 本) の状態 (受信中 / 再接続待ち / 停止)。文言に資格情報・宛先は入れない |
| `peer` | `viewer_id`, `ok`, `message` | その見る人との接続の状態 (answer 待ち / 接続 (P2P) / 失敗 / 離れた) |
| `log` | `message` | 診断用 |
| `update` | `state`, `current`, `latest`, `message`, `extension_deferred` | 自己更新 (`host/src/update.rs`、host の exe と拡張のフォルダ)。`state` は `checking` → `latest` (最新) / `installed` (`latest` の版を入れた。host は次の起動から、拡張はディスクの manifest.json の版が変わるので拡張が読み込み直す) / `deferred` (見る人が居るので拡張は後で) / `failed` (今の版のまま) / `unsupported` (Windows 以外)。`message` は人が読む説明。`extension_deferred` (0.4.5〜): 新しい拡張を見る人が居る間は待たせた (拡張は 2 分後に確認し直す) |

## host の中の流れ

見る人ごと (`start` 1 回ぶん、`host/src/peer.rs`):

1. カメラへ向かう経路で LAN の IP を決め、その IP の UDP を 1 本開く (見る人ごとに別の UDP)
2. STUN で外から見えるアドレスを 1 回聞く (取れなければ LAN 内だけで繋がる)
3. host 候補 + srflx 候補を入れた offer (映像 H.264 sendonly + 音声 PCMU sendrecv + データチャネル `ptz`) を作って返す。
   音声は映像と同じ stream id (`alc-cam`) なので、`event.streams[0]` を video に付けるだけの見る側でも映像は変わらない
4. answer を受け、ICE が繋がったら H.264 をそのまま (再エンコードなし) 送る。カメラの音は PCMU 20ms ごと (下の「音声」)
5. `stop` / 切断 / 30 秒繋がらない → その人の分だけ畳む。他の人には響かない

カメラ (`host/src/camera.rs`・`host/src/hub.rs`):

- RTSP はカメラにつき 1 本 (`crates/cam-rtsp`、Digest 認証、TCP interleaved)。最初の見る人の `start` で開き、
  最後の見る人が居なくなったらカメラへ TEARDOWN を送って畳む
- 届いたフレームは、繋がっている見る人全員の PeerConnection へ複製して書く
- 直近の GOP (キーフレームから) を 1 つ持つ。後から繋がった見る人にはまずそれを流し、以後は他の人と同じフレームを流す。
  キーフレームの AU は先頭に SPS/PPS を必ず含むので、途中参加でもデコーダが組める。
  GOP が無ければ (キーフレームがまだ来ていなければ) 次の IDR から流す

カメラ中継規約 (ippoan/alc-gw の `docs/camera-relay-convention.md`) との対応:
映像は H.264 パススルー (規約の「映像のみ」に対し、0.4.4 から音声を足した)・STUN のみ (TURN なし)・device 側 non-trickle・見る人の出入りで
RTSP と PeerConnection を起こす/畳む、はすべて同じ。違いは device (拡張の service worker) が Chrome の動いている間だけ居ることと、
cam-room v2 で見る人を複数持てることだけ。上り帯域は 人数 × stream2 になる。

## 見る側 ⇄ host: 首振り (データチャネル `ptz`)

host の offer には映像のほかにデータチャネル `ptz` (ordered・reliable) が 1 本入っている。繋がると見る側の
`RTCPeerConnection` に `datachannel` イベントで届く。映像と同じ P2P の経路で、**拡張・cam-room は関わらない**
(cam-room に入れる人 = admin と運行管理者席は全員操作できる)。中身は 1 通 1 個の JSON 文字列。
首振りより前の host は `ptz` を出さないので、見る側は届かなければボタンを出さない。

host → 見る側:

| event | 中身 | いつ |
|---|---|---|
| `caps` | `dirs` (`["up","down","left","right"]`), `home` (bool), `presets` (`[1,2,3,4]`), `zoom` (bool) | チャネルが開いた直後に 1 回。見る側はこれを見てボタンを出す |
| `result` | `action`, `ok`, `unsupported`, `message` | 命令の結果。命令した人にだけ返す |

```json
{"type":"ptz","event":"caps","dirs":["up","down","left","right"],"home":true,"presets":[1,2,3,4],"zoom":false}
{"type":"ptz","event":"result","action":"move","ok":true,"unsupported":false,"message":""}
```

見る側 → host (`type` は `ptz`):

| action | 中身 | 意味 |
|---|---|---|
| `move` | `dir` (`up` / `down` / `left` / `right`) | その向きへ動かす。**押している間 300ms ごとに再送する** (1 秒再送が無ければ host が止める) |
| `stop` | — | 離した。自分の `move` だけ止める (後から他の人が動かしていればそちらは止めない) |
| `home` | — | ホームポジションへ |
| `preset` | `preset` (1〜4) | プリセット位置へ (位置の登録はカメラの管理画面で) |
| `zoom` | `dir` (`in` / `out`) | このカメラは非対応。`unsupported: true` を返す |

`result` の返し方: `move` は押し始め (向きを変えた時も) の 1 回目のステップの成功と、すべての失敗を返す。
`stop` はすぐ `ok`。`home` / `preset` はカメラの応答で返す。読めない命令は `ok: false` と理由を返す。
`unsupported: true` はカメラがその CGI を持たない (404) か、ズーム。`message` に資格情報・宛先は入れない。

host の中 (`host/src/ptz.rs`、交通整理は `crates/cam-ptz`):

- カメラの管理画面 (RTSP URL と同じホストの 80 番、同じアカウント、HTTP Digest) の公開仕様の CGI を叩く:
  方向は `POST /camera-cgi/com/ptz.cgi?move=<dir>&stepSize=1` (1 回で 1 ステップ動く形なので、押している間は
  host がステップを繰り返す)、ホームは `move=home`、プリセットは `POST /camera-cgi/com/ptzconfig.cgi?movepresetpoint=<n>`
- カメラへの要求は常に 1 本ずつ (前の応答を待つ。仕様の「API は同時 3 セッションまで」と RTSP の分を食わないため)。
  ステップの間は最短 200ms。見る人 4 人の命令は最後に来たもの優先 (押し続けの再送は間引かれ、まだ送っていない
  プリセットは後の命令で上書きされる)。失敗したら押し続けでも繰り返さない。見る人が離れたらその人の命令は捨てる
- e2e の偽カメラだけ、管理画面の port を環境変数 `ALC_CAM_HTTP_PORT` で変える

## 音声 (カメラのマイクとスピーカー、0.4.4〜)

聞く (カメラのマイク → 見る人全員) と話す (見る人 1 人の声 → カメラのスピーカー) を、映像と同じ PeerConnection の
音声 1 本 (PCMU・8kHz・sendrecv) で運ぶ。拡張・cam-room は関わらない。0.4.3 までの host の offer には音声が無いので、
見る側は音声のトラックが無ければ音のボタンを、`talk` の `caps` が来なければ「押して話す」を出さない。

形式 (I-O DATA の公開仕様 iodata-solution/qwatch-api の `QwatchAPI_Specification.html`。対象は TS-NS310W 等で、
TS-NS320W も同じとみなしている):

| 向き | カメラ側 | WebRTC 側 | 変換 (`crates/cam-audio`、pure Rust) |
|---|---|---|---|
| 聞く | RTSP の音声トラック。仕様は「音声 AAC (32kHz)」。SDP の `m=audio` の `a=rtpmap:<pt> MPEG4-GENERIC/<rate>` と `a=fmtp: … config=<AudioSpecificConfig>` で見分ける | PCMU 8kHz | AAC (RFC 3640 AAC-hbr の AU) をデコード → モノラル → 8kHz → μ-law。G.711 (PCMU / PCMA) のカメラはそのまま 8kHz の μ-law に |
| 話す | `GET /camera-cgi/audio/transmit.cgi` (Keep-Alive、HTTP Digest、管理画面の port = 80)。受ける形式は応答の `Content-Type` で `audio/aac-lc` (ADTS・8kHz) / `audio/aac` (ADTS・16kHz) / `audio/gsm` | PCMU 8kHz | μ-law → 8kHz / 16kHz → AAC-LC (ADTS、16 / 24kbps、TNS なし)。GSM は非対応 (仕様では TS-NS310W は AAC のみ) |

- RTSP: 映像の SETUP のあとに音声のトラックも同じセッションで SETUP する (interleaved 2-3)。カメラが断ったら (2xx 以外・
  映像と同じ番号を返した) 音声なしで PLAY する。AAC のデコーダは filmcraft-aac、LATM (`MP4A-LATM`) と HE-AAC は使わない
- 聞く: 音は映像を流し始めた人 (GOP を渡した人) 全員へ同じものを書く。送れない人がいても外さない (映像は続ける)
- 話す: `transmit.cgi` は 401 なら Digest を付けて**繋ぎ直して**送り直す (仕様: 一部の版は切断・再接続しないと受けない)。
  `200 OK` の後ろ (ヘッダか本文) に `Content-Type: audio/…` が来たら送り始める。1.5 秒待っても来なければ `audio/aac` とみなす。
  `Too many users` は他の誰か (カメラのアプリ等) が通話中。1 回に書くのは 330 バイトまで (仕様の受信の上限)。
  誰かが話している間だけ接続を 1 本保ち、話す人が替わっても繋ぎ直さない。誰も話さなくなったら切る (= カメラ側の通話も終わる)

### 見る側 ⇄ host: 通話の順番 (データチャネル `ptz` の `type: "talk"`)

首振りと同じデータチャネル `ptz` に `type` が `talk` のメッセージを載せる (首振りの見る側は `type` が `ptz` でないものを捨てる)。

見る側 → host:

| action | 意味 |
|---|---|
| `start` | 押した。**押している間 500ms ごとに送り直す** (1.5 秒送り直しが無ければ host は離したとみなす) |
| `stop` | 離した。今話しているのが自分のときだけ効く |

```json
{"type":"talk","action":"start"}
```

声は、押している間だけ音声の送り口 (`RTCRtpSender`) にマイクのトラックを付けて送る (離したら `replaceTrack(null)`)。
answer で音声を `sendrecv` にしておけば付け外しに再ネゴシエーションは要らない。host は話している人以外の声を捨てる。

順番 (`cam_audio::Floor`): 同時に話せるのは 1 人。**後から押した人が優先**。外された人は押したまま (送り直し) では取り返さず、
離して押し直すと取り返す。カメラ側で話せなかった人 (`result` の `ok: false`) も押し直すまで受けない。

host → 見る側:

| event | 中身 | いつ |
|---|---|---|
| `caps` | `speak: true` | チャネルが開いた直後に 1 回 (首振りの `caps` の直後)。見る側はこれを見て「押して話す」を出す |
| `state` | `talking` (誰かが話している), `mine` (受け取った人自身が話している) | 話す人が替わるたびに全員へ |
| `result` | `ok`, `busy` (他の誰かがカメラで通話中), `message` | 話している人へ、カメラのスピーカーに繋がった (`ok: true`、`message` に形式) か、繋がらなかった・切れたか |

```json
{"type":"talk","event":"caps","speak":true}
{"type":"talk","event":"state","talking":true,"mine":false}
{"type":"talk","event":"result","ok":false,"busy":true,"message":"他の人がカメラで通話中です"}
```

e2e の偽カメラ (`test/fake-rtsp.mjs` / `test/fake-cgi.mjs`) は AAC 32kHz の音声トラック (`test/fixture.aacbin`、
`cargo run -p cam-audio --example aac-fixture -- test/fixture.aacbin` で作り直せる) と `transmit.cgi` (Digest・`audio/aac`・
2 本目は `Too many users`) を持つ。見る側 (`test/viewer.html`) は Chrome の偽マイクで話す。

## 見る側からの自己更新の確認 (データチャネル `ptz` の `type: "update"`、0.4.5〜)

管理者の画面 (カメラ一覧・遠隔点呼のカメラ欄) から、拠点 PC の自己更新を頼める。中身は拡張の「今すぐ確認」(`update_check`) と同じ
`update::check` で、経過・結果は拡張 (`update`) と見る人全員へ返す。0.4.4 までの host は `caps` を出さないので、見る側はボタンを出さない。

| 向き | メッセージ | いつ |
|---|---|---|
| host → 見る側 | `{"type":"update","event":"caps","host":"0.4.5","extension":"0.4.5"}` (`extension` は host の隣の `extension\manifest.json` の版。読めなければ null) | チャネルが開いた直後 (首振り・通話の `caps` の後) |
| 見る側 → host | `{"type":"update","action":"check"}` | 「更新を確認」を押した。確認中に重ねて来たものは捨てる |
| host → 見る側 | `{"type":"update","event":"state","state","current","latest","message"}` (`state` は拡張への `update` と同じ: checking / latest / installed / deferred / failed / unsupported) | 確認の経過・結果。見る人全員へ |

- host の exe は入れ替えるだけで、新しい版で動くのは次の起動から (見る人が全員抜けて host が終わり、次に誰かが繋いだとき)
- 拡張は見る人が居る間は入れ替えない (`Part::Deferred`)。そのとき host → 拡張の `update` に `extension_deferred: true` を付け、
  拡張は 2 分後に確認し直す (まだ見る人が居ればまた待ち)。見る人が居なくなった後の確認で入れ替わり、拡張が読み込み直す
- 入れ替え済みの exe と同じもの (SHA256 が同じ) は落とし直さない (待ちの間の確認し直しで 5MB を繰り返し落とさない)
