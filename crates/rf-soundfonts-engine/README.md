# RF-Soundfonts engine (preview)

Engine for the initial Downloadable Sounds Level 1/2 subset required by
RackForge. DLS standardizes the container and synthesizer behavior, but does
not require two collections to contain the same instruments, samples, regions,
loops, or articulations. It also does not guarantee a General MIDI map unless
the bank declares itself GM-compatible.

The repository does not include DLS banks or content extracted from them.

The initial implementation supports:

- RIFF `DLS ` collections;
- the `ptbl` table and `wvpl` pool;
- instruments, note/velocity regions, and wave links;
- mono PCM16 waves;
- `wsmp` tuning with signed fine correction, attenuation, and loops;
- the EG1 envelope in centibels from `art1`/`art2`;
- the EG2 pitch envelope, including `EG2 → Pitch` depth;
- DLS LFO frequency, delay, and `CC1`-controlled pitch/attenuation depth;
- offline rendering at 48 kHz;
- low-latency MIDI playback through ALSA on Linux ARM64.

Not every DLS-2 articulation destination, filter, modulation matrix, wave
format other than mono PCM16, or proprietary chunk is interpreted yet. A bank
that uses those capabilities can be valid DLS and still fall outside the
current RF-Soundfonts compatibility range.

```text
cargo run --release -- inspect /path/to/bank.dls
cargo run --release -- render /path/to/bank.dls 0 0 60 piano-c4.wav
rf-soundfonts-live --bank 0 --program 0 /path/to/bank.dls
```

Banks are user-provided resources and must have a license that permits their
use on the target device.
