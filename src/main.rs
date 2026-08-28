use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use rhydadb::storage::Storage;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_dir = std::env::var("RHYDADB_DATA").unwrap_or_else(|_| "data".to_string());
    let listen = std::env::var("RHYDADB_LISTEN").unwrap_or_else(|_| "127.0.0.1:9042".to_string());

    let storage = Arc::new(Storage::open(&data_dir)?);
    let health_listen = std::env::var("RHYDADB_HEALTH_LISTEN").unwrap_or_else(|_| {
        if std::env::var("RHYDADB_URING").is_ok() {
            "0.0.0.0:8080".to_string()
        } else {
            "127.0.0.1:8080".to_string()
        }
    });

    // Thread-per-core io_uring path: RHYDADB_URING=1 + --features uring on Linux
    if std::env::var("RHYDADB_URING").is_ok() {
        #[cfg(all(feature = "uring", target_os = "linux"))]
        {
            tracing::info!("starting monoio thread-per-core server on {listen} with health {health_listen}");
            // Health still on tokio runtime in a separate OS thread to avoid mixing runtimes
            let hl = health_listen.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(rhydadb::health::run(hl));
            });
            return rhydadb::server_monoio::run_blocking(storage, listen);
        }
        #[cfg(not(all(feature = "uring", target_os = "linux")))]
        {
            tracing::warn!("RHYDADB_URING set but uring feature not enabled or not on Linux, falling back to tokio");
        }
    }

    // Default tokio multi-thread server
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let hl = health_listen.clone();
        tokio::spawn(rhydadb::health::run(hl));
        let listener = tokio::net::TcpListener::bind(&listen).await?;
        tracing::info!("RhydaDB listening on {listen}, data dir: {data_dir}, health {health_listen}");
        rhydadb::server::run(listener, storage).await
    })
}
