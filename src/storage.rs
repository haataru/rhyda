use crate::cql_value::Value;
use crate::schema::{ColumnDef, ColumnType, KeyspaceDef, TableDef};
use anyhow::Result;
use cql3_parser::cassandra_statement::CassandraStatement;
use rocksdb::{DB, DBCompressionType, Direction, IteratorMode, Options, WriteBatch, WriteOptions};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

const KS_PREFIX: &[u8] = b"ks!";
const TB_PREFIX: &[u8] = b"tb!";
const DT_PREFIX: &[u8] = b"dt!";

const AST_CACHE_CAPACITY: usize = 8192;

const MAX_BATCH_OPS: usize = 1024;

const WAL_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

type DoneTx = tokio::sync::oneshot::Sender<()>;
type DoneSignal = tokio::sync::oneshot::Receiver<()>;

enum WriteOp {
    Put {
        key: Vec<u8>,
        val: Vec<u8>,
        done: Option<DoneTx>,
    },
    Delete {
        key: Vec<u8>,
        done: Option<DoneTx>,
    },
    /// Range tombstone routed through the pipeline so it can never overtake
    /// row ops queued for the same engine (DROP/TRUNCATE safety).
    DeleteRange {
        start: Vec<u8>,
        end: Vec<u8>,
        done: Option<DoneTx>,
    },
}

/// Handle returned to the caller for every asynchronous data write. The
/// server awaits it before acknowledging the request, preserving strict
/// read-your-writes without blocking any worker thread. Completion is
/// signalled individually per op (the committer drops the sender), so one
/// commit wakes exactly the tasks waiting on it — no broadcast herds.
pub struct WriteTicket(Option<DoneSignal>);

struct EngineWriter {
    tx: Sender<WriteOp>,
    handles: Mutex<Vec<thread::JoinHandle<()>>>,
}

pub struct Storage {
    /// Independent RocksDB instances. Index 0 holds metadata (`ks!`/`tb!`)
    /// plus data when only one engine is configured; data is distributed
    /// across all of them by partition hash.
    dbs: Vec<Arc<DB>>,
    keyspaces: RwLock<HashMap<String, KeyspaceDef>>,
    tables: RwLock<HashMap<(String, String), Arc<TableDef>>>,
    ast_cache: Mutex<HashMap<String, Arc<CassandraStatement>>>,
    writers: Vec<EngineWriter>,
    write_sync: bool,
    manual_wal: bool,
    wal_ctl: Option<Sender<()>>,
    wal_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Storage {
    fn drop(&mut self) {
        // Signal background threads and wait for them so every engine lock
        // file is released and every accepted write is committed before the
        // directory can be reused.
        drop(self.wal_ctl.take());
        if let Some(handle) = self.wal_thread.take() {
            let _ = handle.join();
        }
        // Move writers out first so every Sender handle drops: collectors
        // observe the disconnected channel only then, finish their batches,
        // and let the committers publish the final watermark and exit.
        let writers: Vec<EngineWriter> = std::mem::take(&mut self.writers);
        let mut joins = Vec::new();
        for w in writers {
            drop(w.tx);
            if let Ok(mut hs) = w.handles.lock() {
                joins.extend(hs.drain(..));
            }
        }
        for h in joins {
            let _ = h.join();
        }
        self.dbs.clear();
    }
}

fn batch_apply(batch: &mut WriteBatch, op: WriteOp, done: &mut Vec<Option<DoneTx>>) {
    match op {
        WriteOp::Put { key, val, done: d } => {
            batch.put(&key, &val);
            done.push(d);
        }
        WriteOp::Delete { key, done: d } => {
            batch.delete(&key);
            done.push(d);
        }
        WriteOp::DeleteRange { start, end, done: d } => {
            batch.delete_range(&start, &end);
            done.push(d);
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn env_num(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let write_sync = std::env::var("RHYDADB_SYNC").is_ok();
        let cache_mb = env_num("RHYDADB_CACHE_MB", 1024, 8, 1 << 20);
        let memtable_mb = env_num("RHYDADB_MEMTABLE_MB", 128, 8, 4096);

        // The engine shard count is persisted on first open so restarts never
        // misroute rows written under a different layout.
        let root = path.as_ref().to_path_buf();
        let marker = root.join("ENGINE-SHARDS");
        let shard_count: usize = match std::fs::read_to_string(&marker) {
            Ok(text) => text.trim().parse().unwrap_or(1),
            Err(_) => {
                let n = env_num(
                    "RHYDADB_ENGINE_SHARDS",
                    cpus.clamp(2, 8).min(8),
                    1,
                    64,
                );
                std::fs::create_dir_all(&root)?;
                std::fs::write(&marker, n.to_string())?;
                n
            }
        };

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.increase_parallelism(cpus as i32);
        opts.set_max_background_jobs((cpus / shard_count.max(1)).clamp(2, 8) as i32);
        opts.optimize_level_style_compaction(memtable_mb * 1024 * 1024 * 2);
        opts.set_write_buffer_size(memtable_mb * 1024 * 1024);
        opts.set_max_write_buffer_number(6);
        opts.set_min_write_buffer_number_to_merge(2);
        // Let concurrent writers insert into the memtable in parallel instead
        // of serializing every write on one critical section.
        opts.set_allow_concurrent_memtable_write(true);
        opts.set_enable_write_thread_adaptive_yield(true);

        let mut bb_opts = rocksdb::BlockBasedOptions::default();
        let cache = rocksdb::Cache::new_lru_cache(cache_mb * 1024 * 1024);
        bb_opts.set_block_cache(&cache);
        bb_opts.set_block_size(16 * 1024);
        bb_opts.set_bloom_filter(10.0, true);
        bb_opts.set_cache_index_and_filter_blocks(true);
        bb_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
        opts.set_block_based_table_factory(&bb_opts);

        // Manual WAL buffering batches syscalls but its periodic flush holds
        // the WAL mutex long enough to stall writers; keep it opt-in.
        let manual_wal = std::env::var("RHYDADB_MANUAL_WAL").is_ok() && !write_sync;
        if manual_wal {
            opts.set_manual_wal_flush(true);
        }

        let mut dbs = Vec::with_capacity(shard_count);
        for i in 0..shard_count {
            let dir = root.join(format!("engine-{i}"));
            let db = DB::open(&opts, &dir)?;
            dbs.push(Arc::new(db));
        }

        let wal_thread: Option<(Sender<()>, thread::JoinHandle<()>)> = if manual_wal {
            let dbs_for_wal = dbs.clone();
            let (wal_ctl, wal_rx) = channel::<()>();
            let handle = thread::Builder::new()
                .name("rhydadb-wal-flush".to_string())
                .spawn(move || loop {
                    match wal_rx.recv_timeout(WAL_FLUSH_INTERVAL) {
                        Ok(()) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    for db in &dbs_for_wal {
                        let _ = db.flush_wal(true);
                    }
                })?;
            Some((wal_ctl, handle))
        } else {
            None
        };

        // Micro-batching pipelines exist in BOTH modes: collectors drain
        // concurrent data writes into rotating WriteBatch buffers so the
        // memtable lock is hit once per batch, not once per request.
        let writers = dbs
            .iter()
            .enumerate()
            .map(|(i, db)| spawn_writer(db.clone(), i, write_sync))
            .collect();

        let storage = Self {
            dbs,
            keyspaces: RwLock::new(HashMap::new()),
            tables: RwLock::new(HashMap::new()),
            ast_cache: Mutex::new(HashMap::new()),
            writers,
            write_sync,
            manual_wal,
            wal_ctl: wal_thread.as_ref().map(|(ctl, _)| ctl.clone()),
            wal_thread: wal_thread.map(|(_, handle)| handle),
        };
        storage.seed_system()?;
        Ok(storage)
    }

    fn seed_system(&self) -> Result<()> {
        if self.get_keyspace("system")?.is_some() {
            return Ok(());
        }
        self.put_keyspace(&KeyspaceDef {
            name: "system".to_string(),
            replication: vec![],
        })?;

        let local_def = TableDef {
            keyspace: "system".to_string(),
            name: "local".to_string(),
            columns: vec![
                col("key", ColumnType::Text, true),
                col("bootstrapped", ColumnType::Text, false),
                col("broadcast_address", ColumnType::Inet, false),
                col("cluster_name", ColumnType::Text, false),
                col("cql_version", ColumnType::Text, false),
                col("data_center", ColumnType::Text, false),
                col("gossip_generation", ColumnType::Int, false),
                col("host_id", ColumnType::Uuid, false),
                col("listen_address", ColumnType::Inet, false),
                col("native_protocol_version", ColumnType::Text, false),
                col("partitioner", ColumnType::Text, false),
                col("rack", ColumnType::Text, false),
                col("release_version", ColumnType::Text, false),
                col("rpc_address", ColumnType::Inet, false),
                col("schema_version", ColumnType::Uuid, false),
                col("tokens", ColumnType::Set(Box::new(ColumnType::Text)), false),
            ],
            partition_keys: vec!["key".to_string()],
            clustering_keys: vec![],
        };
        self.put_table(&local_def)?;

        let host_id = uuid::Uuid::new_v4();
        let schema_version = uuid::Uuid::new_v4();
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let row = vec![
            (0u16, Value::Text("local".to_string())),
            (1u16, Value::Text("COMPLETED".to_string())),
            (2u16, Value::Inet(localhost)),
            (3u16, Value::Text("RhydaDB Cluster".to_string())),
            (4u16, Value::Text("3.4.5".to_string())),
            (5u16, Value::Text("datacenter1".to_string())),
            (6u16, Value::Int(0)),
            (7u16, Value::Uuid(*host_id.as_bytes())),
            (8u16, Value::Inet(localhost)),
            (9u16, Value::Text("4".to_string())),
            (
                10u16,
                Value::Text("org.apache.cassandra.dht.Murmur3Partitioner".to_string()),
            ),
            (11u16, Value::Text("rack1".to_string())),
            (12u16, Value::Text("4.0.7".to_string())),
            (13u16, Value::Inet(localhost)),
            (14u16, Value::Uuid(*schema_version.as_bytes())),
            (
                15u16,
                Value::Set(vec![Value::Text("-9223372036854775808".to_string())]),
            ),
        ];
        self.put_row_sync(
            "system",
            "local",
            &[b"local".to_vec()],
            &encode_row_columns(&row),
        )?;

        let peers_def = TableDef {
            keyspace: "system".to_string(),
            name: "peers".to_string(),
            columns: vec![
                col("peer", ColumnType::Inet, true),
                col("data_center", ColumnType::Text, false),
                col("host_id", ColumnType::Uuid, false),
                col("preferred_ip", ColumnType::Inet, false),
                col("rack", ColumnType::Text, false),
                col("release_version", ColumnType::Text, false),
                col("rpc_address", ColumnType::Inet, false),
                col("schema_version", ColumnType::Uuid, false),
                col("tokens", ColumnType::Set(Box::new(ColumnType::Text)), false),
            ],
            partition_keys: vec!["peer".to_string()],
            clustering_keys: vec![],
        };
        self.put_table(&peers_def)?;

        let text2 = ColumnType::Text;
        let set_text = ColumnType::Set(Box::new(text2.clone()));
        let list_text = ColumnType::List(Box::new(text2.clone()));
        let map_tt = ColumnType::Map(Box::new(text2.clone()), Box::new(text2.clone()));
        let map_tb = ColumnType::Map(Box::new(text2.clone()), Box::new(ColumnType::Blob));
        let sys_schema_defs = vec![
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "keyspaces".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("durable_writes", ColumnType::Boolean, false),
                    col("replication", map_tt.clone(), false),
                ],
                partition_keys: vec!["keyspace_name".to_string()],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "tables".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("table_name", text2.clone(), true),
                    col("bloom_filter_fp_chance", ColumnType::Double, false),
                    col("caching", map_tt.clone(), false),
                    col("comment", text2.clone(), false),
                    col("compaction", map_tt.clone(), false),
                    col("compression", map_tt.clone(), false),
                    col("crc_check_chance", ColumnType::Double, false),
                    col("dclocal_read_repair_chance", ColumnType::Double, false),
                    col("default_time_to_live", ColumnType::Int, false),
                    col("extensions", map_tb.clone(), false),
                    col("flags", set_text.clone(), false),
                    col("gc_grace_seconds", ColumnType::Int, false),
                    col("max_index_interval", ColumnType::Int, false),
                    col("memtable_flush_period_in_ms", ColumnType::Int, false),
                    col("min_index_interval", ColumnType::Int, false),
                    col("read_repair_chance", ColumnType::Double, false),
                    col("speculative_retry", text2.clone(), false),
                ],
                partition_keys: vec!["keyspace_name".to_string(), "table_name".to_string()],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "columns".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("table_name", text2.clone(), true),
                    col("column_name", text2.clone(), true),
                    col("clustering_order", text2.clone(), false),
                    col("column_name_bytes", ColumnType::Blob, false),
                    col("kind", text2.clone(), false),
                    col("position", ColumnType::Int, false),
                    col("type", text2.clone(), false),
                ],
                partition_keys: vec![
                    "keyspace_name".to_string(),
                    "table_name".to_string(),
                    "column_name".to_string(),
                ],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "indexes".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("table_name", text2.clone(), true),
                    col("index_name", text2.clone(), true),
                    col("kind", text2.clone(), false),
                    col("options", text2.clone(), false),
                ],
                partition_keys: vec![
                    "keyspace_name".to_string(),
                    "table_name".to_string(),
                    "index_name".to_string(),
                ],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "triggers".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("table_name", text2.clone(), true),
                    col("trigger_name", text2.clone(), true),
                    col("options", text2.clone(), false),
                ],
                partition_keys: vec![
                    "keyspace_name".to_string(),
                    "table_name".to_string(),
                    "trigger_name".to_string(),
                ],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "types".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("type_name", text2.clone(), true),
                    col("field_names", list_text.clone(), false),
                    col("field_types", list_text.clone(), false),
                ],
                partition_keys: vec!["keyspace_name".to_string(), "type_name".to_string()],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "functions".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("function_name", text2.clone(), true),
                    col("argument_names", list_text.clone(), false),
                    col("argument_types", list_text.clone(), false),
                    col("body", text2.clone(), false),
                    col("called_on_null_input", ColumnType::Boolean, false),
                    col("language", text2.clone(), false),
                    col("return_type", text2.clone(), false),
                    col("deterministic", ColumnType::Boolean, false),
                    col("monotonic", ColumnType::Boolean, false),
                    col("monotonic_on", set_text.clone(), false),
                ],
                partition_keys: vec![
                    "keyspace_name".to_string(),
                    "function_name".to_string(),
                    "argument_types".to_string(),
                ],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "aggregates".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("aggregate_name", text2.clone(), true),
                    col("argument_types", list_text.clone(), true),
                    col("deterministic", ColumnType::Boolean, false),
                    col("final_func", text2.clone(), false),
                    col("initcond", text2.clone(), false),
                    col("return_type", text2.clone(), false),
                    col("state_func", text2.clone(), false),
                    col("state_type", text2.clone(), false),
                ],
                partition_keys: vec![
                    "keyspace_name".to_string(),
                    "aggregate_name".to_string(),
                    "argument_types".to_string(),
                ],
                clustering_keys: vec![],
            },
            TableDef {
                keyspace: "system_schema".to_string(),
                name: "views".to_string(),
                columns: vec![
                    col("keyspace_name", text2.clone(), true),
                    col("view_name", text2.clone(), true),
                    col("base_table_name", text2.clone(), false),
                    col("include_all_columns", ColumnType::Boolean, false),
                    col("where_clause", text2.clone(), false),
                    col("bloom_filter_fp_chance", ColumnType::Double, false),
                    col("caching", map_tt.clone(), false),
                    col("comment", text2.clone(), false),
                    col("compaction", map_tt.clone(), false),
                    col("compression", map_tt.clone(), false),
                    col("crc_check_chance", ColumnType::Double, false),
                    col("dclocal_read_repair_chance", ColumnType::Double, false),
                    col("default_time_to_live", ColumnType::Int, false),
                    col("extensions", map_tb.clone(), false),
                    col("flags", set_text.clone(), false),
                    col("gc_grace_seconds", ColumnType::Int, false),
                    col("max_index_interval", ColumnType::Int, false),
                    col("memtable_flush_period_in_ms", ColumnType::Int, false),
                    col("min_index_interval", ColumnType::Int, false),
                    col("read_repair_chance", ColumnType::Double, false),
                    col("speculative_retry", text2.clone(), false),
                ],
                partition_keys: vec!["keyspace_name".to_string(), "view_name".to_string()],
                clustering_keys: vec![],
            },
        ];
        self.put_keyspace(&KeyspaceDef {
            name: "system_schema".to_string(),
            replication: vec![],
        })?;
        for def in sys_schema_defs {
            self.put_table(&def)?;
        }
        Ok(())
    }

    fn engine_of(&self, routing_key: &[u8]) -> usize {
        if self.dbs.len() <= 1 {
            0
        } else {
            (fnv1a(routing_key) % self.dbs.len() as u64) as usize
        }
    }

    /// Hash over the partition identity so every row of a partition always
    /// lands on the same engine.
    fn data_engine(&self, keyspace: &str, table: &str, first_part: &[u8]) -> usize {
        let mut route = Vec::with_capacity(keyspace.len() + table.len() + first_part.len());
        route.extend_from_slice(DT_PREFIX);
        route.extend_from_slice(keyspace.as_bytes());
        route.push(b'!');
        route.extend_from_slice(table.as_bytes());
        route.push(b'!');
        route.extend_from_slice(first_part);
        self.engine_of(&route)
    }

    /// Write options for metadata/DDL: always synchronous so schema changes
    /// are immediately visible and durable regardless of data-write mode.
    fn meta_write_opts(&self) -> WriteOptions {
        let mut w = WriteOptions::default();
        if self.write_sync {
            w.set_sync(true);
        } else {
            w.disable_wal(true);
        }
        w
    }

    /// Metadata/plain write: always engine 0.
    pub fn put(&self, key: &[u8], val: &[u8]) -> Result<()> {
        let wopts = self.meta_write_opts();
        self.dbs[0]
            .put_opt(key, val, &wopts)
            .map_err(|e| anyhow::anyhow!("rocksdb put failed: {e}"))
    }

    /// Metadata/plain read: engine 0.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.dbs[0].get(key)?.map(|v| v.to_vec()))
    }

    /// Metadata/plain delete: engine 0.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        let wopts = self.meta_write_opts();
        self.dbs[0]
            .delete_opt(key, &wopts)
            .map_err(|e| anyhow::anyhow!("rocksdb delete failed: {e}"))
    }

    /// Metadata scan: engine 0 only.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.for_each_on_engine(0, prefix, |k, v| {
            out.push((k, v));
            true
        })?;
        Ok(out)
    }

    fn for_each_on_engine<F>(&self, engine: usize, prefix: &[u8], mut f: F) -> Result<()>
    where
        F: FnMut(Vec<u8>, Vec<u8>) -> bool,
    {
        let iter = self.dbs[engine].iterator(IteratorMode::From(prefix, Direction::Forward));
        for item in iter {
            let (k, v) = item?;
            if !k.starts_with(prefix) {
                break;
            }
            if !f(k.into(), v.into()) {
                break;
            }
        }
        Ok(())
    }

    /// Streams entries under `prefix` on one engine; callback returns false
    /// to stop early.
    pub fn for_each_in_prefix<F>(&self, prefix: &[u8], f: F) -> Result<()>
    where
        F: FnMut(Vec<u8>, Vec<u8>) -> bool,
    {
        self.for_each_on_engine(0, prefix, f)
    }

    /// Streams every row of a table across all engines (unordered).
    pub fn for_each_in_table<F>(&self, keyspace: &str, table: &str, mut f: F) -> Result<()>
    where
        F: FnMut(Vec<u8>, Vec<u8>) -> bool,
    {
        let prefix = self.data_prefix(keyspace, table);
        for engine in 0..self.dbs.len() {
            let mut stop = false;
            self.for_each_on_engine(engine, &prefix, |k, v| {
                stop = !f(k, v);
                !stop
            })?;
            if stop {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Streams rows of one partition; the partition maps to exactly one
    /// engine so clustering order within it is preserved.
    pub fn for_each_in_partition<F>(
        &self,
        keyspace: &str,
        table: &str,
        partition: &[Vec<u8>],
        mut f: F,
    ) -> Result<()>
    where
        F: FnMut(Vec<u8>, Vec<u8>) -> bool,
    {
        let mut prefix = self.data_prefix(keyspace, table);
        prefix.extend(encode_key(partition));
        let engine = self.data_engine(keyspace, table, partition.first().map(|p| p.as_slice()).unwrap_or(&[]));
        self.for_each_on_engine(engine, &prefix, f)
    }

    pub fn delete_prefix(&self, prefix: &[u8]) -> Result<()> {
        match prefix_upper_bound(prefix) {
            Some(end) => {
                let wopts = self.meta_write_opts();
                let mut batch = WriteBatch::default();
                batch.delete_range(prefix, &end);
                self.dbs[0]
                    .write_opt(batch, &wopts)
                    .map_err(|e| anyhow::anyhow!("rocksdb delete_range failed: {e}"))
            }
            None => {
                for (k, _) in self.scan_prefix(prefix)? {
                    self.delete(&k)?;
                }
                Ok(())
            }
        }
    }

    /// Removes a table's data range from every engine through the write
    /// pipelines (so it cannot overtake queued row ops) and returns the
    /// tickets the caller must settle.
    fn delete_data_prefix(&self, data_prefix: &[u8]) -> Result<Vec<WriteTicket>> {
        let mut tickets = Vec::new();
        match prefix_upper_bound(data_prefix) {
            Some(end) => {
                for engine in 0..self.dbs.len() {
                    let w = &self.writers[engine];
                    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                    w.tx
                        .send(WriteOp::DeleteRange {
                            start: data_prefix.to_vec(),
                            end: end.clone(),
                            done: Some(done_tx),
                        })
                        .map_err(|_| anyhow::anyhow!("storage writer is gone"))?;
                    tickets.push(WriteTicket(Some(done_rx)));
                }
            }
            None => {
                // 0xFF-saturated prefix: fall back to per-row deletes.
                for engine in 0..self.dbs.len() {
                    self.for_each_on_engine(engine, data_prefix, |k, _| {
                        let w = &self.writers[engine];
                        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                        let _ = w.tx.send(WriteOp::Delete {
                            key: k,
                            done: Some(done_tx),
                        });
                        tickets.push(WriteTicket(Some(done_rx)));
                        true
                    })?;
                }
            }
        }
        Ok(tickets)
    }

    pub fn put_keyspace(&self, ks: &KeyspaceDef) -> Result<()> {
        let key = [KS_PREFIX, ks.name.as_bytes()].concat();
        self.put(&key, &serde_json::to_vec(ks)?)?;
        self.keyspaces
            .write()
            .expect("keyspace cache poisoned")
            .insert(ks.name.clone(), ks.clone());
        Ok(())
    }

    pub fn get_keyspace(&self, name: &str) -> Result<Option<KeyspaceDef>> {
        if let Some(ks) = self
            .keyspaces
            .read()
            .expect("keyspace cache poisoned")
            .get(name)
            .cloned()
        {
            return Ok(Some(ks));
        }
        let key = [KS_PREFIX, name.as_bytes()].concat();
        match self.get(&key)? {
            Some(bytes) => {
                let ks: KeyspaceDef = serde_json::from_slice(&bytes)?;
                self.keyspaces
                    .write()
                    .expect("keyspace cache poisoned")
                    .insert(name.to_string(), ks.clone());
                Ok(Some(ks))
            }
            None => Ok(None),
        }
    }

    pub fn delete_keyspace(&self, name: &str) -> Result<Vec<WriteTicket>> {
        let ks_key = [KS_PREFIX, name.as_bytes()].concat();
        self.delete(&ks_key)?;
        let tb_prefix = [TB_PREFIX, name.as_bytes(), b"!"].concat();
        self.delete_prefix(&tb_prefix)?;
        // Wipe every data row of the keyspace on every engine in one range
        // delete per engine (routed through the pipelines).
        let dt_ks_prefix = [DT_PREFIX, name.as_bytes(), b"!"].concat();
        let tickets = self.delete_data_prefix(&dt_ks_prefix)?;
        self.keyspaces
            .write()
            .expect("keyspace cache poisoned")
            .remove(name);
        self.tables
            .write()
            .expect("tables cache poisoned")
            .retain(|(ks, _), _| ks != name);
        Ok(tickets)
    }

    pub fn put_table(&self, table: &TableDef) -> Result<()> {
        let key = [
            TB_PREFIX,
            table.keyspace.as_bytes(),
            b"!",
            table.name.as_bytes(),
        ]
        .concat();
        self.put(&key, &serde_json::to_vec(table)?)?;
        let arc = Arc::new(table.clone());
        self.tables
            .write()
            .expect("tables cache poisoned")
            .insert((table.keyspace.clone(), table.name.clone()), arc);
        Ok(())
    }

    pub fn get_table(&self, keyspace: &str, table: &str) -> Result<Option<Arc<TableDef>>> {
        {
            let map = self.tables.read().expect("tables cache poisoned");
            if let Some(def) = map
                .iter()
                .find_map(|((ks, tb), v)| (ks == keyspace && tb == table).then_some(v))
            {
                return Ok(Some(def.clone()));
            }
        }
        let key = [TB_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes()].concat();
        match self.get(&key)? {
            Some(bytes) => {
                let def: TableDef = serde_json::from_slice(&bytes)?;
                let def = Arc::new(def);
                self.tables
                    .write()
                    .expect("tables cache poisoned")
                    .insert((keyspace.to_string(), table.to_string()), def.clone());
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    pub fn delete_table(&self, keyspace: &str, table: &str) -> Result<Vec<WriteTicket>> {
        let tb_key = [TB_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes()].concat();
        self.delete(&tb_key)?;
        let dt_prefix = [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat();
        let tickets = self.delete_data_prefix(&dt_prefix)?;
        self.tables
            .write()
            .expect("tables cache poisoned")
            .remove(&(keyspace.to_string(), table.to_string()));
        Ok(tickets)
    }

    pub fn truncate_table(
        &self,
        keyspace: &str,
        table: &str,
    ) -> Result<Vec<WriteTicket>> {
        let dt_prefix = [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat();
        self.delete_data_prefix(&dt_prefix)
    }

    pub fn ast_cache_get(&self, query: &str) -> Option<Arc<CassandraStatement>> {
        self.ast_cache
            .lock()
            .expect("ast cache poisoned")
            .get(query)
            .cloned()
    }

    pub fn ast_cache_put(&self, query: &str, stmt: Arc<CassandraStatement>) {
        let mut cache = self.ast_cache.lock().expect("ast cache poisoned");
        if cache.len() >= AST_CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(query.to_string(), stmt);
    }

    fn data_prefix(&self, keyspace: &str, table: &str) -> Vec<u8> {
        [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat()
    }

    pub fn get_row(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
    ) -> Result<Option<Vec<u8>>> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        let engine = self.data_engine(
            keyspace,
            table,
            key_parts.first().map(|p| p.as_slice()).unwrap_or(&[]),
        );
        Ok(self.dbs[engine].get(&key)?.map(|v| v.to_vec()))
    }

    /// Synchronous row write used only by DDL schema mirrors and seeding:
    /// immediately visible, independent of the data pipelines.
    pub fn put_row_sync(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
        value: &[u8],
    ) -> Result<()> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        let engine = self.data_engine(
            keyspace,
            table,
            key_parts.first().map(|p| p.as_slice()).unwrap_or(&[]),
        );
        let wopts = self.meta_write_opts();
        self.dbs[engine]
            .put_opt(&key, value, &wopts)
            .map_err(|e| anyhow::anyhow!("rocksdb put failed: {e}"))
    }

    /// Synchronous row delete for DDL schema mirrors.
    pub fn delete_row_sync(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
    ) -> Result<()> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        let engine = self.data_engine(
            keyspace,
            table,
            key_parts.first().map(|p| p.as_slice()).unwrap_or(&[]),
        );
        let wopts = self.meta_write_opts();
        self.dbs[engine]
            .delete_opt(&key, &wopts)
            .map_err(|e| anyhow::anyhow!("rocksdb delete failed: {e}"))
    }

    /// Asynchronous micro-batched row write. Returns a ticket the caller must
    /// settle (await) before acknowledging the request downstream.
    pub fn put_row(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
        value: &[u8],
    ) -> Result<WriteTicket> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        self.write_data_op(
            keyspace,
            table,
            key_parts,
            move |done| WriteOp::Put {
                key,
                val: value.to_vec(),
                done,
            },
        )
    }

    /// Asynchronous micro-batched row delete.
    pub fn delete_row(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
    ) -> Result<WriteTicket> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        self.write_data_op(
            keyspace,
            table,
            key_parts,
            move |done| WriteOp::Delete { key, done },
        )
    }

    fn write_data_op(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
        op: impl FnOnce(Option<DoneTx>) -> WriteOp,
    ) -> Result<WriteTicket> {
        let engine = self.data_engine(
            keyspace,
            table,
            key_parts.first().map(|p| p.as_slice()).unwrap_or(&[]),
        );
        let w = &self.writers[engine];
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        w.tx
            .send(op(Some(done_tx)))
            .map_err(|_| anyhow::anyhow!("storage writer is gone"))?;
        Ok(WriteTicket(Some(done_rx)))
    }

    /// Awaits until every write represented by the tickets is committed to
    /// its engine's memtable. Never blocks a worker thread; drains in place.
    pub async fn settle(&self, tickets: &mut Vec<WriteTicket>) {
        for t in tickets.iter_mut() {
            if let Some(rx) = t.0.take() {
                let _ = rx.await;
            }
        }
        tickets.clear();
    }

    #[allow(dead_code)]
    pub fn scan_rows(&self, keyspace: &str, table: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.for_each_in_table(keyspace, table, |k, v| {
            out.push((k, v));
            true
        })?;
        Ok(out)
    }

    #[allow(dead_code)]
    pub fn scan_partition(
        &self,
        keyspace: &str,
        table: &str,
        partition_key: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.for_each_in_partition(keyspace, table, partition_key, |k, v| {
            out.push((k, v));
            true
        })?;
        Ok(out)
    }
}

/// Exclusive end bound covering everything that starts with `prefix`.
/// Returns None when every byte is 0xFF (unbounded increment).
fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for b in end.iter_mut().rev() {
        if *b < 0xFF {
            *b += 1;
            return Some(end);
        }
    }
    None
}

fn col(name: &str, col_type: ColumnType, is_partition_key: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        col_type,
        is_partition_key,
        is_clustering: false,
    }
}

/// Per-engine micro-batching pipeline: a collector drains concurrent data
/// writes into rotating WriteBatch buffers (up to MAX_BATCH_OPS ops each)
/// while a committer performs the actual RocksDB write and publishes the
/// committed ticket watermark. The collector never blocks on disk, so writers
/// keep flowing ("no stop"), and each memtable lock acquisition is amortized
/// over a whole batch.
fn spawn_writer(db: Arc<DB>, id: usize, write_sync: bool) -> EngineWriter {
    let (ops_tx, ops_rx) = channel::<WriteOp>();
    let (commit_tx, commit_rx) = channel::<(WriteBatch, Vec<Option<DoneTx>>)>();

    let handles: Mutex<Vec<thread::JoinHandle<()>>> = Mutex::new(Vec::new());

    {
        let h = thread::Builder::new()
            .name(format!("rhyda-collector-{id}"))
            .spawn(move || {
                while let Ok(first) = ops_rx.recv() {
                    let mut batch = WriteBatch::default();
                    let mut done: Vec<Option<DoneTx>> = Vec::new();
                    batch_apply(&mut batch, first, &mut done);
                    while batch.len() < MAX_BATCH_OPS {
                        match ops_rx.try_recv() {
                            Ok(op) => batch_apply(&mut batch, op, &mut done),
                            Err(_) => break,
                        }
                    }
                    if commit_tx.send((batch, done)).is_err() {
                        return;
                    }
                }
                drop(commit_tx);
            })
            .expect("spawn collector");
        handles.lock().expect("handles").push(h);
    }

    {
        let committed_tx = ();
        let _ = committed_tx;
        let h = thread::Builder::new()
            .name(format!("rhyda-committer-{id}"))
            .spawn(move || {
                let mut wopts = WriteOptions::default();
                if write_sync {
                    wopts.set_sync(true);
                } else {
                    wopts.disable_wal(true);
                }
                while let Ok((batch, done)) = commit_rx.recv() {
                    if let Err(e) = db.write_opt(batch, &wopts) {
                        eprintln!("rhyda-committer-{id}: write batch failed: {e}");
                    }
                    // Dropping the senders completes every waiter of this
                    // batch — one wakeup per waiting task, nothing more.
                    drop(done);
                }
            })
            .expect("spawn committer");
        handles.lock().expect("handles").push(h);
    }

    EngineWriter { tx: ops_tx, handles }
}

pub fn encode_key(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

pub fn decode_key_data(prefix: &[u8], full_key: &[u8]) -> Result<Vec<Vec<u8>>> {
    if !full_key.starts_with(prefix) {
        return Err(anyhow::anyhow!("key does not belong to table"));
    }
    let mut rest = &full_key[prefix.len()..];
    let mut parts = Vec::new();
    while !rest.is_empty() {
        if rest.len() < 2 {
            return Err(anyhow::anyhow!("corrupt key"));
        }
        let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        if rest.len() < 2 + len {
            return Err(anyhow::anyhow!("corrupt key"));
        }
        parts.push(rest[2..2 + len].to_vec());
        rest = &rest[2 + len..];
    }
    Ok(parts)
}

pub fn decode_key(keyspace: &str, table: &str, full_key: &[u8]) -> Result<Vec<Vec<u8>>> {
    let prefix = [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat();
    decode_key_data(&prefix, full_key)
}

pub fn encode_row_columns(values: &[(u16, Value)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (idx, v) in values {
        out.extend_from_slice(&idx.to_be_bytes());
        out.extend(v.to_wire());
    }
    out
}

pub fn decode_row_columns(data: &[u8], table: &TableDef) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = vec![Value::Null; table.columns.len()];
    let mut off = 0usize;
    while off < data.len() {
        if off + 2 > data.len() {
            return Err(anyhow::anyhow!("corrupt row value"));
        }
        let idx = u16::from_be_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        let col = table
            .columns
            .get(idx)
            .ok_or_else(|| anyhow::anyhow!("row references unknown column index {idx}"))?;
        if off + 4 > data.len() {
            return Err(anyhow::anyhow!("corrupt row value"));
        }
        let len = i32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        if len == -1 {
            out[idx] = Value::Null;
            off += 4;
            continue;
        }
        if len < 0 || off + 4 + len as usize > data.len() {
            return Err(anyhow::anyhow!("corrupt row value"));
        }
        let len = len as usize;
        let v = Value::from_wire(&data[off..off + 4 + len], &col.col_type)?;
        out[idx] = v;
        off += 4 + len;
    }
    Ok(out)
}
