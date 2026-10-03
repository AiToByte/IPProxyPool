# IPProxyPool gateway image (Phase C6: multi-stage release build).
# Builder pins rust:1.93-bookworm to match the dev toolchain (rustc 1.93.0);
# runtime is slim + CA certs (reqwest TLS + GeoIP updater downloads).
FROM rust:1.93-bookworm AS builder

# OPT-R16 E7: the base image ships a C/C++ toolchain (cc/gcc) but NOT cmake.
# The pingora dependency chain pulls in `libz-ng-sys`, whose build script
# shells out to cmake to compile zlib-ng; without it the build dies with
#   thread 'main' panicked at cmake-0.1.58/src/lib.rs:1132:
#   failed to execute command: No such file or directory (os error 2)
#   is `cmake` not installed?
# This is invisible on Windows dev machines: the MSVC toolchain links zlib
# from the import library, so cmake is never invoked there. That asymmetry
# is why it only surfaced in the Docker build (CI run 37123522261,
# `Gateway image build`), and why it stayed red through several earlier runs
# that failed earlier for unrelated reasons.
#
# build-essential + pkg-config cover the other native crates in the tree
# (ring/openssl-sys/zstd); cmake is the piece that was actually missing.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Dependency layer: manifest first so the (slow) dependency build is cached
# and only re-runs when Cargo.lock changes, not on every source edit.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch so cargo notices the real sources replaced the stub.
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/pingora-proxy-gateway /usr/local/bin/pingora-proxy-gateway
EXPOSE 8916 9091
ENTRYPOINT ["/usr/local/bin/pingora-proxy-gateway"]