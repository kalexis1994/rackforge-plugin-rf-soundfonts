# RackForge RF-Soundfonts

RF-Soundfonts is an independently versioned RackForge instrument plugin that reads
user-provided DLS Level 1/2 banks. This repository owns the DLS parser and
synthesizer, the native RackForge plugin adapter, the portable SoundFont
component, and their static Web surfaces.

The repository never distributes a Microsoft, Roland, or third-party sound
bank. Users install their own `.dls` resource through RackForge.

## Layout

```text
crates/rf-soundfonts-engine/  Sample-library parser and playback engine
plugin/                       Native RackForge ABI adapter and Web surfaces
portable-plugin/              Cross-platform wasm-v1 SoundFont component
```

## Local development

For adjacent checkouts named `rackforge` and `rackforge-plugin-rf-soundfonts`, copy
`.cargo/config.toml.example` to `.cargo/config.toml`. This replaces the pinned
Git SDK source with the local RackForge API crates without changing release
metadata.

```bash
cargo test --workspace
cargo build --release -p rackforge-rf-soundfonts
```

Package the current platform build on Linux with:

```bash
bash tools/build-package.sh
```

The resulting directory has the `.rfplugin` extension and contains only the
manifest, native binary, and static Web assets. DLS banks and user libraries
are external data and are never copied into the package.

## Continuous delivery

Every push to `main` builds and tests the portable WebAssembly plugin on GitHub
Actions. A successful run publishes `RF-Soundfonts-<version>.rfplugin` and its
SHA-256 checksum as a workflow artifact retained for 30 days. The workflow can
also be started manually from the Actions page.

The workflow reads the private RackForge SDK at its pinned revision through a
dedicated, read-only deploy key. The private half is stored in this repository
as the `RACKFORGE_DEPLOY_KEY` Actions secret; it grants no write access.

## Compatibility

Version `0.1.0` targets RackForge Plugin API 1.5 and `little@1`. Its SDK
dependency is pinned to RackForge source revision
`136960b3dd7748865b6a0a3d43af9c69bbcdea16` until the SDK crates receive
independent published releases.

During the split, `.cargo/config.toml` is intentionally local-only: it points
at the adjacent RackForge checkout containing API 1.5. Once those host changes
are committed, the Git revision above must be advanced to that commit before
publishing the first package.

The code originated in the RackForge monorepository and was separated after
RackForge commit `136960b3dd7748865b6a0a3d43af9c69bbcdea16`.
