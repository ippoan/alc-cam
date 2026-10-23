//! postgres への接続。段は 2 つだけ (上から順に見て、最初にあったものを使う):
//!
//! | 順 | env | 読むもの | 経路 |
//! |---|---|---|---|
//! | 1 | 本番 | Hyperdrive の binding `CAM_HYPERDRIVE` (設定は実行用ロールのもの 1 つを複数の worker で共有) | [`alc_worker_db::hyperdrive::connect`] |
//! | 2 | ローカル (`wrangler dev --env local`) | 文字列 `DATABASE_URL` (`.dev.vars` / `--var`) | 接続文字列の host:port へ。`sslmode=disable` なら平文、それ以外は STARTTLS |
//!
//! **1 は binding が無い (undefined) ときだけ 2 へ落ちる。** 在るのに使えないときは 500 で、2 へは落とさない
//! (判定は kit が持つ)。どちらも無ければ 503。エラーの文に接続先・資格情報を入れない。

use alc_worker_db::PgClient;
use cam_ingest_api::ApiError;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{Config, NoTls};
use worker::postgres_tls::PassthroughTls;
use worker::{console_error, Env, SecureTransport, Socket};

/// 本番の Hyperdrive の binding (`[[hyperdrive]]`、wrangler.toml のトップレベルにだけ置く)
const CAM_HYPERDRIVE_BINDING: &str = "CAM_HYPERDRIVE";
/// ローカル専用の接続文字列
const DATABASE_URL: &str = "DATABASE_URL";

fn internal(stage: &str) -> ApiError {
    console_error!("alc-cam: db {stage}");
    ApiError::new(500, "DB に繋げません")
}

pub async fn connect(env: &Env) -> Result<PgClient, ApiError> {
    match alc_worker_db::hyperdrive::connect(env, CAM_HYPERDRIVE_BINDING).await {
        Ok(Some(client)) => return Ok(client),
        // binding が無い → ローカルの文字列の段へ
        Ok(None) => {}
        // 在るのに使えない → 500 (次の段へ落とさない)。kit の Display は binding 名・段・kind だけ
        Err(e) => {
            console_error!("alc-cam: {e}");
            return Err(ApiError::new(500, "DB に繋げません"));
        }
    }
    let Ok(url) = env.secret(DATABASE_URL) else {
        console_error!("alc-cam: no {CAM_HYPERDRIVE_BINDING} binding and no {DATABASE_URL}");
        return Err(ApiError::new(503, "DB が設定されていません"));
    };
    let config: Config = url.to_string().parse().map_err(|_| internal("parse"))?;
    let Some(Host::Tcp(host)) = config.get_hosts().first().cloned() else {
        return Err(internal("host"));
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let client = if config.get_ssl_mode() == SslMode::Disable {
        let socket = Socket::builder()
            .connect(host, port)
            .map_err(|_| internal("socket"))?;
        let (client, connection) = config
            .connect_raw(socket, NoTls)
            .await
            .map_err(|e| internal(&format!("handshake {}", alc_worker_db::kind(&e))))?;
        spawn(connection);
        client
    } else {
        let socket = Socket::builder()
            .secure_transport(SecureTransport::StartTls)
            .connect(host, port)
            .map_err(|_| internal("socket"))?;
        let (client, connection) = config
            .connect_raw(socket, PassthroughTls)
            .await
            .map_err(|e| internal(&format!("handshake {}", alc_worker_db::kind(&e))))?;
        spawn(connection);
        client
    };
    Ok(PgClient::new(client))
}

fn spawn<F>(connection: F)
where
    F: std::future::Future<Output = Result<(), tokio_postgres::Error>> + 'static,
{
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = connection.await {
            console_error!("alc-cam: db connection {}", alc_worker_db::kind(&e));
        }
    });
}
