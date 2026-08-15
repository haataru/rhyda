# RhydaDB

RhydaDB is a lightweight, single-node CQL database server written in Rust. It speaks the Cassandra Native Protocol v4, stores data in RocksDB, and drops in as a drop-in replacement for development and small-scale workloads — no Java, no cluster, no external services.

## Overview

RhydaDB accepts connections from any CQL v4 client (the official Python `cassandra-driver`, `cqlsh --protocol-version=4`, Go/Node drivers, ...) and serves the core Cassandra query surface: keyspaces, tables, and row CRUD with prepared statements. Internally it is a three-layer system — a tokio protocol server, a schema-driven query engine over a tree-sitter CQL grammar, and a RocksDB storage engine with an async write pipeline — described in [docs/](docs/).

## Key Features

- **Native Protocol v4** — full framing, compression negotiation, typed metadata, and error codes over TCP.
- **Prepared statements** — PREPARE/EXECUTE with bind metadata, partition-key positions, and skip-metadata result sets.
- **RocksDB storage** — LZ4-compressed, durable, restart-safe; `ks!`/`tb!`/`dt!` key layout separates metadata from row data.
- **WriteBatch group commit** — a dedicated writer thread coalesces up to 512 writes per fsync (~5x throughput in durable mode).
- **AST cache** — queries are parsed once per text (8192-entry bounded cache), so hot workloads skip parsing entirely.
- **Schema cache** — keyspace/table definitions are cached in memory and invalidated on DDL.
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

| Variable | Default | Meaning |
|---|---|---|
| `RHYDADB_DATA` | `./data` | RocksDB data directory |
| `RHYDADB_LISTEN` | `127.0.0.1:9042` | Bind address |
| `RHYDADB_SYNC` | unset | Durable mode: fsync per WriteBatch instead of per write |

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