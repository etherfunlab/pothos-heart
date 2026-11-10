# syntax=docker/dockerfile:1.6
# The pothos-heart image: one binary, `serve` by default, `migrate` on demand.
# Migrations are embedded at compile time; pothos.toml is read from the
# working directory unless POTHOS_WEIGHTS points elsewhere.

# ---- builder ----
FROM rust:1.98-bookworm AS builder
WORKDIR /build

# Pre-fetch crates so source edits don't bust this layer.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo fetch --locked && \
    rm -rf src

COPY build.rs ./
COPY migrations ./migrations
COPY src ./src
COPY pothos.toml ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked && \
    cp target/release/pothos-heart /pothos-heart

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /pothos-heart /usr/local/bin/pothos-heart
WORKDIR /app
COPY pothos.toml ./

ENV BIND_ADDR=0.0.0.0:8090 \
    RUST_LOG=info
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/pothos-heart"]
CMD ["serve"]
