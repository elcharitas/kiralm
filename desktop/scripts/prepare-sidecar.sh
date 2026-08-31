#!/bin/sh
set -eu

profile=${1:-release}
case "$profile" in
  release) cargo_profile=release ;;
  debug) cargo_profile=debug ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 2 ;;
esac

desktop_dir=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
repo_dir=$(CDPATH= cd -- "$desktop_dir/.." && pwd)
target_triple=$(rustc -vV | sed -n 's/^host: //p')
destination="$desktop_dir/src-tauri/binaries/kiralm-$target_triple"
temporary="$destination.$$.tmp"

npm --prefix "$repo_dir/ui" run build
if [ "$cargo_profile" = release ]; then
  cargo build --release --manifest-path "$repo_dir/Cargo.toml"
else
  cargo build --manifest-path "$repo_dir/Cargo.toml"
fi

mkdir -p "$desktop_dir/src-tauri/binaries"
cp "$repo_dir/target/$cargo_profile/kiralm" "$temporary"
chmod 755 "$temporary"
mv "$temporary" "$destination"
