# S4 (CPU build) — multi-stage Dockerfile
#
# Build:
#   docker build -t s4:cpu .
# Run:
#   docker run --rm -p 8014:8014 s4:cpu \
#     --endpoint-url https://s3.us-east-1.amazonaws.com --host 0.0.0.0
#
# For GPU build see Dockerfile.gpu.

# ---- builder ----
FROM rust:1.95-slim-bookworm AS builder
WORKDIR /usr/src/s4

# build deps for zstd-sys + ring/openssl-free TLS via rustls
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config build-essential ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Cache deps separately for fast rebuilds
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY scripts ./scripts

RUN cargo build --release -p s4-server --bin s4

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/s4/target/release/s4 /usr/local/bin/s4
COPY LICENSE NOTICE /usr/share/doc/s4/

# Run as non-root
RUN useradd -r -u 10001 s4
USER s4

EXPOSE 8014
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://localhost:8014/health || exit 1

ENTRYPOINT ["/usr/local/bin/s4"]
CMD ["--host", "0.0.0.0", "--port", "8014", "--log-format", "json"]
