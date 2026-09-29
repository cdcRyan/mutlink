#!/bin/sh
# Download a release. Set MUTLINK_USE_GH=1 for authenticated private repositories.
set -eu
repo=${MUTLINK_REPO:-cdcRyan/mutlink}
version=${MUTLINK_VERSION:-latest}
install_dir=${MUTLINK_INSTALL_DIR:-"$HOME/.local/bin"}
use_gh=${MUTLINK_USE_GH:-0}
fail() { printf 'mutlink: %s\n' "$*" >&2; exit 1; }
printf '%s\n' "$repo" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' || fail "Invalid repository"
case "$(uname -s)" in
  Darwin) platform=apple-darwin ;;
  Linux) platform=unknown-linux-gnu ;;
  *) fail "Unsupported OS. On Windows use install.ps1." ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) fail "Unsupported CPU architecture: $(uname -m)" ;;
esac
if [ "$platform" = unknown-linux-gnu ]; then
  command -v getconf >/dev/null 2>&1 && getconf GNU_LIBC_VERSION >/dev/null 2>&1 ||
    fail "This release requires glibc Linux. On musl/Alpine, build from source."
fi
if command -v sha256sum >/dev/null 2>&1; then
  hash_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
  hash_tool=shasum
else
  fail "Install sha256sum or shasum to verify downloads."
fi
if [ "$use_gh" = 1 ]; then
  command -v gh >/dev/null 2>&1 || fail "Install GitHub CLI and run gh auth login first."
  if [ "$version" = latest ]; then
    version=$(gh release view --repo "$repo" --json tagName --jq .tagName)
  fi
else
  command -v curl >/dev/null 2>&1 || fail "curl is required."
  if [ "$version" = latest ]; then
    resolved=$(curl --proto '=https' --tlsv1.2 -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
      fail "Cannot find release. For a private repo, set MUTLINK_USE_GH=1 and log in with gh."
    version=${resolved##*/}
  fi
fi
printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$' ||
  fail "Invalid version; expected v0.1.0 or latest."
asset="mutlink-$version-$arch-$platform.tar.gz"
temp_dir=$(mktemp -d)
stage=
trap 'rm -rf "$temp_dir"; if [ -n "$stage" ]; then rm -f "$stage"; fi' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Downloading %s from %s…\n' "$asset" "$repo"
if [ "$use_gh" = 1 ]; then
  gh release download "$version" --repo "$repo" --pattern "$asset" --pattern SHA256SUMS --dir "$temp_dir"
else
  for file in "$asset" SHA256SUMS; do
    curl --proto '=https' --tlsv1.2 -fSL --retry 3 \
      "https://github.com/$repo/releases/download/$version/$file" -o "$temp_dir/$file"
  done
fi
expected=$(awk -v file="$asset" '$2 == file {print $1}' "$temp_dir/SHA256SUMS")
printf '%s\n' "$expected" | grep -Eq '^[a-fA-F0-9]{64}$' || fail "Missing or invalid SHA256 for $asset"
if [ "$hash_tool" = sha256sum ]; then
  actual=$(sha256sum "$temp_dir/$asset" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$temp_dir/$asset" | awk '{print $1}')
fi
[ "$expected" = "$actual" ] || fail "Checksum mismatch; existing installation was not changed."
tar -xzf "$temp_dir/$asset" -C "$temp_dir" mutlink
[ -f "$temp_dir/mutlink" ] && [ ! -L "$temp_dir/mutlink" ] || fail "Archive has no regular mutlink binary."
chmod 755 "$temp_dir/mutlink"
"$temp_dir/mutlink" --version
mkdir -p "$install_dir"
stage=$(mktemp "$install_dir/.mutlink.XXXXXXXX")
cp "$temp_dir/mutlink" "$stage"
chmod 755 "$stage"
if [ -e "$install_dir/mutlink" ]; then
  cp "$install_dir/mutlink" "$install_dir/mutlink.previous"
fi
mv -f "$stage" "$install_dir/mutlink"
stage=
printf 'Installed: %s/mutlink\n' "$install_dir"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) printf 'Add %s to PATH, then open a new terminal.\n' "$install_dir" ;;
esac
printf 'Run: mutlink --help\nAfter upgrading, run mutlink stop before starting a new session.\n'
