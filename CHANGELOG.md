# Changelog

All user-visible changes to the RF-Soundfonts plugin. Versions follow the
plugin manifest; a `vX.Y.Z` tag publishes the matching GitHub release.

## 0.3.0 — 2026-08-20

### Added

- The surfaces have their own visual identity — a dark green instrument
  faceplate set in Strait — so the plugin reads as separately installed
  software rather than as part of the RackForge shell.
- PLAY and CONFIG are organised into tabs inside a fixed frame. Neither
  surface scrolls the page any more; only a long list scrolls, inside its own
  panel.
- The plugin publishes every preset of the loaded SoundFont as a dynamic
  catalog, grouped by bank, and plays the one you select. Saved racks that
  referenced the old factory preset id still restore.
- A `Volume` parameter (automatable, sample-accurate) — the plugin previously
  exposed no parameters at all.
- The PLAY surface shows the active sound, lists the loaded bank for one-click
  selection, and controls the volume. It was previously a static page.
- A SoundFont chosen in CONFIG is installed into private plugin storage and
  survives restarts. A `Restore factory bank` button removes it again on hosts
  that support `plugin.clear_resource`.

### Fixed

- The library browser no longer offers `.sf3` files, which the synthesizer
  cannot decode.
- `tools/build-portable-package.ps1` works on Windows again: it now uses the
  system `tar`, which understands Windows paths, instead of Git's MSYS one.
- The workspace, both package manifests, and the native runtime descriptor
  agree on one version, enforced by tests. The SDK is pinned to a single
  RackForge revision shared by Cargo.toml and CI.

### Changed

- CI gates on `cargo fmt` and `clippy -D warnings`, and caches the verified
  YDP source archive so packaging does not depend on the upstream mirror.

## 0.2.1 — 2026-08-12

- Includes the plugin license in the package.

## 0.2.0 — 2026-08-11

- First portable `wasm-v1` release: YDP Grand Piano factory bank, PLAY and
  CONFIG Web surfaces, reproducible packaging, and GitHub releases from CI.
