#!/usr/bin/env bash
#
# Verify that the GraphBLAS pin is consistent across the three places that
# encode it:
#
#   1. crates/issundb-graphblas-sys/build.rs  (GRAPHBLAS_VERSION, _URL, _SHA256)
#   2. the external/GraphBLAS submodule        (GxB_IMPLEMENTATION_* in the header)
#   3. .gitmodules                             (the tracked branch/tag)
#
# build.rs uses the submodule for in-repo builds and the pinned tarball for
# crates.io consumers, so the two must describe the same release. By default
# this also downloads the pinned tarball and checks its SHA-256 against the
# constant, which catches both a stale checksum and an upstream archive that
# was regenerated. Pass --no-network (or set SKIP_DOWNLOAD=1) to skip only that
# last step for offline runs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

build_rs="crates/issundb-graphblas-sys/build.rs"
header="external/GraphBLAS/Include/GraphBLAS.h"

skip_download="${SKIP_DOWNLOAD:-0}"
[[ "${1:-}" == "--no-network" ]] && skip_download=1

fail() {
  echo "error: $*" >&2
  exit 1
}

# 1. Constants from build.rs.
version="$(grep -oP 'GRAPHBLAS_VERSION: &str = "\K[^"]+' "$build_rs" || true)"
url="$(grep -oP 'https://github\.com/[^"]+\.tar\.gz' "$build_rs" || true)"
sha="$(grep -oP 'GRAPHBLAS_SHA256: &str = "\K[0-9a-f]+' "$build_rs" || true)"
[[ -n "$version" ]] || fail "could not read GRAPHBLAS_VERSION from $build_rs"
[[ -n "$url" ]] || fail "could not read GRAPHBLAS_URL from $build_rs"
[[ -n "$sha" ]] || fail "could not read GRAPHBLAS_SHA256 from $build_rs"
echo "build.rs: version=$version sha256=$sha"

# 2. URL must point at the tag matching the version.
[[ "$url" == *"/v${version}.tar.gz" ]] \
  || fail "GRAPHBLAS_URL ($url) does not end in /v${version}.tar.gz"

# 3. .gitmodules tracked branch must be the matching tag.
branch="$(git config -f .gitmodules submodule.external/GraphBLAS.branch || true)"
[[ "$branch" == "v${version}" ]] \
  || fail ".gitmodules branch for external/GraphBLAS is '$branch', expected 'v${version}'"

# 4. Submodule source version must match.
[[ -f "$header" ]] \
  || fail "$header not found; run 'git submodule update --init external/GraphBLAS'"
major="$(grep -oP 'GxB_IMPLEMENTATION_MAJOR\s+\K[0-9]+' "$header")"
minor="$(grep -oP 'GxB_IMPLEMENTATION_MINOR\s+\K[0-9]+' "$header")"
sub="$(grep -oP 'GxB_IMPLEMENTATION_SUB\s+\K[0-9]+' "$header")"
submodule_version="${major}.${minor}.${sub}"
[[ "$submodule_version" == "$version" ]] \
  || fail "submodule GraphBLAS version is $submodule_version, build.rs pins $version"
echo "submodule: version=$submodule_version (matches)"

# 5. Pinned tarball checksum (network).
if [[ "$skip_download" == "1" ]]; then
  echo "skipping tarball download (--no-network)"
else
  tmp="$(mktemp)"
  trap 'rm -f "$tmp"' EXIT
  echo "downloading $url to verify checksum..."
  curl -sSfL --retry 3 -o "$tmp" "$url"
  actual="$(sha256sum "$tmp" | cut -d' ' -f1)"
  [[ "$actual" == "$sha" ]] \
    || fail "tarball checksum is $actual, build.rs pins $sha (stale pin or regenerated upstream archive)"
  echo "tarball: sha256=$actual (matches)"
fi

echo "GraphBLAS pin is consistent."
