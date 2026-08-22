# RF-Soundfonts

RF-Soundfonts is a portable RackForge instrument plugin built around the
RustySynth SoundFont engine and RF's resident sample engine. Its factory
program is the openly licensed
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

## Using the plugin

Out of the box the plugin plays the YDP Grand Piano. To add your own RF
instruments:

1. Open the plugin's **CONFIG** surface from RackForge's Plugins section and
   press **Add library** and choose an instrument file. RF-Soundfonts never
   receives a native path.
2. RF-Soundfonts asks RackForge to gather the selected instrument plus its
   sibling samples and artwork into one validated private resource. This is
   automatic and remains available after a restart.
3. CONFIG keeps every added source in a reusable library list. **Add library**
   remains available after each import; choosing a listed library makes it the
   active instrument without forgetting the others.
4. The **PLAY** surface keeps a stable browser and performance frame, while the
   active bank supplies the artwork, palette, wording and supported internal
   composition of its instrument stage. Banks without a presentation manifest
   receive a neutral SoundFont fallback instead of inheriting another bank's
   identity.
5. **Restore factory bank** in CONFIG uses a two-step confirmation before
   returning to the factory piano. Added libraries remain in the list.

Freely licensed SoundFonts are available from
[FreePats](https://freepats.zenvoid.org/) and other community collections.

### Imported RF instruments

Imported instruments may use ordinary WAV, WAVE, or FLAC samples. Referenced
TGA, PNG, and JPEG artwork is validated, converted to a bounded web image, and
shown by PLAY. If any referenced sample is missing, installation is rejected.

Supported RF signal processing keeps the source instrument's scope and order:
two-pole low-pass/high-pass and one-band parametric EQ run per voice/group
before mixing, then program insert reverb and stereo delay process the mixed instrument. The DSP
uses bounded delay memory, guarded feedback and finite-value protection for
stable real-time playback. PLAY exposes an automatable **FX Amount** control
when the selected instrument has a wet effect; 100% preserves the bank setting and 0%
keeps the direct sound while the internal effect tail remains ready.

Kontakt triangle pitch LFOs are retained per group, including their frequency,
delay, direct pitch depth, and MIDI CC route for performance-controlled
vibrato. Controller values are tracked independently per MIDI channel.

An RF bank remains useful as a transferable, browser-installable archive and
as the foundation for RF-authored instruments.

The converter preserves the folder structure, includes every compatible
instrument map, sample and artwork file, and writes a small `bank.json`. The
plugin keeps the compressed archive in memory but decodes only the samples used
by the selected instrument.

## Bank-owned presentation

PLAY is a constrained renderer rather than one hard-coded instrument skin.
Each packaged bank owns a `presentation.json` and its visual assets under
`web/banks/<bank-id>/`; `web/bank-presentations.json` only discovers those
profiles. The schema offers a bounded set of layouts and modules, so a bank can
look and read differently without injecting HTML, CSS or JavaScript. See
[`docs/BANK_PRESENTATIONS.md`](docs/BANK_PRESENTATIONS.md) for the contract.

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

RF-Soundfonts `0.5.14` targets RackForge Plugin API `1.7`, the portable
`wasm-v1` runtime, and the `little@1` controller surface. RackForge SDK sources
and package tooling remain pinned to tested Git revisions for reproducible
builds.

## Licensing

RF-Soundfonts is distributed under GPL-3.0-or-later. The YDP Grand Piano is
licensed under Creative Commons Attribution 3.0 Unported. RustySynth is
distributed under the MIT license. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)
for attribution and source links.
