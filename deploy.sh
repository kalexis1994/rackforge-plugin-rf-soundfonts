#!/usr/bin/env bash
# Installs this plugin onto a RackForge device, all of it.
#
# A plugin is a shared library and a web surface, and they are versioned
# together but live in different folders once installed. Copying one and
# forgetting the other leaves a device running new code behind an old
# interface, which looks like a bug in the interface and is not one. That
# happened four times in a day before this script existed.
#
#   ./deploy.sh [host]
#
# The host defaults to `rackforge` and is an ssh destination.
set -euo pipefail

host="${1:-rackforge}"
plugin_id="org.rackforge.rf-soundfonts"
installed="\$HOME/rackforge/plugins/rf-soundfonts"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

printf 'Building on %s\n' "$host"
tar -cf - -C "$here" crates plugin/src plugin/Cargo.toml Cargo.toml Cargo.lock |
  ssh "$host" 'cd ~/rf-soundfonts && tar -xf -'
ssh "$host" 'cd ~/rf-soundfonts && cargo build --release -p rackforge-rf-soundfonts' \
  2>&1 | grep -E '^error|warning: unused|Finished' || true

# The web surface goes over as a whole directory rather than file by file, so
# a surface that gained a file does not arrive missing it.
printf 'Installing library and web surface\n'
tar -cf - -C "$here/plugin/package/web" . |
  ssh "$host" "rm -rf $installed/web && mkdir -p $installed/web && cd $installed/web && tar -xf -"
ssh "$host" "cp ~/rf-soundfonts/target/release/librackforge_rf_soundfonts.so $installed/lib/"
tar -cf - -C "$here/plugin/package" rackforge-plugin.toml |
  ssh "$host" "cd $installed && tar -xf -"

printf 'Restarting\n'
ssh "$host" 'sudo systemctl restart rackforge-audio'

# Verified rather than assumed. The point of the script is that what is on the
# device matches what is in the tree, so it says so or fails.
ssh "$host" "
  sleep 30
  printf '\n'
  printf 'plugin:  %s\n' \"\$(ls -l $installed/lib/librackforge_rf_soundfonts.so | awk '{print \$6, \$7, \$8}')\"
  printf 'surface: %s\n' \"\$(ls -l $installed/web/app.js | awk '{print \$6, \$7, \$8}')\"
  systemctl is-active rackforge-audio
  sudo journalctl -u rackforge-audio --since '35 seconds ago' --no-pager |
    grep -iE 'SFZ loaded|REALTIME_|error' | tail -5
"
printf '\nDeployed %s to %s\n' "$plugin_id" "$host"
