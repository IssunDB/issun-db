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
#   - Cmake builds SuiteSparse:GraphBLAS from the external/GraphBLAS submodule
#     (issundb-graphblas-sys), position-independent with a dynamic libgomp.
#   - Clang plus libclang-dev back the bindgen invocations.
#   - G++/gcc/make (build-essential) compile GraphBLAS, usearch, and the LMDB sources.
#   - Pkg-config plus libssl-dev resolve the remaining system libraries.
# The build context must include the checked-out external/GraphBLAS submodule
# (run `git submodule update --init external/GraphBLAS` before building).
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
    && cp target/release/issundb-cli target/release/issundb-rest target/release/issundb-mcp /out/

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM debian:trixie-slim

# Shared libraries the binaries link: libstdc++ for the C++ engine libraries
# (usearch), and libgomp (the GNU OpenMP runtime) which GraphBLAS now links
# dynamically rather than bundling statically. libgcc_s ships in the slim base.
# ca-certificates is included for completeness.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libstdc++6 \
    libgomp1 \
    ca-certificates \
    nano htop duff \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/issundb-cli /out/issundb-rest /out/issundb-mcp /usr/local/bin/

# Default location for the LMDB database directory; mount a volume here to
# persist data across container restarts. ISSUNDB_DB_PATH makes every binary
# default to this directory, so no --db-path argument is needed. The REST and
# MCP defaults are set for container use (bound to all interfaces, MCP over
# Streamable HTTP) so each server with no flags listens on its published port;
# the binaries themselves default to loopback and MCP stdio when run outside
# the image.
ENV ISSUNDB_DB_PATH=/data
ENV ISSUNDB_REST_HOST=0.0.0.0
ENV ISSUNDB_REST_PORT=7474
ENV ISSUNDB_MCP_TRANSPORT=http
ENV ISSUNDB_MCP_BIND=0.0.0.0:8000
VOLUME ["/data"]

# The REST API listens here by default; the MCP HTTP transport uses 8000.
EXPOSE 7474

# Default to the interactive CLI against the mounted database (run with -it).
# The container network is the isolation boundary for the servers; TLS and auth
# are the reverse proxy's job. Override the command to run a server instead, for
# example:
#   docker run --rm -p 7474:7474 -v db:/data IMAGE issundb-rest
#   docker run --rm -p 8000:8000 -v db:/data IMAGE issundb-mcp
CMD ["issundb-cli"]
