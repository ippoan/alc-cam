# 拡張 ⇄ alc-cam-host のメッセージ

Chrome の native messaging (4 バイトの長さ + UTF-8 JSON、`host/src/native.rs`)。
host は拡張のサイドパネルが `connectNative('org.ippoan.alc_cam_host')` した時に起動し、
パネルが閉じて stdin が閉じると終わる。stdout にはこの枠以外を書かない (ログは stderr)。

## 拡張 → host

| type | 中身 | いつ |
|---|---|---|
| `config` | `rtsp_url`, `stun` (`host:port`) | 起動直後に 1 回 |
| `start` | - | cam-room で `peer_joined {role: admin}` を受けた時 (見る人が来た) |
| `stop` | - | `peer_left {role: admin}` か、cam-room の WebSocket が切れた時 |
| `sdp_answer` | `sdp` | cam-room から answer が来た時 |
| `ice_candidate` | `candidate` (RTCIceCandidateInit) | 見る側が trickle で送ってきた時 (任意。読めなければ捨てる) |

## host → 拡張

| type | 中身 | 意味 |
|---|---|---|
| `ready` | `version` | 起動した。見る人が居れば拡張は `start` を送り直す (host が落ちて起動し直した時のため) |
| `sdp_offer` | `sdp` | 拡張はそのまま cam-room へ `sdp_offer` として送る。**候補は全部入っている (non-trickle)** |
| `camera` | `ok`, `message` | カメラの状態 (受信中 / 再接続待ち / 停止)。文言に資格情報・宛先は入れない |
| `peer` | `ok`, `message` | 見る側との接続の状態 (answer 待ち / 接続 (P2P) / 失敗) |
| `log` | `message` | 診断用 |

## host の中の流れ (`start` 1 回ぶん)

1. カメラへ向かう経路で LAN の IP を決め、その IP の UDP を 1 本開く
2. STUN で外から見えるアドレスを 1 回聞く (取れなければ LAN 内だけで繋がる)
3. host 候補 + srflx 候補を入れた offer (映像のみ・H.264・sendonly) を作って返す
4. 同時にカメラの RTSP を受け始める (`crates/cam-rtsp`、Digest 認証、TCP interleaved)。
   繋がるまでは直近の GOP を 1 つだけ持っておき、繋がった瞬間にキーフレームから出す
5. answer を受け、ICE が繋がったら H.264 をそのまま (再エンコードなし) 送る
6. `stop` / 切断 / 30 秒繋がらない → カメラへ TEARDOWN を送って畳む

カメラ中継規約 (ippoan/alc-gw の `docs/camera-relay-convention.md`) との対応:
映像のみ・H.264 パススルー・STUN のみ (TURN なし)・device 側 non-trickle・見る人の出入りで
RTSP と PeerConnection を起こす/畳む、はすべて同じ。違いは「拡張を開いている間だけ device が居る」ことだけ。
