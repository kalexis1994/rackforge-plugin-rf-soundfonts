# Bank presentation contract

RF-Soundfonts PLAY has two layers:

- the stable renderer owns sound search, selection, status, host communication,
  accessibility, responsive behavior and parameter writes;
- a bank profile owns the instrument stage's artwork, palette, wording,
  composition and optional modules.

This follows the library-instrument model without cloning any product's visual
design. A bank supplies data and assets; it never supplies executable code.

## Discovery

`web/bank-presentations.json` uses schema version 1. It contains the neutral
fallback and a list of package-relative profile documents:

```json
{
  "schema_version": 1,
  "fallback": { "layout": "studio", "modules": ["instrument", "controls"] },
  "profiles": ["banks/ydp-grand-piano/presentation.json"]
}
```

References must match `banks/<bank-id>/presentation.json`. At most 64 profiles
are loaded. A missing, malformed or unsupported profile is ignored without
breaking PLAY.

## Profile

A profile has this shape:

```json
{
  "schema_version": 1,
  "id": "ydp-grand-piano",
  "match": { "bank_name_contains": ["ydp grand"] },
  "layout": "cinematic",
  "theme": {
    "ground": "#08110f",
    "surface": "#10231f",
    "accent": "#c8ead9",
    "structure": "#568f79"
  },
  "artwork": "banks/ydp-grand-piano/artwork.png",
  "artwork_alt": "A grand piano in a dark green recording studio",
  "copy": {
    "edition": "YDP GRAND · FACTORY EDITION",
    "kicker": "ACOUSTIC GRAND",
    "control_kicker": "PIANO OUTPUT",
    "control_title": "Performance",
    "engine": "RUSTYSYNTH · 96 VOICES",
    "footer": "YDP GRAND PIANO",
    "credit": "FREEPATS · CC BY 3.0"
  },
  "modules": ["artwork", "instrument", "controls", "keyboard"],
  "keyboard": true
}
```

Supported layouts are `studio`, `cinematic`, `compact` and `minimal`.
Supported modules are `artwork`, `instrument`, `controls` and `keyboard`.
Matching may use exact `bank_ids`, `bank_name_contains` or
`sound_name_contains`. Matching is case-insensitive and the first matching
profile wins.

Colors must be six-digit hexadecimal values. Artwork must stay under the
profile's `banks/` tree or the plugin's validated `branding/` tree. Copy fields,
profile counts, match arrays and path shapes are bounded by the renderer.
Unknown fields have no effect.

## External banks

A plain `.sf2` has no standard for arbitrary artwork or UI layout, so installed
external SoundFonts use the neutral fallback. Imported RF instrument maps may
name artwork; an RF bank carries that image beside its samples, and the processor
converts it to a small bounded JPEG data image for the matching sound. This is
artwork only—never HTML, CSS, or JavaScript. A richer RF-authored presentation
bundle will require a broader validated schema and host asset contract.
