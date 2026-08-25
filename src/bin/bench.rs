use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const OPCODE_ERROR: u8 = 0x00;
const OPCODE_READY: u8 = 0x02;
const OPCODE_RESULT: u8 = 0x08;

#[derive(Clone)]
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

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Read,
    Write,
    Mixed,
}

fn parse_args() -> Args {
    let mut a = Args {
        addr: "127.0.0.1:9042".into(),
        conns: 8,
        pipeline: 128,
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

fn frame(stream: i16, opcode: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(0x04);
    out.push(0);
    out.extend_from_slice(&stream.to_be_bytes());
    out.push(opcode);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
}

fn long_string(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&(s.len() as i32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn startup_body(out: &mut Vec<u8>) {
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(11u16.to_be_bytes()));
    out.extend_from_slice(b"CQL_VERSION");
    out.extend_from_slice(&(5u16.to_be_bytes()));
    out.extend_from_slice(b"3.0.0");
}

fn query_params_with_values(values: &[&[u8]], skip_meta: bool, out: &mut Vec<u8>) {
    out.extend_from_slice(&0x0001u16.to_be_bytes());
    let mut flags = 0x01;
    if skip_meta {
        flags |= 0x02;
    }
    out.push(flags);
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for v in values {
        out.extend_from_slice(&(v.len() as i32).to_be_bytes());
        out.extend_from_slice(v);
    }
}

struct Conn {
    reader: BufReader<TcpStream>,
    write_buf: Vec<u8>,
    next_stream: i16,
}

impl Conn {
    fn connect(addr: &str) -> anyhow::Result<Self> {
        let sock = TcpStream::connect(addr)?;
        sock.set_nodelay(true)?;
        let mut c = Conn {
            reader: BufReader::with_capacity(256 * 1024, sock.try_clone()?),
            write_buf: Vec::with_capacity(512 * 1024),
            next_stream: 0,
        };
        let mut w = sock;
        let mut body = Vec::new();
        startup_body(&mut body);
        frame(0, 0x01, &body, &mut c.write_buf);
        w.write_all(&c.write_buf)?;
        c.write_buf.clear();
        let (op, _payload) = c.read_response()?;
        assert_eq!(op, OPCODE_READY, "expected READY");
        Ok(c)
    }

    fn read_response(&mut self) -> anyhow::Result<(u8, Vec<u8>)> {
        let mut hdr = [0u8; 9];
        self.reader.read_exact(&mut hdr)?;
        let opcode = hdr[4];
        let len = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body)?;
        Ok((opcode, body))
    }

    fn roundtrip_query(&mut self, cql: &str) -> anyhow::Result<Vec<u8>> {
        let mut body = Vec::new();
        long_string(cql, &mut body);
        body.extend_from_slice(&0x0001u16.to_be_bytes());
        body.push(0x00);
        let mut buf = Vec::new();
        frame(self.next_stream, 0x07, &body, &mut buf);
        self.next_stream = self.next_stream.wrapping_add(1);
        let mut sock = self.reader.get_ref().try_clone()?;
        sock.write_all(&buf)?;
        let (op, payload) = self.read_response()?;
        if op == OPCODE_ERROR {
            anyhow::bail!("query error: {}", String::from_utf8_lossy(&payload));
        }
        Ok(payload)
    }

    fn prepare(&mut self, cql: &str) -> anyhow::Result<Vec<u8>> {
        let mut body = Vec::new();
        long_string(cql, &mut body);
        let mut buf = Vec::new();
        frame(self.next_stream, 0x09, &body, &mut buf);
        self.next_stream = self.next_stream.wrapping_add(1);
        let mut sock = self.reader.get_ref().try_clone()?;
        sock.write_all(&buf)?;
        let (op, payload) = self.read_response()?;
        if op == OPCODE_ERROR {
            anyhow::bail!("prepare error: {}", String::from_utf8_lossy(&payload));
        }
        let kind = i32::from_be_bytes(payload[0..4].try_into().unwrap());
        assert_eq!(kind, 4, "expected PREPARED result");
        let idlen = u16::from_be_bytes(payload[4..6].try_into().unwrap()) as usize;
        Ok(payload[6..6 + idlen].to_vec())
    }
}

struct Workload {
    insert_id: Vec<u8>,
    select_id: Vec<u8>,
    rng: u64,
    value: Vec<u8>,
    mode: Mode,
    keys: u64,
    filled: u64,
}

impl Workload {
    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn key(&mut self) -> u64 {
        match self.mode {
            Mode::Write => 1 + self.rand() % self.keys,
            _ => 1 + self.rand() % self.filled.max(1),
        }
    }

    fn execute_frame(&mut self, is_insert: bool, out: &mut Vec<u8>, stream: i16) {
        let k = self.key();
        let kbe = k.to_be_bytes();
        let mut body = Vec::with_capacity(128);
        let id = if is_insert {
            &self.insert_id
        } else {
            &self.select_id
        };
        body.extend_from_slice(&(id.len() as u16).to_be_bytes());
        body.extend_from_slice(id);
        if is_insert {
            query_params_with_values(&[&kbe, &self.value], false, &mut body);
        } else {
            query_params_with_values(&[&kbe], true, &mut body);
        }
        frame(stream, 0x0A, &body, out);
    }
}

fn main() -> anyhow::Result<()> {
    let args = Arc::new(parse_args());
    eprintln!(
        "bench: addr={} conns={} pipeline={} seconds={} mode={:?} keys={}",
        args.addr, args.conns, args.pipeline, args.seconds, args.mode, args.keys
    );

    // setup connection: schema + fill
    let mut setup = Conn::connect(&args.addr)?;
    let _ = setup.roundtrip_query("DROP KEYSPACE bench");
    setup.roundtrip_query(
        "CREATE KEYSPACE bench WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
    )?;
    setup.roundtrip_query("CREATE TABLE bench.kv (id bigint PRIMARY KEY, val text)")?;
    eprintln!("schema ready");

    if args.mode != Mode::Write && args.fill_keys > 0 {
        let t = Instant::now();
        let ins_id = setup.prepare("INSERT INTO bench.kv (id, val) VALUES (?, ?)")?;
        let value = vec![b'x'; args.value_len];
        let mut pending = 0usize;
        let mut key: u64 = 1;
        let mut wl = Workload {
            insert_id: ins_id.clone(),
            select_id: Vec::new(),
            rng: 0x9E3779B97F4A7C15,
            value: value.clone(),
            mode: Mode::Write,
            keys: args.fill_keys.max(1),
            filled: 0,
        };
        let mut out = Vec::with_capacity(1024 * 1024);
        let mut sock = setup.reader.get_ref().try_clone()?;
        let mut next_stream: i16 = 0;
        while key <= args.fill_keys {
            while pending < args.pipeline && key <= args.fill_keys {
                let s = next_stream;
                next_stream = next_stream.wrapping_add(1) & 0x7FFF;
                wl.execute_frame(true, &mut out, s);
                pending += 1;
                key += 1;
            }
            sock.write_all(&out)?;
            out.clear();
            for _ in 0..pending {
                let (op, payload) = setup.read_response()?;
                if op == OPCODE_ERROR {
                    anyhow::bail!("fill error: {}", String::from_utf8_lossy(&payload));
                }
            }
            pending = 0;
        }
        eprintln!(
            "filled {} keys in {:.2}s",
            args.fill_keys,
            t.elapsed().as_secs_f64()
        );
    }

    let running = Arc::new(AtomicBool::new(true));
    let total_ops = Arc::new(AtomicU64::new(0));
    let total_err = Arc::new(AtomicU64::new(0));
    let first_err: Arc<std::sync::Mutex<Option<String>>> = Default::default();

    let mut handles = Vec::new();
    let barrier = Arc::new(std::sync::Barrier::new(args.conns));
    for cid in 0..args.conns {
        let args = args.clone();
        let running = running.clone();
        let total_ops = total_ops.clone();
        let total_err = total_err.clone();
        let first_err = first_err.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || -> anyhow::Result<()> {
            let mut c = Conn::connect(&args.addr)?;
            let ins_id = c.prepare("INSERT INTO bench.kv (id, val) VALUES (?, ?)")?;
            let sel_id = c.prepare("SELECT * FROM bench.kv WHERE id = ?")?;
            let mut wl = Workload {
                insert_id: ins_id,
                select_id: sel_id,
                rng: 0x2545F4914F6CDD1D ^ ((cid as u64 + 1) * 0x9E3779B97F4A7C15),
                value: vec![b'v'; args.value_len],
                mode: args.mode,
                keys: args.keys,
                filled: args.fill_keys.min(args.keys),
            };
            let mut sock = c.reader.get_ref().try_clone()?;
            let mut out = Vec::with_capacity(1024 * 1024);
            barrier.wait();
            while running.load(Ordering::Relaxed) {
                let n = args.pipeline;
                let mut streams = Vec::with_capacity(n);
                for i in 0..n {
                    let s = (c.next_stream) as i16;
                    c.next_stream = c.next_stream.wrapping_add(1);
                    let is_ins = match args.mode {
                        Mode::Write => true,
                        Mode::Read => false,
                        Mode::Mixed => i % 2 == 0,
                    };
                    wl.execute_frame(is_ins, &mut out, s);
                    streams.push(s);
                }
                sock.write_all(&out)?;
                out.clear();
                for _ in 0..n {
                    let (op, payload) = c.read_response()?;
                    if op == OPCODE_ERROR {
                        total_err.fetch_add(1, Ordering::Relaxed);
                        let mut fe = first_err.lock().unwrap();
                        if fe.is_none() {
                            *fe = Some(String::from_utf8_lossy(&payload).to_string());
                        }
                    }
                }
                total_ops.fetch_add(n as u64, Ordering::Relaxed);
            }
            Ok(())
        }));
    }

    let dur = Duration::from_secs(args.seconds);
    let t0 = Instant::now();
    std::thread::sleep(dur);
    running.store(false, Ordering::Relaxed);

    let mut errs: Vec<String> = Vec::new();
    for h in handles {
        if let Err(e) = h.join().unwrap() {
            errs.push(format!("{e}"));
        }
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let ops = total_ops.load(Ordering::Relaxed);
    let nerr = total_err.load(Ordering::Relaxed);
    println!(
        "ops={} elapsed={:.2}s throughput={:.0} ops/s errors={}",
        ops, elapsed, ops as f64 / elapsed, nerr
    );
    if let Some(e) = first_err.lock().unwrap().take() {
        println!("first error: {e}");
    }
    for e in errs {
        println!("conn failure: {e}");
    }
    Ok(())
}
