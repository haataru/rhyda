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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};

const MAX_FRAME_LENGTH: usize = 64 * 1024 * 1024;

/// Max concurrently executing requests per connection (protocol-level
/// pipelining depth). Responses may be written out of order; the CQL binary
/// protocol correlates them by stream id.
const PER_CONN_INFLIGHT: usize = 512;

/// Writer task flushes once its pending response bytes exceed this.
const WRITE_FLUSH_THRESHOLD: usize = 256 * 1024;

#[derive(Clone)]
struct PreparedStmt {
    stmt: Arc<CassandraStatement>,
    bind_types: Vec<query::ColumnType>,
}

/// Per-connection shared state with fine-grained synchronization so the hot
/// path (EXECUTE) never blocks: only PREPARE/STARTUP/USE touch write locks.
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
        self.keyspace
            .read()
            .expect("keyspace lock poisoned")
            .clone()
    }

    fn set_keyspace(&self, ks: &str) {
        *self
            .keyspace
            .write()
            .expect("keyspace lock poisoned") = Some(Arc::from(ks));
    }
}

pub async fn run(listener: TcpListener, storage: Arc<Storage>) -> anyhow::Result<()> {
    loop {
        let (socket, addr) = listener.accept().await?;
        let storage = storage.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(socket, storage).await {
                tracing::warn!(%addr, "connection closed with error: {e}");
            }
        });
    }
}

async fn handle_connection(socket: TcpStream, storage: Arc<Storage>) -> anyhow::Result<()> {
    let _ = socket.set_nodelay(true);

    let (resp_tx, mut resp_rx) = mpsc::channel::<Arc<[u8]>>(PER_CONN_INFLIGHT * 2);
    let (read_half, mut write_half) = socket.into_split();
    let writer = tokio::spawn(async move {
        let mut pending: Vec<Arc<[u8]>> = Vec::new();
        let mut queued_bytes = 0usize;
        while let Some(resp) = resp_rx.recv().await {
            queued_bytes += resp.len();
            pending.push(resp);
            // Coalesce: keep draining whatever is ready right now.
            while queued_bytes < WRITE_FLUSH_THRESHOLD {
                match resp_rx.try_recv() {
                    Ok(r) => {
                        queued_bytes += r.len();
                        pending.push(r);
                    }
                    Err(_) => break,
                }
            }
            let iovec: Vec<std::io::IoSlice> = pending
                .iter()
                .map(|p| std::io::IoSlice::new(p.as_ref()))
                .collect();
            if write_half.write_vectored(&iovec).await.is_err() {
                return;
            }
            pending.clear();
            queued_bytes = 0;
        }
    });

    let _writer = writer;

    let mut reader = BufReader::with_capacity(64 * 1024, read_half);

    let state = Arc::new(ConnShared::new());
    let inflight = Arc::new(Semaphore::new(PER_CONN_INFLIGHT));

    let features = ProtocolFeatures::default();

    loop {
        let mut header = [0u8; 9];
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }

        let version = header[0];
        let flags = header[1];
        let stream = i16::from_be_bytes([header[2], header[3]]);
        let opcode_raw = header[4];
        let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;

        if version != 0x04 {
            let resp = protocol::error(
                version,
                stream,
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
                stream,
                0x000A,
                "Frame too large",
                state.compression(),
            );
            let _ = resp_tx.send(Arc::from(resp)).await;
            break;
        }

        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await?;

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
                        stream,
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

        // Control-plane opcodes stay on the reader task: STARTUP/OPTIONS are
        // trivially cheap, PREPARE must register before any EXECUTE of the new
        // id can arrive, and USE must be applied before later statements on
        // the same connection observe the session keyspace.
        let inline = opcode_raw != RequestOpcode::Query as u8
            && opcode_raw != RequestOpcode::Execute as u8;

        if inline || is_session_use(&body) {
            match handle_request(&storage, &state, opcode_raw, &body, stream, &features).await {
                Ok(resp) => {
                    let _ = resp_tx.send(Arc::from(resp)).await;
                }
                Err(e) => {
                    let resp =
                        protocol::error(version, stream, e.code, &e.message, state.compression());
                    let _ = resp_tx.send(Arc::from(resp)).await;
                }
            }
            continue;
        }

        let permit = inflight.clone().acquire_owned().await?;
        let storage = storage.clone();
        let state = state.clone();
        let resp_tx = resp_tx.clone();
        tokio::spawn(async move {
            let resp = match handle_request(&storage, &state, opcode_raw, &body, stream, &features)
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    protocol::error(version, stream, e.code, &e.message, state.compression())
                }
            };
            let _ = resp_tx.send(Arc::from(resp)).await;
            drop(permit);
        });
    }
    Ok(())
}

fn is_session_use(body: &[u8]) -> bool {
    // QUERY body: <long string query> — sniff "USE " prefix cheaply.
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

    // Await micro-batch commits so a response is never sent before its write
    // is visible in the engine's memtable (strict read-your-writes).
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
                Startup::deserialize_with_features(&mut buf, features).map_err(|_| {
                    query::QueryError {
                        code: 0x000A,
                        message: "Malformed STARTUP message".to_string(),
                    }
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
            let q = Query::deserialize_with_features(&mut buf, features).map_err(|_| {
                query::QueryError {
                    code: 0x000A,
                    message: "Malformed QUERY message".to_string(),
                }
            })?;
            let query_text = q.contents.to_string();
            let values: Vec<types::RawValue> = q.parameters.values.iter().collect();
            let ks = state.keyspace();
            let page_size = q.parameters.page_size;
            let paging_state = q
                .parameters
                .paging_state
                .as_bytes_slice()
                .map(|b| b.to_vec());
            let result = query::execute_with_values_paged(
                storage,
                ks.as_deref(),
                &query_text,
                &values,
                page_size,
                paging_state,
            );
            let response = settle_then(storage, result, |resp| {
                if let query::Response::SetKeyspace(ref name) = resp {
                    state.set_keyspace(name);
                }
                respond(version, stream, compression, Ok(resp), q.parameters.skip_metadata)
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
            let stmt = query::parse_cached(storage, query_text).map_err(|e| query::QueryError {
                code: 0x2000,
                message: e.message,
            })?;
            let session_ks = state.keyspace();
            let info =
                query::prepared_info(storage, session_ks.as_deref(), &stmt)?;
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
            let page_size = params.page_size;
            let paging_state = params
                .paging_state
                .as_bytes_slice()
                .map(|b| b.to_vec());
            let result = query::execute_prepared_paged(
                storage,
                ks.as_deref(),
                &prepared.stmt,
                &raw_values,
                page_size,
                paging_state,
            );
            settle_then(storage, result, |resp| {
                respond(version, stream, compression, Ok(resp), params.skip_metadata)
                    .unwrap_or_else(|e| {
                        protocol::error(version, stream, e.code, &e.message, compression)
                    })
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
            paging_state,
        }) => Ok(protocol::result_rows(
            version,
            stream,
            &keyspace,
            &table,
            &cols,
            &rows,
            skip_metadata,
            paging_state.as_deref(),
            compression,
        )),
        Ok(query::Response::Void) => Ok(protocol::result_void(version, stream, compression)),
        Ok(query::Response::SetKeyspace(ks)) => Ok(protocol::result_set_keyspace(
            version,
            stream,
            &ks,
            compression,
        )),
        Ok(query::Response::SchemaChange {
            change,
            target,
            keyspace,
            table,
        }) => Ok(protocol::result_schema_change(
            version,
            stream,
            change,
            target,
            &keyspace,
            &table,
            compression,
        )),
        Err(e) => Err(e),
    }
}
