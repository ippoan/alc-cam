# VPC Network binding を workers-rs から使えるかの実機確認 (#2-0)

2026-10-23 (JST 18 時台)、`npx wrangler@4.152.0 dev --remote` で実測。確認用 branch `probe/2-0-vpc-network` だけに置き、merge しない。
カメラの IP・資格情報は `.dev.vars` (gitignore 済み) で渡し、ここでは `{cam}` と書く。

## 設定

```toml
[[vpc_networks]]
binding = "CAM_NET"
tunnel_id = "589ac3b8-a73b-4cb8-b2ba-61719490b47d"
remote = true
```

wrangler 4.152.0 はこの形をそのまま受け付け、bindings の表に `env.CAM_NET (589ac3b8-…) VPC Network` と出た。
Rust 側は `camera.rs` の `VpcHttp` (`TYPE_NAME = "Fetcher"`、unchecked で受ける) をそのまま流用し、
`Fetcher::fetch_request` で投げて動いた。**VPC Service と同じ受け方で VPC Network も使える**。

確認の口は `src/probe.rs` (`GET /__probe`。`PROBE_CAM_IP` が無ければ 404)。
`Camera` には宛先を差し替える `with_base` を足した (VPC Network では URL の host が宛先そのものになるため)。

## 結果

| | 確かめたこと | 結果 |
|---|---|---|
| (a) | `CAM_NET.fetch("http://{cam}/")` | **届いた**。`redirect: manual` で `302` (Location `http://{cam}/preview.html?t=…`、Server `IPC/2.0.0`)、follow だと遷移先で `401` + `WWW-Authenticate: Digest` (qop・nonce あり)。`/camera-cgi/admin/sdcard.cgi` を素で叩くと `401` Digest |
| (b) | `Camera::with_base(env, "CAM_NET", auth, "http://{cam}")` で SD 一覧 (`sdcard.cgi?action=generate&dir=TS-NS320W-55/Schedule/20261023/17…`) | **200**。1499 B、`parse_listing` で 16 ファイル。Digest の付け直しはそのまま通る |
| (c) | `RequestRedirect::Manual` | **効く**。カメラの `302` も `http://github.com/` の `301` (Location `https://github.com/`) もそのまま返る。follow では追う (github は 200) |
| (d) | 公開ホストを `CAM_NET` に投げる | **拒否されず VPC の外 (インターネット) へ出る**。`http://example.com/` → `200` (Server `cloudflare`)、`https://1.1.1.1/cdn-cgi/trace` → `200`。trace の出口 IP は **ohishi-data の出口 IP と一致** (この PC の出口とは不一致)、`warp=off gateway=off`。= Tunnel の先 (拠点の回線) から外へ出ている |

## 失敗のときの返り方 (fetch が投げるエラー文そのまま)

| 宛先 | エラー |
|---|---|
| 解決できない名前 (`no-such-host.invalid`、`ohishi-data`、素の `cam`) | `Error: dns_error` (約 0.1 秒) |
| 経路の無い私設 IP (`10.255.255.1`) | `Error: handshake timeout` (約 5 秒) |
| カメラの閉じたポート (`{cam}:9`) | `Error: connection_refused` |
| `127.0.0.1:22` / `127.0.0.1:3100` / `localhost:3100` | `Error: connection_refused` |

`127.0.0.1:22` は ohishi-data では sshd が待っているのに refused だった。ohishi-data 上の `cloudflared` は
systemd でも docker でも見えなかったので、この Tunnel の connector は ohishi-data とは別のホスト (同じ拠点の NAT の内側)
の可能性がある。loopback が塞がれているのか、別ホストの loopback に当たったのかは切り分けていない。

## 設計への含み

- カメラ台帳に「宛先 IP」を持たせ、1 本の `CAM_NET` から `http://<IP>/…` を組み立てる形は成り立つ (c1 の列を IP で確定してよい)。
- ただし **binding は宛先を絞らない**: 公開インターネットにも、Tunnel の先の網の任意の IP・ポートにも出られる。
  台帳の IP を管理画面から登録させるなら、Worker 側で宛先を検査する必要がある (私設アドレス帯だけ許す・ポートを固定する・
  台帳に無い宛先には投げない、など)。VPC Service のように「Cloudflare 側で宛先を固定する」安全は失われる。
- エラーは文字列 (`dns_error` / `handshake timeout` / `connection_refused`) で返るので、カメラ不達の表示はこれで分けられる。
