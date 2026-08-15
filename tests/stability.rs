use rhydadb::server;
use rhydadb::storage::Storage;
use scylla_cql::frame::request::query::{PagingState, Query, QueryParameters};
use scylla_cql::frame::request::Startup;
use scylla_cql::frame::SerializedRequest;
use scylla_cql::serialize::row::SerializedValues;
use scylla_cql::Consistency;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const OPCODE_ERROR: u8 = 0x00;
const OPCODE_READY: u8 = 0x02;
const OPCODE_RESULT: u8 = 0x08;

async fn start_server(dir: &std::path::Path) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let storage = Arc::new(Storage::open(dir).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        server::run(listener, storage).await.unwrap();
    });
    (addr, handle)
}

async fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut options = HashMap::new();
    options.insert(Cow::Borrowed("CQL_VERSION"), Cow::Borrowed("3.4.5"));
    let startup = Startup { options };
    let req = SerializedRequest::make(&startup, None, false).unwrap();
    s.write_all(req.get_data()).await.unwrap();
    let mut header = [0u8; 9];
    s.read_exact(&mut header).await.unwrap();
    assert_eq!(header[4], OPCODE_READY);
    s
}

async fn query(stream: &mut TcpStream, text: &str) -> (u8, Vec<u8>) {
    let params = QueryParameters {
        consistency: Consistency::One,
        serial_consistency: None,
        timestamp: None,
        page_size: None,
        paging_state: PagingState::start(),
        skip_metadata: false,
        values: Cow::Borrowed(SerializedValues::EMPTY),
    };
    let q = Query {
        contents: Cow::Borrowed(text),
        parameters: params,
    };
    let req = SerializedRequest::make(&q, None, false).unwrap();
    stream.write_all(req.get_data()).await.unwrap();
    let mut header = [0u8; 9];
    stream.read_exact(&mut header).await.unwrap();
    let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await.unwrap();
    (header[4], body)
}

struct RowsResponse {
    names: Vec<String>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
}

fn parse_result(body: &[u8]) -> (i32, Option<RowsResponse>) {
    let mut cur = body;
    let read_i32 = |cur: &mut &[u8]| -> i32 {
        let v = i32::from_be_bytes(cur[0..4].try_into().unwrap());
        *cur = &cur[4..];
        v
    };
    let read_short = |cur: &mut &[u8]| -> u16 {
        let v = u16::from_be_bytes(cur[0..2].try_into().unwrap());
        *cur = &cur[2..];
        v
    };
    let read_string = |cur: &mut &[u8]| -> String {
        let len = read_short(cur) as usize;
        let s = String::from_utf8(cur[..len].to_vec()).unwrap();
        *cur = &cur[len..];
        s
    };
    let kind = read_i32(&mut cur);
    if kind != 2 {
        return (kind, None);
    }
    let flags = read_i32(&mut cur);
    let col_count = read_i32(&mut cur) as usize;
    if flags & 1 != 0 {
        let _ = read_string(&mut cur);
        let _ = read_string(&mut cur);
    }
    let mut names = Vec::new();
    for _ in 0..col_count {
        names.push(read_string(&mut cur));
        let _type = read_short(&mut cur);
    }
    let rows_count = read_i32(&mut cur) as usize;
    let mut rows = Vec::new();
    for _ in 0..rows_count {
        let mut row = Vec::new();
        for _ in 0..col_count {
            let len = read_i32(&mut cur);
            if len < 0 {
                row.push(None);
            } else {
                let v = cur[..len as usize].to_vec();
                cur = &cur[len as usize..];
                row.push(Some(v));
            }
        }
        rows.push(row);
    }
    (kind, Some(RowsResponse { names, rows }))
}

fn write_frame(_stream: &mut TcpStream, version: u8, flags: u8, stream_id: i16, opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(9 + body.len());
    frame.push(version);
    frame.push(flags);
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.push(opcode);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    frame
}

#[tokio::test]
async fn concurrent_hammer() {
    let dir = std::env::temp_dir().join(format!("rhydadb-hammer-{}", std::process::id()));
    let (addr, server_handle) = start_server(&dir).await;

    let mut setup = connect(addr).await;
    let (op, _) = query(&mut setup, "CREATE KEYSPACE hammer WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}").await;
    assert_eq!(op, OPCODE_RESULT);
    let (op, _) = query(&mut setup, "CREATE TABLE hammer.items (client int, idx int, val int, PRIMARY KEY (client, idx))").await;
    assert_eq!(op, OPCODE_RESULT);

    let mut handles = Vec::new();
    for client in 0..8 {
        handles.push(tokio::spawn(async move {
            let mut s = connect(addr).await;
            for i in 0..50 {
                let (op, _) = query(&mut s, &format!("INSERT INTO hammer.items (client, idx, val) VALUES ({client}, {i}, {i})")).await;
                assert_eq!(op, OPCODE_RESULT, "insert failed for client {client} idx {i}");
            }
            let (op, body) = query(&mut s, &format!("SELECT val FROM hammer.items WHERE client = {client}")).await;
            assert_eq!(op, OPCODE_RESULT);
            let (kind, res) = parse_result(&body);
            assert_eq!(kind, 2);
            assert_eq!(res.unwrap().rows.len(), 50, "client {client} lost rows");
            for i in 0..50i32 {
                let (op, body) = query(&mut s, &format!("SELECT val FROM hammer.items WHERE client = {client} AND idx = {i}")).await;
                assert_eq!(op, OPCODE_RESULT);
                let (kind, res) = parse_result(&body);
                assert_eq!(kind, 2);
                let res = res.unwrap();
                assert_eq!(res.names, vec!["val"], "client {client} idx {i}");
                assert_eq!(res.rows.len(), 1, "client {client} idx {i}");
                assert_eq!(res.rows[0][0], Some(i.to_be_bytes().to_vec()), "client {client} idx {i} wrong value");
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let (op, body) = query(&mut setup, "SELECT * FROM hammer.items").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, 2);
    assert_eq!(res.unwrap().rows.len(), 400);

    drop(setup);
    server_handle.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn garbage_frames_do_not_kill_server() {
    let dir = std::env::temp_dir().join(format!("rhydadb-garbage-{}", std::process::id()));
    let (addr, _server_handle) = start_server(&dir).await;

    let mut c = connect(addr).await;
    let (op, _) = query(&mut c, "CREATE KEYSPACE g WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}").await;
    assert_eq!(op, OPCODE_RESULT);

    // Wrong protocol version -> protocol error and connection close.
    let mut v = connect(addr).await;
    let frame = write_frame(&mut v, 0x05, 0x00, 0, 0x07, &[]);
    v.write_all(&frame).await.unwrap();
    let mut header = [0u8; 9];
    let n = v.read(&mut header).await.unwrap();
    assert!(n >= 5);
    assert_eq!(header[4], OPCODE_ERROR);
    drop(v);

    // Huge declared length -> error frame, no allocation blowup.
    let mut v = connect(addr).await;
    let frame = write_frame(&mut v, 0x04, 0x00, 0, 0x07, &[]);
    let mut huge = frame.clone();
    huge[5..9].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    v.write_all(&huge).await.unwrap();
    let mut header = [0u8; 9];
    v.read_exact(&mut header).await.unwrap();
    assert_eq!(header[4], OPCODE_ERROR);
    drop(v);

    // Unknown opcode -> error frame.
    let mut v = connect(addr).await;
    let frame = write_frame(&mut v, 0x04, 0x00, 0, 0xFF, &[]);
    v.write_all(&frame).await.unwrap();
    let mut header = [0u8; 9];
    v.read_exact(&mut header).await.unwrap();
    assert_eq!(header[4], OPCODE_ERROR);
    drop(v);

    // Malformed QUERY body (truncated) -> error frame.
    let mut v = connect(addr).await;
    let frame = write_frame(&mut v, 0x04, 0x00, 0, 0x07, &[0x00, 0x01]);
    v.write_all(&frame).await.unwrap();
    let mut header = [0u8; 9];
    v.read_exact(&mut header).await.unwrap();
    assert_eq!(header[4], OPCODE_ERROR);
    drop(v);

    // Random garbage bytes on a fresh connection -> connection just dies.
    let mut v = connect(addr).await;
    let mut rng = 0x12345678u32;
    let mut garbage = Vec::new();
    for _ in 0..200 {
        rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
        garbage.push((rng >> 24) as u8);
    }
    v.write_all(&garbage).await.unwrap();
    let mut buf = [0u8; 64];
    let _ = v.read(&mut buf).await;
    drop(v);

    // Compressed frame without negotiated compression -> error frame.
    let mut v = connect(addr).await;
    let frame = write_frame(&mut v, 0x04, 0x01, 0, 0x07, &[0x00, 0x00, 0x00, 0x00]);
    v.write_all(&frame).await.unwrap();
    let mut header = [0u8; 9];
    v.read_exact(&mut header).await.unwrap();
    assert_eq!(header[4], OPCODE_ERROR);
    drop(v);

    // Server still alive and functional.
    let mut ok = connect(addr).await;
    let (op, _) = query(&mut ok, "SELECT * FROM g.system_schema_keyspaces_missing").await;
    assert_eq!(op, OPCODE_ERROR);
    let (op, _) = query(&mut ok, "SELECT * FROM system_schema.keyspaces").await;
    assert_eq!(op, OPCODE_RESULT);
    drop(ok);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn persistence_across_restart() {
    let dir = std::env::temp_dir().join(format!("rhydadb-persist-{}", std::process::id()));
    let (addr, server_handle) = start_server(&dir).await;

    let mut c = connect(addr).await;
    let (op, _) = query(&mut c, "CREATE KEYSPACE p WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}").await;
    assert_eq!(op, OPCODE_RESULT);
    let (op, _) = query(&mut c, "CREATE TABLE p.users (id int PRIMARY KEY, name text)").await;
    assert_eq!(op, OPCODE_RESULT);
    let (op, _) = query(&mut c, "INSERT INTO p.users (id, name) VALUES (1, 'alice')").await;
    assert_eq!(op, OPCODE_RESULT);
    let (op, _) = query(&mut c, "INSERT INTO p.users (id, name) VALUES (2, 'bob')").await;
    assert_eq!(op, OPCODE_RESULT);
    drop(c);

    // Simulate restart: stop the server (releases the RocksDB lock) and reopen
    // the same data directory with a fresh Storage.
    server_handle.abort();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let storage = Storage::open(&dir).unwrap();
    let table = storage.get_table("p", "users").unwrap().expect("table should survive restart");
    assert_eq!(table.columns.len(), 2);
    let row1 = storage.get_row("p", "users", &[1i32.to_be_bytes().to_vec()]).unwrap().expect("row 1");
    let row2 = storage.get_row("p", "users", &[2i32.to_be_bytes().to_vec()]).unwrap().expect("row 2");
    assert!(String::from_utf8_lossy(&row1).contains("alice"));
    assert!(String::from_utf8_lossy(&row2).contains("bob"));
    assert!(storage.get_row("p", "users", &[3i32.to_be_bytes().to_vec()]).unwrap().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}