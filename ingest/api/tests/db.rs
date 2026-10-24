//! api の SQL と口 (`execute`) を実 DB で確かめる (ippoan/alc-lineworks-worker の tests/support の型)。
//!
//! - **`CAM_TEST_ADMIN_DATABASE_URL` が未設定なら失敗する** (skip して緑にしない)。値は準備用の superuser の
//!   接続文字列で、**使い捨ての DB (database 名 `postgres`) に向けること** (ロール・schema・全 migration を作る)。表示はしない
//! - 準備の SQL は ippoan/alc-migrations から版を固定して取る (`scripts/ALC_MIGRATIONS_REV`、
//!   `bash scripts/fetch-migrations.sh` が `ingest/.alc-migrations/` に取り出す)。流す順は init_local_db.sql →
//!   空の `_sqlx_migrations` → migrations (1 ファイル 1 transaction) → local_app_grants.sql
//! - 本体のテストは、本番の worker と同じ実行用ロール `alc_api_rt` (migration 158。表の所有者ではない・`NOSUPERUSER`・
//!   `NOBYPASSRLS`) で繋ぐ ([`rt`] が毎回確かめる)。パスワードは使い捨ての DB 専用の固定の文字列で、秘密ではない
//! - 行の準備 (テナント・端末のイベント・既存の ONVIF のカメラ) だけ superuser で入れる

use alc_worker_db::PgClient;
use cam_ingest_api::pg::{self, ClipRecord};
use cam_ingest_api::{execute, parse, Caller, Role};
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use tokio_postgres::{Client, Config, NoTls};
use uuid::Uuid;

const ADMIN_URL_ENV: &str = "CAM_TEST_ADMIN_DATABASE_URL";
const RT_ROLE: &str = "alc_api_rt";
/// 使い捨ての DB 専用 (秘密ではない)
const PASSWORD: &str = "cam-ingest-test-throwaway";
/// テストの暗号化の鍵の材料 (秘密ではない)
const KEY: &str = "cam-ingest-test-key";
/// 準備を直列にする advisory lock の鍵
const PREPARE_LOCK: i64 = 2_002_003;

fn admin_config() -> Config {
    let url = std::env::var(ADMIN_URL_ENV).unwrap_or_else(|_| {
        panic!("{ADMIN_URL_ENV} が未設定。実 DB のテストは接続先が無ければ失敗させる (skip しない)")
    });
    url.parse()
        .unwrap_or_else(|_| panic!("{ADMIN_URL_ENV} が接続文字列として読めない"))
}

async fn raw_connect(config: &Config) -> Client {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .unwrap_or_else(|_| panic!("{ADMIN_URL_ENV} の DB に繋げない"));
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn admin() -> Client {
    let mut config = admin_config();
    config.options("-c search_path=alc_api,public");
    raw_connect(&config).await
}

fn migrations_dir() -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.alc-migrations"));
    assert!(
        dir.join("migrations").is_dir(),
        ".alc-migrations が無い。先に `bash scripts/fetch-migrations.sh` を流す"
    );
    dir
}

/// 準備をプロセスの中で 1 回だけ流す。190 の列 (hub_measurements.via_device_id) が在れば (前に流した) 何もしない
async fn prepare() {
    static PREPARED: OnceCell<()> = OnceCell::const_new();
    PREPARED
        .get_or_init(|| async {
            let su = admin().await;
            su.batch_execute(&format!("SELECT pg_advisory_lock({PREPARE_LOCK})"))
                .await
                .unwrap();
            let done: bool = su
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = 'alc_api' AND table_name = 'hub_measurements' AND column_name = 'via_device_id')",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if !done {
                migrate(&su).await;
            }
            su.batch_execute(&format!("SELECT pg_advisory_unlock({PREPARE_LOCK})"))
                .await
                .unwrap();
            let rev = std::fs::read_to_string(migrations_dir().join(".rev")).unwrap();
            eprintln!(
                "[cam-ingest-db] 実 DB で準備済み (migrations は {})",
                rev.trim()
            );
        })
        .await;
}

async fn migrate(su: &Client) {
    let dir = migrations_dir();
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap();
    su.batch_execute(&read(dir.join("scripts/init_local_db.sql")))
        .await
        .unwrap();
    su.batch_execute("CREATE TABLE IF NOT EXISTS alc_api._sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), success BOOLEAN NOT NULL, checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL)")
        .await
        .unwrap();
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("migrations"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    assert!(
        files.iter().any(|f| f
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_cam_recording.sql")),
        "取り出した migrations に 187 (cam_recording) が無い (scripts/ALC_MIGRATIONS_REV が古い)"
    );
    assert!(
        files.iter().any(|f| f
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_cameras_rtsp.sql")),
        "取り出した migrations に 188 (cameras_rtsp) が無い (scripts/ALC_MIGRATIONS_REV が古い)"
    );
    assert!(
        files.iter().any(|f| f
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_cam_stream_devices.sql")),
        "取り出した migrations に 189 (cam_stream_devices) が無い (scripts/ALC_MIGRATIONS_REV が古い)"
    );
    assert!(
        files.iter().any(|f| f
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_device_key_id_columns.sql")),
        "取り出した migrations に 190 (device_key_id_columns) が無い (scripts/ALC_MIGRATIONS_REV が古い)"
    );
    for file in files {
        let name = file.file_name().unwrap().to_owned();
        if let Err(e) = su
            .batch_execute(&format!("BEGIN;\n{}\n;COMMIT;", read(file)))
            .await
        {
            panic!("migration {name:?}: {:?}", e.code());
        }
    }
    su.batch_execute(&read(dir.join("scripts/local_app_grants.sql")))
        .await
        .unwrap();
    su.batch_execute(&format!("ALTER ROLE {RT_ROLE} LOGIN PASSWORD '{PASSWORD}'"))
        .await
        .unwrap();
}

/// worker と同じ実行用ロールの接続。**superuser / BYPASSRLS / 表の所有者なら panic**
async fn rt() -> PgClient {
    prepare().await;
    let mut config = admin_config();
    config.user(RT_ROLE).password(PASSWORD);
    let client = raw_connect(&config).await;
    let row = client
        .query_one(
            "SELECT (SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user), \
             (SELECT bool_or(tableowner = current_user) FROM pg_tables WHERE schemaname = 'alc_api' \
              AND tablename IN ('cameras', 'cam_camera_devices', 'cam_stream_devices', 'cam_clips', 'hub_measurements', 'vein_enroll_events'))",
            &[],
        )
        .await
        .unwrap();
    assert!(!row.get::<_, bool>(0), "RLS を素通りするロールで繋いでいる");
    assert!(!row.get::<_, bool>(1), "表の所有者で繋いでいる");
    PgClient::new(client)
}

async fn tenant() -> Uuid {
    prepare().await;
    let id = Uuid::new_v4();
    admin()
        .await
        .execute(
            "INSERT INTO alc_api.tenants (id, name) VALUES ($1, $2)",
            &[&id, &format!("cam-ingest-test-{id}")],
        )
        .await
        .unwrap();
    id
}

/// テストごとに重ならない宛先 (VPC Service の binding 名。同じ DB を何度流しても部分 unique に当たらないよう乱数)
fn random_binding() -> String {
    format!("CAM_TEST_{}", Uuid::new_v4().simple().to_string().to_uppercase())
}

fn caller(tenant: Uuid, developer: bool) -> Caller {
    Caller {
        tenant,
        developer,
        role: Role::Admin,
        device_id: None,
    }
}

/// 運行管理者席の鍵 (auth-worker の端末用の proxy から届く呼び手)
fn tenko_manager(tenant: Uuid) -> Caller {
    Caller {
        tenant,
        developer: false,
        role: Role::TenkoManager,
        device_id: None,
    }
}

/// 拠点 PC の配信の鍵 (auth-worker の端末用の proxy から届く呼び手。`X-Device-Id` は鍵の JWT の sub)
fn cam_host(tenant: Uuid, device: &str) -> Caller {
    Caller {
        tenant,
        developer: false,
        role: Role::CamHost,
        device_id: Some(device.to_owned()),
    }
}

/// 口を通す (parse → execute)。応答の (status, body)
async fn call(
    pg: &mut PgClient,
    c: &Caller,
    method: &str,
    path: &str,
    query: &str,
    body: Value,
) -> (u16, Value) {
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let op = match parse(method, path, query, &body, c, 7_200_000) {
        Ok(op) => op,
        Err(e) => return (e.status, e.body()),
    };
    let key = op.needs_key().then_some(KEY);
    match execute(op, c, pg, key).await {
        Ok(r) => (r.status, r.body),
        Err(e) => (e.status, e.body()),
    }
}

#[tokio::test]
async fn cameras_crud_and_devices() {
    let t = tenant().await;
    let mut pg = rt().await;
    let dev = caller(t, true);
    let admin_only = caller(t, false);

    // 既存の ONVIF の行 (backend が作る形。rec_* は NULL) も一覧に出る
    let onvif: Uuid = admin()
        .await
        .query_one("INSERT INTO alc_api.cameras (tenant_id, name, ip) VALUES ($1, '既存の ONVIF', '192.168.9.9') RETURNING id", &[&t])
        .await
        .unwrap()
        .get(0);

    // 宛先なしなら developer でなくても作れる
    let (s, plain) = call(
        &mut pg,
        &admin_only,
        "POST",
        "/cameras",
        "",
        json!({"name": "入口", "device_ids": ["dev-b", "dev-a", "dev-b"]}),
    )
    .await;
    assert_eq!(s, 201, "{plain}");
    assert_eq!(plain["recording"], false);
    assert_eq!(plain["device_ids"], json!(["dev-a", "dev-b"]));
    assert_eq!(plain["has_password"], false);
    let active: bool = admin()
        .await
        .query_one(
            "SELECT active FROM alc_api.cameras WHERE id = $1",
            &[&Uuid::parse_str(plain["id"].as_str().unwrap()).unwrap()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!active, "ingest が作るカメラは ONVIF の死活監視の対象外");

    // developer でなければ宛先 (vpc_binding) 付きでは作れない (403 で、行は増えない)
    let (s, _) = call(&mut pg, &admin_only, "POST", "/cameras", "", json!({"name": "勝手に", "vpc_binding": random_binding()})).await;
    assert_eq!(s, 403);
    let (_, list) = call(&mut pg, &admin_only, "GET", "/cameras", "", Value::Null).await;
    assert!(list["cameras"].as_array().unwrap().iter().all(|c| c["name"] != "勝手に"));

    // developer は宛先・資格情報付きで作れる。パスワードは返さず、暗号化して保存する
    let binding = random_binding();
    let (s, cam) = call(&mut pg, &dev, "POST", "/cameras", "", json!({"name": "事務所", "vpc_binding": binding, "rec_username": "admin", "rec_password": "pw-1", "device_ids": ["dev-c"]})).await;
    assert_eq!(s, 201, "{cam}");
    assert_eq!(cam["vpc_binding"], json!(binding));
    assert_eq!(cam["recording"], true);
    assert_eq!(cam["has_password"], true);
    assert!(cam.get("rec_password").is_none() && cam.get("rec_password_encrypted").is_none());
    let id = Uuid::parse_str(cam["id"].as_str().unwrap()).unwrap();
    let src = pg::clip_source(&mut pg, t, id).await.unwrap().unwrap();
    let enc = src.rec_password_encrypted.unwrap();
    assert_ne!(enc, "pw-1");
    assert_eq!(
        alc_worker_crypto::secret::decrypt_secret(&enc, KEY).unwrap(),
        "pw-1"
    );
    assert_eq!(src.vpc_binding.as_deref(), Some(binding.as_str()));

    // 一覧: テナントの全行 (既存の ONVIF の行を含む)。can_edit_destination は developer のときだけ
    let (s, list) = call(&mut pg, &admin_only, "GET", "/cameras", "", Value::Null).await;
    assert_eq!(s, 200);
    assert_eq!(list["can_edit_destination"], false);
    let names: Vec<&str> = list["cameras"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["事務所", "入口", "既存の ONVIF"]);
    let onvif_json = list["cameras"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!(onvif))
        .unwrap();
    assert_eq!(onvif_json["vpc_binding"], Value::Null);
    assert_eq!(onvif_json["device_ids"], json!([]));
    assert_eq!(
        call(&mut pg, &dev, "GET", "/cameras", "", Value::Null)
            .await
            .1["can_edit_destination"],
        true
    );

    let path = format!("/cameras/{id}");
    // developer でない admin: 名前・端末は変えられる。宛先を同じ値で送るのは変更ではないので通る
    let (s, after) = call(
        &mut pg,
        &admin_only,
        "PATCH",
        &path,
        "",
        json!({"name": "事務所 2", "vpc_binding": binding, "device_ids": ["dev-x", "dev-y"]}),
    )
    .await;
    assert_eq!(s, 200, "{after}");
    assert_eq!(after["name"], "事務所 2");
    assert_eq!(after["device_ids"], json!(["dev-x", "dev-y"]));
    // 宛先を変える・消すのは 403 で、何も変わらない
    let (s, _) = call(
        &mut pg,
        &admin_only,
        "PATCH",
        &path,
        "",
        json!({"name": "変わらない", "vpc_binding": random_binding()}),
    )
    .await;
    assert_eq!(s, 403);
    let (s, _) = call(
        &mut pg,
        &admin_only,
        "PATCH",
        &path,
        "",
        json!({"vpc_binding": null}),
    )
    .await;
    assert_eq!(s, 403);
    let now = pg::list_cameras(&mut pg, t, Some(id))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        (now.name.as_str(), now.vpc_binding.as_deref()),
        ("事務所 2", Some(binding.as_str()))
    );

    // パスワード: "" は変えない、値は変える、null は消す
    let (s, _) = call(
        &mut pg,
        &admin_only,
        "PATCH",
        &path,
        "",
        json!({"rec_password": ""}),
    )
    .await;
    assert_eq!(s, 200);
    let enc2 = pg::clip_source(&mut pg, t, id)
        .await
        .unwrap()
        .unwrap()
        .rec_password_encrypted
        .unwrap();
    assert_eq!(enc2, enc);
    let (s, _) = call(
        &mut pg,
        &admin_only,
        "PATCH",
        &path,
        "",
        json!({"rec_password": "pw-2"}),
    )
    .await;
    assert_eq!(s, 200);
    let enc3 = pg::clip_source(&mut pg, t, id)
        .await
        .unwrap()
        .unwrap()
        .rec_password_encrypted
        .unwrap();
    assert_eq!(
        alc_worker_crypto::secret::decrypt_secret(&enc3, KEY).unwrap(),
        "pw-2"
    );
    let (s, c) = call(
        &mut pg,
        &admin_only,
        "PATCH",
        &path,
        "",
        json!({"rec_password": null, "device_ids": []}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(c["has_password"], false);
    assert_eq!(c["device_ids"], json!([]));

    // developer は取り込みをやめられる (vpc_binding: null)
    let (s, c) = call(&mut pg, &dev, "PATCH", &path, "", json!({"vpc_binding": null})).await;
    assert_eq!(s, 200);
    assert_eq!(c["recording"], false);

    // 無いカメラは 404
    let (s, _) = call(
        &mut pg,
        &dev,
        "PATCH",
        &format!("/cameras/{}", Uuid::new_v4()),
        "",
        json!({"name": "x"}),
    )
    .await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn tenko_manager_lists_own_tenant_cameras_without_recording_settings() {
    let a = tenant().await;
    let b = tenant().await;
    let mut pg = rt().await;
    let (s, cam) = call(
        &mut pg,
        &caller(a, true),
        "POST",
        "/cameras",
        "",
        json!({"name": "A の入口", "vpc_binding": random_binding(), "rec_username": "u", "rec_password": "p", "device_ids": ["dev-b", "dev-a"]}),
    )
    .await;
    assert_eq!(s, 201, "{cam}");
    let id = cam["id"].clone();

    // 席の鍵の一覧は id・name・device_ids だけ (録画の設定と can_edit_destination は返さない)
    let (s, list) = call(&mut pg, &tenko_manager(a), "GET", "/cameras", "", Value::Null).await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["cameras"]);
    let mine: Vec<&Value> = list["cameras"].as_array().unwrap().iter().filter(|c| c["id"] == id).collect();
    assert_eq!(
        mine,
        vec![&json!({"id": id, "name": "A の入口", "device_ids": ["dev-a", "dev-b"]})]
    );
    for c in list["cameras"].as_array().unwrap() {
        assert_eq!(c.as_object().unwrap().len(), 3, "{c}");
    }

    // 別のテナントの席の鍵からは見えない
    let (s, other) = call(&mut pg, &tenko_manager(b), "GET", "/cameras", "", Value::Null).await;
    assert_eq!(s, 200);
    assert_eq!(other, json!({"cameras": []}));

    // 一覧以外は 403 で、何も変わらない
    let (s, _) = call(&mut pg, &tenko_manager(a), "PATCH", &format!("/cameras/{}", id.as_str().unwrap()), "", json!({"name": "x"})).await;
    assert_eq!(s, 403);
    let (s, _) = call(&mut pg, &tenko_manager(a), "POST", "/cameras", "", json!({"name": "x"})).await;
    assert_eq!(s, 403);
    let (s, _) = call(&mut pg, &tenko_manager(a), "GET", "/device-candidates", "", Value::Null).await;
    assert_eq!(s, 403);
    let (_, admin_list) = call(&mut pg, &caller(a, false), "GET", "/cameras", "", Value::Null).await;
    assert_eq!(admin_list["cameras"].as_array().unwrap().len(), 1);
    assert_eq!(admin_list["cameras"][0]["name"], "A の入口");
    assert_eq!(admin_list["cameras"][0]["has_password"], true);
}

#[tokio::test]
async fn rtsp_settings_are_edited_and_listed_without_password() {
    let t = tenant().await;
    let mut pg = rt().await;
    let a = caller(t, false);

    // backend の形の INSERT (新しい列を書かない) は rtsp_port = 554・他は NULL
    admin()
        .await
        .execute("INSERT INTO alc_api.cameras (tenant_id, name, ip) VALUES ($1, '既存の ONVIF', '192.168.9.9')", &[&t])
        .await
        .unwrap();
    let (s, cam) = call(&mut pg, &a, "POST", "/cameras", "", json!({"name": "入口", "lan_ip": "192.168.10.20", "rtsp_path": "/stream1", "rec_username": "viewer", "rec_password": "pw-rtsp"})).await;
    assert_eq!(s, 201, "{cam}");
    assert_eq!((&cam["lan_ip"], &cam["rtsp_port"], &cam["rtsp_path"]), (&json!("192.168.10.20"), &json!(554), &json!("stream1")));
    let id = cam["id"].as_str().unwrap().to_owned();

    let (_, list) = call(&mut pg, &a, "GET", "/cameras", "", Value::Null).await;
    let cams = list["cameras"].as_array().unwrap();
    let onvif = cams.iter().find(|c| c["name"] == "既存の ONVIF").unwrap();
    assert_eq!((&onvif["lan_ip"], &onvif["rtsp_port"], &onvif["rtsp_path"]), (&Value::Null, &json!(554), &Value::Null));
    // 一覧にパスワード (平文・暗号文) は出ない
    let text = list.to_string();
    assert!(!text.contains("pw-rtsp"));
    for c in cams {
        assert!(c.get("rec_password").is_none() && c.get("rec_password_encrypted").is_none() && c.get("rtsp").is_none(), "{c}");
    }

    // PATCH: 省略は変えない・値は変える・port の null は既定 (554) に戻す・lan_ip の null は消す
    let path = format!("/cameras/{id}");
    let (s, p) = call(&mut pg, &a, "PATCH", &path, "", json!({"rtsp_port": 8554, "rtsp_path": "live/ch00_0"})).await;
    assert_eq!(s, 200, "{p}");
    assert_eq!((&p["lan_ip"], &p["rtsp_port"], &p["rtsp_path"]), (&json!("192.168.10.20"), &json!(8554), &json!("live/ch00_0")));
    let (_, p) = call(&mut pg, &a, "PATCH", &path, "", json!({"rtsp_port": null, "lan_ip": null})).await;
    assert_eq!((&p["lan_ip"], &p["rtsp_port"], &p["rtsp_path"]), (&Value::Null, &json!(554), &json!("live/ch00_0")));
    // 検査で落ちたら何も変わらない
    let (s, _) = call(&mut pg, &a, "PATCH", &path, "", json!({"lan_ip": "203.0.113.5", "rtsp_path": "x"})).await;
    assert_eq!(s, 400);
    let (_, list) = call(&mut pg, &a, "GET", "/cameras", "", Value::Null).await;
    let now = list["cameras"].as_array().unwrap().iter().find(|c| c["id"] == json!(id)).unwrap().clone();
    assert_eq!((&now["lan_ip"], &now["rtsp_path"]), (&Value::Null, &json!("live/ch00_0")));
    // DB の CHECK (API を通らない書き込みも 1〜65535)
    let e = admin()
        .await
        .execute("UPDATE alc_api.cameras SET rtsp_port = 0 WHERE tenant_id = $1", &[&t])
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some(&tokio_postgres::error::SqlState::CHECK_VIOLATION));
}

#[tokio::test]
async fn cam_host_gets_only_its_linked_cameras_with_decrypted_password() {
    let a = tenant().await;
    let b = tenant().await;
    let mut pg = rt().await;
    let device = format!("alarm:{}", Uuid::new_v4().simple());
    let other_device = format!("alarm:{}", Uuid::new_v4().simple());
    let admin_a = caller(a, false);

    // 1. 担当で RTSP 付き (資格情報も) 2. 担当だが RTSP の設定なし 3. パスワード無し 4. 別の端末の担当 5. 別テナントで同じ端末 id
    let (s, full) = call(&mut pg, &admin_a, "POST", "/cameras", "", json!({"name": "A 入口", "lan_ip": "192.168.10.20", "rtsp_port": 8554, "rtsp_path": "stream1", "rec_username": "viewer", "rec_password": "pw-host", "device_ids": [device, "dev-x"]})).await;
    assert_eq!(s, 201, "{full}");
    let (s, bare) = call(&mut pg, &admin_a, "POST", "/cameras", "", json!({"name": "A 倉庫", "rec_username": "viewer", "rec_password": "pw-2", "device_ids": [device]})).await;
    assert_eq!(s, 201, "{bare}");
    let (s, nopass) = call(&mut pg, &admin_a, "POST", "/cameras", "", json!({"name": "A 事務所", "lan_ip": "10.1.2.3", "rtsp_path": "h264", "device_ids": [device]})).await;
    assert_eq!(s, 201, "{nopass}");
    let (s, _) = call(&mut pg, &admin_a, "POST", "/cameras", "", json!({"name": "A 別の端末", "lan_ip": "192.168.10.30", "rtsp_path": "stream1", "rec_password": "pw-other", "device_ids": [other_device]})).await;
    assert_eq!(s, 201);
    let (s, _) = call(&mut pg, &caller(b, false), "POST", "/cameras", "", json!({"name": "B 入口", "lan_ip": "192.168.10.40", "rtsp_path": "stream1", "rec_password": "pw-b", "device_ids": [device]})).await;
    assert_eq!(s, 201);

    // 順位表が空なので結び付けた端末として全員 1 位。並びは名前の順 (DB の照合順)
    let (s, mine) = call(&mut pg, &cam_host(a, &device), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(s, 200, "{mine}");
    assert_eq!(
        mine,
        json!({"cameras": [
            {"id": nopass["id"], "name": "A 事務所", "rank": 1, "rtsp": {"host": "10.1.2.3", "port": 554, "path": "h264", "username": null, "password": null}},
            {"id": bare["id"], "name": "A 倉庫", "rank": 1, "rtsp": null},
            {"id": full["id"], "name": "A 入口", "rank": 1, "rtsp": {"host": "192.168.10.20", "port": 8554, "path": "stream1", "username": "viewer", "password": "pw-host"}},
        ]})
    );
    // 担当の無い端末・別テナントの鍵は自分の分だけ
    let (_, none) = call(&mut pg, &cam_host(a, "alarm:unlinked"), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(none, json!({"cameras": []}));
    let (_, in_b) = call(&mut pg, &cam_host(b, &device), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(in_b["cameras"].as_array().unwrap().len(), 1);
    assert_eq!(in_b["cameras"][0]["name"], "B 入口");
    assert_eq!(in_b["cameras"][0]["rtsp"]["password"], "pw-b");

    // 鍵は他の口を使えない (書き込みも起きない)
    let (s, _) = call(&mut pg, &cam_host(a, &device), "GET", "/cameras", "", Value::Null).await;
    assert_eq!(s, 403);
    let (s, _) = call(&mut pg, &cam_host(a, &device), "PATCH", &format!("/cameras/{}", full["id"].as_str().unwrap()), "", json!({"lan_ip": "192.168.99.99"})).await;
    assert_eq!(s, 403);
    let (_, again) = call(&mut pg, &cam_host(a, &device), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(again, mine);

    // 鍵が違えば 500 (平文も暗号文も応答に出ない)
    let op = parse("GET", "/cameras/mine", "", "", &cam_host(a, &device), 0).unwrap();
    let e = execute(op, &cam_host(a, &device), &mut pg, Some("wrong-key")).await.unwrap_err();
    assert_eq!(e.status, 500);
    assert!(!e.body().to_string().contains("pw-host"));
}

#[tokio::test]
async fn cam_host_gets_cameras_by_stream_priority() {
    let a = tenant().await;
    let b = tenant().await;
    let mut pg = rt().await;
    let admin_a = caller(a, false);
    let pc1 = format!("alarm:{}", Uuid::new_v4().simple());
    let pc2 = format!("alarm:{}", Uuid::new_v4().simple());
    let measure = format!("alarm:{}", Uuid::new_v4().simple());
    let mine = |c: &Value| -> Vec<(Value, Value)> {
        c["cameras"].as_array().unwrap().iter().map(|x| (x["name"].clone(), x["rank"].clone())).collect()
    };

    // 入口: 切り出し用に 3 台を結び付け、配信は pc1 → pc2 の順。倉庫: 配信は pc2 → pc1 (pc2 が主)
    let (s, gate) = call(&mut pg, &admin_a, "POST", "/cameras", "", json!({"name": "入口", "lan_ip": "192.168.10.20", "rtsp_path": "stream1", "device_ids": [pc1, pc2, measure], "stream_device_ids": [pc1, pc2]})).await;
    assert_eq!(s, 201, "{gate}");
    assert_eq!(gate["stream_device_ids"], json!([pc1, pc2]));
    let (s, _) = call(&mut pg, &admin_a, "POST", "/cameras", "", json!({"name": "倉庫", "stream_device_ids": [pc2, pc1]})).await;
    assert_eq!(s, 201);
    // 別テナントの同じ鍵 (見えない)
    let (s, _) = call(&mut pg, &caller(b, false), "POST", "/cameras", "", json!({"name": "B", "stream_device_ids": [pc1]})).await;
    assert_eq!(s, 201);

    // 順位の順 (同じ順位は名前の順)。切り出し用に結び付けただけの端末は、順位表のあるカメラを受け持たない
    let (_, m1) = call(&mut pg, &cam_host(a, &pc1), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine(&m1), vec![(json!("入口"), json!(1)), (json!("倉庫"), json!(2))]);
    let (_, m2) = call(&mut pg, &cam_host(a, &pc2), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine(&m2), vec![(json!("倉庫"), json!(1)), (json!("入口"), json!(2))]);
    let (_, m3) = call(&mut pg, &cam_host(a, &measure), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(m3, json!({"cameras": []}));
    // 結び付け (切り出し・遠隔点呼) は変えていない
    let (_, list) = call(&mut pg, &admin_a, "GET", "/cameras", "", Value::Null).await;
    let row = list["cameras"].as_array().unwrap().iter().find(|c| c["id"] == gate["id"]).unwrap().clone();
    let mut linked = vec![pc1.clone(), pc2.clone(), measure.clone()];
    linked.sort();
    assert_eq!(row["device_ids"], json!(linked));
    assert_eq!(row["stream_device_ids"], json!([pc1, pc2]));

    // 並べ替え (送ると全差し替え)。省略は変えない
    let path = format!("/cameras/{}", gate["id"].as_str().unwrap());
    let (s, swapped) = call(&mut pg, &admin_a, "PATCH", &path, "", json!({"stream_device_ids": [pc2, measure, pc1]})).await;
    assert_eq!(s, 200, "{swapped}");
    assert_eq!(swapped["stream_device_ids"], json!([pc2, measure, pc1]));
    let (_, kept) = call(&mut pg, &admin_a, "PATCH", &path, "", json!({"name": "入口 2"})).await;
    assert_eq!(kept["stream_device_ids"], json!([pc2, measure, pc1]));
    let (_, m1) = call(&mut pg, &cam_host(a, &pc1), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine(&m1), vec![(json!("倉庫"), json!(2)), (json!("入口 2"), json!(3))]);
    // 重複は 400 (何も変えない)
    let (s, _) = call(&mut pg, &admin_a, "PATCH", &path, "", json!({"stream_device_ids": [pc1, pc1]})).await;
    assert_eq!(s, 400);

    // 空にすると、結び付けた端末の全員が 1 位に戻る (順位を入れる前の扱い)
    let (s, cleared) = call(&mut pg, &admin_a, "PATCH", &path, "", json!({"stream_device_ids": []})).await;
    assert_eq!(s, 200);
    assert_eq!(cleared["stream_device_ids"], json!([]));
    let (_, m3) = call(&mut pg, &cam_host(a, &measure), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine(&m3), vec![(json!("入口 2"), json!(1))]);
    let (_, m1) = call(&mut pg, &cam_host(a, &pc1), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine(&m1), vec![(json!("入口 2"), json!(1)), (json!("倉庫"), json!(2))]);

    // 別テナントの鍵には別テナントの分だけ
    let (_, in_b) = call(&mut pg, &cam_host(b, &pc1), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine(&in_b), vec![(json!("B"), json!(1))]);

    // カメラを消すと順位表も消える (CASCADE。残っているのは倉庫の 2 行だけ)
    let su = admin().await;
    let rows: i64 = su
        .query_one("SELECT count(*) FROM alc_api.cam_stream_devices WHERE tenant_id = $1", &[&a])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 2);
    let warehouse = Uuid::parse_str(m1["cameras"][1]["id"].as_str().unwrap()).unwrap();
    su.execute("DELETE FROM alc_api.cameras WHERE id = $1", &[&warehouse]).await.unwrap();
    let rows: i64 = su
        .query_one("SELECT count(*) FROM alc_api.cam_stream_devices WHERE tenant_id = $1", &[&a])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn other_tenant_rows_are_invisible() {
    let a = tenant().await;
    let b = tenant().await;
    let mut pg = rt().await;
    let (s, cam) = call(
        &mut pg,
        &caller(a, true),
        "POST",
        "/cameras",
        "",
        json!({"name": "A のカメラ", "vpc_binding": random_binding(), "device_ids": ["dev-a"]}),
    )
    .await;
    assert_eq!(s, 201);
    let id = Uuid::parse_str(cam["id"].as_str().unwrap()).unwrap();
    pg::record_clip(
        &mut pg,
        a,
        ClipRecord {
            camera_id: id,
            from_ms: 1_000_000,
            to_ms: 2_000_000,
            r2_key: "k".into(),
            size_bytes: Some(1),
            status: "ready",
            error: None,
        },
    )
    .await
    .unwrap();

    // B からは一覧にも、PATCH にも、切り出しの元にも出てこない
    let (s, list) = call(
        &mut pg,
        &caller(b, true),
        "GET",
        "/cameras",
        "",
        Value::Null,
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(list["cameras"], json!([]));
    let (s, _) = call(
        &mut pg,
        &caller(b, true),
        "PATCH",
        &format!("/cameras/{id}"),
        "",
        json!({"name": "乗っ取り"}),
    )
    .await;
    assert_eq!(s, 404);
    assert_eq!(pg::clip_source(&mut pg, b, id).await.unwrap(), None);
    // B のテナントで A のカメラに切り出しを記録しようとしても FK / RLS で落ちる
    assert!(pg::record_clip(
        &mut pg,
        b,
        ClipRecord {
            camera_id: id,
            from_ms: 3_000_000,
            to_ms: 4_000_000,
            r2_key: "k".into(),
            size_bytes: None,
            status: "failed",
            error: None
        }
    )
    .await
    .is_err());
    // RLS だけでも見えない (WHERE の tenant_id を外した文を B のテナントで流す)
    let seen: i64 = pg
        .tenant_tx(b, move |tx| {
            Box::pin(async move {
                let n = tx.query_typed_one("SELECT (SELECT count(*) FROM alc_api.cameras) + (SELECT count(*) FROM alc_api.cam_camera_devices) + (SELECT count(*) FROM alc_api.cam_clips)", &[]).await?;
                Ok(n.get::<_, i64>(0))
            })
        })
        .await
        .unwrap();
    assert_eq!(seen, 0);
    // A のものは名前が変わっていない
    assert_eq!(
        pg::list_cameras(&mut pg, a, Some(id)).await.unwrap()[0].name,
        "A のカメラ"
    );
}

#[tokio::test]
async fn same_destination_in_another_tenant_is_409() {
    let a = tenant().await;
    let b = tenant().await;
    let mut pg = rt().await;
    let binding = random_binding();
    let (s, _) = call(
        &mut pg,
        &caller(a, true),
        "POST",
        "/cameras",
        "",
        json!({"name": "A", "vpc_binding": binding}),
    )
    .await;
    assert_eq!(s, 201);
    let (s, body) = call(
        &mut pg,
        &caller(b, true),
        "POST",
        "/cameras",
        "",
        json!({"name": "B", "vpc_binding": binding}),
    )
    .await;
    assert_eq!(s, 409);
    // 他テナントの情報 (宛先・名前・id) を返さない
    let text = body.to_string();
    assert!(
        !text.contains(&binding) && !text.contains(&a.to_string()),
        "{text}"
    );
    // PATCH で同じ宛先にしても 409
    let (_, cb) = call(
        &mut pg,
        &caller(b, true),
        "POST",
        "/cameras",
        "",
        json!({"name": "B"}),
    )
    .await;
    let (s, _) = call(
        &mut pg,
        &caller(b, true),
        "PATCH",
        &format!("/cameras/{}", cb["id"].as_str().unwrap()),
        "",
        json!({"vpc_binding": binding}),
    )
    .await;
    assert_eq!(s, 409);
}

#[tokio::test]
async fn device_candidates_are_counted() {
    let t = tenant().await;
    let other = tenant().await;
    let su = admin().await;
    // hub_measurements: kind ごと・古い行・対象外の kind・他テナント・recorded_at が NULL の行
    let rows: [(Uuid, &str, &str, &str); 8] = [
        (t, "alarm:d1", "alcohol", "now() - interval '1 day'"),
        (t, "alarm:d1", "alcohol", "now() - interval '2 days'"),
        (t, "alarm:d1", "blood_pressure", "now() - interval '3 days'"),
        (t, "alarm:d1", "temperature", "now() - interval '1 day'"),
        (t, "alarm:d2", "timecard", "now() - interval '5 hours'"),
        (t, "alarm:d2", "license", "now() - interval '40 days'"),
        (t, "alarm:d3", "alcohol", "now() - interval '45 days'"),
        (other, "alarm:d1", "alcohol", "now() - interval '1 day'"),
    ];
    for (i, (tenant, device, kind, at)) in rows.iter().enumerate() {
        su.execute(
            &format!("INSERT INTO alc_api.hub_measurements (tenant_id, device_id, kind, payload, seq, recorded_at) VALUES ($1, $2, $3, '{{}}', $4, {at})"),
            &[tenant, device, kind, &(i as i64)],
        )
        .await
        .unwrap();
    }
    // recorded_at が NULL なら created_at で数える
    su.execute("INSERT INTO alc_api.hub_measurements (tenant_id, device_id, kind, payload, seq, recorded_at) VALUES ($1, 'alarm:d2', 'timecard', '{}', 100, NULL)", &[&t]).await.unwrap();
    // 指静脈の登録 (registered_device_id が NULL の行は数えない)
    let emp: Uuid = su.query_one("INSERT INTO alc_api.employees (tenant_id, nfc_id, name) VALUES ($1, $2, '乗務員') RETURNING id", &[&t, &format!("nfc-{}", Uuid::new_v4())]).await.unwrap().get(0);
    su.execute("INSERT INTO alc_api.vein_enroll_events (tenant_id, employee_id, registered_device_id) VALUES ($1, $2, 'alarm:d3'), ($1, $2, NULL)", &[&t, &emp]).await.unwrap();

    let mut pg = rt().await;
    let c = caller(t, false);
    let (s, body) = call(&mut pg, &c, "GET", "/device-candidates", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    let got: Vec<(String, Value)> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["device_id"].as_str().unwrap().to_owned(),
                r["counts"].clone(),
            )
        })
        .collect();
    // 新しい順: alarm:d3 (指静脈 = 今) → alarm:d2 (created_at = 今の timecard) → alarm:d1
    assert_eq!(
        got,
        vec![
            (
                "alarm:d3".to_owned(),
                json!({"alcohol": 0, "blood_pressure": 0, "timecard": 0, "license": 0, "vein_enroll": 1})
            ),
            (
                "alarm:d2".to_owned(),
                json!({"alcohol": 0, "blood_pressure": 0, "timecard": 2, "license": 0, "vein_enroll": 0})
            ),
            (
                "alarm:d1".to_owned(),
                json!({"alcohol": 2, "blood_pressure": 1, "timecard": 0, "license": 0, "vein_enroll": 0})
            ),
        ]
    );
    assert!(body[0]["last_at"].as_str().unwrap().ends_with("+09:00"));

    // days=60 なら 40・45 日前も入る
    let (_, body) = call(
        &mut pg,
        &c,
        "GET",
        "/device-candidates",
        "days=60",
        Value::Null,
    )
    .await;
    let dev3 = body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["device_id"] == "alarm:d3")
        .unwrap();
    assert_eq!(dev3["counts"]["alcohol"], 1);
    let dev2 = body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["device_id"] == "alarm:d2")
        .unwrap();
    assert_eq!(dev2["counts"]["license"], 1);
}

#[tokio::test]
async fn dev_device_candidates_are_marked() {
    let t = tenant().await;
    let other = tenant().await;
    let su = admin().await;
    let emp: Uuid = su.query_one("INSERT INTO alc_api.employees (tenant_id, nfc_id, name) VALUES ($1, $2, '乗務員') RETURNING id", &[&t, &format!("nfc-{}", Uuid::new_v4())]).await.unwrap().get(0);
    let emp_other: Uuid = su.query_one("INSERT INTO alc_api.employees (tenant_id, nfc_id, name) VALUES ($1, $2, '他社') RETURNING id", &[&other, &format!("nfc-{}", Uuid::new_v4())]).await.unwrap().get(0);
    // 本番の行 (app.device_dev 未設定)
    su.execute("INSERT INTO alc_api.hub_measurements (tenant_id, device_id, kind, payload, seq, recorded_at) VALUES ($1, 'alarm:p1', 'alcohol', '{}', 1, now() - interval '3 hours')", &[&t]).await.unwrap();
    // 開発端末の行は RLS の WITH CHECK に合わせ、app.device_dev='1' を立てた transaction で入れる
    for (tenant, employee, device, at) in [
        (t, emp, "alarm:d1", "now() - interval '1 hour'"),
        (t, emp, "alarm:d2", "now() - interval '2 hours'"),
        (other, emp_other, "alarm:dother", "now() - interval '1 hour'"),
    ] {
        // 指静脈の登録 (created_at = 今) は alarm:d1 と他テナントだけ。alarm:d2 は 2 時間前の timecard が最終
        let vein = if device == "alarm:d2" {
            String::new()
        } else {
            format!("INSERT INTO alc_api.vein_enroll_events (tenant_id, employee_id, registered_device_id) VALUES ('{tenant}', '{employee}', '{device}');")
        };
        su.batch_execute(&format!(
            "BEGIN; SELECT set_config('app.device_dev', '1', true); \
             INSERT INTO alc_api.hub_measurements (tenant_id, device_id, kind, payload, seq, recorded_at) VALUES ('{tenant}', '{device}', 'timecard', '{{}}', 2, {at}); \
             {vein} COMMIT;"
        ))
        .await
        .unwrap();
    }
    let flags: Vec<bool> = su
        .query("SELECT is_dev FROM alc_api.hub_measurements WHERE tenant_id = $1 AND device_id LIKE 'alarm:d%' ORDER BY device_id", &[&t])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(flags, [true, true], "準備の行が is_dev = true で入っていない");

    let mut pg = rt().await;
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/device-candidates", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    let got: Vec<(&str, bool)> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["device_id"].as_str().unwrap(), r["is_dev"].as_bool().unwrap()))
        .collect();
    // 新しい順に、本番と開発端末が混ざって並ぶ。他テナントの開発端末は出ない
    assert_eq!(got, [("alarm:d1", true), ("alarm:d2", true), ("alarm:p1", false)]);
    assert_eq!(body[0]["counts"]["timecard"], 1);
    assert_eq!(body[0]["counts"]["vein_enroll"], 1);
    assert_eq!(body[2]["counts"]["alcohol"], 1);

    // set_config は transaction 単位: 同じ接続の次の呼び出し (本番の軸) に漏れない
    let (_, again) = call(&mut pg, &caller(t, false), "GET", "/device-candidates", "", Value::Null).await;
    assert_eq!(again, body);
    let leaked: Option<String> = pg
        .tenant_tx(t, move |tx| {
            Box::pin(async move {
                let r = tx.query_typed_one("SELECT current_setting('app.device_dev', true)", &[]).await?;
                Ok(r.get::<_, Option<String>>(0))
            })
        })
        .await
        .unwrap();
    assert!(matches!(leaked.as_deref(), None | Some("")), "{leaked:?}");
}

#[tokio::test]
async fn clips_are_recorded() {
    let t = tenant().await;
    let mut pg = rt().await;
    let (_, cam) = call(
        &mut pg,
        &caller(t, true),
        "POST",
        "/cameras",
        "",
        json!({"name": "C", "vpc_binding": random_binding()}),
    )
    .await;
    let id = Uuid::parse_str(cam["id"].as_str().unwrap()).unwrap();
    let from = 1_792_000_000_000_i64;
    let first = pg::record_clip(
        &mut pg,
        t,
        ClipRecord {
            camera_id: id,
            from_ms: from,
            to_ms: from + 90_000,
            r2_key: "clips/a".into(),
            size_bytes: None,
            status: "failed",
            error: Some("カメラに届きません".into()),
        },
    )
    .await
    .unwrap();
    // 同じ開始のやり直しは同じ行を上書きする
    let again = pg::record_clip(
        &mut pg,
        t,
        ClipRecord {
            camera_id: id,
            from_ms: from,
            to_ms: from + 90_500,
            r2_key: "clips/b".into(),
            size_bytes: Some(1234),
            status: "ready",
            error: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(first, again);
    let row = admin()
        .await
        .query_one(
            "SELECT status, r2_key, size_bytes, error, (extract(epoch FROM started_at) * 1000)::int8, (extract(epoch FROM ended_at) * 1000)::int8 FROM alc_api.cam_clips WHERE tenant_id = $1",
            &[&t],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "ready");
    assert_eq!(row.get::<_, String>(1), "clips/b");
    assert_eq!(row.get::<_, Option<i64>>(2), Some(1234));
    assert_eq!(row.get::<_, Option<String>>(3), None);
    assert_eq!(
        (row.get::<_, i64>(4), row.get::<_, i64>(5)),
        (from, from + 90_500)
    );
}

// ---- cron の自動切り出し (`cam_ingest_api::auto`) と GET /events ----

use cam_ingest_api::auto::{run_camera, AutoCamera, ListedFile, Recorder};

/// 2026-10-23 12:00:00 JST (時のフォルダの頭)
const T: i64 = 1_792_724_400_000;
const S: i64 = 1000;

/// 端末のイベントを 1 行入れる (時刻は UNIX ms。`at_sql` が Some ならその式を recorded_at に)。
/// 開発端末の行は RLS の WITH CHECK に合わせ、`app.device_dev='1'` を立てた transaction で入れる
async fn hub_event(su: &Client, t: Uuid, device: &str, kind: &str, recorded: Option<i64>, created: i64, dev: bool) -> Uuid {
    hub_event_via(su, t, device, None, kind, recorded, created, dev).await
}

/// `via` = 接続に使った hub の ID (`device` に機体の鍵の ID を入れた行が、鍵 ⇔ hub の結び付きになる)
#[allow(clippy::too_many_arguments)]
async fn hub_event_via(su: &Client, t: Uuid, device: &str, via: Option<&str>, kind: &str, recorded: Option<i64>, created: i64, dev: bool) -> Uuid {
    let id = Uuid::new_v4();
    let at = |ms: i64| format!("'epoch'::timestamptz + {ms} * interval '1 millisecond'");
    let recorded = recorded.map(at).unwrap_or_else(|| "NULL".into());
    let via = via.map(|v| format!("'{v}'")).unwrap_or_else(|| "NULL".into());
    let set = if dev { "SELECT set_config('app.device_dev', '1', true);" } else { "" };
    su.batch_execute(&format!(
        "BEGIN; {set} INSERT INTO alc_api.hub_measurements (id, tenant_id, device_id, via_device_id, kind, payload, seq, recorded_at, created_at) \
         VALUES ('{id}', '{t}', '{device}', {via}, '{kind}', '{{}}', {seq}, {recorded}, {created}); COMMIT;",
        seq = id.as_u128() as i64 & i64::MAX,
        created = at(created),
    ))
    .await
    .unwrap();
    id
}

/// measurements の 1 行 (PC 経由の測定)。`measured` = アルコールの時刻、`bp` = (血圧の時刻, 最高血圧)
async fn measurement(su: &Client, t: Uuid, device: Option<&str>, status: &str, measured: i64, bp: Option<(i64, Option<i32>)>, dev: bool) -> Uuid {
    let emp: Uuid = su
        .query_one("INSERT INTO alc_api.employees (tenant_id, nfc_id, name) VALUES ($1, $2, '乗務員') RETURNING id", &[&t, &format!("nfc-{}", Uuid::new_v4())])
        .await
        .unwrap()
        .get(0);
    let id = Uuid::new_v4();
    let at = |ms: i64| format!("'epoch'::timestamptz + {ms} * interval '1 millisecond'");
    let device = device.map(|d| format!("'{d}'")).unwrap_or_else(|| "NULL".into());
    let (medical_at, systolic) = match bp {
        Some((ms, sys)) => (at(ms), sys.map(|v| v.to_string()).unwrap_or_else(|| "NULL".into())),
        None => ("NULL".into(), "NULL".into()),
    };
    let set = if dev { "SELECT set_config('app.device_dev', '1', true);" } else { "" };
    su.batch_execute(&format!(
        "BEGIN; {set} INSERT INTO alc_api.measurements (id, tenant_id, employee_id, device_id, status, measured_at, medical_measured_at, systolic) \
         VALUES ('{id}', '{t}', '{emp}', {device}, '{status}', {m}, {medical_at}, {systolic}); COMMIT;",
        m = at(measured),
    ))
    .await
    .unwrap();
    id
}

async fn vein_event(su: &Client, t: Uuid, device: &str, created: i64, dev: bool) -> Uuid {
    let emp: Uuid = su
        .query_one("INSERT INTO alc_api.employees (tenant_id, nfc_id, name) VALUES ($1, $2, '乗務員') RETURNING id", &[&t, &format!("nfc-{}", Uuid::new_v4())])
        .await
        .unwrap()
        .get(0);
    let id = Uuid::new_v4();
    let set = if dev { "SELECT set_config('app.device_dev', '1', true);" } else { "" };
    su.batch_execute(&format!(
        "BEGIN; {set} INSERT INTO alc_api.vein_enroll_events (id, tenant_id, employee_id, registered_device_id, created_at) \
         VALUES ('{id}', '{t}', '{emp}', '{device}', 'epoch'::timestamptz + {created} * interval '1 millisecond'); COMMIT;"
    ))
    .await
    .unwrap();
    id
}

/// 録画を取り込むカメラを作る (結び付ける端末つき)
async fn recording_camera(pg: &mut PgClient, t: Uuid, devices: &[&str]) -> Uuid {
    let (s, cam) = call(pg, &caller(t, true), "POST", "/cameras", "", json!({"name": "C", "vpc_binding": random_binding(), "device_ids": devices})).await;
    assert_eq!(s, 201, "{cam}");
    Uuid::parse_str(cam["id"].as_str().unwrap()).unwrap()
}

/// `name` の頭の数字 = 12 時からの秒 (開始時刻)
fn sd(secs: i64, closed: bool) -> ListedFile {
    let start_ms = T + secs * S;
    let c = cam_sd::jst_compact(start_ms);
    let name = format!("Schedule{}_{}{}.mp4", &c[..8], &c[9..], if closed { "" } else { "_!" });
    ListedFile { kind: "Schedule".into(), date: "20261023".into(), hour: "12".into(), name, size: 100, start_ms, closed }
}

/// カメラと R2 の差し替え: 一覧は決めた値、取り寄せと切り出しは呼ばれた順に覚える
#[derive(Default)]
struct Fake {
    files: Vec<ListedFile>,
    list_fails: bool,
    fail_fetch: Vec<String>,
    fail_cut: bool,
    lists: usize,
    fetches: Vec<String>,
    cuts: Vec<(String, Vec<String>, i64, i64)>,
}

impl Recorder for Fake {
    async fn list(&mut self, _from_ms: i64, _to_ms: i64) -> Result<Vec<ListedFile>, String> {
        self.lists += 1;
        if self.list_fails {
            return Err("カメラに届きません".into());
        }
        Ok(self.files.clone())
    }

    async fn fetch_raw(&mut self, file: &ListedFile) -> Result<(), String> {
        assert!(file.closed, "書き込み途中のファイルを取り寄せた");
        self.fetches.push(file.name.clone());
        if self.fail_fetch.contains(&file.name) {
            return Err(format!("{} の取り出しが 500 で失敗", file.name));
        }
        Ok(())
    }

    async fn cut(&mut self, key: &str, files: &[&ListedFile], from_ms: i64, to_ms: i64) -> Result<u64, String> {
        for f in files {
            assert!(self.fetches.contains(&f.name), "R2 にそろえていないファイルから切った");
        }
        self.cuts.push((key.to_owned(), files.iter().map(|f| f.name.clone()).collect(), from_ms, to_ms));
        if self.fail_cut {
            return Err("moov が見つかりません".into());
        }
        Ok(4242)
    }
}

/// cam_clips の (開始, 終了 (12 時からの秒), status, error) を開始の順に
async fn clips_of(t: Uuid, cam: Uuid) -> Vec<(i64, i64, String, Option<String>)> {
    admin()
        .await
        .query(
            "SELECT (extract(epoch FROM started_at) * 1000)::int8, (extract(epoch FROM ended_at) * 1000)::int8, status, error
               FROM alc_api.cam_clips WHERE tenant_id = $1 AND camera_id = $2 ORDER BY started_at",
            &[&t, &cam],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| ((r.get::<_, i64>(0) - T) / S, (r.get::<_, i64>(1) - T) / S, r.get(2), r.get(3)))
        .collect()
}

/// cam_clip_events の (元の行, is_dev, clip の開始 (12 時からの秒))
async fn clip_events_of(t: Uuid, cam: Uuid) -> Vec<(Uuid, bool, i64)> {
    admin()
        .await
        .query(
            "SELECT e.source_id, e.is_dev, (extract(epoch FROM c.started_at) * 1000)::int8
               FROM alc_api.cam_clip_events e JOIN alc_api.cam_clips c ON c.tenant_id = e.tenant_id AND c.id = e.clip_id
              WHERE e.tenant_id = $1 AND e.camera_id = $2 ORDER BY e.occurred_at, e.source_id",
            &[&t, &cam],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), (r.get::<_, i64>(2) - T) / S))
        .collect()
}

async fn scanned_until(t: Uuid, cam: Uuid) -> Option<i64> {
    admin()
        .await
        .query_one("SELECT (extract(epoch FROM rec_scanned_until) * 1000)::int8 FROM alc_api.cameras WHERE tenant_id = $1 AND id = $2", &[&t, &cam])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn auto_events_are_picked_from_linked_devices_on_both_axes() {
    let t = tenant().await;
    let other = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let cam = recording_camera(&mut pg, t, &["prod-1", "dev-1"]).await;
    let (from, to) = (T, T + 3600 * S);

    let alcohol = hub_event(&su, t, "prod-1", "alcohol", Some(T + 10 * S), T + 99 * S, false).await;
    // recorded_at が NULL なら created_at
    let timecard = hub_event(&su, t, "prod-1", "timecard", None, T + 20 * S, false).await;
    // 範囲の頭ちょうどは入る・終わりちょうどは入らない
    let at_from = hub_event(&su, t, "prod-1", "blood_pressure", Some(from), from, false).await;
    hub_event(&su, t, "prod-1", "license", Some(to), to, false).await;
    // kind 外・結び付けていない端末・他テナントの同じ端末 id
    hub_event(&su, t, "prod-1", "temperature", Some(T + 30 * S), T + 30 * S, false).await;
    hub_event(&su, t, "prod-9", "alcohol", Some(T + 30 * S), T + 30 * S, false).await;
    hub_event(&su, other, "prod-1", "alcohol", Some(T + 30 * S), T + 30 * S, false).await;
    // 開発端末の行・指静脈の登録 (本番と開発端末)
    let dev_license = hub_event(&su, t, "dev-1", "license", Some(T + 40 * S), T + 40 * S, true).await;
    let vein = vein_event(&su, t, "prod-1", T + 50 * S, false).await;
    let dev_vein = vein_event(&su, t, "dev-1", T + 60 * S, true).await;
    vein_event(&su, other, "dev-1", T + 60 * S, true).await;

    let got = pg::auto_events(&mut pg, t, cam, from, to).await.unwrap();
    let mut seen: Vec<(Uuid, &str, &str, i64, bool)> =
        got.iter().map(|e| (e.source_id, e.source_table.as_str(), e.kind.as_str(), (e.at_ms - T) / S, e.is_dev)).collect();
    seen.sort_by_key(|x| x.3);
    assert_eq!(
        seen,
        vec![
            (at_from, "hub_measurements", "blood_pressure", 0, false),
            (alcohol, "hub_measurements", "alcohol", 10, false),
            (timecard, "hub_measurements", "timecard", 20, false),
            (dev_license, "hub_measurements", "license", 40, true),
            (vein, "vein_enroll_events", "vein_enroll", 50, false),
            (dev_vein, "vein_enroll_events", "vein_enroll", 60, true),
        ]
    );

    // cam_clip_events に入れたもの (本番・開発端末の両方) は次から拾わない。同じものをもう一度入れても落ちない
    let clip = pg::record_clip(&mut pg, t, ClipRecord { camera_id: cam, from_ms: T, to_ms: T + 70 * S, r2_key: "clips/x".into(), size_bytes: Some(1), status: "ready", error: None }).await.unwrap();
    let picked: Vec<_> = got.iter().filter(|e| [alcohol, dev_license].contains(&e.source_id)).cloned().collect();
    pg::record_clip_events(&mut pg, t, cam, clip, picked.clone()).await.unwrap();
    pg::record_clip_events(&mut pg, t, cam, clip, picked).await.unwrap();
    let rest: Vec<Uuid> = pg::auto_events(&mut pg, t, cam, from, to).await.unwrap().iter().map(|e| e.source_id).collect();
    assert_eq!(rest.len(), 4);
    assert!(!rest.contains(&alcohol) && !rest.contains(&dev_license));
    let rows = clip_events_of(t, cam).await;
    assert_eq!(rows, vec![(alcohol, false, 0), (dev_license, true, 0)]);
    let kinds: Vec<(String, Option<String>)> = su
        .query("SELECT kind, device_id FROM alc_api.cam_clip_events WHERE tenant_id = $1 ORDER BY occurred_at", &[&t])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(kinds, vec![("alcohol".into(), Some("prod-1".into())), ("license".into(), Some("dev-1".into()))]);

    // 別のカメラでは同じイベントをまた拾う (1 イベントが複数カメラに映るのは可)
    let cam2 = recording_camera(&mut pg, t, &["prod-1"]).await;
    let on2 = pg::auto_events(&mut pg, t, cam2, from, to).await.unwrap();
    assert!(on2.iter().any(|e| e.source_id == alcohol));

    // 走査済み時刻・テナントとカメラの列挙
    pg::set_scanned_until(&mut pg, t, cam, T + 1234).await.unwrap();
    assert_eq!(scanned_until(t, cam).await, Some(T + 1234));
    assert!(pg::recording_tenants(&mut pg).await.unwrap().contains(&t));
    let (_, plain) = call(&mut pg, &caller(t, false), "POST", "/cameras", "", json!({"name": "ONVIF だけ"})).await;
    let plain = Uuid::parse_str(plain["id"].as_str().unwrap()).unwrap();
    let cams: Vec<Uuid> = pg::recording_cameras(&mut pg, t).await.unwrap().iter().map(|c| c.id).collect();
    assert!(cams.contains(&cam) && cams.contains(&cam2) && !cams.contains(&plain));
    let me = pg::recording_cameras(&mut pg, t).await.unwrap().into_iter().find(|c| c.id == cam).unwrap();
    assert_eq!(me.scanned_until_ms, Some(T + 1234));
}

#[tokio::test]
async fn cron_cuts_closed_windows_and_defers_the_rest() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let cam = recording_camera(&mut pg, t, &["prod-1", "dev-1"]).await;
    // 12:00:00 / 12:04:00 は閉じた、12:08:00 は書き込み途中 → 閉じた終わりは 12:08:00 (480 秒)
    let mut fake = Fake { files: vec![sd(0, true), sd(240, true), sd(480, false)], ..Default::default() };
    let e1 = hub_event(&su, t, "prod-1", "alcohol", Some(T + 100 * S), T + 100 * S, false).await;
    let e2 = hub_event(&su, t, "prod-1", "blood_pressure", Some(T + 115 * S), T + 115 * S, false).await; // e1 と重なる → 1 本
    let e3 = vein_event(&su, t, "prod-1", T + 200 * S, false).await; // 同じファイルの別のまとまり (取り寄せは使い回す)
    let e4 = hub_event(&su, t, "dev-1", "timecard", Some(T + 235 * S), T + 235 * S, true).await; // 2 ファイルにまたがる
    let e5 = hub_event(&su, t, "prod-1", "license", Some(T + 470 * S), T + 470 * S, false).await; // e6 と窓が重なる → [460, 485]
    let e6 = hub_event(&su, t, "prod-1", "alcohol", Some(T + 475 * S), T + 475 * S, false).await; // まとまりの終わり 485 が閉じた終わり 480 を越える → 次の回
    let e7 = hub_event(&su, t, "dev-1", "alcohol", Some(T + 600 * S), T + 600 * S, true).await;

    let c = AutoCamera { id: cam, scanned_until_ms: Some(T) };
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 700 * S, 40).await.unwrap();
    assert_eq!((o.events, o.groups, o.ready, o.failed, o.deferred, o.fetched), (7, 5, 3, 0, 2, 2), "{}", o.line());
    // e5 と e6 は窓が重なるので 1 本 ([460, 485]) で、その終わりが 480 を越える → まとめて次の回
    assert_eq!(fake.lists, 1);
    assert_eq!(fake.fetches, [sd(0, true).name, sd(240, true).name]);
    let cuts: Vec<(Vec<String>, i64, i64)> = fake.cuts.iter().map(|(_, f, a, b)| (f.clone(), (a - T) / S, (b - T) / S)).collect();
    assert_eq!(
        cuts,
        vec![
            (vec![sd(0, true).name], 90, 125),
            (vec![sd(0, true).name], 190, 210),
            (vec![sd(0, true).name, sd(240, true).name], 225, 245),
        ]
    );
    assert_eq!(fake.cuts[0].0, cam_ingest_api::clip_r2_key(t, cam, T + 90 * S, T + 125 * S));
    assert_eq!(clips_of(t, cam).await, vec![(90, 125, "ready".into(), None), (190, 210, "ready".into(), None), (225, 245, "ready".into(), None)]);
    assert_eq!(clip_events_of(t, cam).await, vec![(e1, false, 90), (e2, false, 90), (e3, false, 190), (e4, true, 225)]);
    // 走査済みは次の回へ回したまとまりの頭 (最初のイベント 470 - 10 秒)
    assert_eq!(scanned_until(t, cam).await, Some(T + 460 * S));

    // GET /events: その日のイベントと、切り出し済みならその clip (次の回待ちは null)
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/events", &format!("camera_id={cam}&date=2026-10-23"), Value::Null).await;
    assert_eq!(s, 200, "{body}");
    let rows = body.as_array().unwrap();
    let ids: Vec<String> = rows.iter().map(|r| r["source_id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, [e1, e2, e3, e4, e5, e6, e7].map(|u| u.to_string()));
    assert_eq!(rows[0]["kind"], "alcohol");
    assert_eq!(rows[0]["occurred_at"], "2026-10-23T12:01:40.000+09:00");
    assert_eq!(rows[0]["device_id"], "prod-1");
    assert_eq!(rows[0]["is_dev"], false);
    assert_eq!(rows[0]["clip"]["status"], "ready");
    assert_eq!(rows[0]["clip"]["key"], fake.cuts[0].0.as_str());
    assert_eq!(rows[0]["clip"]["clip_id"], rows[1]["clip"]["clip_id"]);
    assert_eq!(rows[2]["source_table"], "vein_enroll_events");
    assert_eq!(rows[2]["kind"], "vein_enroll");
    assert_eq!((rows[3]["is_dev"].as_bool(), rows[3]["clip"]["status"].as_str()), (Some(true), Some("ready")));
    assert!(rows[4]["clip"].is_null() && rows[5]["clip"].is_null() && rows[6]["clip"].is_null());
    // 前日・他テナント・無いカメラ
    let (_, prev) = call(&mut pg, &caller(t, false), "GET", "/events", &format!("camera_id={cam}&date=2026-10-22"), Value::Null).await;
    assert_eq!(prev, json!([]));
    let other = tenant().await;
    let (s, _) = call(&mut pg, &caller(other, false), "GET", "/events", &format!("camera_id={cam}&date=2026-10-23"), Value::Null).await;
    assert_eq!(s, 404);

    // 次の回: 12:08:00 が閉じ、12:12:00 が書き込み途中に。残りを切って upper まで進む
    fake.files = vec![sd(0, true), sd(240, true), sd(480, true), sd(720, false)];
    fake.fetches.clear();
    fake.cuts.clear();
    let c = AutoCamera { id: cam, scanned_until_ms: scanned_until(t, cam).await };
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 800 * S, 40).await.unwrap();
    assert_eq!((o.events, o.ready, o.deferred), (3, 2, 0), "{}", o.line());
    assert_eq!(fake.fetches, [sd(240, true).name, sd(480, true).name]);
    assert_eq!(scanned_until(t, cam).await, Some(T + 800 * S));
    let all = clip_events_of(t, cam).await;
    assert_eq!(&all[4..], &[(e5, false, 460), (e6, false, 460), (e7, true, 590)]);

    // その次の回: 新しいイベントが無い → 一覧を取りに行かずに upper まで
    let lists = fake.lists;
    let c = AutoCamera { id: cam, scanned_until_ms: scanned_until(t, cam).await };
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 900 * S, 40).await.unwrap();
    assert_eq!((o.events, fake.lists), (0, lists));
    assert_eq!(scanned_until(t, cam).await, Some(T + 900 * S));
}

#[tokio::test]
async fn cron_failures_are_recorded_and_not_retried() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let cam = recording_camera(&mut pg, t, &["prod-1"]).await;
    let files = vec![sd(0, true), sd(240, true), sd(480, false)];
    let e0 = hub_event(&su, t, "prod-1", "alcohol", Some(T - 600 * S), T - 600 * S, false).await; // 録画の前
    let e1 = hub_event(&su, t, "prod-1", "alcohol", Some(T + 50 * S), T + 50 * S, false).await;
    let e2 = hub_event(&su, t, "prod-1", "alcohol", Some(T + 150 * S), T + 150 * S, false).await;
    let e3 = hub_event(&su, t, "prod-1", "alcohol", Some(T + 300 * S), T + 300 * S, false).await;

    // 12:00:00 の取り寄せが失敗 → そのファイルにかかる 2 本とも failed (取り寄せは 1 回だけ)、12:04:00 の 1 本は切れる
    let mut fake = Fake { files: files.clone(), fail_fetch: vec![sd(0, true).name], ..Default::default() };
    let c = AutoCamera { id: cam, scanned_until_ms: Some(T - 3600 * S) };
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 700 * S, 40).await.unwrap();
    assert_eq!((o.ready, o.failed, o.fetched), (1, 3, 2), "{}", o.line());
    assert_eq!(fake.fetches, [sd(0, true).name, sd(240, true).name]);
    let clips = clips_of(t, cam).await;
    assert_eq!(clips[0], (-610, -590, "failed".into(), Some("その時間帯の録画が SD にありません".into())));
    assert_eq!(clips[1].2, "failed");
    assert!(clips[1].3.as_deref().unwrap().contains("取り出しが 500"));
    assert_eq!(clips[2].2, "failed");
    assert_eq!(clips[3], (290, 310, "ready".into(), None));
    // failed でもイベントとの対応は残る → 次の回に拾い直さない
    let ev: Vec<Uuid> = clip_events_of(t, cam).await.iter().map(|e| e.0).collect();
    assert_eq!(ev, [e0, e1, e2, e3]);
    let o = run_camera(&mut pg, t, &AutoCamera { id: cam, scanned_until_ms: Some(T - 3600 * S) }, &mut fake, T + 700 * S, 40).await.unwrap();
    assert_eq!((o.events, o.ready, o.failed), (0, 0, 0));

    // 切り出しの失敗も failed
    let cam2 = recording_camera(&mut pg, t, &["prod-1"]).await;
    let mut fake = Fake { files: files.clone(), fail_cut: true, ..Default::default() };
    let o = run_camera(&mut pg, t, &AutoCamera { id: cam2, scanned_until_ms: Some(T) }, &mut fake, T + 700 * S, 40).await.unwrap();
    // 24 時間前から見るので、録画の前の e0 (録画なしで failed) も入る
    assert_eq!((o.ready, o.failed), (0, 4));
    assert!(clips_of(t, cam2).await.iter().skip(1).all(|c| c.2 == "failed" && c.3.as_deref() == Some("moov が見つかりません")));

    // 一覧が取れない → 何も残さず、走査済みも進めない
    let cam3 = recording_camera(&mut pg, t, &["prod-1"]).await;
    let mut fake = Fake { list_fails: true, ..Default::default() };
    let o = run_camera(&mut pg, t, &AutoCamera { id: cam3, scanned_until_ms: None }, &mut fake, T + 700 * S, 40).await.unwrap();
    assert!(o.listing_failed);
    // 初回は 2 時間前から → 4 本とも次の回
    assert_eq!(o.deferred, 4);
    assert!(clips_of(t, cam3).await.is_empty());
    assert_eq!(scanned_until(t, cam3).await, None);

    // 上限: 1 本だけ切って、残りの頭から次の回
    let cam4 = recording_camera(&mut pg, t, &["prod-1"]).await;
    let mut fake = Fake { files, ..Default::default() };
    let o = run_camera(&mut pg, t, &AutoCamera { id: cam4, scanned_until_ms: Some(T) }, &mut fake, T + 700 * S, 2).await.unwrap();
    assert_eq!((o.ready, o.failed, o.deferred), (1, 1, 2));
    assert_eq!(scanned_until(t, cam4).await, Some(T + 140 * S));
}

#[tokio::test]
async fn cron_picks_events_that_arrived_after_the_scanned_until() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let cam = recording_camera(&mut pg, t, &["prod-1"]).await;
    // 走査済みが 12:10:00 まで進んだあとに、それより前の発生時刻 (12:01:40) のイベントが遅れて届いた
    let late = hub_event(&su, t, "prod-1", "alcohol", Some(T + 100 * S), T + 100 * S, false).await;
    pg::set_scanned_until(&mut pg, t, cam, T + 600 * S).await.unwrap();
    let mut fake = Fake { files: vec![sd(0, true), sd(240, true), sd(480, false)], ..Default::default() };
    let c = AutoCamera { id: cam, scanned_until_ms: scanned_until(t, cam).await };
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 700 * S, 40).await.unwrap();
    assert_eq!((o.events, o.ready, o.failed, o.deferred), (1, 1, 0, 0), "{}", o.line());
    assert_eq!(clips_of(t, cam).await, vec![(90, 110, "ready".into(), None)]);
    assert_eq!(clip_events_of(t, cam).await, vec![(late, false, 90)]);
    // 次の回は拾い直さない
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 800 * S, 40).await.unwrap();
    assert_eq!((o.events, o.ready), (0, 0));
}

// ---- 機体の鍵の ID への畳み込み (hub_measurements.via_device_id / measurements.device_id) ----

/// 現在時刻 (UNIX ms)。候補の集計は `now()` から数えるので、行の時刻はここからの相対で入れる
fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// 候補の応答を (device_id, is_dev, counts) の並びにする
fn candidate_rows(body: &Value) -> Vec<(String, bool, Value)> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|r| (r["device_id"].as_str().unwrap().to_owned(), r["is_dev"].as_bool().unwrap(), r["counts"].clone()))
        .collect()
}

fn counts(alcohol: i64, blood_pressure: i64, timecard: i64, license: i64, vein_enroll: i64) -> Value {
    json!({"alcohol": alcohol, "blood_pressure": blood_pressure, "timecard": timecard, "license": license, "vein_enroll": vein_enroll})
}

#[tokio::test]
async fn candidates_fold_linked_hub_rows_into_the_key_and_hide_the_rest() {
    let t = tenant().await;
    let other = tenant().await;
    let su = admin().await;
    let n = now_ms();
    let h = 3_600 * S;
    let (key, key2, hub) = ("alarm:aaaa0001", "alarm:bbbb0002", "cores3-a");

    // 結び付きの元: 鍵の ID の行に via_device_id = hub の ID (これもアルコール 1 件)
    hub_event_via(&su, t, key, Some(hub), "alcohol", Some(n - h), n - h, false).await;
    // 結び付き前に hub の ID で入った行 (鍵の行に合算される)・指静脈の登録
    hub_event(&su, t, hub, "alcohol", Some(n - 2 * h), n - 2 * h, false).await;
    hub_event(&su, t, hub, "timecard", Some(n - h / 2), n - h / 2, false).await;
    vein_event(&su, t, hub, n - 5 * h, false).await;
    // 結び付きの無い hub・ブラウザ打刻・自己ペアの乱数 ID は候補に出さない
    hub_event(&su, t, "cores3-z", "alcohol", Some(n - h), n - h, false).await;
    hub_event(&su, t, "browser", "timecard", Some(n - h), n - h, false).await;
    hub_event(&su, t, "kiosk-3f9c2a", "license", Some(n - h), n - h, false).await;
    vein_event(&su, t, "kiosk-3f9c2a", n - h, false).await;
    // measurements (PC 経由): アルコールは completed だけ、血圧は最高血圧と測定時刻がある行だけ
    measurement(&su, t, Some(key), "completed", n - 3 * h, None, false).await;
    measurement(&su, t, Some(key), "started", n - 3 * h, None, false).await;
    measurement(&su, t, Some(key), "completed", n - 4 * h, Some((n - 4 * h, Some(120))), false).await;
    // 同じ鍵・同じ kind の hub の行 (n - h) の ±60 秒に在る measurements は重複として数えない
    measurement(&su, t, Some(key), "completed", n - h + 20 * S, None, false).await;
    // 結び付きの無い鍵の measurements だけの端末も候補になる。端末の ID が無い行は数えない
    measurement(&su, t, Some(key2), "completed", n - 6 * h, None, false).await;
    measurement(&su, t, None, "completed", n - h, Some((n - h, Some(130))), false).await;
    // 他テナントの同じ hub の ID の結び付きは混ざらない
    hub_event_via(&su, other, "alarm:other0003", Some(hub), "alcohol", Some(n - h), n - h, false).await;

    let mut pg = rt().await;
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/device-candidates", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        candidate_rows(&body),
        vec![
            // alcohol: 結び付きの元 1 + hub の ID の旧行 1 + measurements 2 (n-3h と、血圧と同じ行の n-4h。重複の 1 件と started は数えない) / 血圧 1 / 打刻 1 / 指静脈 1
            (key.to_owned(), false, counts(4, 1, 1, 0, 1)),
            (key2.to_owned(), false, counts(1, 0, 0, 0, 0)),
        ]
    );
    // 応答の形は今のまま
    let keys: Vec<&String> = body[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["counts", "device_id", "is_dev", "last_at"]);
    assert!(body[0]["last_at"].as_str().unwrap().ends_with("+09:00"));
}

#[tokio::test]
async fn links_are_not_folded_across_the_is_dev_axis() {
    let t = tenant().await;
    let su = admin().await;
    let n = now_ms();
    let h = 3_600 * S;
    // 本番の軸の結び付き (hub-p → 鍵 p) と、開発端末の軸の旧行 (hub-p の ID のまま)
    hub_event_via(&su, t, "alarm:prod0001", Some("hub-p"), "alcohol", Some(n - h), n - h, false).await;
    hub_event(&su, t, "hub-p", "timecard", Some(n - h), n - h, true).await;
    // 開発端末の軸の結び付き (hub-d → 鍵 d) と、本番の軸の旧行 (hub-d の ID のまま)
    hub_event_via(&su, t, "alarm:dev00002", Some("hub-d"), "alcohol", Some(n - h), n - h, true).await;
    hub_event(&su, t, "hub-d", "timecard", Some(n - h), n - h, false).await;

    let mut pg = rt().await;
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/device-candidates", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    // 軸を跨いで畳まない: 逆の軸の旧行は鍵に合算されず (件数は結び付きの元の 1 件だけ)、alarm: でないので出ない
    assert_eq!(
        candidate_rows(&body),
        vec![
            ("alarm:dev00002".to_owned(), true, counts(1, 0, 0, 0, 0)),
            ("alarm:prod0001".to_owned(), false, counts(1, 0, 0, 0, 0)),
        ]
    );
}

#[tokio::test]
async fn measurement_events_are_picked_and_cut_into_clip_events() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let key = "alarm:cccc0001";
    let cam = recording_camera(&mut pg, t, &[key]).await;
    // 1 行がアルコール (measured_at) と血圧 (medical_measured_at) の 2 イベントになる
    let both = measurement(&su, t, Some(key), "completed", T + 10 * S, Some((T + 20 * S, Some(118))), false).await;
    // 開発端末の軸の行
    let dev_alc = measurement(&su, t, Some(key), "completed", T + 30 * S, None, true).await;
    // 出ない行: status が started のアルコール (血圧が無ければ何も出ない)・最高血圧の無い血圧・端末の ID が無い行
    measurement(&su, t, Some(key), "started", T + 40 * S, None, false).await;
    measurement(&su, t, Some(key), "started", T + 41 * S, Some((T + 41 * S, None)), false).await;
    measurement(&su, t, None, "completed", T + 42 * S, Some((T + 42 * S, Some(120))), false).await;
    measurement(&su, t, Some("alarm:other000"), "completed", T + 43 * S, None, false).await;

    let got = pg::auto_events(&mut pg, t, cam, T, T + 3600 * S).await.unwrap();
    let mut seen: Vec<(Uuid, &str, &str, i64, &str, bool)> = got
        .iter()
        .map(|e| (e.source_id, e.source_table.as_str(), e.kind.as_str(), (e.at_ms - T) / S, e.device_id.as_str(), e.is_dev))
        .collect();
    seen.sort_by_key(|x| x.3);
    assert_eq!(
        seen,
        vec![
            (both, "measurements", "alcohol", 10, key, false),
            (both, "measurements_bp", "blood_pressure", 20, key, false),
            (dev_alc, "measurements", "alcohol", 30, key, true),
        ]
    );

    // cron の切り出しで cam_clip_events に入る (migration 190 の CHECK が source_table を通す)
    let mut fake = Fake { files: vec![sd(0, true), sd(240, true), sd(480, false)], ..Default::default() };
    let c = AutoCamera { id: cam, scanned_until_ms: Some(T) };
    let o = run_camera(&mut pg, t, &c, &mut fake, T + 700 * S, 40).await.unwrap();
    assert_eq!((o.events, o.groups, o.ready, o.failed), (3, 1, 1, 0), "{}", o.line());
    let rows: Vec<(String, String, Option<String>, bool)> = su
        .query(
            "SELECT source_table, kind, device_id, is_dev FROM alc_api.cam_clip_events WHERE tenant_id = $1 AND camera_id = $2 ORDER BY occurred_at, source_table",
            &[&t, &cam],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("measurements".into(), "alcohol".into(), Some(key.into()), false),
            ("measurements_bp".into(), "blood_pressure".into(), Some(key.into()), false),
            ("measurements".into(), "alcohol".into(), Some(key.into()), true),
        ]
    );
    // 次の回は同じ測定を拾わない (同じ source_id が alcohol / bp の 2 つの source_table で入っていても衝突しない)
    assert!(pg::auto_events(&mut pg, t, cam, T, T + 3600 * S).await.unwrap().is_empty());

    // その日の口にも出る (clip つき)
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/events", &format!("camera_id={cam}&date=2026-10-23"), Value::Null).await;
    assert_eq!(s, 200, "{body}");
    let tables: Vec<(&str, &str, bool)> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["source_table"].as_str().unwrap(), r["kind"].as_str().unwrap(), r["clip"].is_object()))
        .collect();
    assert_eq!(tables, [("measurements", "alcohol", true), ("measurements_bp", "blood_pressure", true), ("measurements", "alcohol", true)]);
}

#[tokio::test]
async fn measurement_events_duplicating_hub_events_within_60s_are_dropped() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let key = "alarm:dddd0001";
    let cam = recording_camera(&mut pg, t, &[key]).await;
    let (from, to) = (T, T + 1800 * S);

    // hub のアルコール (T+100) の ±60 秒: +50・+60 (境目も重複) は落ち、+70 は残る。-50 も落ちる
    hub_event(&su, t, key, "alcohol", Some(T + 100 * S), T + 100 * S, false).await;
    measurement(&su, t, Some(key), "completed", T + 150 * S, None, false).await;
    measurement(&su, t, Some(key), "completed", T + 160 * S, None, false).await;
    let kept_70 = measurement(&su, t, Some(key), "completed", T + 170 * S, None, false).await;
    measurement(&su, t, Some(key), "completed", T + 50 * S, None, false).await;
    // 血圧も同じ (hub の blood_pressure T+300 に対し measurements の medical_measured_at T+330 は落ち、T+400 は残る)
    hub_event(&su, t, key, "blood_pressure", Some(T + 300 * S), T + 300 * S, false).await;
    measurement(&su, t, Some(key), "started", T + 330 * S, Some((T + 330 * S, Some(110))), false).await;
    let kept_bp = measurement(&su, t, Some(key), "started", T + 400 * S, Some((T + 400 * S, Some(111))), false).await;
    // kind が違う hub (timecard) の近くのアルコール・別の端末の hub の近くのアルコールは落とさない
    hub_event(&su, t, key, "timecard", Some(T + 500 * S), T + 500 * S, false).await;
    let kept_kind = measurement(&su, t, Some(key), "completed", T + 510 * S, None, false).await;
    hub_event(&su, t, "alarm:eeee0002", "alcohol", Some(T + 600 * S), T + 600 * S, false).await;
    let kept_dev = measurement(&su, t, Some(key), "completed", T + 610 * S, None, false).await;
    // hub の ID の旧行 (結び付きで同じ鍵に畳まれる) とも重複と見なす。結び付きの元の行は窓の外 (T+2000) でも効く
    hub_event(&su, t, "hub-d", "alcohol", Some(T + 700 * S), T + 700 * S, false).await;
    hub_event_via(&su, t, key, Some("hub-d"), "alcohol", Some(T + 2000 * S), T + 2000 * S, false).await;
    measurement(&su, t, Some(key), "completed", T + 720 * S, None, false).await;
    // 他テナントの hub は重複の相手にならない
    let other = tenant().await;
    hub_event(&su, other, key, "alcohol", Some(T + 800 * S), T + 800 * S, false).await;
    let kept_other = measurement(&su, t, Some(key), "completed", T + 810 * S, None, false).await;

    let got = pg::auto_events(&mut pg, t, cam, from, to).await.unwrap();
    let mut m: Vec<(i64, &str, Uuid)> = got
        .iter()
        .filter(|e| e.source_table.starts_with("measurements"))
        .map(|e| ((e.at_ms - T) / S, e.source_table.as_str(), e.source_id))
        .collect();
    m.sort();
    let mut want = vec![
        (170, "measurements", kept_70),
        (400, "measurements_bp", kept_bp),
        (510, "measurements", kept_kind),
        (610, "measurements", kept_dev),
        (810, "measurements", kept_other),
    ];
    want.sort();
    assert_eq!(m, want);
    // hub 側のイベントは落とさない (hub の ID の旧行は鍵の ID で出る)。窓の外の結び付きの元 (T+2000) と別の端末は出ない
    let hubs = got.iter().filter(|e| e.source_table == "hub_measurements").count();
    assert_eq!(hubs, 4, "alcohol (100) / blood_pressure (300) / timecard (500) / hub-d の旧行 (700)");
    assert!(got.iter().all(|e| e.device_id == key), "hub の ID のまま出ている / 別の端末が混ざっている");
}

#[tokio::test]
async fn camera_picks_both_hub_and_key_records_whichever_it_is_linked_to() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    let (key, hub) = ("alarm:ffff0001", "hub-1");
    // 結び付き (鍵の ID の行 + via)・hub の ID の旧行・鍵の ID の行・measurements
    let link = hub_event_via(&su, t, key, Some(hub), "alcohol", Some(T + 5 * S), T + 5 * S, false).await;
    let legacy = hub_event(&su, t, hub, "license", Some(T + 10 * S), T + 10 * S, false).await;
    let direct = hub_event(&su, t, key, "timecard", Some(T + 20 * S), T + 20 * S, false).await;
    // (アルコールの測定は hub のアルコール T+5 の ±60 秒の外に置く)
    let pc = measurement(&su, t, Some(key), "completed", T + 200 * S, None, false).await;
    // 結び付きの無い hub の ID で結び付けたカメラは、その hub の ID のまま拾う。無関係な端末は拾わない
    let solo = hub_event(&su, t, "hub-9", "alcohol", Some(T + 40 * S), T + 40 * S, false).await;
    hub_event(&su, t, "alarm:zzzz0009", "alcohol", Some(T + 50 * S), T + 50 * S, false).await;

    let by_hub = recording_camera(&mut pg, t, &[hub]).await;
    let by_key = recording_camera(&mut pg, t, &[key]).await;
    let by_solo = recording_camera(&mut pg, t, &["hub-9"]).await;
    let want = {
        let mut v = vec![(link, key), (legacy, key), (direct, key), (pc, key)];
        v.sort();
        v
    };
    for cam in [by_hub, by_key] {
        let got = pg::auto_events(&mut pg, t, cam, T, T + 3600 * S).await.unwrap();
        let mut ids: Vec<(Uuid, &str)> = got.iter().map(|e| (e.source_id, e.device_id.as_str())).collect();
        ids.sort();
        assert_eq!(ids, want, "camera {cam}");
        // その日の口も同じ (返す device_id は鍵の ID)
        let day = pg::day_events(&mut pg, t, cam, T, T + 3600 * S).await.unwrap();
        let mut ids: Vec<(Uuid, &str)> = day.iter().map(|d| (d.event.source_id, d.event.device_id.as_str())).collect();
        ids.sort();
        assert_eq!(ids, want, "camera {cam} (day)");
    }
    let got = pg::auto_events(&mut pg, t, by_solo, T, T + 3600 * S).await.unwrap();
    assert_eq!(got.iter().map(|e| (e.source_id, e.device_id.as_str())).collect::<Vec<_>>(), vec![(solo, "hub-9")]);
}

#[tokio::test]
async fn camera_device_ids_are_folded_and_pending_ones_are_reported() {
    let t = tenant().await;
    let su = admin().await;
    let mut pg = rt().await;
    // hub-1 は鍵 aaaa に結び付いている。hub-2 は結び付き無し
    hub_event_via(&su, t, "alarm:aaaa0001", Some("hub-1"), "alcohol", Some(T), T, false).await;
    // 開発端末の軸の結び付き (hub-3 → 鍵 dev) も畳みに使う
    hub_event_via(&su, t, "alarm:dev00003", Some("hub-3"), "alcohol", Some(T), T, true).await;

    let (s, cam) = call(
        &mut pg,
        &caller(t, false),
        "POST",
        "/cameras",
        "",
        json!({"name": "入口", "device_ids": ["hub-2", "hub-1", "alarm:aaaa0001", "alarm:zzzz0009", "hub-3"]}),
    )
    .await;
    assert_eq!(s, 201, "{cam}");
    // 結び付いた hub は鍵に畳まれ (重複は 1 つ)、結び付きの無い hub はそのまま返って pending にも入る
    let folded = json!(["alarm:aaaa0001", "alarm:dev00003", "alarm:zzzz0009", "hub-2"]);
    assert_eq!(cam["device_ids"], folded);
    assert_eq!(cam["pending_device_ids"], json!(["hub-2"]));
    let id = cam["id"].as_str().unwrap().to_owned();

    // 保存は送った ID のまま (畳まない)
    let stored: Vec<String> = su
        .query("SELECT device_id FROM alc_api.cam_camera_devices WHERE tenant_id = $1 ORDER BY device_id", &[&t])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(stored, ["alarm:aaaa0001", "alarm:zzzz0009", "hub-1", "hub-2", "hub-3"]);

    // 一覧・PATCH の応答も畳んで返す。運行管理者席は device_ids だけ (畳んだ値)
    let (_, list) = call(&mut pg, &caller(t, false), "GET", "/cameras", "", Value::Null).await;
    assert_eq!(list["cameras"][0]["device_ids"], folded);
    assert_eq!(list["cameras"][0]["pending_device_ids"], json!(["hub-2"]));
    let (s, patched) = call(&mut pg, &caller(t, false), "PATCH", &format!("/cameras/{id}"), "", json!({"name": "入口 2"})).await;
    assert_eq!(s, 200, "{patched}");
    assert_eq!(patched["device_ids"], folded);
    assert_eq!(patched["pending_device_ids"], json!(["hub-2"]));
    let (_, tm) = call(&mut pg, &tenko_manager(t), "GET", "/cameras", "", Value::Null).await;
    assert_eq!(tm["cameras"][0], json!({"id": id, "name": "入口 2", "device_ids": folded}));

    // 画面が畳んだ ID を送り返せば結び付けが置き換わり、pending も消える
    let (s, replaced) = call(&mut pg, &caller(t, false), "PATCH", &format!("/cameras/{id}"), "", json!({"device_ids": ["alarm:aaaa0001"]})).await;
    assert_eq!(s, 200, "{replaced}");
    assert_eq!((replaced["device_ids"].clone(), replaced["pending_device_ids"].clone()), (json!(["alarm:aaaa0001"]), json!([])));
    let stored: Vec<String> = su
        .query("SELECT device_id FROM alc_api.cam_camera_devices WHERE tenant_id = $1", &[&t])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(stored, ["alarm:aaaa0001"]);

    // 結び付けた端末が無いカメラは結び付きを引かずに空のまま
    let (_, empty) = call(&mut pg, &caller(t, false), "POST", "/cameras", "", json!({"name": "空"})).await;
    assert_eq!((empty["device_ids"].clone(), empty["pending_device_ids"].clone()), (json!([]), json!([])));

    // 拠点 PC の /cameras/mine は変えない (鍵の sub で引く。stream_device_ids の順位表は畳まない)
    let (s, put) = call(&mut pg, &caller(t, false), "PATCH", &format!("/cameras/{id}"), "", json!({"stream_device_ids": ["alarm:aaaa0001"], "lan_ip": "192.168.0.10", "rtsp_path": "s1"})).await;
    assert_eq!(s, 200, "{put}");
    assert_eq!(put["stream_device_ids"], json!(["alarm:aaaa0001"]));
    let (_, mine) = call(&mut pg, &cam_host(t, "alarm:aaaa0001"), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(mine["cameras"].as_array().unwrap().len(), 1);
    let (_, none) = call(&mut pg, &cam_host(t, "hub-1"), "GET", "/cameras/mine", "", Value::Null).await;
    assert_eq!(none["cameras"].as_array().unwrap().len(), 0);
}

// ---- 端末の鍵の状態 (GET /device-key-status) ----

/// 応答の端末の行を (device_id, is_dev, kind, counts) の並びにする
fn status_rows(body: &Value) -> Vec<(String, bool, String, Value)> {
    body["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["device_id"].as_str().unwrap().to_owned(), r["is_dev"].as_bool().unwrap(), r["kind"].as_str().unwrap().to_owned(), r["counts"].clone()))
        .collect()
}

fn status_counts(via_key: i64, hub_id: i64, measurements: i64, vein_enroll: i64, timecard: i64) -> Value {
    json!({"hub_measurements_via_key": via_key, "hub_measurements_hub_id": hub_id, "measurements": measurements, "vein_enroll": vein_enroll, "timecard": timecard})
}

#[tokio::test]
async fn key_status_classifies_folds_and_counts_within_the_period() {
    let t = tenant().await;
    let other = tenant().await;
    let su = admin().await;
    let n = now_ms();
    let h = 3_600 * S;
    let d = 24 * h;
    let (key, key2, hub) = ("alarm:aaaa0001", "alarm:bbbb0002", "cores3-a");

    // 鍵の ID で入った行 (結び付きの元 = アルコール 1 件・打刻 1 件)
    hub_event_via(&su, t, key, Some(hub), "alcohol", Some(n - h), n - h, false).await;
    hub_event_via(&su, t, key, Some(hub), "timecard", Some(n - h / 3), n - h / 3, false).await;
    // 結び付いた hub の ID で入った行 (鍵の行に畳まれ、hub_id 側に数える)。打刻はその内数
    hub_event(&su, t, hub, "alcohol", Some(n - 2 * h), n - 2 * h, false).await;
    hub_event(&su, t, hub, "timecard", Some(n - h / 2), n - h / 2, false).await;
    vein_event(&su, t, hub, n - 5 * h, false).await;
    // 期間 (7 日) の外
    hub_event(&su, t, key, "alcohol", Some(n - 8 * d), n - 8 * d, false).await;
    // measurements: 完了のアルコール・同じ行の血圧・hub の行と ±60 秒の重複 (候補では落とすがここでは数える)。started は数えない
    measurement(&su, t, Some(key), "completed", n - 3 * h, None, false).await;
    measurement(&su, t, Some(key), "started", n - 3 * h, None, false).await;
    measurement(&su, t, Some(key), "completed", n - 4 * h, Some((n - 4 * h, Some(120))), false).await;
    measurement(&su, t, Some(key), "completed", n - h + 20 * S, None, false).await;
    // 測定だけの鍵
    measurement(&su, t, Some(key2), "completed", n - 6 * h, None, false).await;
    // 結び付きの無い hub (ファーム未更新)・ブラウザ・自己ペアの credential (測定にも出る)
    hub_event(&su, t, "cores3-z", "alcohol", Some(n - h), n - h, false).await;
    hub_event(&su, t, "browser", "timecard", Some(n - h), n - h, false).await;
    hub_event(&su, t, "kiosk-3f9c2a", "license", Some(n - h), n - h, false).await;
    vein_event(&su, t, "kiosk-3f9c2a", n - h, false).await;
    // 端末 ID の無い測定 (アルコール + 血圧で 2 件)・期間の外の 1 件
    measurement(&su, t, None, "completed", n - h, Some((n - h, Some(130))), false).await;
    measurement(&su, t, None, "completed", n - 10 * d, None, false).await;
    // 他テナント
    hub_event_via(&su, other, "alarm:other0003", Some(hub), "alcohol", Some(n - h), n - h, false).await;
    hub_event(&su, other, "cores3-other", "timecard", Some(n - h), n - h, false).await;
    measurement(&su, other, None, "completed", n - h, None, false).await;

    let mut pg = rt().await;
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/device-key-status", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["days"], 7);
    assert_eq!(
        status_rows(&body),
        vec![
            // 鍵 via 2 (アルコール・打刻) / hub の ID 2 (畳まれた旧行) / measurements 4 (重複も数える・血圧込み) / 指静脈 1 / 打刻 2 (via 1 + hub 1)
            (key.to_owned(), false, "machine_key".to_owned(), status_counts(2, 2, 4, 1, 2)),
            // last_at が同じ (n - h) の 3 行は device_id の昇順
            ("browser".to_owned(), false, "browser".to_owned(), status_counts(0, 1, 0, 0, 1)),
            ("cores3-z".to_owned(), false, "hub_unlinked".to_owned(), status_counts(0, 1, 0, 0, 0)),
            ("kiosk-3f9c2a".to_owned(), false, "credential".to_owned(), status_counts(0, 1, 0, 1, 0)),
            (key2.to_owned(), false, "machine_key".to_owned(), status_counts(0, 0, 1, 0, 0)),
        ]
    );
    assert!(body["devices"][0]["last_at"].as_str().unwrap().ends_with("+09:00"));
    assert_eq!(body["unattributed"]["measurements"], 2);
    assert!(body["unattributed"]["last_at"].as_str().unwrap().ends_with("+09:00"));

    // 期間を広げると外の行も入る (via_key の旧アルコール 1 件 = 8 日前・端末 ID の無い測定 1 件 = 10 日前)
    let (s, wide) = call(&mut pg, &caller(t, false), "GET", "/device-key-status", "days=30", Value::Null).await;
    assert_eq!(s, 200, "{wide}");
    assert_eq!(wide["days"], 30);
    assert_eq!(status_rows(&wide)[0].3, status_counts(3, 2, 4, 1, 2));
    assert_eq!(wide["unattributed"]["measurements"], 3);

    // 他テナントは自分の分だけ (結び付きも混ざらない: この hub の ID は鍵に畳まれない)
    let (_, o) = call(&mut pg, &caller(other, false), "GET", "/device-key-status", "", Value::Null).await;
    assert_eq!(
        status_rows(&o),
        vec![
            ("alarm:other0003".to_owned(), false, "machine_key".to_owned(), status_counts(1, 0, 0, 0, 0)),
            ("cores3-other".to_owned(), false, "hub_unlinked".to_owned(), status_counts(0, 1, 0, 0, 1)),
        ]
    );
    assert_eq!(o["unattributed"]["measurements"], 1);
}

#[tokio::test]
async fn key_status_with_no_rows_is_empty() {
    let t = tenant().await;
    let mut pg = rt().await;
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/device-key-status", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body, json!({"days": 7, "devices": [], "unattributed": {"measurements": 0, "last_at": null}}));
}

#[tokio::test]
async fn key_status_does_not_cross_the_is_dev_axis() {
    let t = tenant().await;
    let su = admin().await;
    let n = now_ms();
    let h = 3_600 * S;
    // 本番の軸の結び付き (hub-p → 鍵 p) と開発端末の軸の hub-p の旧行 (畳まれない)
    hub_event_via(&su, t, "alarm:prod0001", Some("hub-p"), "alcohol", Some(n - h), n - h, false).await;
    hub_event(&su, t, "hub-p", "timecard", Some(n - h), n - h, true).await;
    // 開発端末の軸の結び付き (hub-d → 鍵 d) と本番の軸の hub-d の旧行
    hub_event_via(&su, t, "alarm:dev00002", Some("hub-d"), "alcohol", Some(n - h), n - h, true).await;
    hub_event(&su, t, "hub-d", "timecard", Some(n - h), n - h, false).await;
    // 端末 ID の無い測定は軸をまたいで合算
    measurement(&su, t, None, "completed", n - h, None, false).await;
    measurement(&su, t, None, "completed", n - h, None, true).await;

    let mut pg = rt().await;
    let (s, body) = call(&mut pg, &caller(t, false), "GET", "/device-key-status", "", Value::Null).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        status_rows(&body),
        vec![
            ("alarm:dev00002".to_owned(), true, "machine_key".to_owned(), status_counts(1, 0, 0, 0, 0)),
            ("alarm:prod0001".to_owned(), false, "machine_key".to_owned(), status_counts(1, 0, 0, 0, 0)),
            ("hub-d".to_owned(), false, "hub_unlinked".to_owned(), status_counts(0, 1, 0, 0, 1)),
            ("hub-p".to_owned(), true, "hub_unlinked".to_owned(), status_counts(0, 1, 0, 0, 1)),
        ]
    );
    assert_eq!(body["unattributed"]["measurements"], 2);
}

#[tokio::test]
async fn key_status_checks_days_and_is_admin_only() {
    let t = tenant().await;
    let mut pg = rt().await;
    for q in ["days=0", "days=91", "days=abc"] {
        let (s, _) = call(&mut pg, &caller(t, false), "GET", "/device-key-status", q, Value::Null).await;
        assert_eq!(s, 400, "{q}");
    }
    let (s, _) = call(&mut pg, &caller(t, false), "GET", "/device-key-status", "days=90", Value::Null).await;
    assert_eq!(s, 200);
    // 運行管理者席の鍵・cam-host の鍵は 403
    let (s, _) = call(&mut pg, &tenko_manager(t), "GET", "/device-key-status", "", Value::Null).await;
    assert_eq!(s, 403);
    let (s, _) = call(&mut pg, &cam_host(t, "alarm:host0001"), "GET", "/device-key-status", "", Value::Null).await;
    assert_eq!(s, 403);
}
