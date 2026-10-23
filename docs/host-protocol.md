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

上限を超える `start`、知らない `viewer_id` の `sdp_answer` / `ice_candidate` は無視して `log` を返す。

## host → 拡張

| type | 中身 | 意味 |
|---|---|---|
| `ready` | `version` | 起動した。拡張は居る見る人ごとに `start` を送り直す (host が落ちて起動し直した時のため) |
| `sdp_offer` | `viewer_id`, `sdp` | 拡張は cam-room へ `sdp_offer {viewer_id, sdp}` として送る (`viewer_id` が `""` なら付けない)。**候補は全部入っている (non-trickle)** |
| `camera` | `ok`, `message` | カメラ (RTSP は 1 本) の状態 (受信中 / 再接続待ち / 停止)。文言に資格情報・宛先は入れない |
| `peer` | `viewer_id`, `ok`, `message` | その見る人との接続の状態 (answer 待ち / 接続 (P2P) / 失敗 / 離れた) |
| `log` | `message` | 診断用 |

## host の中の流れ

見る人ごと (`start` 1 回ぶん、`host/src/peer.rs`):

1. カメラへ向かう経路で LAN の IP を決め、その IP の UDP を 1 本開く (見る人ごとに別の UDP)
2. STUN で外から見えるアドレスを 1 回聞く (取れなければ LAN 内だけで繋がる)
3. host 候補 + srflx 候補を入れた offer (映像のみ・H.264・sendonly) を作って返す
4. answer を受け、ICE が繋がったら H.264 をそのまま (再エンコードなし) 送る
5. `stop` / 切断 / 30 秒繋がらない → その人の分だけ畳む。他の人には響かない

カメラ (`host/src/camera.rs`・`host/src/hub.rs`):

- RTSP はカメラにつき 1 本 (`crates/cam-rtsp`、Digest 認証、TCP interleaved)。最初の見る人の `start` で開き、
  最後の見る人が居なくなったらカメラへ TEARDOWN を送って畳む
- 届いたフレームは、繋がっている見る人全員の PeerConnection へ複製して書く
- 直近の GOP (キーフレームから) を 1 つ持つ。後から繋がった見る人にはまずそれを流し、以後は他の人と同じフレームを流す。
  キーフレームの AU は先頭に SPS/PPS を必ず含むので、途中参加でもデコーダが組める。
  GOP が無ければ (キーフレームがまだ来ていなければ) 次の IDR から流す

カメラ中継規約 (ippoan/alc-gw の `docs/camera-relay-convention.md`) との対応:
映像のみ・H.264 パススルー・STUN のみ (TURN なし)・device 側 non-trickle・見る人の出入りで
RTSP と PeerConnection を起こす/畳む、はすべて同じ。違いは device (拡張の service worker) が Chrome の動いている間だけ居ることと、
cam-room v2 で見る人を複数持てることだけ。上り帯域は 人数 × stream2 になる。
