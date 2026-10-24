# 点呼の PWA → 拡張 のメッセージ

拡張 (`extension/background.js`) は `externally_connectable` で `https://alc.ippoan.org/*` のページからの
`chrome.runtime.sendMessage(拡張ID, message, callback)` だけを受ける。受けた側でも `sender.origin` が
`https://alc.ippoan.org` であることを確かめ、違えば `{ ok: false, error: 'origin' }` を返す
(他の origin のページには `chrome.runtime.sendMessage` 自体が生えない)。

拡張 ID は `icmmpdjddlphkbokdalofdjpkcgfiggd` (manifest の `key` で固定)。

| 送る | 返る | 版 |
|---|---|---|
| `{ type: 'cam_host_token', token, exp }` | `{ ok }` | 0.4.0〜 |
| `{ type: 'cam_status' }` | `{ ok: true, status }` | 0.4.2〜 |
| `{ type: 'cam_control', action }` | `{ ok, message, status }` | 0.4.2〜 |

0.4.1 までの拡張は `cam_status` / `cam_control` に `{ ok: false }` を返す。拡張が無い・止まっているときは
callback で `chrome.runtime.lastError` が立つ (呼び手は「拡張が見つかりません」として扱う)。

## `cam_host_token`

点呼の PWA が CoreS3 の署名で取った cam-host の鍵 (role=device-cam-host、TTL 900 秒) を渡す。`exp` は秒でもミリ秒でもよい。
拡張は鍵をメモリにだけ持ち、担当カメラ (`/cameras/mine`) を引き直す。

## `cam_status`

サイドパネルと同じ状態。`[文言, class]` の組はパネルの表示そのまま (class は `ok` / `warn` / `''`)。

| key | 型 | 中身 |
|---|---|---|
| `version` | string | 拡張の版 |
| `hostVersion` | string | host の版 (起動したか更新の確認で分かったときだけ。他は `''`) |
| `enabled` | boolean | 「停止」していなければ true |
| `tokenLive` | boolean | 期限内の鍵を持っている |
| `tokenExp` | number | 鍵の期限 (ミリ秒、無ければ 0) |
| `tokenSub` | string | 鍵の sub (`alarm:<fp>`。照合の確認用。署名は確かめていない) |
| `assigned` | `{id, name}` \| null | 配る担当カメラ (cam-room の部屋 = id) |
| `rank` | number | 担当カメラの配信の順位 (1 = 主。ingest の `/cameras/mine` の rank)。順位を知らなければ 0 (0.4.3〜) |
| `standby` | boolean | 上位か同位の端末が配信中で待機している (cam-room が 4011 / 4010 で閉じた。30〜60 秒おきに入り直す) (0.4.3〜) |
| `config` | `[文言, class]` | 担当カメラの表示 (取得の失敗・待ちの理由を含む) |
| `host` / `signal` / `camera` | `[文言, class]` | host・シグナリング (cam-room)・カメラ (RTSP) の状態 |
| `viewers` | `{ok, message}[]` | 見る人ごとの映像の状態 |
| `update` | `[文言, class]` | host 更新の表示 |
| `updateState` | string | 最後の更新確認の結果 (`checking` / `latest` / `installed` / `deferred` / `unsupported` / `error`、未確認は `''`) |
| `updateAt` | number | その時刻 (ミリ秒、未確認は 0) |
| `lastError` | `{message, at}` \| null | 直近のエラー (host の停止・cam-room の error・取得や更新の失敗) |

鍵の中身・RTSP のパスワード・RTSP の URL は返さない (文言に `rtsp://…` が混ざっても伏せる)。

## `cam_control`

`action` はパネルと同じ実装を呼ぶ。返事の `status` は操作の直後の `cam_status` と同じ形。

| `action` | すること |
|---|---|
| `stop` / `start` | パネルの「停止」/「開始」と同じ (storage.local の `enabled`)。Chrome を起動し直しても残る |
| `update_check` | パネルの「今すぐ確認」と同じ。結果は後から `updateState` / `updateAt` に出る |
| `refetch` | 今ある鍵で担当カメラ (`/cameras/mine`) を引き直す。鍵が無ければ `ok: false`。同じカメラなら繋ぎ直さない |

知らない `action` は `{ ok: false, error: 'unknown' }`。
