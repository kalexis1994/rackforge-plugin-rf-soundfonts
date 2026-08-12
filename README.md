# RF-Soundfonts

RF-Soundfonts is a portable RackForge instrument plugin built around the
RustySynth SoundFont engine. Its factory program is the openly licensed
**YDP Grand Piano**, so RackForge can provide a playable instrument without
requiring proprietary ROMs or platform-specific binaries.

The released `.rfplugin` uses RackForge's `wasm-v1` runtime and runs unchanged
on Windows, Android, and Raspberry Pi.

## Install

Download `RF-Soundfonts.rfplugin` from the
[latest release](https://github.com/kalexis1994/rackforge-plugin-rf-soundfonts/releases/latest)
and install it from RackForge's Plugins section.

Once the first tagged release exists, the current package will also have a
stable download URL:

```text
https://github.com/kalexis1994/rackforge-plugin-rf-soundfonts/releases/latest/download/RF-Soundfonts.rfplugin
```

The plugin package contains:

- the portable WebAssembly component;
- PLAY and CONFIG Web surfaces;
- the YDP Grand Piano SoundFont and its attribution;
- runtime, parameter, and preset metadata.

## Repository layout

```text
portable-plugin/              Cross-platform wasm-v1 component and package
crates/rf-soundfonts-engine/  Sample-library parsing and playback engine
plugin/                       Legacy native adapter and Web surface
tools/                        Reproducible packaging and installation helpers
```

## Local development

For adjacent checkouts named `rackforge` and
`rackforge-plugin-rf-soundfonts`, copy `.cargo/config.toml.example` to
`.cargo/config.toml`. This replaces the pinned Git SDK sources with the local
RackForge API crates without changing release metadata.

```bash
cargo test --workspace --locked
```

Build the portable package with PowerShell 7 and a RackForge checkout:

```powershell
./tools/build-portable-package.ps1 `
  -RackForgeRoot ../rackforge `
  -Output ./artifacts/RF-Soundfonts.rfplugin
```

The packer downloads the upstream YDP Grand Piano archive, verifies both the
source archive and SoundFont SHA-256 digests, builds the WebAssembly component,
and asks RackForge's own store tool to create the final `.rfplugin` archive.

## Continuous delivery

GitHub Actions performs the following checks:

- pull requests and pushes to `main` test the complete workspace;
- every successful run builds `RF-Soundfonts.rfplugin` and its SHA-256 file as
  a workflow artifact retained for 30 days;
- a version tag such as `v0.2.0` must match the plugin manifest version;
- a successful version tag creates a permanent GitHub release containing
  `RF-Soundfonts.rfplugin` and `RF-Soundfonts.rfplugin.sha256`;
- rerunning a tag safely replaces incomplete release assets.

RackForge is public, so the workflow checks out the pinned SDK and package
tooling without deploy keys or repository secrets.

## Compatibility

RF-Soundfonts `0.2.0` targets RackForge Plugin API `1.7`, the portable
`wasm-v1` runtime, and the `little@1` controller surface. RackForge SDK sources
and package tooling remain pinned to tested Git revisions for reproducible
builds.

## Licensing

RF-Soundfonts is distributed under GPL-3.0-or-later. The YDP Grand Piano is
licensed under Creative Commons Attribution 3.0 Unported. RustySynth is
distributed under the MIT license. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)
for attribution and source links.
