#!/usr/bin/env sh
set -eu

repo="${ATELIER_REPO:-ChrisAdkin8/atelier}"
version="${ATELIER_VERSION:-latest}"
install_dir="${ATELIER_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"

case "$os:$arch" in
  Linux:x86_64)
    target="x86_64-unknown-linux-gnu"
    ;;
  Darwin:x86_64)
    echo "unsupported platform for v0.1.0 release assets: Intel macOS" >&2
    echo "Build from source with: cargo install --path crates/atelier-cli" >&2
    exit 1
    ;;
  Darwin:arm64 | Darwin:aarch64)
    target="aarch64-apple-darwin"
    ;;
  *)
    echo "unsupported platform: $os/$arch" >&2
    exit 1
    ;;
esac

asset="atelier-cli-$target.tar.gz"
base_url="https://github.com/$repo/releases"
if [ "$version" = "latest" ]; then
  download_url="$base_url/latest/download/$asset"
  checksum_url="$base_url/latest/download/$asset.sha256"
else
  download_url="$base_url/download/$version/$asset"
  checksum_url="$base_url/download/$version/$asset.sha256"
fi

tmp="${TMPDIR:-/tmp}/atelier-install.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $asset from $repo..."
curl -fsSL "$download_url" -o "$tmp/$asset"

if curl -fsSL "$checksum_url" -o "$tmp/$asset.sha256"; then
  (
    cd "$tmp"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c "$asset.sha256"
    else
      shasum -a 256 -c "$asset.sha256"
    fi
  )
else
  echo "warning: checksum asset not found; installing without checksum verification" >&2
fi

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$install_dir"
cp "$tmp/atelier-cli-$target/atelier" "$install_dir/atelier"
chmod 0755 "$install_dir/atelier"

echo "Installed atelier to $install_dir/atelier"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    echo "Add $install_dir to PATH to run atelier from any shell." >&2
    ;;
esac
