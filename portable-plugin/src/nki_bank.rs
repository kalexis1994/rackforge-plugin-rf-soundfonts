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
use rf_soundfonts::sfz::instrument::{CcState, SampledRegion, control_defaults, resident_region};
use rf_soundfonts::sfz::parse::{Curve, SfzDocument};
use rf_soundfonts::{
    EnvelopeSpec, LfoSpec, PitchEnvelopeSpec, SampleLoop, SampleParams, Voice, VoiceConfig, Wave,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use zip::ZipArchive;

use crate::nki_effects::{EffectControls, EffectRack, StereoBiquad};

const MAX_ARCHIVE_ENTRIES: usize = 8_192;
const MAX_MAP_BYTES: u64 = 2 * 1_048_576;
const MAX_EXPANDED_SFZ_BYTES: usize = 16 * 1_048_576;
const MAX_SFZ_REGIONS: usize = 8_192;
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
    #[serde(default)]
    instrument_path: String,
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
    map: InstrumentMap,
    artwork_data_url: Option<String>,
}

#[derive(Clone)]
enum InstrumentMap {
    Mapped(NkiDocument),
    Sfz(SfzMap),
}

#[derive(Clone)]
struct SfzMap {
    document: SfzDocument,
    /// Original `sample=` value to normalised archive path.
    sample_paths: BTreeMap<String, String>,
    /// Explicit surface artwork declared by the instrument, when present.
    artwork_path: Option<String>,
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
    Sfz,
}

pub struct Library {
    bytes: Arc<[u8]>,
    id: String,
    name: String,
    instruments: Vec<InstrumentEntry>,
    /// Basename lookup retained for Kontakt and native RF maps.
    samples: BTreeMap<String, SampleEntry>,
    /// Full, case-folded archive paths used by SFZ.
    sample_paths: BTreeMap<String, SampleEntry>,
}

struct Region {
    zone: NkiZone,
    wave_index: usize,
    envelope: EnvelopeSpec,
    lfo: LfoSpec,
    modulation_cc: Option<u8>,
    filters: Vec<NkiFilter>,
    sfz: Option<SampledRegion>,
}

struct ActiveVoice {
    channel: usize,
    voice: Voice,
    modulation_cc: Option<u8>,
    filters: Vec<StereoBiquad>,
    group: u32,
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
    sfz_curves: BTreeMap<u32, Curve>,
    sfz_defaults: CcState,
    sfz_normalisation: f32,
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

fn normalize_archive_path(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if path.starts_with('/') || path.contains(':') {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            value => parts.push(value),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn archive_parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn resolve_archive_reference(base: &str, prefix: &str, reference: &str) -> Option<String> {
    let combined = [base, prefix.trim_matches(['/', '\\']), reference]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    normalize_archive_path(&combined)
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

fn parse_sfz_map(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    path: &str,
    map_bytes: &[u8],
    text_entries: &BTreeMap<String, SampleEntry>,
    sample_entries: &BTreeMap<String, SampleEntry>,
    artwork_entries: &BTreeMap<String, SampleEntry>,
) -> Result<SfzMap, String> {
    let source = String::from_utf8_lossy(map_bytes);
    let base = archive_parent(path);
    let mut macros = BTreeMap::new();
    let expanded = rf_soundfonts::sfz::preprocess::expand_text_with(
        &source,
        Path::new(base),
        &mut macros,
        |include| {
            let display = include.to_string_lossy();
            let normalized = normalize_archive_path(&display).ok_or_else(|| {
                rf_soundfonts::SoundfontError::Invalid(format!(
                    "unsafe SFZ include path {display:?}"
                ))
            })?;
            let key = normalized.to_ascii_lowercase();
            let entry = text_entries.get(&key).ok_or_else(|| {
                rf_soundfonts::SoundfontError::Invalid(format!(
                    "missing SFZ include {normalized:?}"
                ))
            })?;
            let bytes = read_entry(archive, entry.archive_index, MAX_MAP_BYTES)
                .map_err(rf_soundfonts::SoundfontError::Invalid)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        },
    )
    .map_err(|error| format!("{}: {error}", basename(path)))?;
    if expanded.len() > MAX_EXPANDED_SFZ_BYTES {
        return Err(format!(
            "{} expands beyond the supported SFZ text size",
            basename(path)
        ));
    }
    let document = rf_soundfonts::sfz::parse::parse(&expanded)
        .map_err(|error| format!("{}: {error}", basename(path)))?;
    if document.regions.len() > MAX_SFZ_REGIONS {
        return Err(format!("{} declares too many SFZ regions", basename(path)));
    }
    let default_path = document
        .control
        .get("default_path")
        .map(String::as_str)
        .unwrap_or("");
    let mut sample_paths = BTreeMap::new();
    let mut missing = BTreeSet::new();
    for region in &document.regions {
        let Some(reference) = region.get("sample") else {
            continue;
        };
        let Some(resolved) = resolve_archive_reference(base, default_path, reference) else {
            return Err(format!(
                "{} contains an unsafe sample path {reference:?}",
                basename(path)
            ));
        };
        let key = resolved.to_ascii_lowercase();
        if sample_entries.contains_key(&key) {
            sample_paths.insert(reference.clone(), key);
        } else {
            missing.insert(resolved);
        }
    }
    if !missing.is_empty() {
        let examples = missing
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "{} is missing {} referenced sample(s): {}",
            basename(path),
            missing.len(),
            examples
        ));
    }
    if sample_paths.is_empty() {
        return Err(format!("{} references no playable samples", basename(path)));
    }
    let artwork_path = ["image", "image_controls", "image_hd"]
        .into_iter()
        .filter_map(|opcode| document.control.get(opcode))
        .find_map(|reference| {
            resolve_archive_reference(base, "", reference)
                .map(|path| path.to_ascii_lowercase())
                .filter(|path| artwork_entries.contains_key(path))
        });
    Ok(SfzMap {
        document,
        sample_paths,
        artwork_path,
    })
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
        let mut indexed_sample_paths = BTreeMap::new();
        let mut indexed_text = BTreeMap::new();
        let mut indexed_artwork: BTreeMap<String, (SampleEntry, usize)> = BTreeMap::new();
        let mut indexed_artwork_paths = BTreeMap::new();
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
            } else if folded_extension == "sfz" {
                map_entries.push((name.clone(), index, MapKind::Sfz));
                let path = normalize_archive_path(&name)
                    .ok_or_else(|| format!("unsafe SFZ path in RF bank: {name:?}"))?;
                indexed_text.insert(
                    path.to_ascii_lowercase(),
                    SampleEntry {
                        archive_index: index,
                        expanded_bytes: file.size(),
                    },
                );
            } else if matches!(folded_extension.as_str(), "txt" | "inc" | "sfzh") {
                let path = normalize_archive_path(&name)
                    .ok_or_else(|| format!("unsafe include path in RF bank: {name:?}"))?;
                indexed_text.insert(
                    path.to_ascii_lowercase(),
                    SampleEntry {
                        archive_index: index,
                        expanded_bytes: file.size(),
                    },
                );
            } else if matches!(folded_extension.as_str(), "wav" | "wave" | "flac") {
                let key = basename(&name).to_ascii_lowercase();
                let candidate = SampleEntry {
                    archive_index: index,
                    expanded_bytes: file.size(),
                };
                let path = normalize_archive_path(&name)
                    .ok_or_else(|| format!("unsafe sample path in RF bank: {name:?}"))?
                    .to_ascii_lowercase();
                if indexed_sample_paths
                    .insert(path.clone(), candidate)
                    .is_some()
                {
                    return Err(format!("duplicate sample path in RF bank: {path:?}"));
                }
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
                let path = normalize_archive_path(&name)
                    .ok_or_else(|| format!("unsafe artwork path in RF bank: {name:?}"))?
                    .to_ascii_lowercase();
                if indexed_artwork_paths
                    .insert(path.clone(), candidate)
                    .is_some()
                {
                    return Err(format!("duplicate artwork path in RF bank: {path:?}"));
                }
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

        if let Some(selected_path) = manifest
            .as_ref()
            .map(|value| value.instrument_path.trim())
            .filter(|value| !value.is_empty())
        {
            let selected_path = normalize_archive_path(selected_path)
                .ok_or_else(|| "bank.json has an unsafe instrument_path".to_string())?;
            map_entries.retain(|(path, _, _)| path.eq_ignore_ascii_case(&selected_path));
            if map_entries.is_empty() {
                return Err(format!(
                    "bank.json selects a missing instrument {selected_path:?}"
                ));
            }
        }

        map_entries.sort_by(|left, right| left.0.cmp(&right.0));
        let mut instruments = Vec::new();
        let mut used_ids = BTreeSet::new();
        for (path, index, kind) in map_entries {
            let map_bytes = read_entry(&mut archive, index, MAX_MAP_BYTES)?;
            let fallback = basename(&path)
                .rsplit_once('.')
                .map_or(basename(&path), |(stem, _)| stem);
            let (source_id, name, map, id_prefix) = match kind {
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
                    (slug(&name), name, InstrumentMap::Mapped(document), "nki")
                }
                MapKind::Rf => {
                    let instrument =
                        serde_json::from_slice::<RfInstrument>(&map_bytes).map_err(|error| {
                            format!("{}: invalid RF instrument: {error}", basename(&path))
                        })?;
                    let (id, name, document) = instrument
                        .into_document(fallback)
                        .map_err(|error| format!("{}: {error}", basename(&path)))?;
                    (id, name, InstrumentMap::Mapped(document), "rf")
                }
                MapKind::Sfz => {
                    let sfz = parse_sfz_map(
                        &mut archive,
                        &path,
                        &map_bytes,
                        &indexed_text,
                        &indexed_sample_paths,
                        &indexed_artwork_paths,
                    )?;
                    let name = fallback.to_string();
                    (slug(&name), name, InstrumentMap::Sfz(sfz), "sfz")
                }
            };
            if let InstrumentMap::Mapped(document) = &map {
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
            }
            let base_id = source_id;
            let mut unique_id = base_id.clone();
            let mut suffix = 2;
            while !used_ids.insert(unique_id.clone()) {
                unique_id = format!("{base_id}-{suffix}");
                suffix += 1;
            }
            let artwork_data_url = match &map {
                InstrumentMap::Mapped(document) => document
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
                    }),
                InstrumentMap::Sfz(sfz) => sfz.artwork_path.as_ref().and_then(|path| {
                    let entry = indexed_artwork_paths.get(path)?;
                    let bytes = read_entry(
                        &mut archive,
                        entry.archive_index,
                        entry.expanded_bytes.min(MAX_ARTWORK_BYTES),
                    )
                    .ok()?;
                    artwork_data_url(&bytes, path).ok()
                }),
            };
            instruments.push(InstrumentEntry {
                id: format!("{id_prefix}.{unique_id}"),
                name,
                map,
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
        let sample_paths = indexed_sample_paths;
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
            sample_paths,
        })
    }

    fn load(&self, id: &str, sample_rate: u32) -> Result<LoadedInstrument, String> {
        let instrument = self
            .instruments
            .iter()
            .find(|instrument| instrument.id == id)
            .ok_or_else(|| format!("unknown RF instrument {id:?}"))?;
        match &instrument.map {
            InstrumentMap::Mapped(document) => {
                self.load_mapped(&instrument.name, document, sample_rate)
            }
            InstrumentMap::Sfz(sfz) => self.load_sfz(&instrument.name, sfz, sample_rate),
        }
    }

    fn load_mapped(
        &self,
        instrument_name: &str,
        document: &NkiDocument,
        sample_rate: u32,
    ) -> Result<LoadedInstrument, String> {
        let mut required = BTreeMap::<String, usize>::new();
        let mut sample_keys = Vec::new();
        let mut expanded_bytes = 0_u64;
        for zone in &document.zones {
            let key = zone.sample.to_ascii_lowercase();
            let Some(entry) = self.samples.get(&key) else {
                continue;
            };
            if !required.contains_key(&key) {
                expanded_bytes = expanded_bytes.saturating_add(entry.expanded_bytes);
                if expanded_bytes > MAX_SELECTED_SAMPLE_BYTES {
                    return Err(format!(
                        "instrument {:?} needs too much decoded sample data",
                        instrument_name
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
                instrument_name
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
        for zone in &document.zones {
            let key = zone.sample.to_ascii_lowercase();
            let Some(&wave_index) = required.get(&key) else {
                continue;
            };
            let envelope = document
                .groups
                .get(zone.group)
                .copied()
                .flatten()
                .map_or_else(EnvelopeSpec::default, envelope_from);
            let mut envelope = envelope;
            envelope.release_seconds = envelope.release_seconds.max(MINIMUM_RELEASE_SECONDS);
            let group_lfo = document.group_lfos.get(&zone.group).copied();
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
                filters: document
                    .effects
                    .group_filters
                    .get(&zone.group)
                    .cloned()
                    .unwrap_or_default(),
                sfz: None,
            });
        }
        if regions.is_empty() {
            return Err(format!(
                "instrument {:?} has no playable zones",
                instrument_name
            ));
        }
        let effects = EffectRack::new(&document.effects, sample_rate)
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
            sfz_curves: BTreeMap::new(),
            sfz_defaults: CcState::default(),
            sfz_normalisation: 1.0,
        })
    }

    fn load_sfz(
        &self,
        instrument_name: &str,
        sfz: &SfzMap,
        sample_rate: u32,
    ) -> Result<LoadedInstrument, String> {
        let mut required = BTreeMap::<String, usize>::new();
        let mut sample_keys = Vec::new();
        let mut expanded_bytes = 0_u64;
        for path in sfz.sample_paths.values() {
            let Some(entry) = self.sample_paths.get(path) else {
                continue;
            };
            if !required.contains_key(path) {
                expanded_bytes = expanded_bytes.saturating_add(entry.expanded_bytes);
                if expanded_bytes > MAX_SELECTED_SAMPLE_BYTES {
                    return Err(format!(
                        "SFZ instrument {instrument_name:?} needs too much decoded sample data"
                    ));
                }
                required.insert(path.clone(), sample_keys.len());
                sample_keys.push(path.clone());
            }
        }
        if sample_keys.is_empty() {
            return Err(format!(
                "SFZ instrument {instrument_name:?} found none of its samples"
            ));
        }

        let mut archive = ZipArchive::new(Cursor::new(self.bytes.as_ref()))
            .map_err(|error| format!("cannot reopen RF bank: {error}"))?;
        let mut waves = Vec::with_capacity(sample_keys.len());
        for key in &sample_keys {
            let entry = self.sample_paths[key];
            let bytes = read_entry(&mut archive, entry.archive_index, entry.expanded_bytes)?;
            let wave = if extension(key).eq_ignore_ascii_case("flac") {
                rf_soundfonts::flac::decode(&bytes, key.clone())
            } else {
                rf_soundfonts::wav::decode(&bytes, key.clone())
            }
            .map_err(|error| format!("cannot decode SFZ sample {key:?}: {error}"))?;
            waves.push(wave);
        }

        let mut regions = Vec::new();
        for opcodes in &sfz.document.regions {
            let Some(reference) = opcodes.get("sample") else {
                continue;
            };
            let Some(path) = sfz.sample_paths.get(reference) else {
                continue;
            };
            let Some(&wave_index) = required.get(path) else {
                continue;
            };
            let compiled = resident_region(opcodes, wave_index, &waves[wave_index]);
            let zone = NkiZone {
                sample: reference.clone(),
                key_low: compiled.key_low,
                key_high: compiled.key_high,
                root_key: compiled.pitch_keycenter,
                velocity_low: compiled.velocity_low,
                velocity_high: compiled.velocity_high,
                sample_start: 0,
                group: compiled.group as usize,
                volume: 1.0,
                pan: 0.0,
                tune: 1.0,
                sample_loop: compiled.sample_loop,
            };
            regions.push(Region {
                zone,
                wave_index,
                envelope: compiled.envelope,
                lfo: LfoSpec::default(),
                modulation_cc: None,
                filters: Vec::new(),
                sfz: Some(compiled),
            });
        }
        if regions.is_empty() {
            return Err(format!(
                "SFZ instrument {instrument_name:?} has no playable regions"
            ));
        }
        let defaults = control_defaults(&sfz.document.control);
        let normalisation =
            resident_normalisation(&waves, &regions, &defaults, &sfz.document.curves);
        let effects = EffectRack::new(
            &rf_soundfonts::nki::document::NkiEffects::default(),
            sample_rate,
        )
        .map_err(|error| format!("cannot prepare SFZ effects: {error}"))?;
        Ok(LoadedInstrument {
            waves,
            regions,
            voices: Vec::new(),
            held: [[false; 128]; 16],
            sustain: [false; 16],
            pitch_bend_cents: [0.0; 16],
            controllers: controller_matrix(&defaults),
            effects,
            sfz_curves: sfz.document.curves.clone(),
            sfz_defaults: defaults,
            sfz_normalisation: normalisation,
        })
    }

    fn catalog_json(&self) -> Vec<u8> {
        let presets: Vec<_> = self
            .instruments
            .iter()
            .enumerate()
            .map(|(order, instrument)| {
                let summary = match &instrument.map {
                    InstrumentMap::Mapped(document) => {
                        let lowest = document.zones.iter().map(|zone| zone.key_low).min();
                        let highest = document.zones.iter().map(|zone| zone.key_high).max();
                        let mut summary = match (lowest, highest) {
                            (Some(low), Some(high)) => format!(
                                "RF Instrument · {} zones · keys {low}–{high}",
                                document.zones.len()
                            ),
                            _ => "RF instrument".into(),
                        };
                        let program_effects: Vec<_> = document
                            .effects
                            .program
                            .iter()
                            .map(|effect| match effect {
                                NkiProgramEffect::Reverb(_) => "Reverb",
                                NkiProgramEffect::Delay(_) => "Delay",
                            })
                            .collect();
                        if !program_effects.is_empty() {
                            summary.push_str(" · FX ");
                            summary.push_str(&program_effects.join(" → "));
                        }
                        let filter_count: usize =
                            document.effects.group_filters.values().map(Vec::len).sum();
                        if filter_count > 0 {
                            summary.push_str(&format!(
                                " · {filter_count} group filter{}",
                                if filter_count == 1 { "" } else { "s" }
                            ));
                        }
                        summary
                    }
                    InstrumentMap::Sfz(sfz) => {
                        let low = sfz
                            .document
                            .regions
                            .iter()
                            .filter_map(|region| {
                                region.get("key").or_else(|| region.get("lokey")).and_then(
                                    |value| rf_soundfonts::sfz::instrument::parse_note(value),
                                )
                            })
                            .min()
                            .unwrap_or(0);
                        let high = sfz
                            .document
                            .regions
                            .iter()
                            .filter_map(|region| {
                                region.get("key").or_else(|| region.get("hikey")).and_then(
                                    |value| rf_soundfonts::sfz::instrument::parse_note(value),
                                )
                            })
                            .max()
                            .unwrap_or(127);
                        format!(
                            "SFZ Instrument · {} regions · {} samples · keys {low}–{high}",
                            sfz.document.regions.len(),
                            sfz.sample_paths.len()
                        )
                    }
                };
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

fn controller_matrix(defaults: &CcState) -> [[f32; 128]; 16] {
    std::array::from_fn(|_| std::array::from_fn(|index| defaults.get(index as u8)))
}

fn resident_normalisation(
    waves: &[Wave],
    regions: &[Region],
    defaults: &CcState,
    curves: &BTreeMap<u32, Curve>,
) -> f32 {
    let mut peak = 0.0_f32;
    for region in regions {
        let Some(sfz) = region.sfz.as_ref() else {
            continue;
        };
        let Some(wave) = waves.get(region.wave_index) else {
            continue;
        };
        let sample_peak = wave
            .samples
            .iter()
            .fold(0.0_f32, |value, sample| value.max(sample.abs()));
        if sample_peak > 0.0 {
            peak = peak.max(sample_peak * sfz.resolve(defaults, curves).gain);
        }
    }
    if peak <= 0.0 {
        1.0
    } else {
        (0.25 / peak).clamp(0.05, 8.0)
    }
}

impl LoadedInstrument {
    fn reset(&mut self) {
        self.voices.clear();
        self.held = [[false; 128]; 16];
        self.sustain = [false; 16];
        self.pitch_bend_cents = [0.0; 16];
        self.controllers = controller_matrix(&self.sfz_defaults);
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

        let mut cc = self.sfz_defaults.clone();
        for controller in 0..128_u8 {
            cc.set(
                controller,
                self.controllers[channel][usize::from(controller)],
            );
        }

        for region in &self.regions {
            let zone = &region.zone;
            let accepts = region.sfz.as_ref().map_or_else(
                || {
                    (zone.key_low..=zone.key_high).contains(&note)
                        && (zone.velocity_low..=zone.velocity_high).contains(&velocity)
                },
                |sfz| sfz.accepts(note, velocity, &cc),
            );
            if !accepts {
                continue;
            }
            let wave = &self.waves[region.wave_index];
            let (params, config, group, off_by) = if let Some(sfz) = &region.sfz {
                let mut config = sfz.resolve(&cc, &self.sfz_curves);
                config.gain *= self.sfz_normalisation;
                (
                    SampleParams {
                        unity_note: u16::from(sfz.pitch_keycenter),
                        fine_tune: sfz.tune_cents.clamp(-32_768.0, 32_767.0).round() as i16,
                        attenuation_db: 0.0,
                        sample_loop: sfz.sample_loop,
                    },
                    config,
                    sfz.group,
                    sfz.off_by,
                )
            } else {
                let sample_loop = zone
                    .sample_loop
                    .or(wave.sample_params.and_then(|params| params.sample_loop))
                    .filter(|looping| valid_loop(*looping, wave.frame_count()));
                (
                    SampleParams {
                        unity_note: u16::from(zone.root_key),
                        fine_tune: 0,
                        attenuation_db: 0.0,
                        sample_loop,
                    },
                    VoiceConfig {
                        amplitude_envelope: region.envelope,
                        pitch_envelope: PitchEnvelopeSpec::default(),
                        lfo: region.lfo,
                        pitch_offset_cents: 1_200.0 * zone.tune.max(f32::MIN_POSITIVE).log2(),
                        pitch_bend_range_cents: 200.0,
                        modulation_depth: 1.0,
                        gain: zone.volume.max(0.0),
                        pan: zone.pan,
                        velocity_tracking: 1.0,
                    },
                    zone.group as u32,
                    None,
                )
            };
            if let Some(off_by) = off_by {
                for active in &mut self.voices {
                    if active.channel == channel && active.group == off_by {
                        active.voice.fade_out(0.005);
                    }
                }
            }
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
                group,
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

    fn test_wave() -> Vec<u8> {
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
        wave
    }

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
    fn sfz_bank_expands_includes_loads_artwork_and_obeys_controllers() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();

        let cursor = Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("bank.json", options).unwrap();
        archive
            .write_all(
                br#"{"schema_version":1,"id":"portable-sfz","name":"Portable SFZ","instrument_path":"Maps/Piano.sfz"}"#,
            )
            .unwrap();
        archive.start_file("Maps/Piano.sfz", options).unwrap();
        archive
            .write_all(
                b"<control> default_path=../Samples/ set_cc74=127 image=art.png\n#include \"Parts/body.inc\"",
            )
            .unwrap();
        archive.start_file("Maps/Parts/body.inc", options).unwrap();
        archive
            .write_all(b"#include \"../Shared/region.inc\"")
            .unwrap();
        archive
            .start_file("Maps/Shared/region.inc", options)
            .unwrap();
        archive
            .write_all(
                b"<group> locc74=64 ampeg_attack=0.001 ampeg_release=0.02\n<region> key=60 sample=C4.wav",
            )
            .unwrap();
        // The manifest selects Piano.sfz, so an unrelated broken map must not
        // prevent this instrument from loading.
        archive.start_file("Maps/Other.sfz", options).unwrap();
        archive
            .write_all(b"<region> key=61 sample=missing.wav")
            .unwrap();
        archive.start_file("Maps/art.png", options).unwrap();
        archive.write_all(png.get_ref()).unwrap();
        archive.start_file("Samples/C4.wav", options).unwrap();
        archive.write_all(&test_wave()).unwrap();
        let bytes = archive.finish().unwrap().into_inner();

        let mut player = Player::from_bytes(bytes, 48_000).unwrap();
        assert_eq!(player.first_id(), "sfz.piano");
        let catalog: serde_json::Value = serde_json::from_slice(&player.catalog_json()).unwrap();
        assert!(
            catalog["presets"][0]["description"]
                .as_str()
                .unwrap()
                .contains(ARTWORK_MARKER)
        );
        assert!(player.load_preset("sfz.piano"));
        player.dispatch_midi(0x90, 60, 100);
        let mut output = vec![0.0; 960 * 2];
        player.render(&mut output, 2, 0, 960, 1.0, 0.0);
        assert!(output.iter().any(|sample| sample.abs() > 0.01));

        player.reset();
        player.dispatch_midi(0xb0, 74, 0);
        player.dispatch_midi(0x90, 60, 100);
        output.fill(0.0);
        player.render(&mut output, 2, 0, 960, 1.0, 0.0);
        assert!(output.iter().all(|sample| sample.abs() <= f32::EPSILON));
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
            let InstrumentMap::Mapped(document) = &instrument.map else {
                panic!("unexpected SFZ instrument {}", instrument.name);
            };
            let expected = expected_effects
                .get(instrument.name.as_str())
                .unwrap_or_else(|| panic!("unexpected instrument {}", instrument.name));
            assert_eq!(
                (
                    document.effects.program.len(),
                    document.effects.group_filters.values().map(Vec::len).sum(),
                    document.group_lfos.len(),
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
                let InstrumentMap::Mapped(document) = &instrument.map else {
                    panic!("unexpected SFZ instrument {}", instrument.name);
                };
                let low = document
                    .zones
                    .iter()
                    .map(|zone| zone.key_low)
                    .min()
                    .unwrap();
                let high = document
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
