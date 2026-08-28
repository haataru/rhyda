# HAProxy for RhydaDB (single-node)

RhydaDB is single-node and has no built-in auth/TLS (see `docs/05`). In prod it **must** sit behind a TLS-terminating proxy and a firewall that blocks `9042` from the Internet.

## Quick Start

```sh
# 1. Generate self-signed cert (or use Let's Encrypt)
openssl req -x509 -newkey rsa:2048 -keyout haproxy/rhyda.key -out haproxy/rhyda.crt -days 365 -nodes -subj "/CN=rhyda"
cat haproxy/rhyda.crt haproxy/rhyda.key > haproxy/rhyda.pem

# 2. Run RhydaDB (health on 8080)
RHYDADB_DATA=/data RHYDADB_LISTEN=127.0.0.1:9042 RHYDADB_HEALTH_LISTEN=127.0.0.1:8080 ./rhydadb

# 3. Run HAProxy
haproxy -f haproxy/haproxy.cfg

# 4. Firewall (see scripts/firewall.sh)
sudo ./scripts/firewall.sh
```

Clients connect to `haproxy:9043` with TLS:

```python
from cassandra.cluster import Cluster
from ssl import SSLContext, PROTOCOL_TLSv1_2
ctx = SSLContext(PROTOCOL_TLSv1_2)
ctx.check_hostname = False
ctx.verify_mode = 0  # or CERT_REQUIRED with ca.crt for mTLS
cluster = Cluster(["haproxy-host"], port=9043, ssl_context=ctx, protocol_version=4)
```

Health for `haproxy` and `k8s`:

- `GET http://127.0.0.1:8080/health` → `{"status":"ok","uptime_sec":123}` (200)
- `GET /ready` same, `GET /metrics` → Prometheus `rhydadb_up 1` / `rhydadb_uptime_seconds`

`haproxy.cfg` checks `127.0.0.1:9042` via `tcp-check` (CQL) and `127.0.0.1:8080` via `httpchk`. For `mTLS` uncomment `ca-file ... verify required` and distribute `ca.crt` to clients.

## Docker

```sh
docker build -t rhydadb:prod .
docker run -d --name rhydadb --security-opt seccomp=unconfined -p 127.0.0.1:9042:9042 -p 127.0.0.1:8080:8080 -v /data:/data -e RHYDADB_DATA=/data rhydadb:prod
docker run -d --name haproxy --net host -v $PWD/haproxy:/usr/local/etc/haproxy:ro haproxy:2.8 -f /usr/local/etc/haproxy/haproxy.cfg
```
