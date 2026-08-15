use rhydadb::server;
use rhydadb::storage::Storage;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_dir = std::env::var("RHYDADB_DATA").unwrap_or_else(|_| "data".to_string());
    let listen = std::env::var("RHYDADB_LISTEN").unwrap_or_else(|_| "127.0.0.1:9042".to_string());

    let storage = Arc::new(Storage::open(&data_dir)?);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("RhydaDB listening on {listen}, data dir: {data_dir}");
    server::run(listener, storage).await
}