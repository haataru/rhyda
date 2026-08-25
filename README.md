# RhydaDB

**A single-node CQL database in Rust. Speaks Cassandra protocol v4. Runs on sharded RocksDB. Over a million operations per second on one machine.**

```
READ   ~1.8M ops/s      WRITE (non-durable)  ~1.0M ops/s
MIXED  ~1.4M ops/s      WRITE (fsync durable) ~1.1M ops/s
                        READ under Linux      ~2.3M ops/s
```
*20-core desktop, NVMe, pipelined prepared statements. Full methodology in [docs/04](docs/04.%20Performance%20Architecture.md).*

## Why

You want Cassandra's protocol and driver ecosystem — without JVM, clusters,
and ops overhead. RhydaDB is a single binary that speaks native protocol v4,
stores data durably on local disk, and is fast enough to be interesting.

## Quickstart

```sh
cargo build --release
RHYDADB_DATA=./data ./target/release/rhydadb
```

Or with Docker:

```sh
docker build -t rhydadb .
docker run -d --name rhydadb --security-opt seccomp=unconfined -p 9042:9042 rhydadb
```

Connect with any CQL v4 driver:

```python
from cassandra.cluster import Cluster

s = Cluster(["127.0.0.1"], protocol_version=4).connect()
s.execute("CREATE KEYSPACE shop WITH replication = {'class':'SimpleStrategy','replication_factor':1}")
s.set_keyspace("shop")
s.execute("CREATE TABLE kv (k text PRIMARY KEY, v text)")

ins = s.prepare("INSERT INTO kv (k, v) VALUES (?, ?)")
for i in range(1000):
    s.execute(ins, (f"k{i}", "v" * 64))
print(s.execute("SELECT * FROM kv WHERE k = 'k7'").one())
```

Benchmark it yourself:

```sh
./target/release/bench_async --mode mixed --conns 128 --pipeline 256 --seconds 10
```

## What you get

- **Native Protocol v4** — works with official Cassandra drivers; prepared statements, LZ4/Snappy.
- **Pipelined server** — concurrent request execution per connection, out-of-order stream responses, zero locks on the hot path.
- **Sharded storage engine** — up to 64 independent RocksDB instances, partition-hashed routing; writes scale past a single memtable lock.
- **Real durability option** — double-buffered fsync group commit: ~1 fsync per batch of up to 2048 writes, strict read-your-writes preserved.
- **O(1) DDL** — DROP/TRUNCATE via range tombstones, instant at any table size.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `RHYDADB_DATA` | `./data` | Data directory |
| `RHYDADB_LISTEN` | `127.0.0.1:9042` | Bind address |
| `RHYDADB_SYNC` | unset | Enable fsync group-commit durability |
| `RHYDADB_ENGINE_SHARDS` | `min(cpus, 8)` | Engine count, 1–64 |

## Status

Alpha. Single node. No replication, no auth/TLS, no TTL/counters/BATCH/paging.
Excellent as a dev/test Cassandra stand-in, an embedded-style high-throughput
local store, or a base to build those features on.

## Documentation

- [01. Server and Protocol Architecture](docs/01.%20Server%20and%20Protocol%20Architecture.md)
- [02. Query Engine Architecture](docs/02.%20Query%20Engine%20Architecture.md)
- [03. Storage Engine Architecture](docs/03.%20Storage%20Engine%20Architecture.md)
- [04. Performance Architecture](docs/04.%20Performance%20Architecture.md)
- [05. API Reference](docs/05.%20API%20Reference.md)

## Testing

```sh
cargo test
```

Integration tests spawn a real server and speak raw protocol v4: full CQL
flow, prepared statements, concurrent hammering, garbage-frame resilience,
restart persistence.

## License

See [LICENSE](LICENSE).
