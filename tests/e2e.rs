use rhydadb::server;
use rhydadb::storage::Storage;
use scylla_cql::Consistency;
use scylla_cql::frame::SerializedRequest;
use scylla_cql::frame::request::query::{PagingState, Query, QueryParameters};
use scylla_cql::frame::request::{Options, Startup};
use scylla_cql::serialize::row::SerializedValues;
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
    let test_name = std::thread::current()
        .name()
        .unwrap_or("unknown")
        .replace("::", "-");
    let dir =
        std::env::temp_dir().join(format!("rhydadb-test-{}-{}", std::process::id(), test_name));
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
    assert!(
        len <= cur.len(),
        "read_string: len={len} but only {} bytes left",
        cur.len()
    );
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

    let (op, body) = c
        .query("CREATE TABLE test.users (id int PRIMARY KEY, name text, age int)")
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c.query("USE test").await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SET_KEYSPACE);

    let (op, body) = c
        .query("INSERT INTO users (id, name, age) VALUES (1, 'alice', 30)")
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c
        .query("INSERT INTO users (id, name, age) VALUES (2, 'bob', 25)")
        .await;
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

    let (op, body) = c
        .query(
            "CREATE TABLE sensor (device text, ts bigint, temp double, PRIMARY KEY (device, ts))",
        )
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c
        .query("INSERT INTO sensor (device, ts, temp) VALUES ('a', 100, 36.6)")
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c
        .query("INSERT INTO sensor (device, ts, temp) VALUES ('a', 200, 37.0)")
        .await;
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

    let (op, body) = c
        .query("SELECT * FROM sensor WHERE device = 'a' AND ts = 200")
        .await;
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

fn read_short_bytes(cur: &mut &[u8]) -> Vec<u8> {
    let len = read_short(cur) as usize;
    let (b, rest) = cur.split_at(len);
    *cur = rest;
    b.to_vec()
}

fn long_string(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(s.len() as i32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

fn wire_value(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(raw.len() as i32).to_be_bytes());
    out.extend_from_slice(raw);
    out
}

fn wire_set_text(items: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(items.len() as i32).to_be_bytes());
    for it in items {
        out.extend(wire_value(it.as_bytes()));
    }
    out
}

fn execute_body(id: &[u8], values: &[Vec<u8>], skip_metadata: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(id.len() as u16).to_be_bytes());
    out.extend_from_slice(id);
    out.extend_from_slice(&0x0001u16.to_be_bytes());
    out.push(0x01 | if skip_metadata { 0x02 } else { 0 });
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for v in values {
        out.extend_from_slice(v);
    }
    out
}

fn query_body(query: &str, values: &[Vec<u8>]) -> Vec<u8> {
    let mut out = long_string(query);
    out.extend_from_slice(&0x0001u16.to_be_bytes());
    out.push(0x01);
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for v in values {
        out.extend_from_slice(v);
    }
    out
}

impl Client {
    async fn raw_request(&mut self, opcode: u8, body: &[u8]) -> (u8, Vec<u8>) {
        let mut frame = Vec::with_capacity(9 + body.len());
        frame.push(0x04);
        frame.push(0x00);
        frame.extend_from_slice(&0i16.to_be_bytes());
        frame.push(opcode);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(body);
        self.send_frame(&frame).await;
        self.read_frame().await
    }
}

#[tokio::test]
async fn prepared_statements() {
    let (addr, dir) = start_server().await;
    let mut c = Client::connect(addr).await;

    let (op, _) = c.startup().await;
    assert_eq!(op, OPCODE_READY);

    let (op, body) = c
        .query("CREATE KEYSPACE prep WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}")
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, body) = c
        .query("CREATE TABLE prep.t (id int PRIMARY KEY, name text, tags set<text>)")
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_SCHEMA_CHANGE);

    let (op, _) = c.query("USE prep").await;
    assert_eq!(op, OPCODE_RESULT);

    let (op, body) = c
        .raw_request(
            0x09,
            &long_string("INSERT INTO t (id, name, tags) VALUES (?, ?, ?)"),
        )
        .await;
    assert_eq!(op, OPCODE_RESULT);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 4);
    let id = read_short_bytes(&mut cur);
    assert_eq!(id.len(), 2);
    let _bind_flags = read_i32(&mut cur);
    assert_eq!(read_i32(&mut cur), 3);
    assert_eq!(read_i32(&mut cur), 1);
    assert_eq!(read_short(&mut cur), 0);
    assert_eq!(read_string(&mut cur), "prep");
    assert_eq!(read_string(&mut cur), "t");
    let name = read_string(&mut cur);
    assert_eq!(name, "id");
    assert_eq!(read_short(&mut cur), 0x0009);
    let name = read_string(&mut cur);
    assert_eq!(name, "name");
    assert_eq!(read_short(&mut cur), 0x000D);
    let name = read_string(&mut cur);
    assert_eq!(name, "tags");
    assert_eq!(read_short(&mut cur), 0x0022);
    assert_eq!(read_short(&mut cur), 0x000D);
    let _result_flags = read_i32(&mut cur);
    assert_eq!(read_i32(&mut cur), 0);
    assert_eq!(read_string(&mut cur), "prep");
    assert_eq!(read_string(&mut cur), "t");
    assert!(cur.is_empty());

    let exec = execute_body(
        &id,
        &[
            wire_value(&1i32.to_be_bytes()),
            wire_value(b"alice"),
            wire_value(&wire_set_text(&["x", "y"])),
        ],
        false,
    );
    let (op, body) = c.raw_request(0x0A, &exec).await;
    if op != OPCODE_RESULT {
        let mut cur = &body[..];
        eprintln!(
            "EXECUTE error code={:#x} msg={:?}",
            read_i32(&mut cur),
            String::from_utf8_lossy(cur)
        );
    }
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);

    let (op, body) = c
        .raw_request(0x09, &long_string("SELECT * FROM t WHERE id = ?"))
        .await;
    assert_eq!(op, OPCODE_RESULT);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 4);
    let sel_id = read_short_bytes(&mut cur);
    let _bind_flags = read_i32(&mut cur);
    assert_eq!(read_i32(&mut cur), 1);
    assert_eq!(read_i32(&mut cur), 0);
    let ks = read_string(&mut cur);
    if ks != "prep" {
        eprintln!("SELECT PREPARE body: {:02x?}", body);
        eprintln!("ks={ks:?}, rest={:02x?}", cur);
    }
    assert_eq!(ks, "prep");
    assert_eq!(read_string(&mut cur), "t");
    assert_eq!(read_string(&mut cur), "id");
    assert_eq!(read_short(&mut cur), 0x0009);
    let _result_flags = read_i32(&mut cur);
    assert_eq!(read_i32(&mut cur), 3);
    assert_eq!(read_string(&mut cur), "prep");
    assert_eq!(read_string(&mut cur), "t");
    assert_eq!(read_string(&mut cur), "id");
    assert_eq!(read_short(&mut cur), 0x0009);
    assert_eq!(read_string(&mut cur), "name");
    assert_eq!(read_short(&mut cur), 0x000D);
    assert_eq!(read_string(&mut cur), "tags");
    assert_eq!(read_short(&mut cur), 0x0022);
    assert_eq!(read_short(&mut cur), 0x000D);
    assert!(cur.is_empty());

    let exec = execute_body(&sel_id, &[wire_value(&1i32.to_be_bytes())], true);
    let (op, body) = c.raw_request(0x0A, &exec).await;
    assert_eq!(op, OPCODE_RESULT);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), RESULT_ROWS);
    assert_eq!(read_i32(&mut cur), 0);
    assert_eq!(read_i32(&mut cur), -1);
    assert_eq!(read_i32(&mut cur), 1);
    let id_raw = read_i32(&mut cur);
    assert_eq!(id_raw, 4);
    let (v, rest) = cur.split_at(4);
    assert_eq!(v, 1i32.to_be_bytes());
    cur = rest;
    let name_len = read_i32(&mut cur);
    assert_eq!(name_len, 5);
    let (v, rest) = cur.split_at(5);
    assert_eq!(v, b"alice");
    cur = rest;
    let tags_len = read_i32(&mut cur);
    assert_eq!(tags_len as usize, wire_set_text(&["x", "y"]).len());
    let (v, rest) = cur.split_at(tags_len as usize);
    assert_eq!(v, wire_set_text(&["x", "y"]));
    cur = rest;
    assert!(cur.is_empty());

    let (op, body) = c
        .raw_request(0x09, &long_string("UPDATE t SET name = ? WHERE id = ?"))
        .await;
    assert_eq!(op, OPCODE_RESULT);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 4);
    let upd_id = read_short_bytes(&mut cur);
    let _f = read_i32(&mut cur);
    assert_eq!(read_i32(&mut cur), 2);
    assert_eq!(read_i32(&mut cur), 0);
    assert_eq!(read_string(&mut cur), "prep");
    assert_eq!(read_string(&mut cur), "t");
    assert_eq!(read_string(&mut cur), "name");
    assert_eq!(read_short(&mut cur), 0x000D);
    assert_eq!(read_string(&mut cur), "id");
    assert_eq!(read_short(&mut cur), 0x0009);
    let (op, body) = c
        .raw_request(
            0x0A,
            &execute_body(
                &upd_id,
                &[wire_value(b"bob"), wire_value(&2i32.to_be_bytes())],
                false,
            ),
        )
        .await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);
    let (op, body) = c
        .raw_request(0x09, &long_string("SELECT name FROM t WHERE id = ?"))
        .await;
    assert_eq!(op, OPCODE_RESULT);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 4);
    let sel_name_id = read_short_bytes(&mut cur);
    let (op, body) = c
        .raw_request(
            0x0A,
            &execute_body(&sel_name_id, &[wire_value(&2i32.to_be_bytes())], false),
        )
        .await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    let res = res.unwrap();
    assert_eq!(res.names, vec!["name"]);
    assert_eq!(res.rows[0][0], Some(b"bob".to_vec()));

    let (op, body) = c
        .raw_request(0x0A, &execute_body(&[0x00, 0xFF], &[], false))
        .await;
    assert_eq!(op, OPCODE_ERROR);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 0x2200);

    let qv_ins = query_body(
        "INSERT INTO t (id, name) VALUES (?, ?)",
        &[wire_value(&3i32.to_be_bytes()), wire_value(b"carol")],
    );
    let (op, body) = c.raw_request(0x07, &qv_ins).await;
    assert_eq!(op, OPCODE_RESULT);
    assert_eq!(parse_result(&body).0, RESULT_VOID);
    let qv_sel = query_body(
        "SELECT name FROM t WHERE id = ?",
        &[wire_value(&3i32.to_be_bytes())],
    );
    let (op, body) = c.raw_request(0x07, &qv_sel).await;
    assert_eq!(op, OPCODE_RESULT);
    let (kind, res) = parse_result(&body);
    assert_eq!(kind, RESULT_ROWS);
    let res = res.unwrap();
    assert_eq!(res.names, vec!["name"]);
    assert_eq!(res.rows[0][0], Some(b"carol".to_vec()));

    let (op, body) = c
        .raw_request(
            0x07,
            &query_body(
                "INSERT INTO t (id, name) VALUES (?, ?)",
                &[wire_value(&4i32.to_be_bytes())],
            ),
        )
        .await;
    assert_eq!(op, OPCODE_ERROR);
    let mut cur = &body[..];
    assert_eq!(read_i32(&mut cur), 0x2200);

    let _ = std::fs::remove_dir_all(&dir);
}
