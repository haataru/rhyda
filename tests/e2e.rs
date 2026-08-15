use rhydadb::server;
use rhydadb::storage::Storage;
use scylla_cql::frame::request::query::{PagingState, Query, QueryParameters};
use scylla_cql::frame::request::{Startup, Options};
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
const OPCODE_SUPPORTED: u8 = 0x06;
const OPCODE_RESULT: u8 = 0x08;

const RESULT_VOID: i32 = 1;
const RESULT_ROWS: i32 = 2;
const RESULT_SET_KEYSPACE: i32 = 3;
const RESULT_SCHEMA_CHANGE: i32 = 5;

async fn start_server() -> (std::net::SocketAddr, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("rhydadb-test-{}", std::process::id()));
    let storage = Arc::new(Storage::open(&dir).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        server::run(listener, storage).await.unwrap();
    });
    (addr, dir)
}

struct Client {
    stream: TcpStream,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Client {
        Client {
            stream: TcpStream::connect(addr).await.unwrap(),
        }
    }

    async fn send_frame(&mut self, data: &[u8]) {
        self.stream.write_all(data).await.unwrap();
    }

    async fn read_frame(&mut self) -> (u8, Vec<u8>) {
        let mut header = [0u8; 9];
        self.stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 0x84, "unexpected response version byte");
        let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
        let mut body = vec![0u8; length];
        self.stream.read_exact(&mut body).await.unwrap();
        (header[4], body)
    }

    async fn startup(&mut self) -> (u8, Vec<u8>) {
        let mut options = HashMap::new();
        options.insert(Cow::Borrowed("CQL_VERSION"), Cow::Borrowed("3.4.5"));
        let startup = Startup { options };
        let req = SerializedRequest::make(&startup, None, false).unwrap();
        self.send_frame(req.get_data()).await;
        self.read_frame().await
    }

    async fn options(&mut self) -> (u8, Vec<u8>) {
        let req = SerializedRequest::make(&Options, None, false).unwrap();
        self.send_frame(req.get_data()).await;
        self.read_frame().await
    }

    async fn query(&mut self, text: &str) -> (u8, Vec<u8>) {
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
        self.send_frame(req.get_data()).await;
        self.read_frame().await
    }
}

fn read_i32(cur: &mut &[u8]) -> i32 {
    let (b, rest) = cur.split_at(4);
    *cur = rest;
    i32::from_be_bytes(b.try_into().unwrap())
}

fn read_short(cur: &mut &[u8]) -> u16 {
    let (b, rest) = cur.split_at(2);
    *cur = rest;
    u16::from_be_bytes(b.try_into().unwrap())
}

fn read_string(cur: &mut &[u8]) -> String {
    let len = read_short(cur) as usize;
    assert!(len <= cur.len(), "read_string: len={len} but only {} bytes left", cur.len());
    let (s, rest) = cur.split_at(len);
    *cur = rest;
    String::from_utf8(s.to_vec()).unwrap()
}

struct RowsResponse {
    names: Vec<String>,
    rows: Vec<Vec<Option<Vec<u8>>>>,
}

fn parse_result(body: &[u8]) -> (i32, Option<RowsResponse>) {
    let mut cur = body;
    let kind = read_i32(&mut cur);
    if kind != RESULT_ROWS {
        return (kind, None);
    }
    let flags = read_i32(&mut cur);
    let col_count = read_i32(&mut cur) as usize;
    if flags & 1 != 0 {
        let _ks = read_string(&mut cur);
        let _table = read_string(&mut cur);
    }
    let mut names = Vec::new();
    for _ in 0..col_count {
        let name = read_string(&mut cur);
        let _col_type = read_short(&mut cur);
        names.push(name);
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
                let (v, rest) = cur.split_at(len as usize);
                cur = rest;
                row.push(Some(v.to_vec()));
            }
        }
        rows.push(row);
    }
    (kind, Some(RowsResponse { names, rows }))
}

#[tokio::test]
async fn full_cql_flow() {
    let (addr, dir) = start_server().await;
    let mut c = Client::connect(addr).await;

    let (op, body) = c.options().await;
    assert_eq!(op, OPCODE_SUPPORTED);
    assert!(String::from_utf8_lossy(&body).contains("CQL_VERSION"));

    let (op, _) = c.startup().await;
    assert_eq!(op, OPCODE_READY);

    let (op, body) = c
        .query("CREATE KEYSPACE test WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}")
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c.query("CREATE TABLE test.users (id int PRIMARY KEY, name text, age int)").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c.query("USE test").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SET_KEYSPACE);

    let (op, body) = c.query("INSERT INTO users (id, name, age) VALUES (1, 'alice', 30)").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("INSERT INTO users (id, name, age) VALUES (2, 'bob', 25)").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("SELECT * FROM users").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    let res = res.unwrap();
    assert_eq!(res.names, vec!["id", "name", "age"]);
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0][0], Some(1i32.to_be_bytes().to_vec()));
    assert_eq!(res.rows[0][1], Some(b"alice".to_vec()));
    assert_eq!(res.rows[0][2], Some(30i32.to_be_bytes().to_vec()));
    assert_eq!(res.rows[1][0], Some(2i32.to_be_bytes().to_vec()));
    assert_eq!(res.rows[1][1], Some(b"bob".to_vec()));
    assert_eq!(res.rows[1][2], Some(25i32.to_be_bytes().to_vec()));

    let (op, body) = c.query("SELECT name FROM users WHERE id = 2").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    let res = res.unwrap();
    assert_eq!(res.names, vec!["name"]);
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0][0], Some(b"bob".to_vec()));

    let (op, body) = c.query("SELECT id FROM users WHERE id IN (1, 2)").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    let res = res.unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0][0], Some(1i32.to_be_bytes().to_vec()));
    assert_eq!(res.rows[1][0], Some(2i32.to_be_bytes().to_vec()));

    let (op, body) = c.query("CREATE TABLE sensor (device text, ts bigint, temp double, PRIMARY KEY (device, ts))").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c.query("INSERT INTO sensor (device, ts, temp) VALUES ('a', 100, 36.6)").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("INSERT INTO sensor (device, ts, temp) VALUES ('a', 200, 37.0)").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("SELECT * FROM sensor WHERE device = 'a'").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    let res = res.unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0][0], Some(b"a".to_vec()));
    assert_eq!(res.rows[0][1], Some(100i64.to_be_bytes().to_vec()));
    assert_eq!(res.rows[0][2], Some(36.6f64.to_be_bytes().to_vec()));
    assert_eq!(res.rows[1][0], Some(b"a".to_vec()));
    assert_eq!(res.rows[1][1], Some(200i64.to_be_bytes().to_vec()));
    assert_eq!(res.rows[1][2], Some(37.0f64.to_be_bytes().to_vec()));

    let (op, body) = c.query("SELECT * FROM sensor WHERE device = 'a' AND ts = 200").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    assert_eq!(res.unwrap().rows.len(), 1);

    let (op, body) = c.query("UPDATE users SET age = 31 WHERE id = 1").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("SELECT age FROM users WHERE id = 1").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    assert_eq!(res.unwrap().rows[0][0], Some(31i32.to_be_bytes().to_vec()));

    let (op, body) = c.query("DELETE FROM users WHERE id = 2").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("SELECT * FROM users").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    assert_eq!(res.unwrap().rows.len(), 1);

    let (op, body) = c.query("TRUNCATE sensor").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c.query("SELECT * FROM sensor").await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    assert_eq!(res.unwrap().rows.len(), 0);

    let (op, body) = c.query("DROP TABLE users").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c.query("SELECT * FROM users").await;
    assert_eq!(op, OPCODE_ERROR);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 0x2200);

    let _ = std::fs::remove_dir_all(&dir);
}