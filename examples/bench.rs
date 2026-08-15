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
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const CLIENTS: usize = 16;
const OPS_PER_CLIENT: usize = 2500;

async fn raw_query(stream: &mut TcpStream, text: &str) {
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
    assert_eq!(header[4], 0x08, "query failed: {}", String::from_utf8_lossy(&body));
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
    assert_eq!(header[4], 0x02);
    s
}

#[tokio::main]
async fn main() {
    let dir = std::env::temp_dir().join(format!("rhydadb-bench-{}", std::process::id()));
    let storage = Arc::new(Storage::open(&dir).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_storage = storage.clone();
    tokio::spawn(async move {
        server::run(listener, server_storage).await.unwrap();
    });

    let mut setup = connect(addr).await;
    raw_query(&mut setup, "CREATE KEYSPACE bench WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}").await;
    raw_query(&mut setup, "CREATE TABLE bench.items (id int PRIMARY KEY, val int)").await;
    drop(setup);

    // --- INSERT phase ---
    let clients: Vec<TcpStream> = {
        let mut v = Vec::new();
        for _ in 0..CLIENTS {
            v.push(connect(addr).await);
        }
        v
    };

    let t = Instant::now();
    let mut handles = Vec::new();
    for (c, stream) in clients.into_iter().enumerate() {
        handles.push(tokio::spawn(async move {
            let mut s = stream;
            let base = c * OPS_PER_CLIENT;
            for i in 0..OPS_PER_CLIENT {
                raw_query(&mut s, &format!("INSERT INTO bench.items (id, val) VALUES ({}, {})", base + i, i)).await;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let insert_elapsed = t.elapsed();
    let total = (CLIENTS * OPS_PER_CLIENT) as u64;
    println!(
        "INSERT: {} ops in {:?} -> {:.0} RPS ({} clients)",
        total,
        insert_elapsed,
        total as f64 / insert_elapsed.as_secs_f64(),
        CLIENTS
    );

    // --- SELECT by PK phase ---
    let clients: Vec<TcpStream> = {
        let mut v = Vec::new();
        for _ in 0..CLIENTS {
            v.push(connect(addr).await);
        }
        v
    };

    let t = Instant::now();
    let mut handles = Vec::new();
    for (c, stream) in clients.into_iter().enumerate() {
        handles.push(tokio::spawn(async move {
            let mut s = stream;
            let base = c * OPS_PER_CLIENT;
            for i in 0..OPS_PER_CLIENT {
                raw_query(&mut s, &format!("SELECT val FROM bench.items WHERE id = {}", base + i)).await;
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let select_elapsed = t.elapsed();
    println!(
        "SELECT: {} ops in {:?} -> {:.0} RPS ({} clients)",
        total,
        select_elapsed,
        total as f64 / select_elapsed.as_secs_f64(),
        CLIENTS
    );

    let _ = std::fs::remove_dir_all(&dir);
}