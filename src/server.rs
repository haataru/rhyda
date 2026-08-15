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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_FRAME_LENGTH: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct PreparedStmt {
    stmt: Arc<CassandraStatement>,
    bind_types: Vec<query::ColumnType>,
}

struct ConnState {
    version: u8,
    compression: Option<Compression>,
    keyspace: Option<String>,
    prepared: HashMap<Vec<u8>, PreparedStmt>,
    next_prepared_id: u16,
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

async fn handle_connection(mut socket: TcpStream, storage: Arc<Storage>) -> anyhow::Result<()> {
    let mut state = ConnState {
        version: 0x04,
        compression: None,
        keyspace: None,
        prepared: HashMap::new(),
        next_prepared_id: 1,
    };

    loop {
        let mut header = [0u8; 9];
        match socket.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }

        let version = header[0];
        let flags = header[1];
        let stream = i16::from_be_bytes([header[2], header[3]]);
        let opcode = header[4];
        let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;

        if version != 0x04 {
            let resp = protocol::error(
                version,
                stream,
                0x000A,
                "Unsupported protocol version; this server supports CQL v4",
                None,
            );
            socket.write_all(&resp).await?;
            return Ok(());
        }
        if length > MAX_FRAME_LENGTH {
            let resp = protocol::error(
                version,
                stream,
                0x000A,
                "Frame too large",
                state.compression,
            );
            socket.write_all(&resp).await?;
            return Ok(());
        }

        let mut body = vec![0u8; length];
        socket.read_exact(&mut body).await?;

        let body: bytes::Bytes = if flags & 0x01 != 0 {
            match state.compression {
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
                    socket.write_all(&resp).await?;
                    continue;
                }
            }
        } else {
            body.into()
        };

        let resp =
            handle_request(&storage, &mut state, opcode, &body, stream).unwrap_or_else(|e| {
                protocol::error(version, stream, e.code, &e.message, state.compression)
            });
        socket.write_all(&resp).await?;
    }
}

fn handle_request(
    storage: &Storage,
    state: &mut ConnState,
    opcode: u8,
    body: &[u8],
    stream: i16,
) -> Result<Vec<u8>, query::QueryError> {
    let version = state.version;
    let compression = state.compression;
    let opcode = RequestOpcode::try_from(opcode).map_err(|_| query::QueryError {
        code: 0x000A,
        message: "Unknown opcode".to_string(),
    })?;
    let features = ProtocolFeatures::default();

    match opcode {
        RequestOpcode::Options => Ok(protocol::supported(version, stream, compression)),
        RequestOpcode::Startup => {
            let mut buf: &[u8] = body;
            let startup =
                Startup::deserialize_with_features(&mut buf, &features).map_err(|_| {
                    query::QueryError {
                        code: 0x000A,
                        message: "Malformed STARTUP message".to_string(),
                    }
                })?;
            match startup.options.get("COMPRESSION").map(|c| c.to_lowercase()) {
                Some(c) if c == "lz4" => state.compression = Some(Compression::Lz4),
                Some(c) if c == "snappy" => state.compression = Some(Compression::Snappy),
                Some(other) => {
                    return Err(query::QueryError {
                        code: 0x000A,
                        message: format!("Unsupported compression algorithm: {other}"),
                    });
                }
                None => {}
            }
            Ok(protocol::ready(version, stream, state.compression))
        }
        RequestOpcode::Register => Ok(protocol::ready(version, stream, compression)),
        RequestOpcode::Query => {
            let mut buf: &[u8] = body;
            let q = Query::deserialize_with_features(&mut buf, &features).map_err(|_| {
                query::QueryError {
                    code: 0x000A,
                    message: "Malformed QUERY message".to_string(),
                }
            })?;
            let query_text = q.contents.to_string();
            let values: Vec<types::RawValue> = q.parameters.values.iter().collect();
            respond(
                version,
                stream,
                state.compression,
                query::execute_with_values(storage, &mut state.keyspace, &query_text, &values),
                false,
            )
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
            let info = query::prepared_info(storage, &state.keyspace, &stmt)?;
            let id = state.next_prepared_id.to_be_bytes().to_vec();
            state.next_prepared_id = state.next_prepared_id.wrapping_add(1);
            let bind_types: Vec<query::ColumnType> =
                info.bind_specs.iter().map(|(_, t)| t.clone()).collect();
            state
                .prepared
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
                state.compression,
            ))
        }
        RequestOpcode::Execute => {
            let mut buf: &[u8] = body;
            let id = types::read_short_bytes(&mut buf).map_err(|_| query::QueryError {
                code: 0x000A,
                message: "Malformed EXECUTE message".to_string(),
            })?;
            let prepared = state
                .prepared
                .get(id)
                .cloned()
                .ok_or_else(|| query::QueryError {
                    code: 0x2200,
                    message: "Prepared statement not found (unprepared)".to_string(),
                })?;
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
            respond(
                version,
                stream,
                state.compression,
                query::execute_prepared(storage, &mut state.keyspace, &prepared.stmt, &raw_values),
                params.skip_metadata,
            )
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
