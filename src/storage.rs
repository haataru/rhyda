use crate::cql_value::Value;
use crate::schema::{ColumnDef, ColumnType, KeyspaceDef, TableDef};
use anyhow::Result;
use cql3_parser::cassandra_statement::CassandraStatement;
use rocksdb::{DB, DBCompressionType, Direction, IteratorMode, Options, WriteBatch};
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

enum WriteOp {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

pub struct Storage {
    db: Arc<DB>,
    keyspaces: RwLock<HashMap<String, KeyspaceDef>>,
    tables: RwLock<HashMap<(String, String), Arc<TableDef>>>,
    ast_cache: Mutex<HashMap<String, Arc<CassandraStatement>>>,
    shards: Vec<Sender<WriteOp>>,
    wal_ctl: Option<Sender<()>>,
    wal_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Storage {
    fn drop(&mut self) {
        // Signal the periodic WAL flusher and wait for it to exit so the
        // RocksDB lock file is released before the directory is reused.
        drop(self.wal_ctl.take());
        if let Some(handle) = self.wal_thread.take() {
            let _ = handle.join();
        }
    }
}

fn batch_apply(batch: &mut WriteBatch, op: WriteOp) {
    match op {
        WriteOp::Put(k, v) => {
            batch.put(&k, &v);
        }
        WriteOp::Delete(k) => {
            batch.delete(&k);
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
        let cpus = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let write_sync = std::env::var("RHYDADB_SYNC").is_ok();
        let cache_mb = env_num("RHYDADB_CACHE_MB", 256, 8, 1 << 20);
        let memtable_mb = env_num("RHYDADB_MEMTABLE_MB", 64, 8, 4096);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(DBCompressionType::Lz4);
        opts.increase_parallelism(cpus as i32);
        opts.set_max_background_jobs(cpus as i32);
        opts.optimize_level_style_compaction(memtable_mb * 1024 * 1024 * 2);
        opts.set_write_buffer_size(memtable_mb * 1024 * 1024);
        opts.set_max_write_buffer_number(6);
        opts.set_min_write_buffer_number_to_merge(2);

        let mut bb_opts = rocksdb::BlockBasedOptions::default();
        let cache = rocksdb::Cache::new_lru_cache(cache_mb * 1024 * 1024);
        bb_opts.set_block_cache(&cache);
        bb_opts.set_block_size(16 * 1024);
        bb_opts.set_bloom_filter(10.0, true);
        bb_opts.set_cache_index_and_filter_blocks(true);
        bb_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
        opts.set_block_based_table_factory(&bb_opts);

        if !write_sync {
            // Cassandra-like periodic commitlog durability: buffer WAL in
            // memory and flush it on a timer instead of per-write syscalls.
            opts.set_manual_wal_flush(true);
        }

        let db = Arc::new(DB::open(&opts, path)?);

        let wal_thread: Option<(Sender<()>, thread::JoinHandle<()>)> = if !write_sync {
            let wal_db = db.clone();
            let (wal_ctl, wal_rx) = channel::<()>();
            let wal_thread = thread::Builder::new()
                .name("rhydadb-wal-flush".to_string())
                .spawn(move || loop {
                    match wal_rx.recv_timeout(WAL_FLUSH_INTERVAL) {
                        Ok(()) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    let _ = wal_db.flush_wal(true);
                })?;
            Some((wal_ctl, wal_thread))
        } else {
            None
        };

        let shards = if write_sync {
            let shard_count = env_num("RHYDADB_WRITE_SHARDS", cpus.clamp(1, 16), 1, 64);
            (0..shard_count)
                .map(|i| spawn_shard(db.clone(), i))
                .collect()
        } else {
            Vec::new()
        };

        let storage = Self {
            db,
            keyspaces: RwLock::new(HashMap::new()),
            tables: RwLock::new(HashMap::new()),
            ast_cache: Mutex::new(HashMap::new()),
            shards,
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
        self.put_row(
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

    fn enqueue(&self, key: &[u8], op: impl FnOnce() -> WriteOp) -> Result<()> {
        if self.shards.is_empty() {
            return Ok(());
        }
        let idx = (fnv1a(key) % self.shards.len() as u64) as usize;
        self.shards[idx]
            .send(op())
            .map_err(|_| anyhow::anyhow!("storage writer is gone"))?;
        Ok(())
    }

    pub fn put(&self, key: &[u8], val: &[u8]) -> Result<()> {
        if self.shards.is_empty() {
            return self
                .db
                .put(key, val)
                .map_err(|e| anyhow::anyhow!("rocksdb put failed: {e}"));
        }
        let k = key.to_vec();
        let v = val.to_vec();
        self.enqueue(key, move || WriteOp::Put(k, v))
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.get(key)?.map(|v| v.to_vec()))
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        if self.shards.is_empty() {
            return self
                .db
                .delete(key)
                .map_err(|e| anyhow::anyhow!("rocksdb delete failed: {e}"));
        }
        let k = key.to_vec();
        self.enqueue(key, || WriteOp::Delete(k))
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        self.for_each_in_prefix(prefix, |k, v| {
            out.push((k, v));
            true
        })?;
        Ok(out)
    }

    /// Streams entries under `prefix`; the callback returns false to stop
    /// early (no further RocksDB reads are performed after that).
    pub fn for_each_in_prefix<F>(&self, prefix: &[u8], mut f: F) -> Result<()>
    where
        F: FnMut(Vec<u8>, Vec<u8>) -> bool,
    {
        let iter = self
            .db
            .iterator(IteratorMode::From(prefix, Direction::Forward));
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

    pub fn delete_prefix(&self, prefix: &[u8]) -> Result<()> {
        match prefix_upper_bound(prefix) {
            Some(end) => {
                let mut batch = WriteBatch::default();
                batch.delete_range(prefix, &end);
                self.db
                    .write_opt(batch, &Default::default())
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

    pub fn delete_keyspace(&self, name: &str) -> Result<()> {
        let ks_key = [KS_PREFIX, name.as_bytes()].concat();
        self.delete(&ks_key)?;
        let tb_prefix = [TB_PREFIX, name.as_bytes(), b"!"].concat();
        self.delete_prefix(&tb_prefix)?;
        let dt_prefix = [DT_PREFIX, name.as_bytes(), b"!"].concat();
        self.delete_prefix(&dt_prefix)?;
        self.keyspaces
            .write()
            .expect("keyspace cache poisoned")
            .remove(name);
        self.tables
            .write()
            .expect("tables cache poisoned")
            .retain(|(ks, _), _| ks != name);
        Ok(())
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
        if let Some(def) = self
            .tables
            .read()
            .expect("tables cache poisoned")
            .get(&(keyspace.to_string(), table.to_string()))
            .cloned()
        {
            return Ok(Some(def));
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

    pub fn delete_table(&self, keyspace: &str, table: &str) -> Result<()> {
        let tb_key = [TB_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes()].concat();
        self.delete(&tb_key)?;
        let dt_prefix = [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat();
        self.delete_prefix(&dt_prefix)?;
        self.tables
            .write()
            .expect("tables cache poisoned")
            .remove(&(keyspace.to_string(), table.to_string()));
        Ok(())
    }

    pub fn truncate_table(&self, keyspace: &str, table: &str) -> Result<()> {
        let dt_prefix = [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat();
        self.delete_prefix(&dt_prefix)
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
        Ok(self.get(&key)?.map(|v| v.to_vec()))
    }

    pub fn put_row(
        &self,
        keyspace: &str,
        table: &str,
        key_parts: &[Vec<u8>],
        value: &[u8],
    ) -> Result<()> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        self.put(&key, value)
    }

    pub fn delete_row(&self, keyspace: &str, table: &str, key_parts: &[Vec<u8>]) -> Result<()> {
        let mut key = self.data_prefix(keyspace, table);
        key.extend(encode_key(key_parts));
        self.delete(&key)
    }

    pub fn scan_rows(&self, keyspace: &str, table: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_prefix(&self.data_prefix(keyspace, table))
    }

    pub fn scan_partition(
        &self,
        keyspace: &str,
        table: &str,
        partition_key: &[Vec<u8>],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut prefix = self.data_prefix(keyspace, table);
        prefix.extend(encode_key(partition_key));
        self.scan_prefix(&prefix)
    }

    pub fn data_prefix_for(&self, keyspace: &str, table: &str) -> Vec<u8> {
        self.data_prefix(keyspace, table)
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

/// Durable mode only: per-shard pipeline where a collector drains operations
/// into rotating buffers while a committer performs the fsync'd write. The
/// collector never blocks on disk, so writers keep flowing ("no stop").
fn spawn_shard(db: Arc<DB>, id: usize) -> Sender<WriteOp> {
    let (ops_tx, ops_rx) = channel::<WriteOp>();
    let (commit_tx, commit_rx) = channel::<WriteBatch>();

    {
        thread::Builder::new()
            .name(format!("rhyda-collector-{id}"))
            .spawn(move || {
                while let Ok(first) = ops_rx.recv() {
                    let mut batch = WriteBatch::default();
                    batch_apply(&mut batch, first);
                    let mut n = 1usize;
                    while n < MAX_BATCH_OPS {
                        match ops_rx.try_recv() {
                            Ok(op) => {
                                batch_apply(&mut batch, op);
                                n += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    if commit_tx.send(batch).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn collector");
    }

    thread::Builder::new()
        .name(format!("rhyda-committer-{id}"))
        .spawn(move || {
            let mut wopts = rocksdb::WriteOptions::default();
            wopts.set_sync(true);
            while let Ok(batch) = commit_rx.recv() {
                if let Err(e) = db.write_opt(batch, &wopts) {
                    eprintln!("rhyda-committer-{id}: write batch failed: {e}");
                }
            }
        })
        .expect("spawn committer");

    ops_tx
}

pub fn encode_key(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

pub fn decode_key(keyspace: &str, table: &str, full_key: &[u8]) -> Result<Vec<Vec<u8>>> {
    let prefix = [DT_PREFIX, keyspace.as_bytes(), b"!", table.as_bytes(), b"!"].concat();
    if !full_key.starts_with(&prefix) {
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
