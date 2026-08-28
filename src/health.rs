use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Simple HTTP health server for haproxy and k8s probes.
/// Listens on `RHYDADB_HEALTH_LISTEN` (default `127.0.0.1:8080` on Windows,
/// `0.0.0.0:8080` in Docker) and serves:
/// - `GET /health` -> 200 `{"status":"ok","uptime_sec":123}`
/// - `GET /ready`  -> 200 if storage is ready (always ok for single-node)
/// - `GET /metrics` -> Prometheus-style `rhydadb_up 1`
pub async fn run(health_addr: String) {
    let start = Instant::now();
    let listener = match tokio::net::TcpListener::bind(&health_addr).await {
        Ok(l) => {
            tracing::info!("health listening on {health_addr}");
            l
        }
        Err(e) => {
            tracing::warn!("health bind failed on {health_addr}: {e}, health disabled");
            return;
        }
    };

    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("health accept error: {e}");
                continue;
            }
        };
        let uptime = start.elapsed().as_secs();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or("");
            let (status, body, ctype) = if line.starts_with("GET /health") || line.starts_with("GET /ready") {
                let body = format!(r#"{{"status":"ok","uptime_sec":{}}}"#, uptime);
                (200, body, "application/json")
            } else if line.starts_with("GET /metrics") {
                let body = format!(
                    "# HELP rhydadb_up Whether the server is up\n# TYPE rhydadb_up gauge\nrhydadb_up 1\n# HELP rhydadb_uptime_seconds Uptime\n# TYPE rhydadb_uptime_seconds counter\nrhydadb_uptime_seconds {uptime}\n"
                );
                (200, body, "text/plain; version=0.0.4")
            } else if line.starts_with("GET /") {
                (404, r#"{"status":"not_found"}"#.to_string(), "application/json")
            } else {
                (400, r#"{"status":"bad_request"}"#.to_string(), "application/json")
            };
            let resp = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                if status == 200 { "OK" } else if status == 404 { "Not Found" } else { "Bad Request" },
                ctype,
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.flush().await;
            tracing::debug!(%peer, "health {} -> {status}", line);
        });
    }
}
