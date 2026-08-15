use crate::cql_value::Value;
use crate::schema::ColumnType;
use scylla_cql::frame::response::ResponseOpcode;
use scylla_cql::frame::types;
use scylla_cql::frame::Compression;
use std::collections::HashMap;

/// Serialize a complete response frame (header + body) for a given stream.
/// Body is compressed if compression was negotiated with the client.
pub fn response_frame(
    version: u8,
    stream: i16,
    opcode: ResponseOpcode,
    body: &[u8],
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut flags = 0u8;
    let mut payload = body.to_vec();
    if let Some(comp) = compression {
        flags |= 0x01;
        let mut compressed = Vec::new();
        types_compress(body, comp, &mut compressed);
        payload = compressed;
    }
    let mut out = Vec::with_capacity(9 + payload.len());
    out.push(version | 0x80);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.push(opcode as u8);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

fn types_compress(body: &[u8], comp: Compression, out: &mut Vec<u8>) {
    scylla_cql::frame::compress_append(body, comp, out).expect("compression failed");
}

/// READY response.
pub fn ready(version: u8, stream: i16, compression: Option<Compression>) -> Vec<u8> {
    response_frame(version, stream, ResponseOpcode::Ready, &[], compression)
}

/// SUPPORTED response with the server's capabilities.
pub fn supported(version: u8, stream: i16, compression: Option<Compression>) -> Vec<u8> {
    let mut options = HashMap::new();
    options.insert("CQL_VERSION".to_string(), vec!["3.4.5".to_string()]);
    options.insert("COMPRESSION".to_string(), vec!["lz4".to_string(), "snappy".to_string()]);
    options.insert("PROTOCOL_VERSIONS".to_string(), vec!["4".to_string()]);
    let mut body = Vec::new();
    types::write_string_multimap(&options, &mut body).expect("supported body serialization");
    response_frame(version, stream, ResponseOpcode::Supported, &body, compression)
}

/// ERROR response with the given protocol error code and message.
pub fn error(version: u8, stream: i16, code: i32, message: &str, compression: Option<Compression>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&code.to_be_bytes());
    types::write_string(message, &mut body).expect("error message serialization");
    response_frame(version, stream, ResponseOpcode::Error, &body, compression)
}

/// RESULT:Void (kind 0x0001).
pub fn result_void(version: u8, stream: i16, compression: Option<Compression>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0001i32.to_be_bytes());
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}

/// RESULT:SetKeyspace (kind 0x0003).
pub fn result_set_keyspace(version: u8, stream: i16, keyspace: &str, compression: Option<Compression>) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0003i32.to_be_bytes());
    types::write_string(keyspace, &mut body).expect("set_keyspace serialization");
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}

/// RESULT:SchemaChange (kind 0x0005).
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

/// RESULT:Rows (kind 0x0002) with global table spec in the metadata.
pub fn result_rows(
    version: u8,
    stream: i16,
    keyspace: &str,
    table: &str,
    cols: &[(String, ColumnType)],
    rows: &[Vec<Value>],
    compression: Option<Compression>,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0002i32.to_be_bytes()); // kind: Rows
    body.extend_from_slice(&0x0001i32.to_be_bytes()); // flags: global table spec
    body.extend_from_slice(&(cols.len() as i32).to_be_bytes()); // column count
    types::write_string(keyspace, &mut body).expect("metadata serialization");
    types::write_string(table, &mut body).expect("metadata serialization");
    for (name, ty) in cols {
        types::write_string(name, &mut body).expect("metadata serialization");
        ty.write_wire_type(&mut body); // [short] type id (+ element types for collections)
    }
    body.extend_from_slice(&(rows.len() as i32).to_be_bytes());
    for row in rows {
        for v in row {
            body.extend(v.to_wire());
        }
    }
    response_frame(version, stream, ResponseOpcode::Result, &body, compression)
}