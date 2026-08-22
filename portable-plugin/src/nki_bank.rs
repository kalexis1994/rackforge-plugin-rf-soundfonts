//! Self-contained RF instrument banks for the portable processor.
//!
//! An RF bank retains its instrument maps and referenced audio, while decoding
//! only the samples needed by the selected instrument. That keeps a whole
//! library installable as one RackForge resource without decoding every
//! instrument into WebAssembly memory at startup.

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use rf_soundfonts::nki::document::{
    NkiDocument, NkiEnvelope, NkiFilter, NkiProgramEffect, NkiZone,
};
use rf_soundfonts::{
    EnvelopeSpec, LfoSpec, PitchEnvelopeSpec, SampleLoop, SampleParams, Voice, VoiceConfig, Wave,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::sync::Arc;
use zip::ZipArchive;

use crate::nki_effects::{EffectControls, EffectRack, StereoBiquad};

const MAX_ARCHIVE_ENTRIES: usize = 8_192;
const MAX_MAP_BYTES: u64 = 2 * 1_048_576;
const MAX_ARTWORK_BYTES: u64 = 4 * 1_048_576;
const MAX_SELECTED_SAMPLE_BYTES: u64 = 160 * 1_048_576;
const MAX_VOICES: usize = 128;
const MINIMUM_RELEASE_SECONDS: f32 = 0.005;
const ARTWORK_MARKER: &str = "\u{001e}RF_ARTWORK=";

#[derive(Deserialize)]
struct BankManifest {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Clone, Copy, Deserialize)]
struct RfEnvelope {
    #[serde(default)]
    attack_seconds: f32,
    #[serde(default = "default_decay")]
    decay_seconds: f32,
    #[serde(default = "default_sustain")]
    sustain_level: f32,
    #[serde(default = "default_release")]
    release_seconds: f32,
}

#[derive(Deserialize)]
struct RfLayer {
    sample: String,
    #[serde(default = "default_velocity_low")]
    velocity_low: u8,
    #[serde(default = "default_velocity_high")]
    velocity_high: u8,
    #[serde(default)]
    gain_db: f32,
    #[serde(default)]
    pan: f32,
    #[serde(default)]
    tune_semitones: f32,
    #[serde(default)]
    sample_start: usize,
}

#[derive(Deserialize)]
struct RfZone {
    #[serde(default)]
    name: String,
    key_low: u8,
    key_high: u8,
    root_key: u8,
    #[serde(default)]
    envelope_override: Option<RfEnvelope>,
    #[serde(default)]
    layers: Vec<RfLayer>,
}

#[derive(Deserialize)]
struct RfInstrument {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    artwork: Option<String>,
    envelope: RfEnvelope,
    #[serde(default)]
    zones: Vec<RfZone>,
}

fn default_decay() -> f32 {
    0.15
}
fn default_sustain() -> f32 {
    1.0
}
fn default_release() -> f32 {
    0.35
}
fn default_velocity_low() -> u8 {
    1
}
fn default_velocity_high() -> u8 {
    127
}

impl Default for RfEnvelope {
    fn default() -> Self {
        Self {
            attack_seconds: 0.0,
            decay_seconds: default_decay(),
            sustain_level: default_sustain(),
            release_seconds: default_release(),
        }
    }
}

impl RfEnvelope {
    fn validate(self, context: &str) -> Result<NkiEnvelope, String> {
        let values = [
            self.attack_seconds,
            self.decay_seconds,
            self.sustain_level,
            self.release_seconds,
        ];
        if values.iter().any(|value| !value.is_finite())
            || !(0.0..=30.0).contains(&self.attack_seconds)
            || !(0.0..=30.0).contains(&self.decay_seconds)
            || !(0.0..=1.0).contains(&self.sustain_level)
            || !(0.0..=30.0).contains(&self.release_seconds)
        {
            return Err(format!("{context} has an invalid envelope"));
        }
        Ok(NkiEnvelope {
            attack_seconds: self.attack_seconds,
            hold_seconds: 0.0,
            decay_seconds: self.decay_seconds,
            sustain_level: self.sustain_level,
            release_seconds: self.release_seconds,
        })
    }
}

impl RfInstrument {
    fn into_document(self, fallback_name: &str) -> Result<(String, String, NkiDocument), String> {
        if self.schema_version > 1 {
            return Err("this RF instrument schema is newer than the plugin".into());
        }
        let name = if self.name.trim().is_empty() {
            fallback_name.to_string()
        } else {
            self.name.trim().to_string()
        };
        let id = if self.id.trim().is_empty() {
            slug(&name)
        } else {
            slug(&self.id)
        };
        let global_envelope = self.envelope.validate("RF instrument")?;
        let mut groups = vec![Some(global_envelope)];
        let mut zones = Vec::new();
        for (zone_index, zone) in self.zones.into_iter().enumerate() {
            let zone_label = if zone.name.trim().is_empty() {
                format!("RF zone {}", zone_index + 1)
            } else {
                format!("RF zone {:?}", zone.name.trim())
            };
            if zone.key_low > zone.key_high
                || zone.key_high > 127
                || zone.root_key < zone.key_low
                || zone.root_key > zone.key_high
            {
                return Err(format!("{zone_label} has an invalid key range"));
            }
            let group = if let Some(envelope) = zone.envelope_override {
                groups.push(Some(envelope.validate(&zone_label)?));
                groups.len() - 1
            } else {
                0
            };
            if zone.layers.is_empty() {
                return Err(format!("{zone_label} has no velocity layers"));
            }
            for (layer_index, layer) in zone.layers.into_iter().enumerate() {
                if layer.sample.trim().is_empty()
                    || layer.velocity_low == 0
                    || layer.velocity_low > layer.velocity_high
                    || layer.velocity_high > 127
                    || !layer.gain_db.is_finite()
                    || !layer.pan.is_finite()
                    || !layer.tune_semitones.is_finite()
                    || !(-60.0..=24.0).contains(&layer.gain_db)
                    || !(-1.0..=1.0).contains(&layer.pan)
                    || !(-48.0..=48.0).contains(&layer.tune_semitones)
                {
                    return Err(format!("{zone_label} layer {} is invalid", layer_index + 1));
                }
                zones.push(NkiZone {
                    sample: basename(layer.sample.trim()).to_string(),
                    key_low: zone.key_low,
                    key_high: zone.key_high,
                    root_key: zone.root_key,
                    velocity_low: layer.velocity_low,
                    velocity_high: layer.velocity_high,
                    sample_start: layer.sample_start,
                    group,
                    volume: 10.0_f32.powf(layer.gain_db / 20.0),
                    pan: layer.pan,
                    tune: 2.0_f32.powf(layer.tune_semitones / 12.0),
                    sample_loop: None,
                });
            }
        }
        if zones.is_empty() {
            return Err("RF instrument has no playable zones".into());
        }
        Ok((
            id,
            name.clone(),
            NkiDocument {
                name,
                wallpaper: self.artwork.map(|value| basename(value.trim()).to_string()),
                zones,
                groups,
                group_lfos: BTreeMap::new(),
                effects: Default::default(),
            },
        ))
    }
}

#[derive(Clone)]
struct InstrumentEntry {
    id: String,
    name: String,
    document: NkiDocument,
    artwork_data_url: Option<String>,
}

#[derive(Clone, Copy)]
struct SampleEntry {
    archive_index: usize,
    expanded_bytes: u64,
}

#[derive(Clone, Copy)]
enum MapKind {
    Nki,
    Rf,
}

pub struct Library {
    bytes: Arc<[u8]>,
    id: String,
    name: String,
    instruments: Vec<InstrumentEntry>,
    samples: BTreeMap<String, SampleEntry>,
}

struct Region {
    zone: NkiZone,
    wave_index: usize,
    envelope: EnvelopeSpec,
    lfo: LfoSpec,
    modulation_cc: Option<u8>,
    filters: Vec<NkiFilter>,
}

struct ActiveVoice {
    channel: usize,
    voice: Voice,
    modulation_cc: Option<u8>,
    filters: Vec<StereoBiquad>,
}

struct LoadedInstrument {
    waves: Vec<Wave>,
    regions: Vec<Region>,
    voices: Vec<ActiveVoice>,
    held: [[bool; 128]; 16],
    sustain: [bool; 16],
    pitch_bend_cents: [f32; 16],
    controllers: [[f32; 128]; 16],
    effects: EffectRack,
}

pub struct Player {
    library: Library,
    loaded: Option<LoadedInstrument>,
    selected_id: Option<String>,
    sample_rate: u32,
}

fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, value)| value)
}

fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !output.is_empty() {
            output.push('-');
            separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "instrument".into()
    } else {
        output
    }
}

fn read_entry(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    index: usize,
    limit: u64,
) -> Result<Vec<u8>, String> {
    let mut file = archive
        .by_index(index)
        .map_err(|error| format!("cannot read bank entry {index}: {error}"))?;
    if file.size() > limit {
        return Err(format!("bank entry {:?} is too large", file.name()));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(file.size() as usize)
        .map_err(|_| format!("bank entry {:?} does not fit in memory", file.name()))?;
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot expand bank entry {:?}: {error}", file.name()))?;
    Ok(bytes)
}

fn artwork_data_url(bytes: &[u8], name: &str) -> Result<String, String> {
    let format = match extension(name).to_ascii_lowercase().as_str() {
        "png" => image::ImageFormat::Png,
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        _ => image::ImageFormat::Tga,
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4_096);
    limits.max_image_height = Some(4_096);
    limits.max_alloc = Some(32 * 1_048_576);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("cannot decode RF instrument artwork: {error}"))?;
    let thumbnail = image.thumbnail(320, 128);
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 62)
        .encode_image(&thumbnail)
        .map_err(|error| format!("cannot prepare RF instrument artwork: {error}"))?;
    Ok(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    ))
}

impl Library {
    pub fn parse(bytes: Vec<u8>) -> Result<Self, String> {
        let bytes: Arc<[u8]> = bytes.into();
        let mut archive = ZipArchive::new(Cursor::new(bytes.as_ref()))
            .map_err(|error| format!("invalid RF bank archive: {error}"))?;
        if archive.is_empty() || archive.len() > MAX_ARCHIVE_ENTRIES {
            return Err("RF bank has an invalid number of entries".into());
        }

        let mut manifest_index = None;
        let mut map_entries = Vec::new();
        let mut indexed_samples: BTreeMap<String, (SampleEntry, usize)> = BTreeMap::new();
        let mut indexed_artwork: BTreeMap<String, (SampleEntry, usize)> = BTreeMap::new();
        for index in 0..archive.len() {
            let file = archive
                .by_index(index)
                .map_err(|error| format!("cannot inspect bank entry {index}: {error}"))?;
            if file.is_dir() {
                continue;
            }
            if file.enclosed_name().is_none() {
                return Err(format!("unsafe path in RF bank: {:?}", file.name()));
            }
            let name = file.name().replace('\\', "/");
            let folded_extension = extension(&name).to_ascii_lowercase();
            if basename(&name).eq_ignore_ascii_case("bank.json") {
                manifest_index.get_or_insert(index);
            } else if folded_extension == "nki" {
                map_entries.push((name, index, MapKind::Nki));
            } else if folded_extension == "rfinstrument" {
                map_entries.push((name, index, MapKind::Rf));
            } else if matches!(folded_extension.as_str(), "wav" | "wave" | "flac") {
                let key = basename(&name).to_ascii_lowercase();
                let candidate = SampleEntry {
                    archive_index: index,
                    expanded_bytes: file.size(),
                };
                // A shallower path wins, matching the native Kontakt loader.
                let candidate_depth = name.matches('/').count();
                match indexed_samples.get(&key) {
                    Some((_, existing_depth)) => {
                        if candidate_depth < *existing_depth {
                            indexed_samples.insert(key, (candidate, candidate_depth));
                        }
                    }
                    None => {
                        indexed_samples.insert(key, (candidate, candidate_depth));
                    }
                }
            } else if matches!(folded_extension.as_str(), "tga" | "png" | "jpg" | "jpeg") {
                let key = basename(&name).to_ascii_lowercase();
                let candidate = SampleEntry {
                    archive_index: index,
                    expanded_bytes: file.size(),
                };
                let candidate_depth = name.matches('/').count();
                match indexed_artwork.get(&key) {
                    Some((_, existing_depth)) if candidate_depth >= *existing_depth => {}
                    _ => {
                        indexed_artwork.insert(key, (candidate, candidate_depth));
                    }
                }
            }
        }

        let manifest = if let Some(index) = manifest_index {
            let manifest_bytes = read_entry(&mut archive, index, MAX_MAP_BYTES)?;
            Some(
                serde_json::from_slice::<BankManifest>(&manifest_bytes)
                    .map_err(|error| format!("invalid bank.json: {error}"))?,
            )
        } else {
            None
        };
        if manifest
            .as_ref()
            .is_some_and(|value| value.schema_version > 1)
        {
            return Err("this RF bank schema is newer than the plugin".into());
        }

        map_entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut instruments = Vec::new();
        let mut used_ids = BTreeSet::new();
        for (path, index, kind) in map_entries {
            let map_bytes = read_entry(&mut archive, index, MAX_MAP_BYTES)?;
            let fallback = basename(&path)
                .rsplit_once('.')
                .map_or(basename(&path), |(stem, _)| stem);
            let (source_id, name, document, id_prefix) = match kind {
                MapKind::Nki => {
                    let text = rf_soundfonts::nki::document::inflate(&map_bytes)
                        .map_err(|error| format!("{}: {error}", basename(&path)))?;
                    let (document, _) = rf_soundfonts::nki::document::parse(&text)
                        .map_err(|error| format!("{}: {error}", basename(&path)))?;
                    let name = if document.name.trim().is_empty() {
                        fallback.to_string()
                    } else {
                        document.name.trim().to_string()
                    };
                    (slug(&name), name, document, "nki")
                }
                MapKind::Rf => {
                    let instrument =
                        serde_json::from_slice::<RfInstrument>(&map_bytes).map_err(|error| {
                            format!("{}: invalid RF instrument: {error}", basename(&path))
                        })?;
                    let (id, name, document) = instrument
                        .into_document(fallback)
                        .map_err(|error| format!("{}: {error}", basename(&path)))?;
                    (id, name, document, "rf")
                }
            };
            if document.zones.is_empty() {
                continue;
            }
            let missing: BTreeSet<_> = document
                .zones
                .iter()
                .map(|zone| zone.sample.to_ascii_lowercase())
                .filter(|sample| !indexed_samples.contains_key(sample))
                .collect();
            if !missing.is_empty() {
                let examples = missing
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "{} is missing {} referenced sample(s): {}",
                    basename(&path),
                    missing.len(),
                    examples
                ));
            }
            let base_id = source_id;
            let mut unique_id = base_id.clone();
            let mut suffix = 2;
            while !used_ids.insert(unique_id.clone()) {
                unique_id = format!("{base_id}-{suffix}");
                suffix += 1;
            }
            let artwork_data_url = document
                .wallpaper
                .as_ref()
                .and_then(|wallpaper| indexed_artwork.get(&wallpaper.to_ascii_lowercase()))
                .and_then(|(entry, _)| {
                    read_entry(
                        &mut archive,
                        entry.archive_index,
                        entry.expanded_bytes.min(MAX_ARTWORK_BYTES),
                    )
                    .ok()
                })
                .and_then(|bytes| {
                    document
                        .wallpaper
                        .as_deref()
                        .and_then(|name| artwork_data_url(&bytes, name).ok())
                });
            instruments.push(InstrumentEntry {
                id: format!("{id_prefix}.{unique_id}"),
                name,
                document,
                artwork_data_url,
            });
        }
        if instruments.is_empty() {
            return Err("RF bank contains no readable instruments".into());
        }

        let manifest_id = manifest.as_ref().map(|value| value.id.trim()).unwrap_or("");
        let manifest_name = manifest
            .as_ref()
            .map(|value| value.name.trim())
            .unwrap_or("");
        let samples = indexed_samples
            .into_iter()
            .map(|(key, (entry, _))| (key, entry))
            .collect();
        Ok(Self {
            bytes,
            id: if manifest_id.is_empty() {
                "rf-library".into()
            } else {
                slug(manifest_id)
            },
            name: if manifest_name.is_empty() {
                "RF instrument library".into()
            } else {
                manifest_name.into()
            },
            instruments,
            samples,
        })
    }

    fn load(&self, id: &str, sample_rate: u32) -> Result<LoadedInstrument, String> {
        let instrument = self
            .instruments
            .iter()
            .find(|instrument| instrument.id == id)
            .ok_or_else(|| format!("unknown RF instrument {id:?}"))?;
        let mut required = BTreeMap::<String, usize>::new();
        let mut sample_keys = Vec::new();
        let mut expanded_bytes = 0_u64;
        for zone in &instrument.document.zones {
            let key = zone.sample.to_ascii_lowercase();
            let Some(entry) = self.samples.get(&key) else {
                continue;
            };
            if !required.contains_key(&key) {
                expanded_bytes = expanded_bytes.saturating_add(entry.expanded_bytes);
                if expanded_bytes > MAX_SELECTED_SAMPLE_BYTES {
                    return Err(format!(
                        "instrument {:?} needs too much decoded sample data",
                        instrument.name
                    ));
                }
                let wave_index = sample_keys.len();
                required.insert(key.clone(), wave_index);
                sample_keys.push(key);
            }
        }
        if sample_keys.is_empty() {
            return Err(format!(
                "instrument {:?} found none of its samples",
                instrument.name
            ));
        }

        let mut archive = ZipArchive::new(Cursor::new(self.bytes.as_ref()))
            .map_err(|error| format!("cannot reopen RF bank: {error}"))?;
        let mut waves = Vec::with_capacity(sample_keys.len());
        for key in &sample_keys {
            let entry = self.samples[key];
            let bytes = read_entry(&mut archive, entry.archive_index, entry.expanded_bytes)?;
            let wave = if extension(key).eq_ignore_ascii_case("flac") {
                rf_soundfonts::flac::decode(&bytes, key.clone())
            } else {
                rf_soundfonts::wav::decode(&bytes, key.clone())
            }
            .map_err(|error| format!("cannot decode sample {key:?}: {error}"))?;
            waves.push(wave);
        }

        let mut regions = Vec::new();
        for zone in &instrument.document.zones {
            let key = zone.sample.to_ascii_lowercase();
            let Some(&wave_index) = required.get(&key) else {
                continue;
            };
            let envelope = instrument
                .document
                .groups
                .get(zone.group)
                .copied()
                .flatten()
                .map_or_else(EnvelopeSpec::default, envelope_from);
            let mut envelope = envelope;
            envelope.release_seconds = envelope.release_seconds.max(MINIMUM_RELEASE_SECONDS);
            let group_lfo = instrument.document.group_lfos.get(&zone.group).copied();
            regions.push(Region {
                zone: zone.clone(),
                wave_index,
                envelope,
                lfo: group_lfo.map_or_else(LfoSpec::default, |lfo| LfoSpec {
                    frequency_hz: lfo.frequency_hz,
                    delay_seconds: lfo.delay_seconds,
                    pitch_depth_cents: lfo.pitch_depth_cents,
                    mod_wheel_pitch_depth_cents: lfo.controller_pitch_depth_cents,
                    attenuation_depth_centibels: 0.0,
                    mod_wheel_attenuation_depth_centibels: 0.0,
                }),
                modulation_cc: group_lfo.and_then(|lfo| lfo.controller),
                filters: instrument
                    .document
                    .effects
                    .group_filters
                    .get(&zone.group)
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        if regions.is_empty() {
            return Err(format!(
                "instrument {:?} has no playable zones",
                instrument.name
            ));
        }
        let effects = EffectRack::new(&instrument.document.effects, sample_rate)
            .map_err(|error| format!("cannot prepare RF instrument effects: {error}"))?;
        Ok(LoadedInstrument {
            waves,
            regions,
            voices: Vec::new(),
            held: [[false; 128]; 16],
            sustain: [false; 16],
            pitch_bend_cents: [0.0; 16],
            controllers: [[0.0; 128]; 16],
            effects,
        })
    }

    fn catalog_json(&self) -> Vec<u8> {
        let presets: Vec<_> = self
            .instruments
            .iter()
            .enumerate()
            .map(|(order, instrument)| {
                let lowest = instrument
                    .document
                    .zones
                    .iter()
                    .map(|zone| zone.key_low)
                    .min();
                let highest = instrument
                    .document
                    .zones
                    .iter()
                    .map(|zone| zone.key_high)
                    .max();
                let summary = match (lowest, highest) {
                    (Some(low), Some(high)) => format!(
                        "RF Instrument · {} zones · keys {low}–{high}",
                        instrument.document.zones.len()
                    ),
                    _ => "RF instrument".into(),
                };
                let program_effects: Vec<_> = instrument
                    .document
                    .effects
                    .program
                    .iter()
                    .map(|effect| match effect {
                        NkiProgramEffect::Reverb(_) => "Reverb",
                        NkiProgramEffect::Delay(_) => "Delay",
                    })
                    .collect();
                let mut summary = summary;
                if !program_effects.is_empty() {
                    summary.push_str(" · FX ");
                    summary.push_str(&program_effects.join(" → "));
                }
                let filter_count: usize = instrument
                    .document
                    .effects
                    .group_filters
                    .values()
                    .map(Vec::len)
                    .sum();
                if filter_count > 0 {
                    summary.push_str(&format!(
                        " · {filter_count} group filter{}",
                        if filter_count == 1 { "" } else { "s" }
                    ));
                }
                let description = instrument
                    .artwork_data_url
                    .as_ref()
                    .map_or(summary.clone(), |artwork| {
                        format!("{summary}{ARTWORK_MARKER}{artwork}")
                    });
                serde_json::json!({
                    "id": instrument.id,
                    "name": instrument.name,
                    "bank": self.id,
                    "order": order,
                    "description": description,
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "banks": [{ "id": self.id, "name": self.name, "order": 0 }],
            "presets": presets,
        }))
        .unwrap_or_default()
    }
}

fn envelope_from(shape: NkiEnvelope) -> EnvelopeSpec {
    EnvelopeSpec {
        attack_seconds: shape.attack_seconds,
        decay_seconds: shape.decay_seconds,
        sustain_level: shape.sustain_level,
        release_seconds: shape.release_seconds,
    }
}

impl LoadedInstrument {
    fn reset(&mut self) {
        self.voices.clear();
        self.held = [[false; 128]; 16];
        self.sustain = [false; 16];
        self.pitch_bend_cents = [0.0; 16];
        self.controllers = [[0.0; 128]; 16];
        self.effects.reset();
    }

    fn release_note(&mut self, channel: usize, note: u8) {
        for active in &mut self.voices {
            if active.channel == channel && active.voice.note == note {
                active.voice.note_off();
            }
        }
    }

    fn note_on(&mut self, channel: usize, note: u8, velocity: u8, sample_rate: u32) {
        self.held[channel][note as usize] = true;
        for active in &mut self.voices {
            if active.channel == channel && active.voice.note == note {
                active.voice.fade_out(0.01);
            }
        }
        self.voices.retain(|voice| !voice.voice.is_finished());

        for region in &self.regions {
            let zone = &region.zone;
            if !(zone.key_low..=zone.key_high).contains(&note)
                || !(zone.velocity_low..=zone.velocity_high).contains(&velocity)
            {
                continue;
            }
            let wave = &self.waves[region.wave_index];
            let sample_loop = zone
                .sample_loop
                .or(wave.sample_params.and_then(|params| params.sample_loop))
                .filter(|looping| valid_loop(*looping, wave.frame_count()));
            let params = SampleParams {
                unity_note: u16::from(zone.root_key),
                fine_tune: 0,
                attenuation_db: 0.0,
                sample_loop,
            };
            let config = VoiceConfig {
                amplitude_envelope: region.envelope,
                pitch_envelope: PitchEnvelopeSpec::default(),
                lfo: region.lfo,
                pitch_offset_cents: 1_200.0 * zone.tune.max(f32::MIN_POSITIVE).log2(),
                pitch_bend_range_cents: 200.0,
                modulation_depth: 1.0,
                gain: zone.volume.max(0.0),
                pan: zone.pan,
                velocity_tracking: 1.0,
            };
            let Ok(mut voice) = Voice::from_wave(wave, params, note, velocity, sample_rate, config)
            else {
                continue;
            };
            if zone.sample_start > 0 && voice.start_at_frame(zone.sample_start).is_err() {
                continue;
            }
            self.voices.push(ActiveVoice {
                channel,
                voice,
                modulation_cc: region.modulation_cc,
                filters: region
                    .filters
                    .iter()
                    .filter_map(|filter| StereoBiquad::new(*filter, sample_rate))
                    .collect(),
            });
        }
        if self.voices.len() > MAX_VOICES {
            self.voices.drain(0..self.voices.len() - MAX_VOICES);
        }
    }

    fn dispatch_midi(&mut self, status: u8, first: u8, second: u8, sample_rate: u32) {
        let channel = usize::from(status & 0x0f);
        match status & 0xf0 {
            0x80 => {
                self.held[channel][first as usize] = false;
                if !self.sustain[channel] {
                    self.release_note(channel, first);
                }
            }
            0x90 if second == 0 => {
                self.held[channel][first as usize] = false;
                if !self.sustain[channel] {
                    self.release_note(channel, first);
                }
            }
            0x90 => self.note_on(channel, first, second, sample_rate),
            0xb0 => {
                self.controllers[channel][usize::from(first)] = f32::from(second) / 127.0;
                match first {
                    64 => {
                        let was_down = self.sustain[channel];
                        self.sustain[channel] = second >= 64;
                        if was_down && !self.sustain[channel] {
                            for note in 0..128_u8 {
                                if !self.held[channel][note as usize] {
                                    self.release_note(channel, note);
                                }
                            }
                        }
                    }
                    120 => {
                        for active in &mut self.voices {
                            if active.channel == channel {
                                active.voice.fade_out(0.005);
                            }
                        }
                    }
                    123 => {
                        for note in 0..128_u8 {
                            self.held[channel][note as usize] = false;
                            self.release_note(channel, note);
                        }
                    }
                    _ => {}
                }
            }
            0xe0 => {
                let value = u16::from(first) | (u16::from(second) << 7);
                self.pitch_bend_cents[channel] = (f32::from(value) - 8_192.0) / 8_192.0 * 200.0;
            }
            _ => {}
        }
    }

    fn render(
        &mut self,
        output: &mut [f32],
        channels: usize,
        start: usize,
        end: usize,
        gain: f32,
        fx_amount: f32,
    ) {
        for frame in start..end {
            let mut mixed = [0.0_f32; 2];
            for active in &mut self.voices {
                let channel = active.channel;
                let modulation = active.modulation_cc.map_or(0.0, |controller| {
                    self.controllers[channel][usize::from(controller)]
                });
                let mut value = active
                    .voice
                    .next_frame_modulated(self.pitch_bend_cents[channel], modulation);
                for filter in &mut active.filters {
                    value = filter.process(value);
                }
                mixed[0] += value[0];
                mixed[1] += value[1];
            }
            mixed = self.effects.process(mixed, fx_amount);
            let target = frame * channels;
            if channels == 1 {
                output[target] = (mixed[0] + mixed[1]) * 0.5 * gain;
            } else {
                output[target] = mixed[0] * gain;
                output[target + 1] = mixed[1] * gain;
            }
        }
        self.voices.retain(|voice| !voice.voice.is_finished());
    }
}

fn valid_loop(sample_loop: SampleLoop, frames: usize) -> bool {
    sample_loop.start < sample_loop.end && sample_loop.end <= frames
}

impl Player {
    pub fn from_bytes(bytes: Vec<u8>, sample_rate: u32) -> Result<Self, String> {
        Ok(Self {
            library: Library::parse(bytes)?,
            loaded: None,
            selected_id: None,
            sample_rate,
        })
    }

    pub fn first_id(&self) -> &str {
        &self.library.instruments[0].id
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if self.sample_rate == sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.loaded = self
            .selected_id
            .as_deref()
            .and_then(|id| self.library.load(id, sample_rate).ok());
    }

    pub fn load_preset(&mut self, id: &str) -> bool {
        match self.library.load(id, self.sample_rate) {
            Ok(loaded) => {
                self.loaded = Some(loaded);
                self.selected_id = Some(id.to_string());
                true
            }
            Err(_) => false,
        }
    }

    pub fn reset(&mut self) {
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.reset();
        }
    }

    pub fn set_effect_controls(&mut self, controls: EffectControls) {
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.effects.set_controls(controls);
        }
    }

    pub fn catalog_json(&self) -> Vec<u8> {
        self.library.catalog_json()
    }

    pub fn dispatch_midi(&mut self, status: u8, first: u8, second: u8) {
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.dispatch_midi(status, first, second, self.sample_rate);
        }
    }

    pub fn render(
        &mut self,
        output: &mut [f32],
        channels: usize,
        start: usize,
        end: usize,
        gain: f32,
        fx_amount: f32,
    ) {
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.render(output, channels, start, end, gain, fx_amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn slugs_are_stable_and_safe() {
        assert_eq!(slug("Hohner Colombiano"), "hohner-colombiano");
        assert_eq!(slug("  Sanfona — Original 2 "), "sanfona-original-2");
        assert_eq!(slug("***"), "instrument");
    }

    #[test]
    fn path_helpers_accept_both_separators() {
        assert_eq!(basename("Samples/Accordion\\C4.WAV"), "C4.WAV");
        assert_eq!(extension("C4.WAV"), "WAV");
    }

    #[test]
    fn rf_zone_envelope_is_inherited_or_overridden_as_a_unit() {
        let instrument: RfInstrument = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "glass-piano",
            "name": "Glass Piano",
            "envelope": {
                "attack_seconds": 0.01,
                "decay_seconds": 0.2,
                "sustain_level": 0.8,
                "release_seconds": 0.4
            },
            "zones": [
                {
                    "name": "Soft C4",
                    "key_low": 60,
                    "key_high": 63,
                    "root_key": 60,
                    "layers": [{ "sample": "soft.wav", "velocity_low": 1, "velocity_high": 80 }]
                },
                {
                    "name": "Hard E4",
                    "key_low": 64,
                    "key_high": 67,
                    "root_key": 64,
                    "envelope_override": {
                        "attack_seconds": 0.5,
                        "decay_seconds": 0.3,
                        "sustain_level": 0.6,
                        "release_seconds": 1.2
                    },
                    "layers": [{ "sample": "hard.wav", "velocity_low": 81, "velocity_high": 127 }]
                }
            ]
        }))
        .unwrap();
        let (id, name, document) = instrument.into_document("fallback").unwrap();
        assert_eq!(id, "glass-piano");
        assert_eq!(name, "Glass Piano");
        assert_eq!(document.groups.len(), 2);
        assert_eq!(document.zones[0].group, 0);
        assert_eq!(document.zones[1].group, 1);
        assert_eq!(document.groups[1].unwrap().attack_seconds, 0.5);
    }

    #[test]
    fn rf_bank_with_native_instrument_map_is_playable() {
        let mut wave = Vec::new();
        let frames = 960_u32;
        let data_bytes = frames * 2;
        wave.extend_from_slice(b"RIFF");
        wave.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16_u32.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&1_u16.to_le_bytes());
        wave.extend_from_slice(&48_000_u32.to_le_bytes());
        wave.extend_from_slice(&96_000_u32.to_le_bytes());
        wave.extend_from_slice(&2_u16.to_le_bytes());
        wave.extend_from_slice(&16_u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&data_bytes.to_le_bytes());
        for frame in 0..frames {
            let sample =
                ((frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 12_000.0) as i16;
            wave.extend_from_slice(&sample.to_le_bytes());
        }

        let cursor = Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("bank.json", options).unwrap();
        archive
            .write_all(br#"{"schema_version":1,"id":"builder-test","name":"Builder Test"}"#)
            .unwrap();
        archive
            .start_file("instruments/builder-test.rfinstrument", options)
            .unwrap();
        archive
            .write_all(
                br#"{
            "schema_version": 1,
            "id": "builder-test",
            "name": "Builder Test",
            "envelope": {
                "attack_seconds": 0.0,
                "decay_seconds": 0.1,
                "sustain_level": 1.0,
                "release_seconds": 0.1
            },
            "zones": [{
                "name": "Middle C",
                "key_low": 60,
                "key_high": 60,
                "root_key": 60,
                "layers": [{
                    "sample": "tone.wav",
                    "velocity_low": 1,
                    "velocity_high": 127
                }]
            }]
        }"#,
            )
            .unwrap();
        archive.start_file("samples/tone.wav", options).unwrap();
        archive.write_all(&wave).unwrap();
        let bytes = archive.finish().unwrap().into_inner();

        let mut player = Player::from_bytes(bytes, 48_000).unwrap();
        assert_eq!(player.first_id(), "rf.builder-test");
        assert!(player.load_preset("rf.builder-test"));
        player.dispatch_midi(0x90, 60, 100);
        let mut output = vec![0.0; 960 * 2];
        player.render(&mut output, 2, 0, 960, 1.0, 0.0);
        assert!(output.iter().any(|sample| sample.abs() > 0.01));
    }

    #[test]
    #[ignore = "set RF_SOUNDFONTS_RFBANK to a locally built RF bank"]
    fn a_real_bank_loads_every_instrument_and_renders_audio() {
        let path = std::env::var("RF_SOUNDFONTS_RFBANK").expect("RF_SOUNDFONTS_RFBANK is not set");
        let bytes = std::fs::read(path).expect("cannot read RF bank");
        let mut player = Player::from_bytes(bytes, 48_000).expect("cannot parse RF bank");
        let catalog_bytes = player.catalog_json();
        assert!(catalog_bytes.len() <= crate::MAX_TRANSFER_BYTES);
        let catalog: serde_json::Value = serde_json::from_slice(&catalog_bytes).unwrap();
        let artwork_count = catalog["presets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|preset| {
                preset["description"]
                    .as_str()
                    .is_some_and(|description| description.contains(ARTWORK_MARKER))
            })
            .count();
        assert_eq!(artwork_count, 5);
        let expected_effects = BTreeMap::from([
            ("AcordClari", (1, 2, 2)),
            ("Sanfona - Original-2", (1, 0, 0)),
            ("Hohner Colombiano", (1, 0, 0)),
            ("Hohner Student 72", (1, 0, 0)),
            ("Pollo Acord", (2, 1, 1)),
            ("AcordSaban", (1, 1, 1)),
            ("AcordSeles", (0, 3, 3)),
        ]);
        for instrument in &player.library.instruments {
            let expected = expected_effects
                .get(instrument.name.as_str())
                .unwrap_or_else(|| panic!("unexpected instrument {}", instrument.name));
            assert_eq!(
                (
                    instrument.document.effects.program.len(),
                    instrument
                        .document
                        .effects
                        .group_filters
                        .values()
                        .map(Vec::len)
                        .sum(),
                    instrument.document.group_lfos.len(),
                ),
                *expected,
                "wrong DSP/modulation topology for {}",
                instrument.name
            );
        }
        eprintln!(
            "catalog: {} bytes, {artwork_count} RF artwork images",
            catalog_bytes.len()
        );
        let instruments: Vec<_> = player
            .library
            .instruments
            .iter()
            .map(|instrument| {
                let low = instrument
                    .document
                    .zones
                    .iter()
                    .map(|zone| zone.key_low)
                    .min()
                    .unwrap();
                let high = instrument
                    .document
                    .zones
                    .iter()
                    .map(|zone| zone.key_high)
                    .max()
                    .unwrap();
                (
                    instrument.id.clone(),
                    instrument.name.clone(),
                    ((u16::from(low) + u16::from(high)) / 2) as u8,
                )
            })
            .collect();
        assert_eq!(instruments.len(), 7);
        for (id, name, note) in instruments {
            assert!(player.load_preset(&id), "cannot load {name}");
            player.dispatch_midi(0x90, note, 110);
            let mut output = vec![0.0; 16_384 * 2];
            player.render(&mut output, 2, 0, 16_384, 0.9, 1.0);
            let peak = output
                .iter()
                .fold(0.0_f32, |current, sample| current.max(sample.abs()));
            eprintln!("{name}: note {note}, peak {peak:.4}");
            assert!(peak > 1e-5, "{name} rendered silence");
        }
    }
}
