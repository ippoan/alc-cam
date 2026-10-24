# alc-cam-sd-ingest — SD 録画の時間指定の切り出し

カメラの SD の録画 (約 4 分・約 100MB ごとの MP4) から、指定した時間帯を 1 本の MP4 にして R2 に置く
Worker `alc-cam-sd-ingest`。再エンコードはしない。

```
auth-worker (/api/cam-clips/) ─Service Binding─▶ alc-cam-sd-ingest ─Workers VPC Service (カメラごとの CAM_*, Tunnel)─▶ カメラ (:80、Digest)
                                     ├▶ Hyperdrive ─▶ alc_api.cameras / cam_camera_devices / cam_clips (テナントの RLS)
                                     ├▶ R2 alc-cam-recordings/<tenant>/<camera_id>/<SD のカメラ名>/<種類>/<YYYYMMDD>/<HH>/<元ファイル>.mp4
                                     └▶ R2 alc-cam-recordings/clips/<tenant>/<camera_id>/<開始>_<終了>.mp4   (切り出した 1 本)
```

口 (中身は `ingest/api`。宛先・資格情報は応答にもログにも出さない。パスワードは返さない):

```
GET   /api/cam-clips/cameras                 → {"can_edit_destination": bool, "cameras": [{id, name, vpc_binding, rec_username, has_password, recording, lan_ip, rtsp_port, rtsp_path, device_ids, rec_scanned_until}]}
                                               運行管理者席の鍵には → {"cameras": [{id, name, device_ids}]} (録画の設定は返さない)
POST  /api/cam-clips/cameras                 {name, vpc_binding?, rec_username?, rec_password?, lan_ip?, rtsp_port?, rtsp_path?, device_ids?} → 201 (active = false で作る)
PATCH /api/cam-clips/cameras/{id}            同じ項目 (省略 = 変えない、vpc_binding: null = 取り込みをやめる、rec_password: "" = 変えない、
                                               lan_ip / rtsp_path: null か "" = 消す、rtsp_port: null = 554 に戻す)
GET   /api/cam-clips/cameras/mine            拠点 PC の配信の鍵だけ → {"cameras": [{id, name, rtsp: {host, port, path, username, password} | null}]}
GET   /api/cam-clips/device-candidates?days=30   直近 days 日 (1〜90) にイベントを出した端末と kind ごとの件数 (行ごとに is_dev: 開発端末の行も軸ごとに読んで返す)
POST  /api/cam-clips/clips                   {"camera_id": "...", "from": "2026-10-23T12:58:00+09:00", "to": "2026-10-23T12:59:30+09:00"}
POST  /api/cam-clips/auto-run                 {} か {"camera_id": "..."} → 200 {"cameras": [...]} (cron と同じ自動切り出しを呼び手のテナントだけで今すぐ回す。camera_id で 1 台に絞れる。dev token では 403)
→ 201 {"clip_id": …, "key": "clips/<tenant>/<camera_id>/20261023T125800_20261023T125930.mp4", "size": …,
       "actual_from": "2026-10-23T12:57:58.257+09:00", "actual_to": "…", "sources": [元ファイル…], "reused": false}
GET   /api/cam-clips/clips/<tenant>/<camera_id>/20261023T125800_20261023T125930.mp4   (Range 対応 = シークできる。他テナントのキーは 404)
GET   /api/cam-clips/events?camera_id=<uuid>&date=2026-10-23   (JST の 1 日)
→ 200 [{"source_table", "source_id", "kind", "occurred_at", "device_id", "is_dev", "clip": {"clip_id", "key", "status", "error"} | null}]
```

- RTSP の接続先 (拠点 PC の配信が繋ぐ先。migration 188): `lan_ip` はプライベートの IPv4 (10/8・172.16/12・192.168/16) だけ、
  `rtsp_port` は 1〜65535 (既定 554)、`rtsp_path` は先頭の `/` を落として保存 (英数と `/ _ - . ? = &`、200 文字まで、`..` 不可)。
  ユーザー名・パスワードは録画用の `rec_username` / `rec_password` を使い回す (列を増やさない)。一覧にパスワードは出ない
- `GET /cameras/mine` は、鍵の端末 (`X-Device-Id`) を `cam_camera_devices` で結び付けたカメラを名前の順に返す。`rtsp` は
  `lan_ip` と `rtsp_path` が両方在るときだけで、無ければ null。`password` は `SSO_ENCRYPTION_KEY` で復号した平文 (未設定なら null)。
  応答にパスワードの平文が出るのはこの口だけ。例 (値は作り物):

  ```json
  {"cameras": [
    {"id": "6f1c…", "name": "入口", "rtsp": {"host": "192.168.10.20", "port": 554, "path": "stream1", "username": "viewer", "password": "example-pass"}},
    {"id": "9a0e…", "name": "倉庫", "rtsp": null}
  ]}
  ```
- `POST /clips` の応答の `truncated` は、要求より短い範囲しか取れなかった (`actual_from` > `from` か `actual_to` < `to`) とき true

### 自動の切り出し (cron、1 時間ごと)

`[triggers] crons = ["0 * * * *"]` (本番だけ。`env.local` は空)。中身は `ingest/api/src/auto.rs` (純関数と 1 カメラ分の流れ)
と `ingest/src/auto.rs` (カメラと R2)。

- テナント (`list_cam_recording_tenant_ids()`) → `rec_ip IS NOT NULL` のカメラの順に巡る
- 範囲は `[rec_scanned_until (NULL なら 2 時間前), now)`。結び付けた端末 (`cam_camera_devices`) の `hub_measurements`
  (alcohol / blood_pressure / timecard / license、時刻は `COALESCE(recorded_at, created_at)`) と `vein_enroll_events` を、
  本番の行と開発端末の行 (`app.device_dev='1'` の transaction) の両方から拾う。`cam_clip_events` に在るものは除く
- 各イベントの前後 10 秒。重なる・接する窓は 1 本にまとめる
- 切ってよいのは、まとまりの終わりが SD の一覧の「最後の閉じたファイルの終わり」(= 一覧で次のファイル、書き込み途中の
  `_!` を含む、の開始。後ろに何も無ければその開始) 以内のときだけ。越えたまとまりと以降、カメラあたり
  `AUTO_CLIP_MAX_GROUPS` (40) を超えた分は次の回へ回し、`rec_scanned_until` をその頭 (最初のイベント - 10 秒) にする
- 同じ SD ファイルにかかるまとまりは R2 の元ファイルを使い回す (1 回の中で取り寄せは 1 ファイル 1 回)
- R2 → `cam_clips` (ready / failed) → `cam_clip_events` の順に書く。失敗しても再試行しない
- SD の一覧が取れなかった回は何も書かず、`rec_scanned_until` も進めない

- 時刻は ISO 8601。時差が無ければ JST。1 回 120 分まで (`MAX_CLIP_SECS`)。同じ時間帯は作り直さず 200 で返す。
  結果 (ready / failed) は `cam_clips` に残す
- 映像の頭は、時間帯の頭の直前のキーフレーム (約 2 秒おき) に寄る → `actual_from` が少し前になる
- 元ファイルの映像は 240 秒の区切りに対して約 238 秒しか無く、つなぎ目の約 1.7 秒の抜けは詰まる
- 手順: SD のいちばん上のフォルダ (カメラ名) を一覧で確かめ、重なる元ファイルを SD の一覧 (`camera-cgi/admin/sdcard.cgi`)
  で探し、R2 に無ければカメラから (`/action/stream?subject=recdownload`、パスに `/mnt/mmc`) 丸ごと取って置く →
  R2 の範囲読み出しで末尾の moov を読む → `crates/cam-mp4` で切ってつなぐ計画を立て → 16MiB ずつ R2 のマルチパートに写す
- カメラの宛先 (VPC Service の binding 名)・ユーザー名・パスワード (暗号化。鍵は Secrets Store の `SSO_ENCRYPTION_KEY`) は
  `alc_api.cameras` の `rec_*` 列に持つ (binding 名は `rec_ip` 列に入る。列名は IP のままで migration はしていない)。
  カメラの IP は Cloudflare の VPC Service の側に在り、repo にも DB にも無い
- 外から直接は届かない (workers_dev / route なし。`ingest/scripts/check-exposure.sh` が CI で検査)。auth-worker の振り分け表
  `ALC_BINDING_ROUTES` から届き、JWT の検証は auth-worker が済ませる。Worker が受ける呼び手は 3 つで、それ以外は 403:
  - 管理者のログイン (画面用の proxy、`X-User-Role: admin`) — 全部の口
  - 運行管理者席の鍵 (端末用の proxy、`X-Device-Role: device-tenko-manager`) — 遠隔点呼の画面が乗務員の端末からカメラを
    引くための `GET /cameras` だけ。他の口は 403。auth-worker 側でも席の許可表 (`TENKO_MANAGER_ROUTES`) がこの GET 1 本だけを通す
  - 拠点 PC の配信の鍵 (端末用の proxy、`X-Device-Role: device-cam-host`。CoreS3 署名の usage `cam-host`) — `GET /cameras/mine`
    だけ。他の口は 403。端末は `X-Device-Id` (鍵の JWT の sub、`alarm:<fingerprint>`) で、無ければ 401・200 文字超は 400

  `X-User-Role` と `X-Device-Role` が両方あるときも 403 (auth-worker はどちらか片方しか付けない)。テナントはどちらも `X-Tenant-ID`
  (席の鍵は鍵を登録したテナント) で決まり、読めるのはそのテナントの行だけ。`X-Tenant-ID` が無ければ 401・UUID でなければ 400。
  宛先 (`vpc_binding`) を書くのは管理者が `X-User-Developer: 1` (開発者の Google ログイン) のときだけで、それ以外は 403
- **宛先の検査** (`ingest/api/src/dest.rs`): 宛先の IP・ポートは VPC Service の側に固定されるので、Worker が見るのは binding 名
  だけ。登録の時とカメラへ fetch する直前の両方で `^CAM_[A-Z0-9_]{1,60}$` (`CAM_HYPERDRIVE` は除く) に合うことと、その binding が
  この Worker に在ることを確かめる (無ければ登録時 400・切り出し時 500)。URL は `http://camera.vpc.internal/...` (host は
  Host ヘッダになるだけ。公開名を書くと VPC の外へ出る)、転送 (3xx) は追わない。同じ binding を別のテナントが持っていれば 409

### カメラを足す (システム管理者)

1. Cloudflare のダッシュボードの Workers VPC で VPC Service を作る (種類 HTTP、宛先 = カメラの IP、ポート 80、
   ohishi-data の Tunnel)。CI のトークンは VPC Service の bind までで、作るのは人の手
2. `ingest/wrangler.toml` のトップレベルに `[[vpc_services]]` を 1 表足す (`binding = "CAM_<型番など>"`、
   `service_id` = 1 の id、`remote = true`) → PR を merge すると CI が本番へ出す
3. 画面でそのカメラの「VPC binding」に 2 の binding 名を入れる (書けるのは開発者の Google ログインのときだけ)

今の binding: `CAM_TS_NS320W_55` (TS-NS320W-55)

確認 (`wrangler dev --env local`: Hyperdrive・VPC Service を持たず、DB は `DATABASE_URL`、R2 は手元の模擬。カメラには届かず、
`vpc_binding` を書く登録は 400 になる):

```bash
cd ingest
npx wrangler@4.152.0 dev --env local --var "DATABASE_URL:postgresql://<user>:<pw>@127.0.0.1:<port>/postgres?sslmode=disable"
curl localhost:8787/api/cam-clips/cameras -H 'X-User-Role: admin' -H 'X-Tenant-ID: <tenant>'
```

テスト (api の単体と実 DB。DB は使い捨ての postgres:17 に向ける。URL が無ければ失敗する):

```bash
cd ingest
bash scripts/check-exposure.sh && bash scripts/check-exposure-test.sh
bash scripts/fetch-migrations.sh   # 版は scripts/ALC_MIGRATIONS_REV
CAM_TEST_ADMIN_DATABASE_URL=postgresql://postgres:<pw>@127.0.0.1:<port>/postgres cargo test --workspace
cargo clippy --target wasm32-unknown-unknown -- -D warnings
```

手元のファイルだけで切り出しを試すなら `cargo run -p cam-mp4 --example clip -- 出力 from_ms to_ms start_ms:入力…`。

デプロイ: main への merge で CI が本番へ deploy する (`.github/workflows/deploy.yml`。staging は無い。PR では `wrangler deploy --dry-run` だけ)。
手元の wrangler の OAuth では Secrets Store の binding を付けられないので手では出さない。手動のやり直しは Actions の `Deploy` を `workflow_dispatch` で回す。
token は org の secret `CLOUDFLARE_API_TOKEN`。deploy の前に `scripts/check-exposure.sh` が落ちれば出ない。
