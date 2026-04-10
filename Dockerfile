# ============================================================
# VTT Validator — Multi-stage Docker build
# ============================================================

# Stage 1: Build
FROM rust:1.85-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY bin/ bin/
COPY tests/ tests/

# Build release binary (cache cargo registry + target dir across builds)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin vtt-validator \
    && cp /build/target/release/vtt-validator /build/vtt-validator \
    && strip /build/vtt-validator

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 vtt

COPY --from=builder /build/vtt-validator /usr/local/bin/vtt-validator

RUN mkdir -p /data/vtt && chown vtt:vtt /data/vtt

USER vtt
WORKDIR /home/vtt

# RPC port
EXPOSE 9944
# P2P port
EXPOSE 30333

HEALTHCHECK --interval=10s --timeout=5s --retries=10 --start-period=15s \
  CMD curl -sf -X POST -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"vtt_chainStatus","params":[]}' \
  http://localhost:9944 || exit 1

ENTRYPOINT ["vtt-validator"]
