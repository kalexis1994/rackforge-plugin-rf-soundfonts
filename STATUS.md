# RF-Soundfonts project status

Snapshot date: 2026-08-22

Plugin ID: `org.rackforge.rf-soundfonts`

## Product boundary

RF-Soundfonts is a library instrument and RF Bank authoring tool. It discovers
and plays sounds from installed DLS, SFZ and supported sample-library formats.
Its Builder owns sample zones and velocity layers inside an individual RF
instrument, but it does not create a second performance catalog or own LIVE
layers, splits or complete performance setups.

- PLAY browses libraries and selects one sound.
- CONFIG reports and manages installed sound resources.
- CONFIG's Builder creates and losslessly edits native RF Bank instruments.
- Creating or editing a Builder instrument auditions its current playable
  draft through the active RackForge MIDI and audio instance without installing
  a temporary library. This uses the explicit `edit_mode` protocol state (`0`
  normal, `1` editing) instead of inferring intent from the open tab.
- RackForge LIVE owns layers, splits, MIDI routing, levels and panorama.
- RackForge presets save the complete rack and the selected plugin sound.

## Implemented engine behavior

- RIFF DLS Level 1/2 parsing, instruments, regions and sample playback.
- SFZ library discovery and streaming sample playback.
- Experimental Kontakt-family document and container readers where the source
  material is not encrypted.
- Note and velocity region selection.
- PCM sample playback with root note, fine tuning, attenuation and loops.
- DLS amplitude and pitch envelopes plus supported LFO destinations.
- Up to 32 voices with sustain and deterministic voice cleanup.
- Live 14-bit pitch bend and CC1 modulation-wheel response.
- Sample-positioned MIDI and parameter events inside an audio block.
- Stereo output and allocation-free voice creation on the audio path.

## RackForge integration

- Native Plugin API 1.5 adapter for the dynamic library catalog.
- Portable `wasm-v1` SoundFont component for Windows, Android and Raspberry Pi.
- The portable component publishes a dynamic preset catalog for the loaded
  SoundFont, selects presets per bank and patch, and exposes a master-volume
  parameter with sample-accurate automation.
- A user bank chosen in CONFIG is installed into private plugin storage
  (`user-soundfont`), so it survives restarts and is delivered to every new
  instance after the factory bank.
- Read-only dynamic catalogs generated from installed libraries.
- State version 4 stores master gain and the selected library sound.
- State readers for versions 1 through 3. A version 3 Custom Program state is
  migrated to its primary library instrument; layer and effect overrides are
  intentionally discarded.
- Plugin-owned PLAY and CONFIG Web surfaces.
- LITTLE surface integration through the standard plugin catalog.

## Deliberately not implemented

- A second Custom Program or performance-program catalog.
- Plugin-owned layers, splits or performance routing.
- Plugin-owned chorus, exciter or room-reverb chains.
- Bank Select and Program Change MIDI.
- Per-channel multitimbral parts.
- Rewriting Kontakt-derived NKI maps from the browser Builder; they are
  preserved read-only when an RF Bank is saved.

## Current limitations

- The native adapter reads DLS and SFZ libraries. The portable component reads
  SF2 SoundFonts and packaged RF instrument libraries, including directly
  selected SFZ maps with their includes, WAV/FLAC samples and declared artwork.
  Portable DLS playback is still pending.
- DLS-2 articulation coverage is partial.
- Proprietary or encrypted library content may be rejected.
- Large libraries still require ARM64 memory and CPU profiling.

## Next recommended milestone

- Port DLS playback to the portable component so both runtimes play the same
  installed libraries.
- Run catalog, state and rendered-audio conformance tests across Windows,
  Android and Raspberry Pi.
- Add redistributable fixture banks so CI can exercise the complete loading
  path legally.
