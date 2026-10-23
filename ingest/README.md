# alc-cam-sd-ingest — SD 録画の時間指定の切り出し

カメラの SD の録画 (約 4 分・約 100MB ごとの MP4) から、指定した時間帯を 1 本の MP4 にして R2 に置く
Worker `alc-cam-sd-ingest`。再エンコードはしない。

```
auth-worker (/api/cam-clips/) ─Service Binding─▶ alc-cam-sd-ingest ─Workers VPC Network (CAM_NET, Tunnel)─▶ カメラ (cameras.rec_ip:80、Digest)
                                     ├▶ Hyperdrive ─▶ alc_api.cameras / cam_camera_devices / cam_clips (テナントの RLS)
                                     ├▶ R2 alc-cam-recordings/<tenant>/<camera_id>/<SD のカメラ名>/<種類>/<YYYYMMDD>/<HH>/<元ファイル>.mp4
                                     └▶ R2 alc-cam-recordings/clips/<tenant>/<camera_id>/<開始>_<終了>.mp4   (切り出した 1 本)
```

口 (中身は `ingest/api`。宛先・資格情報は応答にもログにも出さない。パスワードは返さない):

```
GET   /api/cam-clips/cameras                 → {"can_edit_destination": bool, "cameras": [{id, name, rec_ip, rec_username, has_password, recording, device_ids, rec_scanned_until}]}
POST  /api/cam-clips/cameras                 {name, rec_ip?, rec_username?, rec_password?, device_ids?} → 201 (active = false で作る)
PATCH /api/cam-clips/cameras/{id}            同じ項目 (省略 = 変えない、rec_ip: null = 取り込みをやめる、rec_password: "" = 変えない)
GET   /api/cam-clips/device-candidates?days=30   直近 days 日 (1〜90) にイベントを出した端末と kind ごとの件数
POST  /api/cam-clips/clips                   {"camera_id": "...", "from": "2026-10-23T12:58:00+09:00", "to": "2026-10-23T12:59:30+09:00"}
→ 201 {"clip_id": …, "key": "clips/<tenant>/<camera_id>/20261023T125800_20261023T125930.mp4", "size": …,
       "actual_from": "2026-10-23T12:57:58.257+09:00", "actual_to": "…", "sources": [元ファイル…], "reused": false}
GET   /api/cam-clips/clips/<tenant>/<camera_id>/20261023T125800_20261023T125930.mp4   (Range 対応 = シークできる。他テナントのキーは 404)
```

- 時刻は ISO 8601。時差が無ければ JST。1 回 120 分まで (`MAX_CLIP_SECS`)。同じ時間帯は作り直さず 200 で返す。
  結果 (ready / failed) は `cam_clips` に残す
- 映像の頭は、時間帯の頭の直前のキーフレーム (約 2 秒おき) に寄る → `actual_from` が少し前になる
- 元ファイルの映像は 240 秒の区切りに対して約 238 秒しか無く、つなぎ目の約 1.7 秒の抜けは詰まる
- 手順: SD のいちばん上のフォルダ (カメラ名) を一覧で確かめ、重なる元ファイルを SD の一覧 (`camera-cgi/admin/sdcard.cgi`)
  で探し、R2 に無ければカメラから (`/action/stream?subject=recdownload`、パスに `/mnt/mmc`) 丸ごと取って置く →
  R2 の範囲読み出しで末尾の moov を読む → `crates/cam-mp4` で切ってつなぐ計画を立て → 16MiB ずつ R2 のマルチパートに写す
- カメラの宛先・ユーザー名・パスワード (暗号化。鍵は Secrets Store の `SSO_ENCRYPTION_KEY`) は `alc_api.cameras` の
  `rec_*` 列に持つ。repo には無い
- 外から直接は届かない (workers_dev / route なし。`ingest/scripts/check-exposure.sh` が CI で検査)。auth-worker の振り分け表
  `ALC_BINDING_ROUTES` から届き、JWT の検証は auth-worker が済ませる。Worker は付け直された `X-User-Role` が `admin` で
  なければ 403、`X-Tenant-ID` が無ければ 401・UUID でなければ 400。宛先 (`rec_ip`) を書くのは `X-User-Developer: 1`
  (開発者の Google ログイン) のときだけで、それ以外は 403
- **宛先の検査** (`ingest/api/src/dest.rs`): VPC Network の binding は宛先を絞らないので、登録の時とカメラへ fetch する直前の
  両方で、私設帯 (10/8・172.16/12・192.168/16) の IPv4 のリテラルだけを通す (名前・IPv6・ポート・loopback・link-local は不可)。
  URL は `http://<IP>` (ポート 80)、転送 (3xx) は追わない。同じ宛先を別のテナントが持っていれば 409

確認 (`wrangler dev --env local`: Hyperdrive・VPC を持たず、DB は `DATABASE_URL`、R2 は手元の模擬。カメラには届かない):

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
