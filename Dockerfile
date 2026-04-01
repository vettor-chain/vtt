# ============================================================
# VTT Validator — Multi-stage Docker build
# ============================================================

# Stage 1: Build
FROM rust:1.83-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy workspace manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY bin/ bin/

# Build release binary
RUN cargo build --release --bin vtt-validator \
    && strip /build/target/release/vtt-validator

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 vtt

COPY --from=builder /build/target/release/vtt-validator /usr/local/bin/vtt-validator

USER vtt
WORKDIR /home/vtt

# RPC port
EXPOSE 9944
# P2P port
EXPOSE 30333

ENTRYPOINT ["vtt-validator"]
