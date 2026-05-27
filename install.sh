#!/bin/sh
set -eu

repo="Abyss116/IndexSearch"
install_dir="${INDEXSEARCH_INSTALL_DIR:-"$HOME/.local/bin"}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os:$arch" in
  Linux:x86_64|Linux:amd64) asset="indexsearch-linux-x86_64.tar.gz" ;;
  Darwin:arm64|Darwin:aarch64) asset="indexsearch-macos-aarch64.tar.gz" ;;
  Darwin:x86_64|Darwin:amd64) asset="indexsearch-macos-x86_64.tar.gz" ;;
  *)
    echo "unsupported platform: $os $arch" >&2
    exit 1
    ;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

url="https://github.com/$repo/releases/latest/download/$asset"
echo "downloading $url"
curl -fsSL "$url" -o "$tmp/$asset"
tar -xzf "$tmp/$asset" -C "$tmp"

payload="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name 'indexsearch-*' | head -n 1)"
if [ -z "$payload" ]; then
  echo "archive layout changed" >&2
  exit 1
fi

mkdir -p "$install_dir"
"$payload/indexsearch" install --dir "$install_dir"

echo
"$install_dir/indexsearch" --version
echo "installed indexsearch, is, and is-daemon to $install_dir"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) echo "note: add $install_dir to PATH if your shell cannot find is" ;;
esac
