#!/usr/bin/env bash
# Build fully portable rcdu binaries.
#
#   * macOS  (native arch): depends only on libSystem/libiconv, which exist on every Mac.
#   * Linux  (x86_64 + aarch64): fully STATIC musl binaries — no libc, no .so, no coreutils.
#     Cross-compiled straight from macOS using the toolchain's bundled rust-lld; no Docker,
#     no zig, no external linker required.
#
# Output: dist/rcdu-portable-<os>-<arch>
set -euo pipefail
cd "$(dirname "$0")"

DIST=dist
mkdir -p "$DIST"

# Locate the rust-lld that ships inside the active toolchain.
LLD=$(find "$(rustc --print sysroot)" -name 'rust-lld' 2>/dev/null | head -1)
if [ -z "$LLD" ]; then
  echo "error: rust-lld not found in $(rustc --print sysroot)" >&2
  exit 1
fi

build_linux() {
  local target=$1 arch=$2
  echo ">> Linux static ($arch) :: $target"
  rustup target add "$target" >/dev/null 2>&1 || true
  RUSTFLAGS="-C linker=$LLD -C linker-flavor=ld.lld -C link-self-contained=yes -C strip=symbols" \
    cargo build --release --target "$target"
  cp "target/$target/release/rcdu" "$DIST/rcdu-portable-linux-$arch"
}

build_mac() {
  local arch
  arch=$(uname -m); [ "$arch" = "arm64" ] && arch=aarch64
  echo ">> macOS native ($arch)"
  RUSTFLAGS="-C strip=symbols" cargo build --release
  cp "target/release/rcdu" "$DIST/rcdu-portable-macos-$arch"
}

build_linux x86_64-unknown-linux-musl  x86_64
build_linux aarch64-unknown-linux-musl aarch64
build_mac

echo
echo "Built:"
for f in "$DIST"/*; do
  printf "  %-28s " "$(basename "$f")"
  file -b "$f" | cut -d, -f1-2
done
echo
echo "Copy any of these to a matching machine and run — no install, no dependencies."
