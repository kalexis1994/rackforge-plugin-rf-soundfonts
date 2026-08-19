use rackforge_plugin_sdk::{MidiEvent, ParameterEvent, Processor, export_processor};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};
use std::io::Cursor;
use std::sync::Arc;

const RESOURCE_ID: &str = "factory-soundfont";
/// Sound id published by releases up to 0.2.1, kept so saved racks restore.
const LEGACY_PRESET_ID: &str = "factory.ydp-grand-piano";
const MAX_BANK_BYTES: usize = 128 * 1024 * 1024;
const MAX_FRAMES: usize = 4096;
const MAX_TRANSFER_BYTES: usize = 256 * 1024;
const MASTER_VOLUME_INDEX: u32 = 0;
const DEFAULT_MASTER_VOLUME: f64 = 0.9;
/// More presets than any playable bank needs, few enough that the serialized
/// catalog always has a chance to fit the transfer buffer.
const MAX_CATALOG_PRESETS: usize = 1536;
const MIDI_CHANNELS: i32 = 16;
const PERCUSSION_CHANNEL: i32 = 9;
const DRUM_BANK_OFFSET: i32 = 128;

struct PendingResource {
    expected_bytes: usize,
    bytes: Vec<u8>,
}

/// One playable SoundFont preset: bank number, patch number, display name.
type CatalogEntry = (i32, i32, String);

struct PortableSoundfonts {
    sound_font: Option<Arc<SoundFont>>,
    synth: Option<Synthesizer>,
    pending: Option<PendingResource>,
    sample_rate: i32,
    selected: Option<(i32, i32)>,
    master_volume: f64,
    left: Vec<f32>,
    right: Vec<f32>,
}

impl Default for PortableSoundfonts {
    fn default() -> Self {
        Self {
            sound_font: None,
            synth: None,
            pending: None,
            sample_rate: 48_000,
            selected: None,
            master_volume: DEFAULT_MASTER_VOLUME,
            left: vec![0.0; MAX_FRAMES],
            right: vec![0.0; MAX_FRAMES],
        }
    }
}

fn preset_id(bank: i32, patch: i32) -> String {
    format!("sf.b{bank:03}.p{patch:03}")
}

fn parse_preset_id(id: &str) -> Option<(i32, i32)> {
    let rest = id.strip_prefix("sf.b")?;
    let (bank, patch) = rest.split_once(".p")?;
    if bank.len() != 3 || patch.len() != 3 {
        return None;
    }
    Some((bank.parse().ok()?, patch.parse().ok()?))
}

/// Sorts by bank and patch, drops duplicate locations, and caps the list, so
/// the catalog and preset lookup agree on which entries exist.
fn normalize_entries(mut entries: Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    entries.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    entries.dedup_by_key(|entry| (entry.0, entry.1));
    entries.truncate(MAX_CATALOG_PRESETS);
    entries
}

fn build_catalog_json(entries: &[CatalogEntry]) -> Vec<u8> {
    let mut banks: Vec<i32> = entries.iter().map(|entry| entry.0).collect();
    banks.dedup();
    let banks_json: Vec<serde_json::Value> = banks
        .iter()
        .enumerate()
        .map(|(order, bank)| {
            serde_json::json!({
                "id": format!("b{bank:03}"),
                "name": if *bank >= DRUM_BANK_OFFSET {
                    format!("Drums {bank}")
                } else {
                    format!("Bank {bank}")
                },
                "order": order as i32,
            })
        })
        .collect();
    let presets_json: Vec<serde_json::Value> = entries
        .iter()
        .enumerate()
        .map(|(order, (bank, patch, name))| {
            let name = name.trim();
            serde_json::json!({
                "id": preset_id(*bank, *patch),
                "name": if name.is_empty() {
                    format!("Preset {bank}:{patch:03}")
                } else {
                    name.to_string()
                },
                "bank": format!("b{bank:03}"),
                "order": order as i32,
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "banks": banks_json,
        "presets": presets_json,
    }))
    .unwrap_or_default()
}

/// Serializes the catalog into `destination`, halving the preset list until
/// it fits rather than publishing nothing for an oversized bank.
fn fit_catalog(mut entries: Vec<CatalogEntry>, destination: &mut [u8]) -> Option<usize> {
    loop {
        let bytes = build_catalog_json(&entries);
        if bytes.len() <= destination.len() {
            destination[..bytes.len()].copy_from_slice(&bytes);
            return Some(bytes.len());
        }
        if entries.is_empty() {
            return None;
        }
        entries.truncate(entries.len() / 2);
    }
}

impl PortableSoundfonts {
    fn catalog_entries(sound_font: &SoundFont) -> Vec<CatalogEntry> {
        normalize_entries(
            sound_font
                .get_presets()
                .iter()
                .map(|preset| {
                    (
                        preset.get_bank_number(),
                        preset.get_patch_number(),
                        preset.get_name().to_string(),
                    )
                })
                .collect(),
        )
    }

    fn rebuild_synth(&mut self) -> bool {
        let Some(sound_font) = self.sound_font.as_ref() else {
            self.synth = None;
            return true;
        };
        let mut settings = SynthesizerSettings::new(self.sample_rate);
        settings.block_size = 64;
        settings.maximum_polyphony = 96;
        settings.enable_reverb_and_chorus = false;
        match Synthesizer::new(sound_font, &settings) {
            Ok(mut synth) => {
                synth.set_master_volume(self.master_volume as f32);
                Self::apply_selection(&mut synth, self.selected);
                self.synth = Some(synth);
                true
            }
            Err(_) => {
                self.synth = None;
                false
            }
        }
    }

    /// Points every channel at the selected preset. The percussion channel
    /// adds the drum offset itself, so it receives the bank without it.
    fn apply_selection(synth: &mut Synthesizer, selection: Option<(i32, i32)>) {
        let Some((bank, patch)) = selection else {
            return;
        };
        for channel in 0..MIDI_CHANNELS {
            let channel_bank = if channel == PERCUSSION_CHANNEL && bank >= DRUM_BANK_OFFSET {
                bank - DRUM_BANK_OFFSET
            } else {
                bank
            };
            synth.process_midi_message(channel, 0xB0, 0x00, channel_bank);
            synth.process_midi_message(channel, 0xC0, patch, 0);
        }
    }

    fn dispatch_midi(synth: &mut Synthesizer, event: MidiEvent) {
        let status = event.data[0];
        if event.length == 0 || !(0x80..0xF0).contains(&status) {
            return;
        }
        synth.process_midi_message(
            i32::from(status & 0x0F),
            i32::from(status & 0xF0),
            i32::from(event.data[1] & 0x7F),
            i32::from(event.data[2] & 0x7F),
        );
    }

    fn apply_parameter(synth: &mut Synthesizer, master_volume: &mut f64, event: ParameterEvent) {
        if event.index != MASTER_VOLUME_INDEX || !event.value.is_finite() {
            return;
        }
        *master_volume = event.value.clamp(0.0, 1.0);
        synth.set_master_volume(*master_volume as f32);
    }

    fn render_segment(
        synth: &mut Synthesizer,
        left: &mut [f32],
        right: &mut [f32],
        output: &mut [f32],
        channels: usize,
        start: usize,
        end: usize,
    ) {
        let length = end.saturating_sub(start);
        if length == 0 {
            return;
        }
        synth.render(&mut left[..length], &mut right[..length]);
        for frame in 0..length {
            let target = (start + frame) * channels;
            if channels == 1 {
                output[target] = (left[frame] + right[frame]) * 0.5;
            } else {
                output[target] = left[frame];
                output[target + 1] = right[frame];
            }
        }
    }
}

impl Processor for PortableSoundfonts {
    fn prepare(
        &mut self,
        sample_rate: f64,
        maximum_frames: u32,
        _input_channels: u32,
        output_channels: u32,
    ) -> bool {
        if !sample_rate.is_finite()
            || !(16_000.0..=192_000.0).contains(&sample_rate)
            || maximum_frames as usize > MAX_FRAMES
            || !(1..=2).contains(&output_channels)
        {
            return false;
        }
        self.sample_rate = sample_rate.round() as i32;
        self.rebuild_synth()
    }

    fn set_parameter(&mut self, index: u32, value: f64) -> bool {
        if index != MASTER_VOLUME_INDEX || !value.is_finite() {
            return false;
        }
        self.master_volume = value.clamp(0.0, 1.0);
        if let Some(synth) = self.synth.as_mut() {
            synth.set_master_volume(self.master_volume as f32);
        }
        true
    }

    fn get_parameter(&self, index: u32) -> Option<f64> {
        (index == MASTER_VOLUME_INDEX).then_some(self.master_volume)
    }

    fn reset(&mut self) {
        if let Some(synth) = self.synth.as_mut() {
            synth.reset();
            Self::apply_selection(synth, self.selected);
            synth.set_master_volume(self.master_volume as f32);
        }
    }

    fn begin_resource(&mut self, id: &str, total_bytes: u64) -> bool {
        let Ok(expected_bytes) = usize::try_from(total_bytes) else {
            return false;
        };
        if id != RESOURCE_ID
            || expected_bytes == 0
            || expected_bytes > MAX_BANK_BYTES
            || self.pending.is_some()
        {
            return false;
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(expected_bytes).is_err() {
            return false;
        }
        self.pending = Some(PendingResource {
            expected_bytes,
            bytes,
        });
        true
    }

    fn write_resource(&mut self, offset: u64, bytes: &[u8]) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return false;
        };
        if offset != pending.bytes.len() as u64
            || pending.bytes.len().saturating_add(bytes.len()) > pending.expected_bytes
        {
            return false;
        }
        pending.bytes.extend_from_slice(bytes);
        true
    }

    fn end_resource(&mut self) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        if pending.bytes.len() != pending.expected_bytes {
            return false;
        }
        let mut source = Cursor::new(pending.bytes.as_slice());
        let Ok(sound_font) = SoundFont::new(&mut source) else {
            return false;
        };
        let sound_font = Arc::new(sound_font);
        if let Some((bank, patch)) = self.selected {
            let entries = Self::catalog_entries(&sound_font);
            if !entries
                .iter()
                .any(|entry| entry.0 == bank && entry.1 == patch)
            {
                self.selected = None;
            }
        }
        self.sound_font = Some(sound_font);
        self.rebuild_synth()
    }

    fn write_preset_catalog(&mut self, destination: &mut [u8]) -> Option<usize> {
        let sound_font = self.sound_font.as_ref()?;
        fit_catalog(Self::catalog_entries(sound_font), destination)
    }

    fn load_preset(&mut self, id: &str) -> bool {
        let Some(sound_font) = self.sound_font.as_ref() else {
            return false;
        };
        let entries = Self::catalog_entries(sound_font);
        let selection = if id == LEGACY_PRESET_ID {
            entries.first().map(|entry| (entry.0, entry.1))
        } else {
            parse_preset_id(id)
                .filter(|(bank, patch)| {
                    entries
                        .iter()
                        .any(|entry| entry.0 == *bank && entry.1 == *patch)
                })
        };
        let Some(selection) = selection else {
            return false;
        };
        self.selected = Some(selection);
        if let Some(synth) = self.synth.as_mut() {
            synth.note_off_all(false);
            Self::apply_selection(synth, self.selected);
        }
        true
    }

    fn process(
        &mut self,
        _input: &[f32],
        output: &mut [f32],
        midi: &[MidiEvent],
        parameters: &[ParameterEvent],
        frames: u32,
        _input_channels: u32,
        output_channels: u32,
    ) {
        output.fill(0.0);
        let frames = frames as usize;
        let channels = output_channels as usize;
        if frames > MAX_FRAMES || !(1..=2).contains(&channels) {
            return;
        }
        let master_volume = &mut self.master_volume;
        let Some(synth) = self.synth.as_mut() else {
            // Keep the published volume honest even while no bank is loaded.
            for event in parameters {
                if event.index == MASTER_VOLUME_INDEX && event.value.is_finite() {
                    *master_volume = event.value.clamp(0.0, 1.0);
                }
            }
            return;
        };
        let mut midi_index = 0;
        let mut parameter_index = 0;
        let mut position = 0;
        loop {
            let midi_frame = midi
                .get(midi_index)
                .map(|event| (event.frame as usize).min(frames));
            let parameter_frame = parameters
                .get(parameter_index)
                .map(|event| (event.frame as usize).min(frames));
            let boundary = midi_frame
                .unwrap_or(frames)
                .min(parameter_frame.unwrap_or(frames));
            Self::render_segment(
                synth,
                &mut self.left,
                &mut self.right,
                output,
                channels,
                position,
                boundary,
            );
            position = boundary;
            let mut dispatched = false;
            while let Some(event) = midi.get(midi_index) {
                if (event.frame as usize).min(frames) > position {
                    break;
                }
                Self::dispatch_midi(synth, *event);
                midi_index += 1;
                dispatched = true;
            }
            while let Some(event) = parameters.get(parameter_index) {
                if (event.frame as usize).min(frames) > position {
                    break;
                }
                Self::apply_parameter(synth, master_volume, *event);
                parameter_index += 1;
                dispatched = true;
            }
            if !dispatched {
                break;
            }
        }
    }
}

export_processor!(
    PortableSoundfonts,
    max_frames = MAX_FRAMES,
    max_input_channels = 0,
    max_output_channels = 2,
    max_midi_events = 512,
    max_parameter_events = 16,
    max_transfer_bytes = MAX_TRANSFER_BYTES
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_silent_before_the_factory_bank_arrives() {
        let mut plugin = PortableSoundfonts::default();
        assert!(plugin.prepare(48_000.0, 64, 0, 2));
        let mut output = [1.0; 128];
        plugin.process(&[], &mut output, &[], &[], 64, 0, 2);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn rejects_unknown_resources_and_presets() {
        let mut plugin = PortableSoundfonts::default();
        assert!(!plugin.begin_resource("other", 1));
        assert!(!plugin.load_preset("other"));
        assert!(!plugin.load_preset("sf.b000.p000"));
    }

    #[test]
    fn preset_ids_round_trip() {
        assert_eq!(preset_id(0, 0), "sf.b000.p000");
        assert_eq!(preset_id(128, 25), "sf.b128.p025");
        assert_eq!(parse_preset_id("sf.b000.p000"), Some((0, 0)));
        assert_eq!(parse_preset_id("sf.b128.p025"), Some((128, 25)));
        assert_eq!(parse_preset_id("factory.ydp-grand-piano"), None);
        assert_eq!(parse_preset_id("sf.b0.p0"), None);
        assert_eq!(parse_preset_id("sf.b00a.p000"), None);
        assert_eq!(parse_preset_id(""), None);
    }

    #[test]
    fn entries_are_sorted_deduplicated_and_capped() {
        let entries = normalize_entries(vec![
            (1, 5, "Later".into()),
            (0, 3, "Piano".into()),
            (0, 3, "Duplicate".into()),
            (0, 1, "First".into()),
        ]);
        assert_eq!(
            entries,
            vec![
                (0, 1, "First".into()),
                (0, 3, "Piano".into()),
                (1, 5, "Later".into()),
            ]
        );
        let many = (0..2 * MAX_CATALOG_PRESETS)
            .map(|index| (0, index as i32, format!("P{index}")))
            .collect();
        assert_eq!(normalize_entries(many).len(), MAX_CATALOG_PRESETS);
    }

    #[test]
    fn catalog_json_is_valid_and_named() {
        let bytes = build_catalog_json(&[
            (0, 0, "Grand Piano".into()),
            (0, 1, "   ".into()),
            (128, 0, "Standard Kit".into()),
        ]);
        let catalog: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(catalog["schema_version"], 1);
        assert_eq!(catalog["banks"][0]["id"], "b000");
        assert_eq!(catalog["banks"][1]["name"], "Drums 128");
        assert_eq!(catalog["presets"][0]["id"], "sf.b000.p000");
        assert_eq!(catalog["presets"][0]["name"], "Grand Piano");
        assert_eq!(catalog["presets"][1]["name"], "Preset 0:001");
        assert_eq!(catalog["presets"][2]["bank"], "b128");
    }

    #[test]
    fn oversized_catalogs_shrink_to_fit() {
        let entries: Vec<CatalogEntry> = (0..200)
            .map(|index| (0, index, format!("Preset number {index}")))
            .collect();
        let mut small = [0u8; 2048];
        let length = fit_catalog(entries.clone(), &mut small).unwrap();
        let catalog: serde_json::Value = serde_json::from_slice(&small[..length]).unwrap();
        assert!(catalog["presets"].as_array().unwrap().len() < entries.len());
        let mut tiny = [0u8; 8];
        assert_eq!(fit_catalog(entries, &mut tiny), None);
    }

    #[test]
    fn master_volume_is_clamped_and_readable() {
        let mut plugin = PortableSoundfonts::default();
        assert_eq!(plugin.get_parameter(0), Some(DEFAULT_MASTER_VOLUME));
        assert!(plugin.set_parameter(0, 0.4));
        assert_eq!(plugin.get_parameter(0), Some(0.4));
        assert!(plugin.set_parameter(0, 7.0));
        assert_eq!(plugin.get_parameter(0), Some(1.0));
        assert!(!plugin.set_parameter(0, f64::NAN));
        assert!(!plugin.set_parameter(1, 0.5));
        assert_eq!(plugin.get_parameter(1), None);
    }

    #[test]
    fn parameter_events_apply_without_a_loaded_bank() {
        let mut plugin = PortableSoundfonts::default();
        assert!(plugin.prepare(48_000.0, 64, 0, 2));
        let mut output = [0.0; 128];
        let events = [ParameterEvent {
            frame: 10,
            index: 0,
            value: 0.25,
        }];
        plugin.process(&[], &mut output, &[], &events, 64, 0, 2);
        assert_eq!(plugin.get_parameter(0), Some(0.25));
    }

    #[test]
    #[ignore = "set RF_SOUNDFONTS_SF2 to a local SoundFont file to run"]
    fn a_real_bank_lists_selects_and_sounds() {
        let path = std::env::var("RF_SOUNDFONTS_SF2").expect("RF_SOUNDFONTS_SF2 not set");
        let bytes = std::fs::read(path).expect("cannot read the SoundFont");
        let mut plugin = PortableSoundfonts::default();
        assert!(plugin.prepare(48_000.0, 128, 0, 2));
        assert!(plugin.begin_resource(RESOURCE_ID, bytes.len() as u64));
        for (index, chunk) in bytes.chunks(65536).enumerate() {
            assert!(plugin.write_resource((index * 65536) as u64, chunk));
        }
        assert!(plugin.end_resource());

        let mut destination = vec![0u8; MAX_TRANSFER_BYTES];
        let length = plugin.write_preset_catalog(&mut destination).unwrap();
        let catalog: serde_json::Value =
            serde_json::from_slice(&destination[..length]).unwrap();
        let first_id = catalog["presets"][0]["id"].as_str().unwrap().to_string();
        assert!(plugin.load_preset(&first_id));
        assert!(plugin.load_preset(LEGACY_PRESET_ID));
        assert!(!plugin.load_preset("sf.b999.p999"));

        let note_on = MidiEvent {
            frame: 0,
            data: [0x90, 60, 100],
            length: 3,
        };
        let mut output = vec![0.0f32; 128 * 2];
        plugin.process(&[], &mut output, &[note_on], &[], 128, 0, 2);
        assert!(
            output.iter().any(|sample| sample.abs() > 0.0),
            "a selected preset must produce audio"
        );
    }

    #[test]
    fn package_metadata_matches_the_crate_version() {
        let version = env!("CARGO_PKG_VERSION");
        let manifest = include_str!("../package/rackforge-plugin.toml");
        assert!(
            manifest.contains(&format!("version = \"{version}\"")),
            "rackforge-plugin.toml version must match the crate version {version}"
        );
        let runtime = include_str!("../package/metadata/runtime.json");
        assert!(
            runtime.contains(&format!("\"version\": \"{version}\"")),
            "runtime.json version must match the crate version {version}"
        );
    }
}
