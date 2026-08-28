use crate::cql_value::Value;
use crate::schema::ColumnType;
use scylla_cql::frame::Compression;
use scylla_cql::frame::response::ResponseOpcode;
use scylla_cql::frame::types;
use std::collections::HashMap;

pub fn response_frame(
    version: u8,
    stream: i16,
    opcode: ResponseOpcode,
    body: &[u8],
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut flags = 0u8;
    let payload: Vec<u8> = if let Some(comp) = compression {
        flags |= 0x01;
        let mut compressed = Vec::new();
        scylla_cql::frame::compress_append(body, comp, &mut compressed).expect("compression failed");
        compressed
    } else {
        body.to_vec()
    };
    let mut out = Vec::with_capacity(9 + payload.len());
    out.push(version | 0x80);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.push(opcode as u8);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub fn ready(version: u8, stream: i16, compression: Option<Compression>) -> Vec<u8> {
    response_frame(version, stream, ResponseOpcode::Ready, &[], compression)
}

pub fn supported(version: u8, stream: i16, compression: Option<Compression>) -> Vec<u8> {
    let mut options = HashMap::new();
    options.insert("CQL_VERSION".to_string(), vec!["3.4.5".to_string()]);
    options.insert(
        "COMPRESSION".to_string(),
        vec!["lz4".to_string(), "snappy".to_string()],
    );
    options.insert("PROTOCOL_VERSIONS".to_string(), vec!["4".to_string()]);
    let mut body = Vec::new();
    types::write_string_multimap(&options, &mut body).expect("supported body serialization");
    response_frame(
        version,
        stream,
        ResponseOpcode::Supported,
        &body,
        compression,
    )
}

pub fn error(
    version: u8,
    stream: i16,
    code: i32,
    message: &str,
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&code.to_be_bytes());
    types::write_string(message, &mut body).expect("error message serialization");
    response_frame(version, stream, ResponseOpcode::Error, &body, compression)
}

pub fn result_void(version: u8, stream: i16, compression: Option<Compression>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0001i32.to_be_bytes());
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}

pub fn result_set_keyspace(
    version: u8,
    stream: i16,
    keyspace: &str,
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0003i32.to_be_bytes());
    types::write_string(keyspace, &mut body).expect("set_keyspace serialization");
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}

pub fn result_schema_change(
    version: u8,
    stream: i16,
    change: &str,
    target: &str,
    keyspace: &str,
    table: &str,
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0005i32.to_be_bytes());
    types::write_string(change, &mut body).expect("schema change serialization");
    types::write_string(target, &mut body).expect("schema change serialization");
    types::write_string(keyspace, &mut body).expect("schema change serialization");
    types::write_string(table, &mut body).expect("schema change serialization");
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}

#[allow(clippy::too_many_arguments)]
pub fn result_prepared(
    version: u8,
    stream: i16,
    id: &[u8],
    keyspace: &str,
    table: &str,
    bind_specs: &[(String, ColumnType)],
    pk_indexes: &[u16],
    result_cols: &[(String, ColumnType)],
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0004i32.to_be_bytes());
    types::write_short_bytes(id, &mut body).expect("prepared id serialization");

    body.extend_from_slice(&0x0001i32.to_be_bytes());
    body.extend_from_slice(&(bind_specs.len() as i32).to_be_bytes());
    body.extend_from_slice(&(pk_indexes.len() as i32).to_be_bytes());
    for idx in pk_indexes {
        body.extend_from_slice(&idx.to_be_bytes());
    }
    types::write_string(keyspace, &mut body).expect("metadata serialization");
    types::write_string(table, &mut body).expect("metadata serialization");
    for (name, ty) in bind_specs {
        types::write_string(name, &mut body).expect("metadata serialization");
        ty.write_wire_type(&mut body);
    }

    body.extend_from_slice(&0x0001i32.to_be_bytes());
    body.extend_from_slice(&(result_cols.len() as i32).to_be_bytes());
    types::write_string(keyspace, &mut body).expect("metadata serialization");
    types::write_string(table, &mut body).expect("metadata serialization");
    for (name, ty) in result_cols {
        types::write_string(name, &mut body).expect("metadata serialization");
        ty.write_wire_type(&mut body);
    }

    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}

#[allow(clippy::too_many_arguments)]
pub fn result_rows(
    version: u8,
    stream: i16,
    keyspace: &str,
    table: &str,
    cols: &[(String, ColumnType)],
    rows: &[Vec<Value>],
    skip_metadata: bool,
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0002i32.to_be_bytes());
    if skip_metadata {
        body.extend_from_slice(&0x0000i32.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
    } else {
        body.extend_from_slice(&0x0001i32.to_be_bytes());
        body.extend_from_slice(&(cols.len() as i32).to_be_bytes());
        types::write_string(keyspace, &mut body).expect("metadata serialization");
        types::write_string(table, &mut body).expect("metadata serialization");
        for (name, ty) in cols {
            types::write_string(name, &mut body).expect("metadata serialization");
            ty.write_wire_type(&mut body);
        }
    }
    body.extend_from_slice(&(rows.len() as i32).to_be_bytes());
    // Pre-reserve: estimate 16 bytes per value + payload to avoid reallocs on large rows.
    let est: usize = rows.iter().map(|r| r.len() * 16).sum();
    body.reserve(est);
    for row in rows {
        for v in row {
            v.write_wire(&mut body);
        }
    }
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}
