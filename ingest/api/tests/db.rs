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

/// 準備をプロセスの中で 1 回だけ流す。187 の関数が在れば (前に流した) 何もしない
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
                    "SELECT to_regprocedure('alc_api.list_cam_recording_tenant_ids()') IS NOT NULL",
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
              AND tablename IN ('cameras', 'cam_camera_devices', 'cam_clips', 'hub_measurements', 'vein_enroll_events'))",
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
    }
}

/// 運行管理者席の鍵 (auth-worker の端末用の proxy から届く呼び手)
fn tenko_manager(tenant: Uuid) -> Caller {
    Caller {
        tenant,
        developer: false,
        role: Role::TenkoManager,
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
        (t, "dev-1", "alcohol", "now() - interval '1 day'"),
        (t, "dev-1", "alcohol", "now() - interval '2 days'"),
        (t, "dev-1", "blood_pressure", "now() - interval '3 days'"),
        (t, "dev-1", "temperature", "now() - interval '1 day'"),
        (t, "dev-2", "timecard", "now() - interval '5 hours'"),
        (t, "dev-2", "license", "now() - interval '40 days'"),
        (t, "dev-3", "alcohol", "now() - interval '45 days'"),
        (other, "dev-1", "alcohol", "now() - interval '1 day'"),
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
    su.execute("INSERT INTO alc_api.hub_measurements (tenant_id, device_id, kind, payload, seq, recorded_at) VALUES ($1, 'dev-2', 'timecard', '{}', 100, NULL)", &[&t]).await.unwrap();
    // 指静脈の登録 (registered_device_id が NULL の行は数えない)
    let emp: Uuid = su.query_one("INSERT INTO alc_api.employees (tenant_id, nfc_id, name) VALUES ($1, $2, '乗務員') RETURNING id", &[&t, &format!("nfc-{}", Uuid::new_v4())]).await.unwrap().get(0);
    su.execute("INSERT INTO alc_api.vein_enroll_events (tenant_id, employee_id, registered_device_id) VALUES ($1, $2, 'dev-3'), ($1, $2, NULL)", &[&t, &emp]).await.unwrap();

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
    // 新しい順: dev-3 (指静脈 = 今) → dev-2 (created_at = 今の timecard) → dev-1
    assert_eq!(
        got,
        vec![
            (
                "dev-3".to_owned(),
                json!({"alcohol": 0, "blood_pressure": 0, "timecard": 0, "license": 0, "vein_enroll": 1})
            ),
            (
                "dev-2".to_owned(),
                json!({"alcohol": 0, "blood_pressure": 0, "timecard": 2, "license": 0, "vein_enroll": 0})
            ),
            (
                "dev-1".to_owned(),
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
        .find(|r| r["device_id"] == "dev-3")
        .unwrap();
    assert_eq!(dev3["counts"]["alcohol"], 1);
    let dev2 = body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["device_id"] == "dev-2")
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
    su.execute("INSERT INTO alc_api.hub_measurements (tenant_id, device_id, kind, payload, seq, recorded_at) VALUES ($1, 'prod-1', 'alcohol', '{}', 1, now() - interval '3 hours')", &[&t]).await.unwrap();
    // 開発端末の行は RLS の WITH CHECK に合わせ、app.device_dev='1' を立てた transaction で入れる
    for (tenant, employee, device, at) in [
        (t, emp, "dev-1", "now() - interval '1 hour'"),
        (t, emp, "dev-2", "now() - interval '2 hours'"),
        (other, emp_other, "dev-other", "now() - interval '1 hour'"),
    ] {
        // 指静脈の登録 (created_at = 今) は dev-1 と他テナントだけ。dev-2 は 2 時間前の timecard が最終
        let vein = if device == "dev-2" {
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
        .query("SELECT is_dev FROM alc_api.hub_measurements WHERE tenant_id = $1 AND device_id LIKE 'dev-%' ORDER BY device_id", &[&t])
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
    assert_eq!(got, [("dev-1", true), ("dev-2", true), ("prod-1", false)]);
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
    let id = Uuid::new_v4();
    let at = |ms: i64| format!("'epoch'::timestamptz + {ms} * interval '1 millisecond'");
    let recorded = recorded.map(at).unwrap_or_else(|| "NULL".into());
    let set = if dev { "SELECT set_config('app.device_dev', '1', true);" } else { "" };
    su.batch_execute(&format!(
        "BEGIN; {set} INSERT INTO alc_api.hub_measurements (id, tenant_id, device_id, kind, payload, seq, recorded_at, created_at) \
         VALUES ('{id}', '{t}', '{device}', '{kind}', '{{}}', {seq}, {recorded}, {created}); COMMIT;",
        seq = id.as_u128() as i64 & i64::MAX,
        created = at(created),
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
