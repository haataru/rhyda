# Build stage: compile rhydadb against bundled RocksDB on Ubuntu.
FROM ubuntu:24.04 AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential clang libclang-dev make pkg-config git \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable --profile minimal
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY vendor vendor
COPY src src
COPY tests tests
COPY docs docs
COPY README.md LICENSE ./
RUN cargo build --release --bin rhydadb --bin bench

# Runtime stage: minimal image with the server and the benchmark client.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/rhydadb /usr/local/bin/rhydadb
COPY --from=build /src/target/release/bench /usr/local/bin/bench
ENV RHYDADB_LISTEN=0.0.0.0:9042 \
    RHYDADB_DATA=/data
VOLUME /data
EXPOSE 9042
CMD ["/usr/local/bin/rhydadb"]
