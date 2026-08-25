# RhydaDB

RhydaDB is a lightweight, single-node CQL database server written in Rust. It speaks the Cassandra Native Protocol v4, stores data in RocksDB, and drops in as a drop-in replacement for development and small-scale workloads — no Java, no cluster, no external services.

## Overview

RhydaDB accepts connections from any CQL v4 client (the official Python `cassandra-driver`, `cqlsh --protocol-version=4`, Go/Node drivers, ...) and serves the core Cassandra query surface: keyspaces, tables, and row CRUD with prepared statements. Internally it is a three-layer system — a tokio protocol server, a schema-driven query engine over a tree-sitter CQL grammar, and a RocksDB storage engine with an async write pipeline — described in [docs/](docs/).

## Key Features

- **Native Protocol v4** — full framing, compression negotiation, typed metadata, and error codes over TCP.
- **Fully pipelined server** — concurrent per-request execution with out-of-order stream responses, vectored coalesced writes, TCP_NODELAY, buffered IO; EXECUTE never takes a connection-level lock.
- **Sharded storage engines** — N independent RocksDB instances (up to 64), partition-hashed routing: writes and reads scale past a single engine's memtable lock, partitions stay intact.
- **Durable mode with double-buffered fsync group commit** — per-engine collector/committer pipelines rotate batches of up to 1024 ops so writers never wait on fsync (~1 fsync per 1024 writes).
- **O(1) DDL** — DROP KEYSPACE / TRUNCATE use RocksDB range tombstones across all engines; schema changes are always synchronous and immediately visible.
- **Prepared statements** — PREPARE/EXECUTE with bind metadata, partition-key positions, and skip-metadata result sets.
- **RocksDB storage** — LZ4-compressed, durable, restart-safe; `ks!`/`tb!`/`dt!` key layout separates metadata from row data; tuned block cache, Bloom filters, parallel compaction.
- **AST cache** — queries are parsed once per text (8192-entry bounded cache), so hot workloads skip parsing entirely.
- **Schema cache** — keyspace/table definitions cached as `Arc`, zero-copy lookups on the hot path.
- **Query surface** — USE, CREATE/DROP KEYSPACE, CREATE/DROP TABLE, INSERT, SELECT (`=`, `IN`, `LIMIT`), UPDATE, DELETE, TRUNCATE; types include int/bigint/smallint/tinyint, float/double, boolean, text/blob, timestamp, uuid, inet, and set/list/map collections.
- **Compression** — LZ4 and Snappy negotiated via STARTUP.
- **Driver-friendly system tables** — `system.local` and `system_schema` mirrors keep standard clients happy.

## Building

```sh
cargo build --release
```

The tree-sitter CQL grammar is vendored under `vendor/` (a fork that accepts `?` bind markers in UPDATE assignment values) and wired via `[patch.crates-io]`.

## Running

```sh
RHYDADB_DATA=./data RHYDADB_LISTEN=127.0.0.1:9042 ./target/release/rhydadb
```

> **Windows:** keep the data directory out of OneDrive/Dropbox folders and add
> an antivirus exclusion — sync/scan overhead on SST files costs ~4x write
> throughput (see [docs/04](docs/04.%20Performance%20Architecture.md)).

| Variable | Default | Meaning |
|---|---|---|
| `RHYDADB_DATA` | `./data` | Data directory |
| `RHYDADB_LISTEN` | `127.0.0.1:9042` | Bind address |
| `RHYDADB_SYNC` | unset | Durable mode: sharded double-buffered fsync group commit for data writes |
| `RHYDADB_ENGINE_SHARDS` | `min(cpus, 8)` | Independent RocksDB instances, 1–64; persisted on first start |
| `RHYDADB_CACHE_MB` | `1024` | Shared block cache size |
| `RHYDADB_MEMTABLE_MB` | `128` | Per-engine memtable size |
| `RHYDADB_MANUAL_WAL` | unset | Opt-in manual WAL buffering (non-durable mode) |

### Docker (Ubuntu)

```sh
docker build -t rhydadb .
# io_uring syscalls are blocked by Docker's default seccomp profile:
docker run -d --name rhydadb --security-opt seccomp=unconfined -p 9042:9042 rhydadb
# benchmark inside the container:
docker exec rhydadb bench --mode read --conns 32 --pipeline 256 --seconds 10
```

## Performance

Pipelined CQL clients (`bench`, and `bench_async` — a tokio client with
hundreds of async connections), single node: **~1.0M writes (non-durable),
~1.1M durable fsync'd writes, ~1.8M reads, ~1.35M mixed** per second on a
20-core Windows desktop; 2.27M reads under Docker/Linux. Strict
read-your-writes is preserved via per-op commit gating. Full methodology,
tunables, architecture and the io_uring roadmap:
[docs/04. Performance Architecture](docs/04.%20Performance%20Architecture.md).

Then connect with any CQL v4 client:

```python
from cassandra.cluster import Cluster
c = Cluster(protocol_version=4).connect()
c.execute("CREATE KEYSPACE demo WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}")
c.set_keyspace("demo")
c.execute("CREATE TABLE users (id int PRIMARY KEY, name text)")
c.execute("INSERT INTO users (id, name) VALUES (%s, %s)", (1, "alice"))
print(c.execute("SELECT * FROM users WHERE id = %s", (1,)).one())
```

## Testing

```sh
cargo test
```

Integration tests spawn a real server on a random port and speak raw protocol v4 frames: full CQL flow, prepared statements, concurrent hammering, garbage-frame resilience, and persistence across restart.

## Documentation

- [01. Server and Protocol Architecture](docs/01.%20Server%20and%20Protocol%20Architecture.md)
- [02. Query Engine Architecture](docs/02.%20Query%20Engine%20Architecture.md)
- [03. Storage Engine Architecture](docs/03.%20Storage%20Engine%20Architecture.md)