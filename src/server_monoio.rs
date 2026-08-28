//! Thread-per-core io_uring server (Linux only, feature `uring`).
//! One monoio fusion driver per core, SO_REUSEPORT listeners, no cross-thread
//! wakeups on the hot path. Each core owns its acceptor and connection set.
//! Storage is shared via `Arc<Storage>` (still sharded RocksDB), so the fast
//! path stays thread-local for the common single-PK case.

use crate::protocol;
use crate::query;
use crate::storage::Storage;
use anyhow::anyhow;
use cql3_parser::cassandra_statement::CassandraStatement;
use scylla_cql::frame::Compression;
use scylla_cql::frame::protocol_features::ProtocolFeatures;
use scylla_cql::frame::request::query::QueryParameters;
use scylla_cql::frame::request::{DeserializableRequest, Query, RequestOpcode, Startup};
use scylla_cql::frame::types;
use monoio::io::{AsyncReadRentExt, AsyncWriteRentExt, Splitable};
use std::collections::HashMap;
use std::sync::Arc;

const MAX_FRAME_LENGTH: usize = 64 * 1024 * 1024;
const PER_CONN_INFLIGHT: usize = 512;
const WRITE_FLUSH_THRESHOLD: usize = 256 * 1024;

#[derive(Clone)]
struct PreparedStmt {
    stmt: Arc<CassandraStatement>,
    bind_types: Vec<query::ColumnType>,
}

struct ConnShared {
    compression: std::sync::atomic::AtomicU8,
    keyspace: std::sync::RwLock<Option<Arc<str>>>,
    prepared: std::sync::RwLock<HashMap<Vec<u8>, PreparedStmt>>,
    next_prepared_id: std::sync::atomic::AtomicU16,
}

const COMPRESSION_NONE: u8 = 0;

fn decode_compression(v: u8) -> Option<Compression> {
    match v {
        1 => Some(Compression::Lz4),
        2 => Some(Compression::Snappy),
        _ => None,
    }
}

impl ConnShared {
    fn new() -> Self {
        Self {
            compression: std::sync::atomic::AtomicU8::new(COMPRESSION_NONE),
            keyspace: std::sync::RwLock::new(None),
            prepared: std::sync::RwLock::new(HashMap::new()),
            next_prepared_id: std::sync::atomic::AtomicU16::new(1),
        }
    }
    fn compression(&self) -> Option<Compression> {
        decode_compression(self.compression.load(std::sync::atomic::Ordering::Relaxed))
    }
    fn keyspace(&self) -> Option<Arc<str>> {
        self.keyspace.read().expect("keyspace lock poisoned").clone()
    }
    fn set_keyspace(&self, ks: &str) {
        *self.keyspace.write().expect("keyspace lock poisoned") = Some(Arc::from(ks));
    }
}

/// Entry point for monoio thread-per-core server.
/// Spawns `N` fusion drivers (one per core), each with a SO_REUSEPORT listener.
pub fn run_blocking(storage: Arc<Storage>, listen: String) -> anyhow::Result<()> {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let threads: usize = std::env::var("RHYDADB_URING_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(cpus);
    tracing::info!("uring thread-per-core server on {listen} with {threads} fusion drivers");

    let mut handles = Vec::new();
    for tid in 0..threads {
        let storage = storage.clone();
        let listen = listen.clone();
        let h = std::thread::Builder::new()
            .name(format!("rhyda-uring-{tid}"))
            .spawn(move || {
                let mut rt = monoio::RuntimeBuilder::<monoio::FusionDriver>::new()
                    .with_entries(16384)
                    .enable_timer()
                    .build()
                    .expect("fusion runtime");
                rt.block_on(async move {
                    if let Err(e) = per_core_listener(listen, storage).await {
                        tracing::error!(tid, "listener error: {e}");
                    }
                });
            })?;
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

async fn per_core_listener(listen: String, storage: Arc<Storage>) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = listen.parse()?;
    let socket = socket2::Socket::new(
        if addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        },
        socket2::Type::STREAM,
        None,
    )?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(4096)?;
    let std_listener: std::net::TcpListener = socket.into();
    let listener = monoio::net::TcpListener::from_std(std_listener)?;

    tracing::info!("uring listener ready on {addr} (core {:?})", std::thread::current().id());

    loop {
        let (stream, peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        let storage = storage.clone();
        monoio::spawn(async move {
            if let Err(e) = handle_connection(stream, storage).await {
                tracing::debug!(%peer, "connection closed: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: monoio::net::TcpStream,
    storage: Arc<Storage>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = monoio::io::BufReader::new(read_half);

    // Response channel - bounded to apply backpressure
    let (resp_tx, resp_rx) = async_channel::bounded::<Arc<[u8]>>(PER_CONN_INFLIGHT * 2);

    // Writer task: coalesces ready responses
    let writer = monoio::spawn(async move {
        let mut pending: Vec<Arc<[u8]>> = Vec::new();
        let mut queued_bytes = 0usize;
        while let Ok(resp) = resp_rx.recv().await {
            queued_bytes += resp.len();
            pending.push(resp);
            while queued_bytes < WRITE_FLUSH_THRESHOLD {
                match resp_rx.try_recv() {
                    Ok(r) => {
                        queued_bytes += r.len();
                        pending.push(r);
                    }
                    Err(_) => break,
                }
            }
            for chunk in &pending {
                let buf = chunk.to_vec();
                let (res, _) = write_half.write_all(buf).await;
                if res.is_err() {
                    return;
                }
            }
            pending.clear();
            queued_bytes = 0;
        }
    });

    let state = Arc::new(ConnShared::new());
    // Use tokio semaphore even in monoio - it is future-based and works with any waker
    let inflight = Arc::new(tokio::sync::Semaphore::new(PER_CONN_INFLIGHT));
    let features = ProtocolFeatures::default();

    loop {
        let header_buf = vec![0u8; 9];
        let (res, header_buf) = reader.read_exact(header_buf).await;
        if let Err(e) = res {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(e.into());
        }
        let header: [u8; 9] = header_buf.try_into().unwrap();
        let version = header[0];
        let flags = header[1];
        let stream_id = i16::from_be_bytes([header[2], header[3]]);
        let opcode_raw = header[4];
        let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;

        if version != 0x04 {
            let resp = protocol::error(
                version,
                stream_id,
                0x000A,
                "Unsupported protocol version; this server supports CQL v4",
                None,
            );
            let _ = resp_tx.send(Arc::from(resp)).await;
            break;
        }
        if length > MAX_FRAME_LENGTH {
            let resp = protocol::error(
                version,
                stream_id,
                0x000A,
                "Frame too large",
                state.compression(),
            );
            let _ = resp_tx.send(Arc::from(resp)).await;
            break;
        }
        let body_buf = vec![0u8; length];
        let (res, body_buf) = reader.read_exact(body_buf).await;
        res?;
        let mut body = body_buf;

        let body: bytes::Bytes = if flags & 0x01 != 0 {
            match state.compression() {
                Some(comp) => {
                    let decompressed = scylla_cql::frame::decompress(&body, comp)
                        .map_err(|e| anyhow!("decompression error: {e}"))?;
                    decompressed.into()
                }
                None => {
                    let resp = protocol::error(
                        version,
                        stream_id,
                        0x000A,
                        "Received compressed frame without negotiated compression",
                        None,
                    );
                    let _ = resp_tx.send(Arc::from(resp)).await;
                    continue;
                }
            }
        } else {
            body.into()
        };

        let inline = opcode_raw != RequestOpcode::Query as u8
            && opcode_raw != RequestOpcode::Execute as u8;

        if inline || is_session_use(&body) {
            match handle_request(&storage, &state, opcode_raw, &body, stream_id, &features).await {
                Ok(resp) => {
                    let _ = resp_tx.send(Arc::from(resp)).await;
                }
                Err(e) => {
                    let resp =
                        protocol::error(version, stream_id, e.code, &e.message, state.compression());
                    let _ = resp_tx.send(Arc::from(resp)).await;
                }
            }
            continue;
        }

        let permit = inflight.clone().acquire_owned().await?;
        let storage = storage.clone();
        let state = state.clone();
        let resp_tx = resp_tx.clone();
        monoio::spawn(async move {
            let resp = match handle_request(&storage, &state, opcode_raw, &body, stream_id, &features).await
            {
                Ok(resp) => resp,
                Err(e) => protocol::error(version, stream_id, e.code, &e.message, state.compression()),
            };
            let _ = resp_tx.send(Arc::from(resp)).await;
            drop(permit);
        });
    }

    // Dropping resp_tx will close writer
    drop(resp_tx);
    let _ = writer.await;
    Ok(())
}

fn is_session_use(body: &[u8]) -> bool {
    if body.len() < 4 {
        return false;
    }
    let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let q = body.get(4..4 + len.min(body.len() - 4));
    match q {
        Some(bytes) => {
            let t = if bytes.len() > 16 { &bytes[..16] } else { bytes };
            let lower = t.to_ascii_lowercase();
            lower.starts_with(b"use ")
        }
        None => false,
    }
}

async fn handle_request(
    storage: &Storage,
    state: &ConnShared,
    opcode: u8,
    body: &[u8],
    stream: i16,
    features: &ProtocolFeatures,
) -> Result<Vec<u8>, query::QueryError> {
    use std::sync::atomic::Ordering;
    let version = 0x04u8;
    let compression = state.compression();
    let opcode = RequestOpcode::try_from(opcode).map_err(|_| query::QueryError {
        code: 0x000A,
        message: "Unknown opcode".to_string(),
    })?;

    async fn settle_then(
        storage: &Storage,
        mut result: Result<(query::Response, Vec<crate::storage::WriteTicket>), query::QueryError>,
        respond: impl FnOnce(query::Response) -> Vec<u8>,
    ) -> Result<Vec<u8>, query::QueryError> {
        match &mut result {
            Ok((_, gates)) if !gates.is_empty() => storage.settle(gates).await,
            _ => {}
        }
        match result {
            Ok((resp, _)) => Ok(respond(resp)),
            Err(e) => Err(e),
        }
    }

    match opcode {
        RequestOpcode::Options => Ok(protocol::supported(version, stream, compression)),
        RequestOpcode::Startup => {
            let mut buf: &[u8] = body;
            let startup =
                Startup::deserialize_with_features(&mut buf, features).map_err(|_| query::QueryError {
                    code: 0x000A,
                    message: "Malformed STARTUP message".to_string(),
                })?;
            match startup.options.get("COMPRESSION").map(|c| c.to_lowercase()) {
                Some(c) if c == "lz4" => state.compression.store(1, Ordering::Relaxed),
                Some(c) if c == "snappy" => state.compression.store(2, Ordering::Relaxed),
                Some(other) => {
                    return Err(query::QueryError {
                        code: 0x000A,
                        message: format!("Unsupported compression algorithm: {other}"),
                    });
                }
                None => {}
            }
            Ok(protocol::ready(version, stream, state.compression()))
        }
        RequestOpcode::Register => Ok(protocol::ready(version, stream, compression)),
        RequestOpcode::Query => {
            let mut buf: &[u8] = body;
            let q = Query::deserialize_with_features(&mut buf, features).map_err(|_| query::QueryError {
                code: 0x000A,
                message: "Malformed QUERY message".to_string(),
            })?;
            let query_text = q.contents.to_string();
            let values: Vec<types::RawValue> = q.parameters.values.iter().collect();
            let ks = state.keyspace();
            let result = query::execute_with_values(storage, ks.as_deref(), &query_text, &values);
            let response = settle_then(storage, result, |resp| {
                if let query::Response::SetKeyspace(ref name) = resp {
                    state.set_keyspace(name);
                }
                respond(version, stream, compression, Ok(resp), false)
                    .unwrap_or_else(|e| protocol::error(version, stream, e.code, &e.message, compression))
            })
            .await;
            response
        }
        RequestOpcode::Prepare => {
            let mut buf: &[u8] = body;
            let query_text = types::read_long_string(&mut buf).map_err(|_| query::QueryError {
                code: 0x000A,
                message: "Malformed PREPARE message".to_string(),
            })?;
            let stmt =
                query::parse_cached(storage, query_text).map_err(|e| query::QueryError {
                    code: 0x2000,
                    message: e.message,
                })?;
            let session_ks = state.keyspace();
            let info = query::prepared_info(storage, session_ks.as_deref(), &stmt)?;
            let id = state
                .next_prepared_id
                .fetch_add(1, Ordering::Relaxed)
                .to_be_bytes()
                .to_vec();
            let bind_types: Vec<query::ColumnType> =
                info.bind_specs.iter().map(|(_, t)| t.clone()).collect();
            state
                .prepared
                .write()
                .expect("prepared lock poisoned")
                .insert(id.clone(), PreparedStmt { stmt, bind_types });
            Ok(protocol::result_prepared(
                version,
                stream,
                &id,
                &info.keyspace,
                &info.table,
                &info.bind_specs,
                &info.pk_indexes,
                &info.result_cols,
                compression,
            ))
        }
        RequestOpcode::Execute => {
            let mut buf: &[u8] = body;
            let id = types::read_short_bytes(&mut buf).map_err(|_| query::QueryError {
                code: 0x000A,
                message: "Malformed EXECUTE message".to_string(),
            })?;
            let prepared = {
                let map = state.prepared.read().expect("prepared lock poisoned");
                match map.get(id) {
                    Some(p) => p.clone(),
                    None => {
                        return Err(query::QueryError {
                            code: 0x2200,
                            message: "Prepared statement not found (unprepared)".to_string(),
                        })
                    }
                }
            };
            let params = QueryParameters::deserialize(&mut buf).map_err(|_| query::QueryError {
                code: 0x000A,
                message: "Malformed EXECUTE parameters".to_string(),
            })?;
            let raw_values: Vec<types::RawValue> = params.values.iter().collect();
            if raw_values.len() != prepared.bind_types.len() {
                return Err(query::QueryError {
                    code: 0x2200,
                    message: "Invalid amount of bind variables".to_string(),
                });
            }
            let ks = state.keyspace();
            let result = query::execute_prepared(storage, ks.as_deref(), &prepared.stmt, &raw_values);
            settle_then(storage, result, |resp| {
                respond(version, stream, compression, Ok(resp), params.skip_metadata)
                    .unwrap_or_else(|e| protocol::error(version, stream, e.code, &e.message, compression))
            })
            .await
        }
        RequestOpcode::Batch => Err(query::QueryError {
            code: 0x2200,
            message: "Batches are not supported yet".to_string(),
        }),
        RequestOpcode::AuthResponse => Err(query::QueryError {
            code: 0x000A,
            message: "Authentication is not enabled".to_string(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn respond(
    version: u8,
    stream: i16,
    compression: Option<Compression>,
    result: Result<query::Response, query::QueryError>,
    skip_metadata: bool,
) -> Result<Vec<u8>, query::QueryError> {
    match result {
        Ok(query::Response::Rows {
            keyspace,
            table,
            cols,
            rows,
        }) => Ok(protocol::result_rows(
            version,
            stream,
            &keyspace,
            &table,
            &cols,
            &rows,
            skip_metadata,
            compression,
        )),
        Ok(query::Response::Void) => Ok(protocol::result_void(version, stream, compression)),
        Ok(query::Response::SetKeyspace(ks)) => {
            Ok(protocol::result_set_keyspace(version, stream, &ks, compression))
        }
        Ok(query::Response::SchemaChange {
            change,
            target,
            keyspace,
            table,
        }) => Ok(protocol::result_schema_change(
            version, stream, change, target, &keyspace, &table, compression,
        )),
        Err(e) => Err(e),
    }
}
