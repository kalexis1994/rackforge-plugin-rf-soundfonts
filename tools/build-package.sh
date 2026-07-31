#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
platform="${RACKFORGE_PLATFORM:-linux-aarch64}"
output="${1:-$repo_root/rf-dls-0.1.0.rfplugin}"
binary="$repo_root/target/release/librackforge_rf_dls.so"

case "$platform" in
  linux-aarch64|linux-x86_64) ;;
  *) printf 'Unsupported package platform: %s\n' "$platform" >&2; exit 2 ;;
esac
if [[ "$output" != *.rfplugin ]]; then
  printf 'Plugin package output must end in .rfplugin\n' >&2
  exit 2
fi
if [[ -e "$output" ]]; then
  printf 'Refusing to overwrite existing package %s\n' "$output" >&2
  exit 2
fi

cd "$repo_root"
cargo build --release -p rackforge-rf-dls
test -f "$binary"
install -d "$output/lib" "$output/web"
install -m 0644 plugin/package/rackforge-plugin.toml "$output/rackforge-plugin.toml"
cp -a plugin/package/web/. "$output/web/"
install -m 0755 "$binary" "$output/lib/librackforge_rf_dls.so"

printf 'RFPLUGIN_BUILT path=%s platform=%s sha256=%s\n' \
  "$output" "$platform" "$(sha256sum "$output/lib/librackforge_rf_dls.so" | awk '{print $1}')"
