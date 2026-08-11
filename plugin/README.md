# RF-Soundfonts native adapter

This crate exposes the RF-Soundfonts sample engine through RackForge's native
Plugin API. It is the current dynamic-library implementation for DLS and SFZ
resources.

## Responsibilities

- Load the optional `dls-bank` and `sfz-library` resources supplied by the
  host.
- Publish a read-only catalog containing the instruments discovered in those
  libraries.
- Select and play exactly one library instrument per plugin instance.
- Process MIDI, master gain automation and stereo audio.
- Save the selected sound and master gain as opaque plugin state.
- Provide PLAY and CONFIG Web surfaces owned by the plugin.

Layers, splits, MIDI routing, panorama and complete performance setups belong
to the RackForge rack. This plugin intentionally has no Custom Program catalog,
program editor, program-extension ABI or plugin-owned effects chain.

## State compatibility

State version 4 stores:

- the selected library sound ID;
- master gain.

Readers for state versions 1, 2 and 3 remain available. When a version 3 state
contains a former Custom Program, RF-Soundfonts selects its first enabled DLS
source. Former layer overrides and effects are not retained because those
features now belong to RackForge.

## Resources

`dls-bank` is an optional DLS file. `sfz-library` is an optional directory
containing one or more SFZ instruments and their samples. At least one usable
library is needed to produce sound.

No Microsoft, Roland or third-party sound bank is distributed by this
repository.

## Development

From the repository root:

```bash
cargo test -p rackforge-rf-soundfonts
cargo build --release -p rackforge-rf-soundfonts
```
