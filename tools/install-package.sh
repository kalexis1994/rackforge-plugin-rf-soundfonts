#!/usr/bin/env bash
set -euo pipefail

package="${1:-}"
root="${RACKFORGE_ROOT:-$HOME/rackforge}"
destination="$root/plugins/rf-soundfonts"

if [[ -z "$package" || "$package" != *.rfplugin || ! -d "$package" ]]; then
  printf 'usage: %s PATH_TO.rfplugin\n' "$0" >&2
  exit 2
fi
test -f "$package/rackforge-plugin.toml"
test -f "$package/lib/librackforge_rf_soundfonts.so"
grep -qx 'id = "org.rackforge.rf-soundfonts"' "$package/rackforge-plugin.toml"

install -d "$root/plugins" "$root/backups"
stage="$(mktemp -d "$root/plugins/.rf-soundfonts-stage.XXXXXX")"
backup="$root/backups/rf-soundfonts-$(date -u +%Y%m%dT%H%M%SZ)-$$"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT
cp -a "$package/." "$stage/"

if [[ -d "$destination" ]]; then
  mv "$destination" "$backup"
fi
if ! mv "$stage" "$destination"; then
  if [[ -d "$backup" && ! -e "$destination" ]]; then
    mv "$backup" "$destination"
  fi
  exit 1
fi
trap - EXIT

printf 'RFPLUGIN_INSTALLED path=%s backup=%s\n' \
  "$destination" "$([[ -d "$backup" ]] && printf '%s' "$backup" || printf 'none')"
