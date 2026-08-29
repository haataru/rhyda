//! Honest benchmark: separates parser/network from storage.
//! - `storage` : direct `Storage::get_row` / `put_row` + `settle`, no TCP, no CQL parsing, no `Value::write_wire` for result
//! - `cql`     : full stack via `bench_async` TCP+prepared (for comparison)
//! Reports throughput + p50/p99 latency via hdrhistogram, not just ops/s.

use hdrhistogram::Histogram;
use rhydadb::storage::{Storage, encode_row_columns};
use rhydadb::cql_value::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_KEYS: u64 = 1_000_000;
const DEFAULT_FILL: u64 = 200_000;

fn parse_args() -> (String, u64, u64, usize, u64) {
    let mut data = std::env::var("RHYDADB_DATA").unwrap_or_else(|_| "C:\\temp\\rhydadb-bench-honest".to_string());
    let mut keys = DEFAULT_KEYS;
    let mut fill = DEFAULT_FILL;
    let mut threads: usize = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let mut secs = 10u64;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--data" => data = it.next().unwrap_or(data),
            "--keys" => keys = it.next().unwrap_or_else(|| keys.to_string()).parse().unwrap_or(keys),
            "--fill" => fill = it.next().unwrap_or_else(|| fill.to_string()).parse().unwrap_or(fill),
            "--threads" => threads = it.next().unwrap_or_else(|| threads.to_string()).parse().unwrap_or(threads),
            "--seconds" => secs = it.next().unwrap_or_else(|| secs.to_string()).parse().unwrap_or(secs),
            _ => {}
        }
    }
    (data, keys, fill, threads, secs)
}

fn bench_storage_write(storage: Arc<Storage>, keys: u64, fill: u64, threads: usize, secs: u64) {
    // prepare keyspace/table via Storage API (bypass CQL)
    // We reuse the same storage that already has system tables seeded.
    // Create bench keyspace/table if not exists via CQL for simplicity (uses same Storage)
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        // create via direct Storage put_keyspace/put_table is easier than CQL, but we can just use CQL via storage's query layer?
        // For honest storage bench we will just use put_row/get_row directly on a pre-created table
        // Ensure table exists: create via `cargo run` style is not needed here if we use Storage API directly
        // We'll create TableDef manually if missing
        use rhydadb::schema::{ColumnDef, ColumnType, TableDef, KeyspaceDef};
        if storage.get_keyspace("bench").unwrap().is_none() {
            let _ = storage.put_keyspace(&KeyspaceDef { name: "bench".to_string(), replication: vec![] });
        }
        let tdef = TableDef {
            keyspace: "bench".to_string(),
            name: "kv".to_string(),
            columns: vec![
                ColumnDef { name: "id".to_string(), col_type: ColumnType::BigInt, is_partition_key: true, is_clustering: false },
                ColumnDef { name: "val".to_string(), col_type: ColumnType::Text, is_partition_key: false, is_clustering: false },
            ],
            partition_keys: vec!["id".to_string()],
            clustering_keys: vec![],
        };
        if storage.get_table("bench", "kv").unwrap().is_none() {
            let _ = storage.put_table(&tdef);
        }
    });

    let val = Value::Text("x".repeat(64));
    let row_enc = encode_row_columns(&[(1u16, val.clone())]);

    // fill
    {
        let mut tickets = Vec::new();
        for k in 1..=std::cmp::min(fill, keys) {
            let kp = vec![k.to_be_bytes().to_vec()];
            let t = storage.put_row("bench", "kv", &kp, &row_enc).unwrap();
            tickets.push(t);
            if tickets.len() >= 1024 {
                rt.block_on(storage.settle(&mut tickets));
            }
        }
        rt.block_on(storage.settle(&mut tickets));
    }
    // wait for compaction debt to drain a bit
    std::thread::sleep(Duration::from_millis(500));

    let mut handles = Vec::new();
    let start = Instant::now();
    let end = start + Duration::from_secs(secs);
    let ops = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hist = Arc::new(std::sync::Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).unwrap()));

    for tid in 0..threads {
        let storage = storage.clone();
        let ops = ops.clone();
        let hist = hist.clone();
        let row_enc = row_enc.clone();
        handles.push(std::thread::spawn(move || {
            let mut rng = 0x9E3779B97F4A7C15u64 ^ (tid as u64 * 0x9E3779B97F4A7C15);
            let mut local_hist = Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).unwrap();
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let mut tickets = Vec::new();
            while Instant::now() < end {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let k = 1 + rng % keys;
                let kp = vec![k.to_be_bytes().to_vec()];
                let t0 = Instant::now();
                let tk = storage.put_row("bench", "kv", &kp, &row_enc).unwrap();
                tickets.push(tk);
                if tickets.len() >= 64 {
                    rt.block_on(storage.settle(&mut tickets));
                    let dt = t0.elapsed().as_nanos() as u64;
                    local_hist.record(dt).unwrap();
                    ops.fetch_add(64, std::sync::atomic::Ordering::Relaxed);
                    tickets.clear();
                }
            }
            // drain
            if !tickets.is_empty() {
                let n = tickets.len() as u64;
                rt.block_on(storage.settle(&mut tickets));
                ops.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            }
            let mut g = hist.lock().unwrap();
            g.add(&local_hist).unwrap();
        }));
    }
    for h in handles { let _ = h.join(); }
    let elapsed = start.elapsed().as_secs_f64();
    let total = ops.load(std::sync::atomic::Ordering::Relaxed);
    let h = hist.lock().unwrap();
    println!("storage_write threads={} keys={} fill={} elapsed={:.2}s throughput={:.0} ops/s p50={}us p99={}us max={}us",
        threads, keys, fill, elapsed, total as f64 / elapsed,
        h.value_at_quantile(0.5)/1000, h.value_at_quantile(0.99)/1000, h.max()/1000);
}

fn bench_storage_read(storage: Arc<Storage>, keys: u64, fill: u64, threads: usize, secs: u64) {
    let mut handles = Vec::new();
    let start = Instant::now();
    let end = start + Duration::from_secs(secs);
    let ops = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hist = Arc::new(std::sync::Mutex::new(Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).unwrap()));
    for tid in 0..threads {
        let storage = storage.clone();
        let ops = ops.clone();
        let hist = hist.clone();
        handles.push(std::thread::spawn(move || {
            let mut rng = 0x2545F4914F6CDD1Du64 ^ (tid as u64 * 0x9E3779B97F4A7C15);
            let mut local_hist = Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).unwrap();
            let mut local_ops = 0u64;
            while Instant::now() < end {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let k = 1 + rng % std::cmp::min(keys, fill);
                let kp = vec![k.to_be_bytes().to_vec()];
                let t0 = Instant::now();
                let _ = storage.get_row("bench", "kv", &kp).unwrap();
                let dt = t0.elapsed().as_nanos() as u64;
                local_hist.record(dt).unwrap();
                local_ops += 1;
                if local_ops % 1024 == 0 {
                    ops.fetch_add(1024, std::sync::atomic::Ordering::Relaxed);
                    local_ops = 0;
                }
            }
            ops.fetch_add(local_ops, std::sync::atomic::Ordering::Relaxed);
            let mut g = hist.lock().unwrap();
            g.add(&local_hist).unwrap();
        }));
    }
    for h in handles { let _ = h.join(); }
    let elapsed = start.elapsed().as_secs_f64();
    let total = ops.load(std::sync::atomic::Ordering::Relaxed);
    let h = hist.lock().unwrap();
    println!("storage_read threads={} keys={} elapsed={:.2}s throughput={:.0} ops/s p50={}us p99={}us max={}us",
        threads, keys, elapsed, total as f64 / elapsed,
        h.value_at_quantile(0.5)/1000, h.value_at_quantile(0.99)/1000, h.max()/1000);
}

fn main() -> anyhow::Result<()> {
    let (data, keys, fill, threads, secs) = parse_args();
    println!("bench_honest data={} keys={} fill={} threads={} seconds={}", data, keys, fill, threads, secs);
    // clean previous bench data if requested via env
    let storage = Arc::new(Storage::open(&data)?);
    // honest storage bench (no TCP, no parser)
    println!("--- storage (no network/parser) ---");
    bench_storage_write(storage.clone(), keys, fill, threads, secs);
    bench_storage_read(storage.clone(), keys, fill, threads, secs);
    // For CQL honest numbers, run bench_async separately and compare; we print hint
    println!("--- for CQL honest, run: bench_async --mode read/write --conns 64 --pipeline 128 (includes parser+network) ---");
    println!("--- cassandra-stress (if installed): cassandra-stress write n=100000 -mode native cql3 protocolVersion=4 -rate threads=32 -node 127.0.0.1 ---");
    Ok(())
}
