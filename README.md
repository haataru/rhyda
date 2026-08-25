# RhydaDB

RhydaDB is a single-node CQL database server written in Rust. It speaks the Cassandra Native Protocol v4, stores data in sharded RocksDB engines, and is designed to be fast, self-contained, and easy to run — no JVM, no cluster, no external services.

RhydaDB supports keyspaces and tables with compound partition and clustering keys, prepared statements, collections (`set`, `list`, `map`), and works with standard Cassandra drivers out of the box. Under the hood: fully pipelined request execution, up to 64 independent storage engines with partition-hashed routing, micro-batched group commit with an optional fsync-durable mode, strict read-your-writes consistency, and O(1) `DROP`/`TRUNCATE` through range tombstones.

For more information on design and internals, please refer to the [documentation](docs/01.%20Server%20and%20Protocol%20Architecture.md).

## Getting Started

Build and run:

```sh
cargo build --release
RHYDADB_DATA=./data ./target/release/rhydadb
```

Or with Docker:

```sh
docker build -t rhydadb .
docker run -d --name rhydadb --security-opt seccomp=unconfined -p 9042:9042 rhydadb
```

Then connect with any CQL v4 client:

```python
from cassandra.cluster import Cluster

s = Cluster(["127.0.0.1"], protocol_version=4).connect()
s.execute("CREATE KEYSPACE shop WITH replication = {'class':'SimpleStrategy','replication_factor':1}")
s.set_keyspace("shop")
s.execute("CREATE TABLE kv (k text PRIMARY KEY, v text)")

ins = s.prepare("INSERT INTO kv (k, v) VALUES (?, ?)")
s.execute(ins, ("hello", "world"))
print(s.execute("SELECT * FROM kv WHERE k = 'hello'").one())
```

A pipelined benchmark client is included:

```sh
./target/release/bench_async --mode mixed --conns 128 --pipeline 256 --seconds 10
```

## Documentation

- [Server and Protocol Architecture](docs/01.%20Server%20and%20Protocol%20Architecture.md)
- [Query Engine Architecture](docs/02.%20Query%20Engine%20Architecture.md)
- [Storage Engine Architecture](docs/03.%20Storage%20Engine%20Architecture.md)
- [Performance Architecture](docs/04.%20Performance%20Architecture.md)
- [API Reference](docs/05.%20API%20Reference.md)

## Development

Rust stable is required; RocksDB is compiled automatically as part of the build. Run `cargo test` to execute the integration suite — it spawns a real server and speaks raw protocol v4 end-to-end, including restart persistence checks.

## Status

Alpha software. Single node: no replication, authentication, TTL, counters, batches, or paging. See the [API Reference](docs/05.%20API%20Reference.md) for the exact supported surface.

## License

See [LICENSE](LICENSE).
