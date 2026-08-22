# Changelog

All user-visible changes to the RF-Soundfonts plugin. Versions follow the
plugin manifest; a `vX.Y.Z` tag publishes the matching GitHub release.

## 0.5.13 — 2026-08-22

### Added

- Kontakt triangle pitch LFOs now retain their group frequency, delay, direct
  depth, and MIDI CC-controlled intensity route.
- The portable player tracks all MIDI CC values per channel so an authored
  vibrato route is not limited to a hard-coded global controller.

### Verified

- The seven-instrument accordion suite preserves seven LFOs across its authored
  groups while loading all 245 samples and rendering every instrument.

## 0.5.12 — 2026-08-21

### Changed

- Detailed effect parameters now render as a single vertical list with one
  full-width control per row.
- Parameter names, faders, and numeric values have dedicated columns so values
  remain aligned and never overlap adjacent text.

## 0.5.11 — 2026-08-21

### Changed

- The PLAY output heading now uses a narrow fixed column and smaller type,
  leaving more horizontal room for faders and effect controls.
- Long bank-provided channel labels truncate with an ellipsis and expose their
  full value as a hover tooltip.

## 0.5.10 — 2026-08-21

### Changed

- PLAY now remains constrained to the RackForge plugin viewport. The instrument
  rack scrolls internally when effect controls need more vertical space.
- Bank artwork, title, description, and metadata form a compact upper strip so
  the output and effect controls remain the visual and functional focus.

## 0.5.9 — 2026-08-21

### Changed

- The active RF effect module in PLAY expands in place for detailed editing and
  keeps MIX available in its compact header.
- RF Reverb exposes size, decay, pre-delay, damping, and stereo width. RF Delay
  exposes time, feedback, damping, and stereo crossfeed.
- Advanced effect controls are automatable, smoothed in the DSP, bounded for
  feedback safety, and reset relative to each instrument's authored program.

## 0.5.7 — 2026-08-21

### Changed

- PLAY now presents every previously added instrument library in a modern tree
  browser and activates libraries without returning to CONFIG.
- Libraries with multiple instruments expose them as nested variants; a
  single-instrument library remains one compact top-level item.
- CONFIG remains the only surface that can add, browse, clear, or otherwise
  administer library resources.

## 0.5.6 — 2026-08-21

### Changed

- Removed the decorative piano keyboard from PLAY so the shared instrument
  surface does not imply a particular instrument family.
- The wet-effect fader now names the loaded effect explicitly: **RF Reverb
  Mix**, **RF Delay Mix**, or **RF Reverb + Delay Mix**.

## 0.5.5 — 2026-08-21

### Changed

- Imported formats no longer expose third-party format or product branding in
  PLAY, CONFIG, catalog descriptions, errors, or package metadata. Every loaded
  source is presented consistently as an RF-Soundfonts instrument.

## 0.5.4 — 2026-08-21

### Changed

- CONFIG treats imported files as an added library collection rather than a
  one-time replacement choice. The **Add library** action remains visible.
- Previously added native file grants are listed and can be loaded again;
  restoring the factory piano no longer hides or forgets the collection.

## 0.5.3 — 2026-08-21

### Changed

- CONFIG now opens a filtered instrument selector instead of asking the user
  to authorize and browse a source folder.
- Direct instrument selections automatically gather sibling samples and artwork into
  the private validated resource; the temporary archive stays invisible.

## 0.5.2 — 2026-08-20

### Fixed

- Large legacy PCM instruments no longer exhaust Wasmtime's control-call
  fuel during `resource_end`. The tolerant WAV path now uses specialized,
  allocation-checked 8/16/24/32-bit conversion loops instead of a generic
  per-byte iterator.
- RackForge's new `validate-resource` diagnostic exercises the same empty
  sandbox and control-call budget used by the resource backend, catching
  failures that native Rust tests cannot reproduce.

## 0.5.1 — 2026-08-20

### Fixed

- Direct instrument installation now ships under a new immutable package version,
  ensuring RackForge cannot retain an earlier `0.5.0` WebAssembly component
  while CONFIG uses the newer dependency-bundling flow.
- Added an end-to-end regression that delivers each single-instrument RF
  bundle through `resource_begin`, `resource_write`, and `resource_end`.

### Added

- Legacy HP2 high-pass group filters with cutoff, resonance, stable
  12 dB/octave processing, and frequency-response verification.

## 0.5.0 — 2026-08-20

### Added

- Portable playback for imported RF instruments backed
  by ordinary WAV, WAVE, or FLAC samples.
- A self-contained RF bank archive format and converter, so RackForge can
  install an entire instrument library as one
  private resource without exposing native paths to the WebAssembly plugin.
- Sample-accurate note events with sustain pedal, pitch bend, modulation,
  per-zone key and velocity ranges, loops, start offsets, tuning, level, pan,
  and amplitude envelopes.
- Direct instrument installation on native RackForge hosts: the host gathers the
  selected map and its sibling dependencies without exposing a filesystem path
  to the portable processor. Missing referenced samples reject the install.
- Instrument artwork in TGA, PNG, or JPEG form is converted to bounded JPEG
  artwork and rendered as the selected instrument's bank-owned PLAY background.
- Native, real-time-safe effect translation: two-pole low-pass and
  parametric EQ filters retain their group scope, while ordered program insert
  racks reproduce reverb and stereo delay with bounded feedback and memory.
- An automatable **FX Amount** control in PLAY scales an instrument's wet reverb/delay
  signal without changing its direct voice, envelopes, or group filters.

### Changed

- CONFIG now discovers compatible RF instrument files. The existing user-bank
  resource remains in place, so SoundFont installation and restoration stay
  backward compatible.
- RF instrument archives are indexed at load time, while only the selected
  instrument's referenced samples are decoded into memory.
- PLAY describes each instrument's decoded effect topology and only shows the FX
  control for instruments that actually contain a wet program effect.

## 0.4.1 — 2026-08-20

### Changed

- PLAY and CONFIG now use a more modern glass treatment with layered blur,
  translucent surfaces, luminous borders, restrained aurora gradients and
  stronger depth between the browser, bank stage and controls.
- Tabs, search, bank cards, buttons, range controls and selection states gained
  consistent gradient highlights and clearer interactive feedback.
- The bank-owned palette still drives the instrument area, so the new visual
  system enriches each bank rather than replacing its identity.

## 0.4.0 — 2026-08-20

### Added

- PLAY is now a safe bank-presentation renderer. A bank profile owns its image,
  palette, copy, layout and visible modules while the browser and host controls
  remain stable.
- The factory YDP bank ships its presentation manifest and artwork together in
  `web/banks/ydp-grand-piano/`; unknown SoundFonts use a neutral fallback.
- Dynamic catalog bank names identify factory and user sources, preventing one
  bank's visual profile from leaking into another bank with a similar preset
  name.

### Changed

- The YDP factory view uses its recording-studio artwork as a cinematic bank
  stage rather than making every loaded SoundFont share the same sampler skin.

## 0.3.1 — 2026-08-20

### Added

- PLAY now uses an original sampler-workstation layout: a persistent searchable
  sound browser, loaded-instrument rack, channel output control and performance
  keyboard remain visible together.
- The interface adapts to narrow and short host frames without page scrolling.

### Fixed

- Host requests time out cleanly instead of leaving controls busy forever.
- Rapid folder navigation ignores stale responses, library entries are sorted,
  and only compatible `.sf2` banks are offered.
- Restoring the factory bank now requires a second confirmation and installed
  and factory status cards are strictly mutually exclusive.

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
