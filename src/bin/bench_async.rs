//! High-ceiling benchmark client for RhydaDB.
//!
//! Unlike the classic `bench` (OS-thread per connection), this client is a
//! single-process tokio application: hundreds of cheap async connections,
//! coalesced frame writes, reused read buffers. It exists to measure the
//! SERVER ceiling rather than the client's syscall/scheduling limits.

use bytes::BytesMut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, tcp::OwnedReadHalf};

const OPCODE_ERROR: u8 = 0x00;
const OPCODE_READY: u8 = 0x02;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Read,
    Write,
    Mixed,
}

struct Args {
    addr: String,
    conns: usize,
    pipeline: usize,
    seconds: u64,
    mode: Mode,
    keys: u64,
    fill_keys: u64,
    value_len: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        addr: "127.0.0.1:9042".into(),
        conns: 128,
        pipeline: 256,
        seconds: 10,
        mode: Mode::Read,
        keys: 1_000_000,
        fill_keys: 200_000,
        value_len: 64,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_else(|| panic!("missing value for {k}"));
        match k.as_str() {
            "--addr" => a.addr = v,
            "--conns" => a.conns = v.parse().unwrap(),
            "--pipeline" => a.pipeline = v.parse().unwrap(),
            "--seconds" => a.seconds = v.parse().unwrap(),
            "--mode" => {
                a.mode = match v.as_str() {
                    "read" => Mode::Read,
                    "write" => Mode::Write,
                    "mixed" => Mode::Mixed,
                    _ => panic!("unknown mode {v}"),
                }
            }
            "--keys" => a.keys = v.parse().unwrap(),
            "--fill-keys" => a.fill_keys = v.parse().unwrap(),
            "--value-len" => a.value_len = v.parse().unwrap(),
            other => panic!("unknown arg {other}"),
        }
    }
    a
}

fn frame(stream: i16, opcode: u8, body: &[u8], out: &mut BytesMut) {
    out.reserve(9 + body.len());
    out.extend_from_slice(&[0x04, 0]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(&[opcode]);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
}

fn long_string(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(s.len() as i32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

async fn connect(addr: &str) -> anyhow::Result<(BufReader<OwnedReadHalf>, tokio::net::tcp::OwnedWriteHalf)> {
    let sock = TcpStream::connect(addr).await?;
    sock.set_nodelay(true)?;
    let (r, w) = sock.into_split();
    Ok((BufReader::with_capacity(128 * 1024, r), w))
}

/// Reads one response frame; returns opcode, body placed into scratch.
async fn read_response(
    reader: &mut BufReader<OwnedReadHalf>,
    scratch: &mut Vec<u8>,
) -> anyhow::Result<u8> {
    let mut hdr = [0u8; 9];
    reader.read_exact(&mut hdr).await?;
    let opcode = hdr[4];
    let len = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
    scratch.clear();
    scratch.resize(len, 0);
    reader.read_exact(scratch).await?;
    Ok(opcode)
}

async fn roundtrip_query(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    write_buf: &mut BytesMut,
    scratch: &mut Vec<u8>,
    cql: &str,
    allow_error: bool,
) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    long_string(cql, &mut body);
    body.extend_from_slice(&0x0001u16.to_be_bytes());
    body.push(0x00);
    frame(0, 0x07, &body, write_buf);
    writer.write_all(write_buf).await?;
    write_buf.clear();
    let op = read_response(reader, scratch).await?;
    if op == OPCODE_ERROR && !allow_error {
        anyhow::bail!(
            "query error: {}",
            String::from_utf8_lossy(scratch)
        );
    }
    Ok(scratch.clone())
}

async fn prepare_stmt(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    write_buf: &mut BytesMut,
    scratch: &mut Vec<u8>,
    cql: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    long_string(cql, &mut body);
    frame(1, 0x09, &body, write_buf);
    writer.write_all(write_buf).await?;
    write_buf.clear();
    let op = read_response(reader, scratch).await?;
    if op == OPCODE_ERROR {
        anyhow::bail!("prepare error: {}", String::from_utf8_lossy(scratch));
    }
    let kind = i32::from_be_bytes(scratch[0..4].try_into().unwrap());
    assert_eq!(kind, 4, "expected PREPARED result");
    let idlen = u16::from_be_bytes(scratch[4..6].try_into().unwrap()) as usize;
    Ok(scratch[6..6 + idlen].to_vec())
}

struct ConnCtx {
    insert_id: Vec<u8>,
    select_id: Vec<u8>,
    rng: u64,
    value: Vec<u8>,
    mode: Mode,
    keys: u64,
    filled: u64,
    next_stream: i16,
}

impl ConnCtx {
    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Builds one pipelined EXECUTE frame into `out`; returns true if INSERT.
    fn build_op(&mut self, i_in_batch: usize, out: &mut BytesMut) -> bool {
        let k = match self.mode {
            Mode::Write => 1 + self.rand() % self.keys,
            _ => 1 + self.rand() % self.filled.max(1),
        };
        let kbe = k.to_be_bytes();
        let is_ins = match self.mode {
            Mode::Write => true,
            Mode::Read => false,
            Mode::Mixed => i_in_batch % 2 == 0,
        };
        let (id, flags): (&[u8], u8) = if is_ins {
            (&self.insert_id, 0x01)
        } else {
            (&self.select_id, 0x01 | 0x02)
        };
        let mut body = BytesMut::with_capacity(96);
        body.extend_from_slice(&(id.len() as u16).to_be_bytes());
        body.extend_from_slice(id);
        body.extend_from_slice(&0x0001u16.to_be_bytes());
        body.extend_from_slice(&[flags]);
        let n_values: u16 = if is_ins { 2 } else { 1 };
        body.extend_from_slice(&n_values.to_be_bytes());
        if is_ins {
            body.extend_from_slice(&8i32.to_be_bytes());
            body.extend_from_slice(&kbe);
            body.extend_from_slice(&(self.value.len() as i32).to_be_bytes());
            body.extend_from_slice(&self.value);
        } else {
            body.extend_from_slice(&8i32.to_be_bytes());
            body.extend_from_slice(&kbe);
        }
        let s = self.next_stream;
        self.next_stream = self.next_stream.wrapping_add(1) & 0x7FFF;
        frame(s, 0x0A, &body, out);
        is_ins
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Arc::new(parse_args());
    eprintln!(
        "bench-async: addr={} conns={} pipeline={} seconds={} mode={:?}",
        args.addr, args.conns, args.pipeline, args.seconds, args.mode
    );

    // ---- setup on a dedicated connection ----
    let (mut setup_r, mut setup_w) = connect(&args.addr).await?;
    {
        let mut wb = BytesMut::new();
        let mut scratch = Vec::new();
        // STARTUP inline
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&11u16.to_be_bytes());
        body.extend_from_slice(b"CQL_VERSION");
        body.extend_from_slice(&5u16.to_be_bytes());
        body.extend_from_slice(b"3.0.0");
        frame(0, 0x01, &body, &mut wb);
        setup_w.write_all(&wb).await?;
        wb.clear();
        let op = read_response(&mut setup_r, &mut scratch).await?;
        assert_eq!(op, OPCODE_READY);

        roundtrip_query(&mut setup_r, &mut setup_w, &mut wb, &mut scratch, "DROP KEYSPACE bench", true).await?;
        roundtrip_query(
            &mut setup_r,
            &mut setup_w,
            &mut wb,
            &mut scratch,
            "CREATE KEYSPACE bench WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
            false,
        )
        .await?;
        roundtrip_query(
            &mut setup_r,
            &mut setup_w,
            &mut wb,
            &mut scratch,
            "CREATE TABLE bench.kv (id bigint PRIMARY KEY, val text)",
            false,
        )
        .await?;
        eprintln!("schema ready");

        if args.mode != Mode::Write && args.fill_keys > 0 {
            let t = Instant::now();
            let ins_id =
                prepare_stmt(&mut setup_r, &mut setup_w, &mut wb, &mut scratch, "INSERT INTO bench.kv (id, val) VALUES (?, ?)").await?;
            let mut ctx = ConnCtx {
                insert_id: ins_id,
                select_id: Vec::new(),
                rng: 0x9E3779B97F4A7C15,
                value: vec![b'x'; args.value_len],
                mode: Mode::Write,
                keys: args.fill_keys.max(1),
                filled: 0,
                next_stream: 0,
            };
            let mut pending = 0usize;
            let mut key: u64 = 1;
            while key <= args.fill_keys {
                let mut out = BytesMut::with_capacity(256 * 1024);
                while pending < args.pipeline && key <= args.fill_keys {
                    ctx.build_op(pending, &mut out);
                    pending += 1;
                    key += 1;
                    if out.len() > 1024 * 1024 {
                        break;
                    }
                }
                setup_w.write_all(&out).await?;
                for _ in 0..pending {
                    let op = read_response(&mut setup_r, &mut scratch).await?;
                    if op == OPCODE_ERROR {
                        anyhow::bail!("fill error: {}", String::from_utf8_lossy(&scratch));
                    }
                }
                pending = 0;
            }
            eprintln!("filled {} keys in {:.2}s", args.fill_keys, t.elapsed().as_secs_f64());
        }
    }

    // ---- measurement phase ----
    let running = Arc::new(AtomicBool::new(true));
    let total_ops = Arc::new(AtomicU64::new(0));
    let total_err = Arc::new(AtomicU64::new(0));
    let first_err: Arc<tokio::sync::Mutex<Option<String>>> = Default::default();
    let barrier = Arc::new(tokio::sync::Barrier::new(args.conns));

    let mut handles = Vec::with_capacity(args.conns);
    for cid in 0..args.conns {
        let args = args.clone();
        let running = running.clone();
        let ops = total_ops.clone();
        let errs = total_err.clone();
        let ferr = first_err.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let (mut reader, mut writer) = connect(&args.addr).await?;
            let mut wb = BytesMut::with_capacity(512 * 1024);
            let mut scratch = Vec::with_capacity(64 * 1024);
            let ins_id = prepare_stmt(&mut reader, &mut writer, &mut wb, &mut scratch, "INSERT INTO bench.kv (id, val) VALUES (?, ?)").await?;
            let sel_id = prepare_stmt(&mut reader, &mut writer, &mut wb, &mut scratch, "SELECT * FROM bench.kv WHERE id = ?").await?;
            let mut ctx = ConnCtx {
                insert_id: ins_id,
                select_id: sel_id,
                rng: 0x2545F4914F6CDD1D ^ ((cid as u64 + 7) * 0x9E3779B97F4A7C15),
                value: vec![b'v'; args.value_len],
                mode: args.mode,
                keys: args.keys,
                filled: args.fill_keys.min(args.keys).max(1),
                next_stream: 0,
            };
            barrier.wait().await;
            let n = args.pipeline;
            while running.load(Ordering::Relaxed) {
                wb.clear();
                for i in 0..n {
                    ctx.build_op(i, &mut wb);
                }
                writer.write_all(&wb).await?;
                for _ in 0..n {
                    let op = read_response(&mut reader, &mut scratch).await?;
                    if op == OPCODE_ERROR {
                        errs.fetch_add(1, Ordering::Relaxed);
                        let mut fe = ferr.lock().await;
                        if fe.is_none() {
                            *fe = Some(String::from_utf8_lossy(&scratch).to_string());
                        }
                    }
                }
                ops.fetch_add(n as u64, Ordering::Relaxed);
            }
            Ok::<(), anyhow::Error>(())
        }));
    }

    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(args.seconds)).await;
    running.store(false, Ordering::Relaxed);

    let mut failures = Vec::new();
    for h in handles {
        if let Err(e) = h.await.unwrap() {
            failures.push(format!("{e}"));
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let ops = total_ops.load(Ordering::Relaxed);
    println!(
        "ops={} elapsed={:.2}s throughput={:.0} ops/s errors={}",
        ops,
        elapsed,
        ops as f64 / elapsed,
        total_err.load(Ordering::Relaxed)
    );
    if let Some(e) = first_err.lock().await.take() {
        println!("first error: {e}");
    }
    for f in failures.iter().take(3) {
        println!("conn failure: {f}");
    }
    Ok(())
}
