#!/bin/sh
# Install a local build. No remote scripts, service registration or sudo.
set -eu
project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
install_dir=${MUTLINK_INSTALL_DIR:-"$HOME/.local/bin"}
binary="$project_dir/target/release/mutlink"
if [ -f "$project_dir/mutlink" ]; then
  binary="$project_dir/mutlink"
fi
if [ ! -f "$binary" ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' 'Rust 1.88+ is required to build from source. Install Rust from https://rustup.rs, or use a prebuilt mutlink binary.' >&2
    exit 1
  fi
  cargo build --manifest-path "$project_dir/Cargo.toml" --release --locked
fi
mkdir -p "$install_dir"
if [ -e "$install_dir/mutlink" ]; then
  cp "$install_dir/mutlink" "$install_dir/mutlink.previous"
fi
cp "$binary" "$install_dir/.mutlink.new"
chmod 755 "$install_dir/.mutlink.new"
mv "$install_dir/.mutlink.new" "$install_dir/mutlink"
printf 'Installed: %s/mutlink\nAdd this directory to PATH if needed, then run: mutlink\n' "$install_dir"
