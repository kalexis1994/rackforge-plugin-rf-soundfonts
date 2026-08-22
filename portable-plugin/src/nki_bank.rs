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
    filters: Vec<NkiFilter>,
}

struct ActiveVoice {
    channel: usize,
    voice: Voice,
    filters: Vec<StereoBiquad>,
}

struct LoadedInstrument {
    waves: Vec<Wave>,
    regions: Vec<Region>,
    voices: Vec<ActiveVoice>,
    held: [[bool; 128]; 16],
    sustain: [bool; 16],
    pitch_bend_cents: [f32; 16],
    modulation: [f32; 16],
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
                map_entries.push((name, index));
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
        for (path, index) in map_entries {
            let map_bytes = read_entry(&mut archive, index, MAX_MAP_BYTES)?;
            let text = rf_soundfonts::nki::document::inflate(&map_bytes)
                .map_err(|error| format!("{}: {error}", basename(&path)))?;
            let (document, _) = rf_soundfonts::nki::document::parse(&text)
                .map_err(|error| format!("{}: {error}", basename(&path)))?;
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
            let fallback = basename(&path)
                .rsplit_once('.')
                .map_or(basename(&path), |(stem, _)| stem);
            let name = if document.name.trim().is_empty() {
                fallback.to_string()
            } else {
                document.name.trim().to_string()
            };
            let base_id = slug(&name);
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
                id: format!("nki.{unique_id}"),
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
            regions.push(Region {
                zone: zone.clone(),
                wave_index,
                envelope,
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
            modulation: [0.0; 16],
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
        self.modulation = [0.0; 16];
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
                lfo: LfoSpec::default(),
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
            0xb0 => match first {
                1 => self.modulation[channel] = f32::from(second) / 127.0,
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
            },
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
                let mut value = active
                    .voice
                    .next_frame_modulated(self.pitch_bend_cents[channel], self.modulation[channel]);
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
            ("AcordClari", (1, 2)),
            ("Sanfona - Original-2", (1, 0)),
            ("Hohner Colombiano", (1, 0)),
            ("Hohner Student 72", (1, 0)),
            ("Pollo Acord", (2, 1)),
            ("AcordSaban", (1, 1)),
            ("AcordSeles", (0, 3)),
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
                        .sum()
                ),
                *expected,
                "wrong effect topology for {}",
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
