# syntax=docker/dockerfile:1

# -------- Build Stage --------
FROM rust:1.99-slim-bookworm AS builder

WORKDIR /build

# Build dependencies first so they stay in a cacheable layer. The placeholder
# sources are replaced by the real ones below.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --locked --release && \
    rm -rf src target/release/git-api target/release/deps/git_api* target/release/deps/libgit_api*

COPY src ./src
RUN cargo build --locked --release

# -------- Runtime Stage --------
FROM debian:bookworm-slim

# Git runs the repository commands; curl serves the health probe.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl git && \
    rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m appuser
USER appuser

COPY --from=builder /build/target/release/git-api /usr/local/bin/git-api

WORKDIR /projects/git-api
EXPOSE 8081

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8081/health || exit 1

CMD ["git-api"]
