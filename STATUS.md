# RF-DLS project status

Snapshot date: 2026-08-02

Plugin version: `0.1.0`

Plugin ID: `org.rackforge.rf-dls`

## Current maturity

RF-DLS is a functional RackForge instrument for user-supplied DLS Level 1/2
banks. The parser, sample renderer, custom-program model, RackForge integration,
LITTLE surface, Web surfaces and native packaging path are implemented and
covered by automated tests.

The production plugin still uses RackForge's legacy native C ABI. It is not yet
a portable `wasm-v1` SDK component. Its native implementation remains the
working reference while the portable SDK acquires the dynamic control-plane
features required by a sample-bank plugin.

Verification on 2026-08-02: `cargo test --workspace --locked` passed all 31
plugin and engine tests on Windows x86-64 using the MSVC toolchain.

## Implemented engine behavior

- RIFF `DLS ` collections with `ptbl`, `wvpl`, instruments and regions.
- Note and velocity region selection.
- Mono PCM16 waves and `wsmp` root note, fine tuning, attenuation and loops.
- DLS EG1 amplitude envelope and EG2 pitch envelope.
- DLS LFO delay/rate with pitch and attenuation destinations.
- Up to 32 voices with Note On/Off, sustain and deterministic voice cleanup.
- Live 14-bit pitch bend and CC1 modulation-wheel response.
- Sample-positioned MIDI and parameter events inside an audio block.
- Stereo output and allocation-free voice creation on the audio path.

The DLS bank is a required external resource named `dls-bank`. No Microsoft,
Roland or third-party bank is distributed by this repository or package.

## RackForge integration

- Manifest targets Plugin API 1.5 and `little@1`.
- Dynamic read-only `DLS` catalog built from the installed bank.
- Editable `CUSTOM` catalog stored below the plugin-owned data directory.
- Complete opaque state version 3 with readers for state versions 1 and 2.
- Program document version 6 with migrations from versions 1 through 5.
- Layer A is mandatory; Layer B is optional and preserves its configuration
  while disabled.
- Per-layer source, key/velocity ranges, gain, transpose, fine tune, bend,
  modulation, amplitude/pitch envelopes and LFO overrides.
- Shared exciter, chorus and room-reverb chain with live preview.
- Declarative program editor used by LITTLE and plugin-owned PLAY/CONFIG Web
  surfaces.
- Transactional edit preview, save/cancel behavior and defensive loading of
  malformed or unsafe custom documents.

## MIDI behavior

Implemented:

- Note On and Note Off, including velocity-zero Note On.
- CC1 modulation wheel.
- CC64 sustain.
- CC120 All Sound Off.
- CC121 Reset All Controllers.
- CC123 All Notes Off.
- 14-bit pitch bend with a default range of two semitones.

Not implemented:

- Bank Select and Program Change.
- Per-channel multitimbral parts; all MIDI channels currently address the same
  plugin instance.

## Known format and synthesis limits

- DLS-2 articulation coverage is partial; filters and the complete modulation
  matrix are not interpreted.
- Wave formats other than mono PCM16 are unsupported.
- Proprietary bank chunks may be ignored or rejected.
- Per-layer pan and insertable per-layer FX are not implemented.
- Adding or removing custom-program files currently requires restarting the
  RF-DLS engine to rebuild its catalog.
- Correct playback depends on the user supplying a compatible, legally usable
  DLS bank; the DLS standard does not guarantee General MIDI contents.

## Portable SDK migration blockers

The audio algorithm is portable Rust, but replacing the native adapter requires
more than recompiling it:

1. The SDK must expose a dynamic preset catalog after `dls-bank` has been
   delivered and parsed; the current portable package catalog is static.
2. The versioned program-editor control plane must be available to portable
   guests, including preview, commit and cancellation.
3. Large resource ingestion and bank-owned sample memory need explicit bounded
   policies and ARM64 memory/CPU measurements.
4. The plugin state and custom-program migration corpus must produce identical
   results through native and portable hosts before the native package is
   retired.

Until those contracts exist, the native plugin is the supported implementation
and should not be replaced by a reduced portable wrapper.

## Next recommended milestone

- Add dynamic-catalog and program-editor capabilities to the generic RackForge
  portable SDK.
- Split DLS parsing/resource preparation from the real-time renderer at a
  versioned boundary.
- Build a portable conformance package and compare its catalog, state, program
  migrations and rendered audio against this native implementation.
- Add fixture banks that are redistributable or generated entirely by tests so
  CI can exercise parser and renderer integration legally.
