# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
# Pinned to the workspace MSRV (rust-toolchain.toml). The official rust image
# has no trixie variant for this version, so the build stays on bookworm
# (Debian 12); the runtime stage below is trixie (Debian 13). Building against
# the older glibc and running on the newer one is safe because glibc is
# backward compatible, and a multi-stage build keeps this base out of the
# published image.
FROM rust:1.85-bookworm AS build

WORKDIR /src

# Native build dependencies for the engine's C/C++ libraries:
#   - Cmake builds the vendored SuiteSparse:GraphBLAS (suitesparse_graphblas_sys).
#   - Clang plus libclang-dev back the bindgen invocations.
#   - G++/gcc/make (build-essential) compile usearch and the LMDB sources.
#   - Pkg-config plus libssl-dev resolve the remaining system libraries.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    clang \
    libclang-dev \
    cmake \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Build the three facade binaries in one pass. The cargo registry and the
# target directory are BuildKit cache mounts, so repeat builds skip the
# expensive GraphBLAS compile; the binaries are copied out of the cached
# target tree within the same step because the mount does not persist.
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked \
        -p issundb-cli \
        -p issundb-rest \
        -p issundb-mcp \
    && mkdir -p /out \
    && cp target/release/issundb target/release/issundb-rest target/release/issundb-mcp /out/

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM debian:trixie-slim

# Shared libraries the binaries link at runtime: libgomp for the GraphBLAS
# OpenMP pool and libstdc++ for the C++ engine libraries. ca-certificates is
# included for completeness.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgomp1 \
    libstdc++6 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/issundb /out/issundb-rest /out/issundb-mcp /usr/local/bin/

# Default location for the LMDB database directory; mount a volume here to
# persist data across container restarts.
VOLUME ["/data"]

# The REST API listens here by default; the MCP HTTP transport uses 8000.
EXPOSE 7474

# Default to the REST server bound to all interfaces (the container network is
# the isolation boundary; TLS and auth are the reverse proxy's job). Override
# the command to run `issundb` (CLI) or `issundb-mcp` instead, for example:
#   docker run --rm -it -v db:/data IMAGE issundb /data
#   docker run --rm -p 8000:8000 -v db:/data IMAGE \
#     issundb-mcp --db-path /data --transport http --bind 0.0.0.0:8000
CMD ["issundb-rest", "--db-path", "/data", "--host", "0.0.0.0", "--port", "7474"]
