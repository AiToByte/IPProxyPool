# IPProxyPool gateway image (Phase C6: multi-stage release build).
# Builder pins rust:1.93-bookworm to match the dev toolchain (rustc 1.93.0);
# runtime is slim + CA certs (reqwest TLS + GeoIP updater downloads).
FROM rust:1.93-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/pingora-proxy-gateway /usr/local/bin/pingora-proxy-gateway
EXPOSE 8916 9091
ENTRYPOINT ["/usr/local/bin/pingora-proxy-gateway"]
