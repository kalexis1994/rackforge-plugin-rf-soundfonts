mod sfz_library;

use rackforge_plugin_api::abi::{
    HostApiV1, LOG_LEVEL_ERROR, LOG_LEVEL_INFO, LOG_LEVEL_WARN, MidiEventV1, ParameterEventV1,
    PluginApiV1, ProcessBlockV1, STATUS_INVALID_ARGUMENT, STATUS_INVALID_STATE, STATUS_OK,
    STATUS_UNKNOWN_PARAMETER, SURFACE_EXTENSION_VERSION, SurfaceExtensionApiV1,
    copy_to_host_buffer, pack_version, version_major, version_minor,
};
use rackforge_plugin_api::{
    BankDescriptor, PresetCatalog, PresetDescriptor, SurfaceActivationRequest,
    SurfaceActivationResponse,
};
use rf_soundfonts::{DlsBank, Voice};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;
use std::ptr;
use std::slice;

/// Seconds to fade a voice displaced by a retrigger when the instrument does
/// not say. Long enough to remove the step, short enough to be inaudible.
const DISPLACED_VOICE_FADE_SECONDS: f32 = 0.005;

const RESOURCE_ID: &[u8] = b"dls-bank";
const SFZ_RESOURCE_ID: &[u8] = b"sfz-library";
const PIANO_PRESET_ID: &str = "gm.piano-1";
const MASTER_GAIN_PARAMETER: u32 = 0;
const DEFAULT_MASTER_GAIN: f32 = 0.65;
const MAX_VOICES: usize = 32;
const LEGACY_STATE_SIZE: usize = 16;

const RUNTIME_DESCRIPTOR: &[u8] = concat!(
    r#"{
  "schema_version": 1,
  "id": "org.rackforge.rf-soundfonts",
  "version": ""#,
    env!("CARGO_PKG_VERSION"),
    r#"",
  "state_version": 4
}"#
)
.as_bytes();

const PARAMETER_SCHEMA: &[u8] = br#"{
  "schema_version": 1,
  "pages": [
    { "id": "sound", "name": "Sound", "order": 0, "header": "RF-Soundfonts" }
  ],
  "parameters": [
    {
      "index": 0,
      "id": "master-gain",
      "name": "Volume",
      "page": "sound",
      "order": 0,
      "kind": {
        "type": "float",
        "minimum": 0.0,
        "maximum": 1.0,
        "default": 0.65,
        "step": 0.01,
        "unit": "x"
      },
      "flags": {
        "automatable": true,
        "modulatable": true,
        "read_only": false,
        "advanced": false
      },
      "suggested_control": "knob"
    }
  ]
}"#;

const PRESET_CATALOG: &[u8] = br#"{
  "schema_version": 1,
  "banks": [
    { "id": "gm", "name": "General MIDI", "order": 0 }
  ],
  "presets": [
    {
      "id": "gm.piano-1",
      "name": "Acoustic Grand Piano",
      "bank": "gm",
      "category": "Piano",
      "order": 0,
      "tags": ["acoustic", "gm"]
    }
  ]
}"#;

struct RfDls {
    host: HostApiV1,
    bank: DlsBank,
    voices: Vec<Voice>,
    held_notes: [bool; 128],
    sample_rate: u32,
    maximum_frames: u32,
    output_channels: u32,
    active: bool,
    sustain: bool,
    pitch_bend_normalized: f32,
    modulation_wheel: f32,
    master_gain: f32,
    selected_bank: u32,
    selected_program: u32,
    selected_preset_id: String,
    /// Every installed SFZ instrument, all resident. Absent when no library
    /// resource is installed.
    sfz: Option<sfz_library::SfzLibrary>,
    /// Which of them currently receives notes. `None` means the DLS path.
    selected_sfz: Option<usize>,
}

impl RfDls {
    fn new(host: HostApiV1, bank: DlsBank, selected_bank: u32, selected_program: u32) -> Self {
        Self {
            host,
            bank,
            voices: Vec::with_capacity(MAX_VOICES),
            held_notes: [false; 128],
            sample_rate: 48_000,
            maximum_frames: 0,
            output_channels: 0,
            active: false,
            sustain: false,
            pitch_bend_normalized: 0.0,
            modulation_wheel: 0.0,
            master_gain: DEFAULT_MASTER_GAIN,
            selected_bank,
            selected_program,
            selected_preset_id: dynamic_preset_id(selected_bank, selected_program),
            sfz: None,
            selected_sfz: None,
        }
    }

    /// Adopts an SFZ library discovered beside the plugin.
    pub fn attach_sfz(&mut self, library: sfz_library::SfzLibrary) {
        self.sfz = Some(library);
    }

    fn reset(&mut self) {
        self.voices.clear();
        self.held_notes.fill(false);
        self.sustain = false;
        self.pitch_bend_normalized = 0.0;
        self.modulation_wheel = 0.0;
    }

    fn select_instrument(&mut self, bank: u32, program: u32) -> bool {
        if self.bank.instrument(bank, program).is_none() {
            return false;
        }
        self.selected_bank = bank;
        self.selected_program = program;
        self.selected_preset_id = dynamic_preset_id(bank, program);
        self.reset();
        true
    }

    fn select_preset(&mut self, preset_id: &str) -> bool {
        if let Some(id) = sfz_library::instrument_id(preset_id) {
            return self.select_sfz(id, preset_id);
        }
        // Leaving an SFZ instrument returns the plugin to the DLS path.
        self.selected_sfz = None;
        let selection = if preset_id == PIANO_PRESET_ID {
            Some((0, 0))
        } else {
            parse_dynamic_preset_id(preset_id)
        };
        selection.is_some_and(|(bank, program)| self.select_instrument(bank, program))
    }

    /// Points the keyboard at one loaded SFZ instrument.
    ///
    /// Switching costs nothing measurable: every instrument was loaded at
    /// start-up and stays resident, so this only changes which one receives
    /// the notes. That is the whole point of the streaming work.
    fn select_sfz(&mut self, instrument_id: &str, preset_id: &str) -> bool {
        let Some(index) = self
            .sfz
            .as_ref()
            .and_then(|library| library.index_of(instrument_id))
        else {
            return false;
        };
        self.selected_sfz = Some(index);
        self.selected_preset_id = preset_id.to_owned();
        self.reset();
        true
    }

    fn has_preset(&self, preset_id: &str) -> bool {
        if let Some(id) = sfz_library::instrument_id(preset_id) {
            return self
                .sfz
                .as_ref()
                .is_some_and(|library| library.index_of(id).is_some());
        }
        let selection = if preset_id == PIANO_PRESET_ID {
            Some((0, 0))
        } else {
            parse_dynamic_preset_id(preset_id)
        };
        selection.is_some_and(|(bank, program)| self.bank.instrument(bank, program).is_some())
    }

    fn activate_surface(
        &self,
        request: SurfaceActivationRequest,
    ) -> Result<SurfaceActivationResponse, String> {
        request.validate().map_err(|error| error.to_string())?;
        let focus = request
            .selected_item_id
            .filter(|preset_id| self.has_preset(preset_id));
        Ok(SurfaceActivationResponse::focus(focus))
    }

    fn set_parameter(&mut self, index: u32, value: f64) -> i32 {
        if !value.is_finite() {
            return STATUS_INVALID_ARGUMENT;
        }
        match index {
            MASTER_GAIN_PARAMETER if (0.0..=1.0).contains(&value) => {
                self.master_gain = value as f32;
                STATUS_OK
            }
            MASTER_GAIN_PARAMETER => STATUS_INVALID_ARGUMENT,
            _ => STATUS_UNKNOWN_PARAMETER,
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        self.held_notes[note as usize] = true;
        // Faded, never dropped. Removing a sounding voice takes its output
        // from wherever the waveform happened to be straight to zero, and that
        // step is a click. It is heard on a repeated key, where the note still
        // ringing is displaced by its own retrigger — exactly the case
        // `note_polyphony=1` describes and `off_time` exists to soften.
        let fade = self
            .selected_sfz
            .and_then(|index| self.sfz.as_ref().map(|library| library.off_time(index)))
            .unwrap_or(DISPLACED_VOICE_FADE_SECONDS);
        for voice in self.voices.iter_mut().filter(|voice| voice.note == note) {
            voice.fade_out(fade);
        }
        if let Some(index) = self.selected_sfz {
            // An SFZ instrument resolves its own regions, levels and stereo
            // placement from controller state.
            if let Some(library) = self.sfz.as_ref() {
                for voice in library.voices_for_note(index, note, velocity, self.sample_rate) {
                    if self.voices.len() >= MAX_VOICES {
                        self.voices.remove(0);
                    }
                    self.voices.push(voice);
                }
            }
            return;
        }
        let bank = &self.bank;
        let Some(instrument) = bank.instrument(self.selected_bank, self.selected_program) else {
            return;
        };
        for region in instrument.matching_regions(note, velocity) {
            if let Ok(voice) =
                Voice::new(bank, instrument, region, note, velocity, self.sample_rate)
            {
                if self.voices.len() >= MAX_VOICES {
                    self.voices.remove(0);
                }
                self.voices.push(voice);
            }
        }
    }

    fn note_off(&mut self, note: u8) {
        self.held_notes[note as usize] = false;
        if !self.sustain {
            self.release_note(note);
        }
    }

    fn release_note(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.note == note {
                voice.note_off();
            }
        }
    }

    fn set_sustain(&mut self, enabled: bool) {
        let was_enabled = self.sustain;
        self.sustain = enabled;
        if was_enabled && !enabled {
            for voice in &mut self.voices {
                if !self.held_notes[voice.note as usize] {
                    voice.note_off();
                }
            }
        }
    }

    fn all_notes_off(&mut self) {
        self.held_notes.fill(false);
        if !self.sustain {
            for voice in &mut self.voices {
                voice.note_off();
            }
        }
    }

    fn all_sound_off(&mut self) {
        self.reset();
    }

    fn set_pitch_bend(&mut self, least_significant: u8, most_significant: u8) {
        let raw = i32::from(least_significant & 0x7f) | (i32::from(most_significant & 0x7f) << 7);
        let centered = raw - 8_192;
        let normalized = if centered >= 0 {
            centered as f32 / 8_191.0
        } else {
            centered as f32 / 8_192.0
        };
        self.pitch_bend_normalized = normalized;
    }

    fn set_modulation_wheel(&mut self, value: u8) {
        self.modulation_wheel = f32::from(value & 0x7f) / 127.0;
    }

    fn reset_controllers(&mut self) {
        self.set_sustain(false);
        self.pitch_bend_normalized = 0.0;
        self.modulation_wheel = 0.0;
    }

    fn handle_midi(&mut self, event: MidiEventV1) {
        if event.length == 0 {
            return;
        }
        let status = event.data[0] & 0xf0;
        // Every control change reaches the SFZ side, whatever it is. An SFZ
        // instrument decides its microphone balance, level, stereo image and
        // release length from arbitrary controllers, so filtering to the ones
        // the DLS path recognises would silence half of what the author wrote.
        if status == 0xb0
            && event.length >= 3
            && let Some(library) = self.sfz.as_mut()
        {
            library.set_controller(event.data[1] & 0x7f, event.data[2] & 0x7f);
        }
        match status {
            0x80 if event.length >= 3 => self.note_off(event.data[1] & 0x7f),
            0x90 if event.length >= 3 && event.data[2] != 0 => {
                self.note_on(event.data[1] & 0x7f, event.data[2] & 0x7f);
            }
            0x90 if event.length >= 3 => self.note_off(event.data[1] & 0x7f),
            0xe0 if event.length >= 3 => self.set_pitch_bend(event.data[1], event.data[2]),
            0xb0 if event.length >= 3 && event.data[1] == 1 => {
                self.set_modulation_wheel(event.data[2]);
            }
            0xb0 if event.length >= 3 && event.data[1] == 64 => {
                self.set_sustain(event.data[2] >= 64);
            }
            0xb0 if event.length >= 3 && event.data[1] == 120 => self.all_sound_off(),
            0xb0 if event.length >= 3 && event.data[1] == 121 => self.reset_controllers(),
            0xb0 if event.length >= 3 && event.data[1] == 123 => self.all_notes_off(),
            _ => {}
        }
    }

    /// Sums every live voice into one stereo frame.
    ///
    /// Voices carry their own stereo position now, so the mix stays in two
    /// channels the whole way. Collapsing to mono here and widening again
    /// downstream would discard both the panning and the natural width of a
    /// stereo sample.
    fn render_frame(&mut self) -> [f32; 2] {
        let mut left = 0.0_f32;
        let mut right = 0.0_f32;
        let mut index = 0;
        while index < self.voices.len() {
            let [voice_left, voice_right] = self.voices[index]
                .next_frame_controlled(self.pitch_bend_normalized, self.modulation_wheel);
            left += voice_left;
            right += voice_right;
            if self.voices[index].is_finished() {
                self.voices.swap_remove(index);
            } else {
                index += 1;
            }
        }
        [left * self.master_gain, right * self.master_gain]
    }
}

fn log_host(host: &HostApiV1, level: u32, message: &str) {
    if let Some(log) = host.log {
        // SAFETY: the message remains readable for the callback duration.
        unsafe {
            log(host.context, level, message.as_ptr(), message.len());
        }
    }
}

fn dynamic_preset_id(bank: u32, program: u32) -> String {
    format!("dls.b{bank:08x}.p{program:08x}")
}

fn parse_dynamic_preset_id(id: &str) -> Option<(u32, u32)> {
    let (bank, program) = id.strip_prefix("dls.b")?.split_once(".p")?;
    if bank.len() != 8 || program.len() != 8 {
        return None;
    }
    Some((
        u32::from_str_radix(bank, 16).ok()?,
        u32::from_str_radix(program, 16).ok()?,
    ))
}

fn dynamic_catalog(bank: &DlsBank, sfz: Option<&sfz_library::SfzLibrary>) -> PresetCatalog {
    let mut seen = BTreeSet::new();
    let mut presets = Vec::new();
    let mut instruments = bank.instruments.iter().collect::<Vec<_>>();
    instruments.sort_by_key(|instrument| {
        (
            instrument.is_drum(),
            instrument.bank & 0x7fff_ffff,
            instrument.program,
        )
    });
    for instrument in instruments {
        if !seen.insert((instrument.bank, instrument.program)) {
            continue;
        }
        let is_drum = instrument.is_drum();
        let display_bank = instrument.bank & 0x7fff_ffff;
        let fallback = format!(
            "{} {}",
            if is_drum { "Drum Kit" } else { "Instrument" },
            instrument.program
        );
        let name = if instrument.name.trim().is_empty() {
            fallback
        } else {
            instrument.name.trim().to_owned()
        };
        let description = if is_drum {
            format!("DRUM P{:03}", instrument.program)
        } else {
            format!("B{display_bank:03} P{:03}", instrument.program)
        };
        presets.push(PresetDescriptor {
            id: dynamic_preset_id(instrument.bank, instrument.program),
            name,
            description: Some(description),
            bank: Some("dls".into()),
            category: Some(if is_drum { "Drums" } else { "Instrument" }.into()),
            order: presets.len() as i32,
            tags: vec![if is_drum { "drums" } else { "melodic" }.into()],
            editable: false,
        });
    }
    // Only banks with something in them are published. An installation driven
    // by SFZ alone should reach the installed libraries immediately.
    let mut banks = Vec::new();
    if !bank.instruments.is_empty() {
        banks.push(BankDescriptor {
            id: "dls".into(),
            name: "DLS".into(),
            order: 0,
        });
    }
    // One bank per installed library, so a player picks the library first and
    // the instrument second. With twenty instruments a single flat list is
    // unusable on a two-line controller display, and the grouping matches how
    // the material arrives: one folder per library.
    if let Some(library) = sfz {
        for (offset, name) in library.libraries().iter().enumerate() {
            banks.push(BankDescriptor {
                id: sfz_bank_id(name),
                name: (*name).to_string(),
                order: 1 + offset as i32,
            });
        }
        for (offset, loaded) in library.instruments().iter().enumerate() {
            let summary = loaded.instrument.summary();
            presets.push(PresetDescriptor {
                id: sfz_library::preset_id(&loaded.id),
                name: loaded.name.clone(),
                description: Some(describe(&summary)),
                bank: Some(sfz_bank_id(&loaded.library)),
                category: Some("Instrument".into()),
                order: offset as i32,
                tags: facts(&summary),
                editable: false,
            });
        }
    }

    PresetCatalog {
        schema_version: 1,
        banks,
        presets,
    }
}

#[cfg(test)]
mod fact_tests {
    use super::{describe, facts, note_name};
    use rf_soundfonts::sfz::instrument::InstrumentSummary;

    fn summary() -> InstrumentSummary {
        InstrumentSummary {
            key_low: 0,
            key_high: 127,
            root_low: 21,
            root_high: 108,
            regions: 807,
            samples: 599,
            velocity_layers: 4,
            resident_bytes: 67 * 1_048_576,
            looping: true,
        }
    }

    #[test]
    fn middle_c_is_c4() {
        // The one convention worth pinning: MIDI 60 has been called C3, C4 and
        // C5 by different makers, and a keyboard player reads the label off
        // their own instrument.
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(21), "A0");
        assert_eq!(note_name(108), "C8");
        assert_eq!(note_name(0), "C-1");
    }

    #[test]
    fn a_description_names_the_recorded_range_not_the_reachable_one() {
        // Ten trumpets all answer to the whole keyboard because their
        // outermost regions were stretched to cover it. What tells them apart
        // is where they were actually played.
        assert_eq!(
            describe(&summary()),
            "A0–C8 · 807 zones · 4 layers · 67 MiB"
        );
    }

    #[test]
    fn an_unlayered_instrument_does_not_claim_one_layer() {
        // One velocity span covering everything is the absence of layering,
        // and saying "1 layer" invites the reader to think it means something.
        let mut plain = summary();
        plain.velocity_layers = 1;
        assert!(!describe(&plain).contains("layer"), "{}", describe(&plain));
    }

    #[test]
    fn the_marks_carry_what_a_cover_needs_to_be_drawn() {
        let marks = facts(&summary());
        assert!(marks.contains(&"keys:0-127".to_string()), "{marks:?}");
        assert!(marks.contains(&"roots:21-108".to_string()), "{marks:?}");
        assert!(marks.contains(&"zones:807".to_string()), "{marks:?}");
        assert!(marks.contains(&"layers:4".to_string()), "{marks:?}");
        assert!(marks.contains(&"samples:599".to_string()), "{marks:?}");
        assert!(marks.contains(&"looping".to_string()), "{marks:?}");
    }

    #[test]
    fn an_instrument_that_never_loops_is_not_marked_as_looping() {
        let mut once = summary();
        once.looping = false;
        assert!(!facts(&once).contains(&"looping".to_string()));
    }
}

#[cfg(test)]
mod bank_id_tests {
    use super::sfz_bank_id;

    #[test]
    fn a_library_is_not_renamed_after_its_file_format() {
        // A library called Headroom Piano is not called SFZ Headroom Piano.
        assert_eq!(sfz_bank_id("HeadroomPiano"), "headroompiano");
        assert!(!sfz_bank_id("VSCO2").starts_with("sfz"));
    }

    #[test]
    fn punctuation_becomes_a_single_separator() {
        assert_eq!(
            sfz_bank_id("Virtual Playing  Orchestra"),
            "virtual-playing-orchestra"
        );
        assert_eq!(sfz_bank_id("VSCO-2 CE"), "vsco-2-ce");
    }

    #[test]
    fn a_library_cannot_take_over_a_bank_this_plugin_owns() {
        assert_ne!(sfz_bank_id("DLS"), "dls");
    }

    #[test]
    fn a_nameless_folder_still_yields_an_identifier() {
        assert_eq!(sfz_bank_id("!!!"), "library");
    }
}

/// One line about an instrument, for a surface with room for one line.
fn describe(summary: &rf_soundfonts::sfz::instrument::InstrumentSummary) -> String {
    // The recorded range, not the range it answers to: the second is nearly
    // always the whole keyboard and tells a player nothing.
    let mut text = format!(
        "{}–{} · {} zones",
        note_name(summary.root_low),
        note_name(summary.root_high),
        summary.regions
    );
    if summary.velocity_layers > 1 {
        text.push_str(&format!(" · {} layers", summary.velocity_layers));
    }
    text.push_str(&format!(" · {}", memory(summary.resident_bytes)));
    text
}

/// The same facts as marks, for a surface that wants to draw rather than read.
///
/// Written as `name:value` because a tag is a bare string and the host does
/// not interpret it. Only this plugin and its own web surface read these, so
/// the shape is theirs to agree on; nothing else should depend on it.
fn facts(summary: &rf_soundfonts::sfz::instrument::InstrumentSummary) -> Vec<String> {
    let mut tags = vec![
        "sfz".to_string(),
        format!("keys:{}-{}", summary.key_low, summary.key_high),
        format!("roots:{}-{}", summary.root_low, summary.root_high),
        format!("zones:{}", summary.regions),
        format!("samples:{}", summary.samples),
        format!("layers:{}", summary.velocity_layers),
        format!("bytes:{}", summary.resident_bytes),
    ];
    if summary.looping {
        tags.push("looping".to_string());
    }
    tags
}

/// Resident memory, in the largest unit that still reads as a number.
///
/// A tenth of a megabyte shown as `0 MiB` looks like nothing loaded at all.
fn memory(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{} MiB", bytes / 1_048_576)
    } else {
        format!("{} KiB", bytes / 1_024)
    }
}

/// A MIDI note as a player would name it, with middle C as C4.
fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(note) % 12],
        i16::from(note) / 12 - 1
    )
}

/// Bank identifier for a library, derived from its folder name.
///
/// Not prefixed. The identifier is meant to be internal, but it surfaces in
/// logs and can surface in a UI, and a library called Headroom Piano is not
/// called SFZ Headroom Piano — the format it happens to be written in is not
/// part of its name. Only the two names this plugin already owns are given a
/// suffix, and only if a library collides with them.
fn sfz_bank_id(library: &str) -> String {
    let mut id = String::with_capacity(library.len());
    let mut separator = false;
    for byte in library.bytes() {
        if byte.is_ascii_alphanumeric() {
            if separator && !id.is_empty() {
                id.push('-');
            }
            id.push(char::from(byte.to_ascii_lowercase()));
            separator = false;
        } else {
            separator = true;
        }
    }
    if id.is_empty() {
        id.push_str("library");
    }
    if id == "dls" {
        id.push_str("-library");
    }
    id
}

fn publish_dynamic_catalog(
    host: &HostApiV1,
    bank: &DlsBank,
    sfz: Option<&sfz_library::SfzLibrary>,
) -> Result<(), String> {
    if version_major(host.api_version) != 1
        || version_minor(host.api_version) < 3
        || host.struct_size < size_of::<HostApiV1>() as u32
    {
        return Err("host does not provide dynamic preset API 1.3".into());
    }
    let callback = host
        .publish_preset_catalog
        .ok_or_else(|| "host dynamic preset callback is unavailable".to_owned())?;
    let bytes =
        serde_json::to_vec(&dynamic_catalog(bank, sfz)).map_err(|error| error.to_string())?;
    // SAFETY: bytes remain readable for the callback duration.
    let status = unsafe { callback(host.context, bytes.as_ptr(), bytes.len()) };
    if status != STATUS_OK {
        return Err(format!(
            "host rejected the dynamic DLS catalog with status {status}"
        ));
    }
    Ok(())
}

fn resource_path(host: &HostApiV1) -> Result<PathBuf, String> {
    named_resource_path(host, RESOURCE_ID)
}

/// Names a resource in a message.
fn label(resource: &[u8]) -> String {
    String::from_utf8_lossy(resource).into_owned()
}

/// Asks the host where one declared resource was installed.
///
/// Generalised from the single-bank original because the plugin now has two
/// sources and either may be absent: a missing resource is a fact to act on,
/// not a failure.
fn named_resource_path(host: &HostApiV1, resource: &[u8]) -> Result<PathBuf, String> {
    if version_major(host.api_version) != 1
        || version_minor(host.api_version) < 1
        || host.struct_size < size_of::<HostApiV1>() as u32
    {
        return Err("host does not provide RackForge resource API 1.1".into());
    }
    let callback = host
        .get_resource_path
        .ok_or_else(|| "host resource callback is unavailable".to_owned())?;
    // SAFETY: `resource` is readable and a null destination queries the size.
    let required = unsafe {
        callback(
            host.context,
            resource.as_ptr(),
            resource.len(),
            ptr::null_mut(),
            0,
        )
    };
    if required == 0 || required > 32 * 1024 {
        return Err(format!("{} is missing or invalid", label(resource)));
    }
    let mut bytes = vec![0_u8; required];
    // SAFETY: the destination has exactly the size reported by the host.
    let reported = unsafe {
        callback(
            host.context,
            resource.as_ptr(),
            resource.len(),
            bytes.as_mut_ptr(),
            bytes.len(),
        )
    };
    if reported != required {
        return Err(format!("{} moved while being read", label(resource)));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{} has a non-UTF-8 path", label(resource)))?;
    Ok(PathBuf::from(text))
}

unsafe extern "C" fn write_runtime_descriptor(destination: *mut u8, capacity: usize) -> usize {
    unsafe { copy_to_host_buffer(RUNTIME_DESCRIPTOR, destination, capacity) }
}

unsafe extern "C" fn write_parameter_schema(destination: *mut u8, capacity: usize) -> usize {
    unsafe { copy_to_host_buffer(PARAMETER_SCHEMA, destination, capacity) }
}

unsafe extern "C" fn write_preset_catalog(destination: *mut u8, capacity: usize) -> usize {
    unsafe { copy_to_host_buffer(PRESET_CATALOG, destination, capacity) }
}

unsafe extern "C" fn create(host: *const HostApiV1) -> *mut c_void {
    let Some(host) = (unsafe { host.as_ref() }) else {
        return ptr::null_mut();
    };
    // A missing DLS bank is no longer fatal. The plugin has two sources now,
    // and an installation driven entirely by an SFZ library must still load;
    // refusing it over a bank the player never installed would be the same
    // mistake as demanding hardware the host does not have.
    let bank = match resource_path(host)
        .and_then(|path| DlsBank::open(path).map_err(|error| error.to_string()))
    {
        Ok(bank) => bank,
        Err(reason) => {
            log_host(
                host,
                LOG_LEVEL_INFO,
                &format!("no DLS bank available ({reason}); running on SFZ alone"),
            );
            DlsBank {
                instruments: Vec::new(),
                waves: Vec::new(),
            }
        }
    };
    let result = Ok::<DlsBank, String>(bank).and_then(|bank| {
        let default = bank
            .instrument(0, 0)
            .or_else(|| {
                bank.instruments
                    .iter()
                    .find(|instrument| !instrument.is_drum())
            })
            .or_else(|| bank.instruments.first())
            .map(|instrument| (instrument.bank, instrument.program))
            // An empty bank leaves the selection pointing at nothing,
            // which is correct: an SFZ preset will replace it, and until
            // one is chosen the plugin simply has no sound to make.
            .unwrap_or((0, 0));
        publish_dynamic_catalog(host, &bank, None)?;
        Ok((bank, default))
    });
    match result {
        Ok((bank, (selected_bank, selected_program))) => {
            log_host(host, LOG_LEVEL_INFO, "RF-Soundfonts libraries loaded");
            let mut plugin = RfDls::new(*host, bank, selected_bank, selected_program);
            // Loaded after the DLS bank so a library that fails to read leaves
            // a working instrument rather than no instrument at all.
            if let Ok(root) = named_resource_path(host, SFZ_RESOURCE_ID) {
                let (library, failures) = sfz_library::SfzLibrary::load(&root);
                for failure in &failures {
                    log_host(host, LOG_LEVEL_INFO, &format!("SFZ skipped: {failure}"));
                }
                if !library.is_empty() {
                    log_host(
                        host,
                        LOG_LEVEL_INFO,
                        &format!(
                            "SFZ loaded {} instruments from {} libraries, {} MiB resident",
                            library.instruments().len(),
                            library.libraries().len(),
                            library.resident_bytes() / 1_048_576
                        ),
                    );
                    plugin.attach_sfz(library);
                    let _ = publish_dynamic_catalog(host, &plugin.bank, plugin.sfz.as_ref());
                }
            }
            Box::into_raw(Box::new(plugin)).cast()
        }
        Err(error) => {
            log_host(host, LOG_LEVEL_ERROR, &error);
            ptr::null_mut()
        }
    }
}

unsafe extern "C" fn destroy(instance: *mut c_void) {
    if !instance.is_null() {
        // SAFETY: instances originate from Box::into_raw in create.
        unsafe {
            drop(Box::from_raw(instance.cast::<RfDls>()));
        }
    }
}

unsafe extern "C" fn activate(
    instance: *mut c_void,
    sample_rate: f64,
    maximum_frames: u32,
    input_channels: u32,
    output_channels: u32,
) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if !sample_rate.is_finite()
        || !(8_000.0..=384_000.0).contains(&sample_rate)
        || maximum_frames == 0
        || input_channels != 0
        || output_channels == 0
        || output_channels > 8
    {
        return STATUS_INVALID_ARGUMENT;
    }
    plugin.sample_rate = sample_rate.round() as u32;
    plugin.maximum_frames = maximum_frames;
    plugin.output_channels = output_channels;
    plugin.active = true;
    plugin.reset();
    STATUS_OK
}

unsafe extern "C" fn deactivate(instance: *mut c_void) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    plugin.reset();
    plugin.active = false;
    STATUS_OK
}

unsafe extern "C" fn reset(instance: *mut c_void) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    plugin.reset();
    STATUS_OK
}

unsafe extern "C" fn set_parameter(instance: *mut c_void, parameter_index: u32, value: f64) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    plugin.set_parameter(parameter_index, value)
}

unsafe extern "C" fn get_parameter(
    instance: *mut c_void,
    parameter_index: u32,
    value: *mut f64,
) -> i32 {
    let (Some(plugin), Some(output)) = (unsafe { instance.cast::<RfDls>().as_ref() }, unsafe {
        value.as_mut()
    }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    *output = match parameter_index {
        MASTER_GAIN_PARAMETER => f64::from(plugin.master_gain),
        _ => return STATUS_UNKNOWN_PARAMETER,
    };
    STATUS_OK
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedStateV4 {
    schema_version: u32,
    master_gain: f32,
    selected_preset_id: String,
}

#[derive(Deserialize)]
struct SavedStateV3 {
    schema_version: u32,
    master_gain: f32,
    selected_preset_id: String,
    active_program: LegacyProgramState,
}

#[derive(Deserialize)]
struct LegacyProgramState {
    layers: Vec<LegacyLayerState>,
}

#[derive(Deserialize)]
struct LegacyLayerState {
    enabled: bool,
    source: LegacySourceState,
}

#[derive(Deserialize)]
struct LegacySourceState {
    bank: u32,
    program: u32,
}

fn state_bytes(plugin: &RfDls) -> Vec<u8> {
    let snapshot = SavedStateV4 {
        schema_version: 4,
        master_gain: plugin.master_gain,
        selected_preset_id: plugin.selected_preset_id.clone(),
    };
    let payload = serde_json::to_vec(&snapshot).expect("validated RF-Soundfonts state serializes");
    let mut state = Vec::with_capacity(4 + payload.len());
    state.extend_from_slice(b"RFD4");
    state.extend_from_slice(&payload);
    state
}

unsafe extern "C" fn save_state(
    instance: *mut c_void,
    destination: *mut u8,
    capacity: usize,
) -> usize {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_ref() }) else {
        return 0;
    };
    let bytes = state_bytes(plugin);
    unsafe { copy_to_host_buffer(&bytes, destination, capacity) }
}

unsafe extern "C" fn load_state(instance: *mut c_void, source: *const u8, length: usize) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if source.is_null() || length < 4 {
        return STATUS_INVALID_ARGUMENT;
    }
    let bytes = unsafe { slice::from_raw_parts(source, length) };
    if &bytes[..4] == b"RFD4" {
        let Ok(snapshot) = serde_json::from_slice::<SavedStateV4>(&bytes[4..]) else {
            return STATUS_INVALID_ARGUMENT;
        };
        if snapshot.schema_version != 4
            || snapshot.selected_preset_id.is_empty()
            || snapshot.selected_preset_id.len() > 256
            || !plugin.has_preset(&snapshot.selected_preset_id)
        {
            return STATUS_INVALID_STATE;
        }
        let status = plugin.set_parameter(MASTER_GAIN_PARAMETER, f64::from(snapshot.master_gain));
        if status != STATUS_OK {
            return status;
        }
        return if plugin.select_preset(&snapshot.selected_preset_id) {
            STATUS_OK
        } else {
            STATUS_INVALID_STATE
        };
    }
    if &bytes[..4] == b"RFD3" {
        let Ok(snapshot) = serde_json::from_slice::<SavedStateV3>(&bytes[4..]) else {
            return STATUS_INVALID_ARGUMENT;
        };
        if snapshot.schema_version != 3
            || snapshot.selected_preset_id.is_empty()
            || snapshot.selected_preset_id.len() > 256
        {
            return STATUS_INVALID_STATE;
        }
        let status = plugin.set_parameter(MASTER_GAIN_PARAMETER, f64::from(snapshot.master_gain));
        if status != STATUS_OK {
            return status;
        }
        if plugin.has_preset(&snapshot.selected_preset_id) {
            return if plugin.select_preset(&snapshot.selected_preset_id) {
                STATUS_OK
            } else {
                STATUS_INVALID_STATE
            };
        }
        let Some(primary) = snapshot
            .active_program
            .layers
            .into_iter()
            .find(|layer| layer.enabled)
        else {
            return STATUS_INVALID_STATE;
        };
        return if plugin.select_instrument(primary.source.bank, primary.source.program) {
            STATUS_OK
        } else {
            STATUS_INVALID_STATE
        };
    }
    let (gain, selected) = if &bytes[..4] == b"RFD1" && length == LEGACY_STATE_SIZE {
        let gain = f32::from_le_bytes(bytes[4..8].try_into().expect("fixed slice"));
        let bank = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed slice"));
        let program = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed slice"));
        (gain, dynamic_preset_id(bank, program))
    } else if &bytes[..4] == b"RFD2" && length >= 10 {
        let gain = f32::from_le_bytes(bytes[4..8].try_into().expect("fixed slice"));
        let id_length = u16::from_le_bytes(bytes[8..10].try_into().expect("fixed slice")) as usize;
        if id_length == 0 || length != 10 + id_length {
            return STATUS_INVALID_ARGUMENT;
        }
        let Ok(id) = std::str::from_utf8(&bytes[10..]) else {
            return STATUS_INVALID_ARGUMENT;
        };
        (gain, id.to_owned())
    } else {
        return STATUS_INVALID_ARGUMENT;
    };
    if !plugin.has_preset(&selected) {
        return STATUS_INVALID_STATE;
    }
    let status = plugin.set_parameter(MASTER_GAIN_PARAMETER, f64::from(gain));
    if status != STATUS_OK {
        return status;
    }
    if !plugin.select_preset(&selected) {
        return STATUS_INVALID_STATE;
    }
    STATUS_OK
}

unsafe extern "C" fn load_preset(
    instance: *mut c_void,
    preset_id: *const u8,
    length: usize,
) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if preset_id.is_null() || length == 0 {
        return STATUS_INVALID_ARGUMENT;
    }
    let id = unsafe { slice::from_raw_parts(preset_id, length) };
    let Ok(id) = std::str::from_utf8(id) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if !plugin.select_preset(id) {
        return STATUS_INVALID_STATE;
    }
    plugin.master_gain = DEFAULT_MASTER_GAIN;
    STATUS_OK
}

unsafe extern "C" fn process(instance: *mut c_void, block: *const ProcessBlockV1) -> i32 {
    let (Some(plugin), Some(block)) = (unsafe { instance.cast::<RfDls>().as_mut() }, unsafe {
        block.as_ref()
    }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if !plugin.active
        || block.struct_size < size_of::<ProcessBlockV1>() as u32
        || block.frames == 0
        || block.frames > plugin.maximum_frames
        || block.input_channels != 0
        || block.output_channels != plugin.output_channels
        || block.output_interleaved.is_null()
    {
        return STATUS_INVALID_STATE;
    }
    let output_length = block.frames as usize * block.output_channels as usize;
    let output = unsafe { slice::from_raw_parts_mut(block.output_interleaved, output_length) };
    output.fill(0.0);
    let midi = unsafe {
        event_slice(
            block.midi_events,
            block.midi_event_count,
            block.frames,
            |event: &MidiEventV1| event.frame,
        )
    };
    let parameters = unsafe {
        event_slice(
            block.parameter_events,
            block.parameter_event_count,
            block.frames,
            |event: &ParameterEventV1| event.frame,
        )
    };
    let (Ok(midi), Ok(parameters)) = (midi, parameters) else {
        return STATUS_INVALID_ARGUMENT;
    };

    let mut midi_index = 0;
    let mut parameter_index = 0;
    for frame in 0..block.frames {
        while parameter_index < parameters.len() && parameters[parameter_index].frame == frame {
            let event = parameters[parameter_index];
            let status = plugin.set_parameter(event.parameter_index, event.value);
            if status != STATUS_OK {
                return status;
            }
            parameter_index += 1;
        }
        while midi_index < midi.len() && midi[midi_index].frame == frame {
            plugin.handle_midi(midi[midi_index]);
            midi_index += 1;
        }
        let [left, right] = plugin.render_frame();
        let start = frame as usize * block.output_channels as usize;
        output[start] = if block.output_channels == 1 {
            (left + right) * 0.5
        } else {
            left
        };
        if block.output_channels > 1 {
            output[start + 1] = right;
        }
        for channel in 2..block.output_channels as usize {
            output[start + channel] = (left + right) * 0.5;
        }
    }
    STATUS_OK
}

unsafe fn event_slice<'a, T, F>(
    pointer: *const T,
    count: u32,
    frames: u32,
    frame_of: F,
) -> Result<&'a [T], ()>
where
    F: Fn(&T) -> u32,
{
    if count == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(());
    }
    let events = unsafe { slice::from_raw_parts(pointer, count as usize) };
    if events.iter().any(|event| frame_of(event) >= frames)
        || events
            .windows(2)
            .any(|pair| frame_of(&pair[0]) > frame_of(&pair[1]))
    {
        return Err(());
    }
    Ok(events)
}

unsafe extern "C" fn activate_surface_json(
    instance: *mut c_void,
    source: *const u8,
    source_length: usize,
    destination: *mut u8,
    capacity: usize,
) -> usize {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_ref() }) else {
        return 0;
    };
    let Some(bytes) = (unsafe { read_extension_source(source, source_length) }) else {
        return 0;
    };
    let response = serde_json::from_slice::<SurfaceActivationRequest>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|request| plugin.activate_surface(request))
        .and_then(|response| serde_json::to_vec(&response).map_err(|error| error.to_string()));
    match response {
        Ok(bytes) => unsafe { copy_to_host_buffer(&bytes, destination, capacity) },
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            0
        }
    }
}

unsafe fn read_extension_source<'a>(source: *const u8, source_length: usize) -> Option<&'a [u8]> {
    if source.is_null() || source_length == 0 || source_length > 1024 * 1024 {
        return None;
    }
    Some(unsafe { slice::from_raw_parts(source, source_length) })
}

static PLUGIN_API: PluginApiV1 = PluginApiV1 {
    struct_size: size_of::<PluginApiV1>() as u32,
    api_version: pack_version(1, 3),
    runtime_descriptor_json: write_runtime_descriptor,
    parameter_schema_json: write_parameter_schema,
    preset_catalog_json: write_preset_catalog,
    create,
    destroy,
    activate,
    deactivate,
    reset,
    set_parameter,
    get_parameter,
    save_state,
    load_state,
    load_preset,
    process,
};

static SURFACE_EXTENSION_API: SurfaceExtensionApiV1 = SurfaceExtensionApiV1 {
    struct_size: size_of::<SurfaceExtensionApiV1>() as u32,
    api_version: SURFACE_EXTENSION_VERSION,
    activate: activate_surface_json,
};

#[unsafe(no_mangle)]
pub extern "C" fn rackforge_plugin_entry_v1() -> *const PluginApiV1 {
    ptr::addr_of!(PLUGIN_API)
}

#[unsafe(no_mangle)]
pub extern "C" fn rackforge_surface_extension_entry_v1() -> *const SurfaceExtensionApiV1 {
    ptr::addr_of!(SURFACE_EXTENSION_API)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rackforge_plugin_api::{ParameterSchema, PresetCatalog, RuntimeDescriptor, SurfaceMode};
    use rf_soundfonts::{
        EnvelopeSpec, Instrument, LfoSpec, PitchEnvelopeSpec, Region, SampleLoop, SampleParams,
        Wave,
    };
    use std::sync::Arc;

    #[test]
    fn package_manifest_matches_the_crate_version() {
        let version = env!("CARGO_PKG_VERSION");
        let manifest = include_str!("../package/rackforge-plugin.toml");
        assert!(
            manifest.contains(&format!("version = \"{version}\"")),
            "rackforge-plugin.toml version must match the crate version {version}"
        );
        let descriptor: serde_json::Value = serde_json::from_slice(RUNTIME_DESCRIPTOR).unwrap();
        assert_eq!(descriptor["version"], version);
    }

    fn synthetic_plugin() -> RfDls {
        let samples = (0..512)
            .map(|frame| if frame % 2 == 0 { 0.5 } else { -0.5 })
            .collect::<Vec<_>>();
        RfDls::new(
            HostApiV1::new(ptr::null_mut(), None, None, None, None),
            DlsBank {
                instruments: vec![Instrument {
                    name: "Synthetic Piano".into(),
                    bank: 0,
                    program: 0,
                    regions: vec![Region {
                        key_low: 0,
                        key_high: 127,
                        velocity_low: 0,
                        velocity_high: 127,
                        wave_index: 0,
                        key_group: 0,
                        sample_params: Some(SampleParams {
                            unity_note: 60,
                            fine_tune: 0,
                            attenuation_db: 0.0,
                            sample_loop: Some(SampleLoop { start: 0, end: 512 }),
                        }),
                    }],
                    envelope: EnvelopeSpec {
                        attack_seconds: 0.0,
                        decay_seconds: 1_000.0,
                        sustain_level: 1.0,
                        release_seconds: 0.01,
                    },
                    pitch_envelope: PitchEnvelopeSpec::default(),
                    lfo: LfoSpec::default(),
                }],
                waves: vec![Wave {
                    name: "Synthetic Wave".into(),
                    sample_rate: 48_000,
                    channels: 1,
                    source_bits: 16,
                    samples: Arc::from(samples),
                    sample_params: None,
                }],
            },
            0,
            0,
        )
    }

    #[test]
    fn surface_activation_focuses_only_library_instruments() {
        let plugin = synthetic_plugin();
        let existing = plugin
            .activate_surface(SurfaceActivationRequest::return_to(
                "little@1",
                SurfaceMode::Play,
                Some("dls.b00000000.p00000000".into()),
            ))
            .unwrap();
        assert_eq!(
            existing.focus_item_id.as_deref(),
            Some("dls.b00000000.p00000000")
        );
        let removed_custom = plugin
            .activate_surface(SurfaceActivationRequest::return_to(
                "little@1",
                SurfaceMode::Play,
                Some("custom.user.warm-piano".into()),
            ))
            .unwrap();
        assert_eq!(removed_custom.focus_item_id, None);
    }

    #[test]
    fn exports_valid_metadata() {
        let descriptor: RuntimeDescriptor = serde_json::from_slice(RUNTIME_DESCRIPTOR).unwrap();
        assert_eq!(descriptor.id, "org.rackforge.rf-soundfonts");
        assert_eq!(descriptor.state_version, 4);
        let parameters: ParameterSchema = serde_json::from_slice(PARAMETER_SCHEMA).unwrap();
        assert_eq!(parameters.validate(), Ok(()));
        let presets: PresetCatalog = serde_json::from_slice(PRESET_CATALOG).unwrap();
        assert_eq!(presets.validate(), Ok(()));
    }

    #[test]
    fn dynamic_catalog_contains_only_library_sounds() {
        let plugin = synthetic_plugin();
        let catalog = dynamic_catalog(&plugin.bank, plugin.sfz.as_ref());
        assert_eq!(catalog.validate(), Ok(()));
        assert_eq!(catalog.banks.len(), 1);
        assert_eq!(catalog.banks[0].id, "dls");
        assert_eq!(catalog.presets.len(), 1);
        assert_eq!(catalog.presets[0].id, "dls.b00000000.p00000000");
        assert!(!catalog.presets[0].editable);
        assert!(
            catalog
                .presets
                .iter()
                .all(|preset| preset.bank.as_deref() != Some("custom"))
        );
    }

    #[test]
    fn midi_drives_the_selected_library_instrument() {
        let mut plugin = synthetic_plugin();
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            data: [0x90, 60, 100],
            length: 3,
        });
        assert!(!plugin.voices.is_empty());
        let [left, right] = plugin.render_frame();
        assert_ne!(left + right, 0.0);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            data: [0x80, 60, 0],
            length: 3,
        });
        assert!(!plugin.held_notes[60]);
    }

    #[test]
    fn state_round_trip_preserves_gain_and_selected_sound() {
        let mut plugin = synthetic_plugin();
        plugin.master_gain = 0.42;
        let state = state_bytes(&plugin);
        assert_eq!(&state[..4], b"RFD4");

        plugin.master_gain = 0.9;
        plugin.selected_preset_id = "invalid".into();
        let status = unsafe {
            load_state(
                (&mut plugin as *mut RfDls).cast(),
                state.as_ptr(),
                state.len(),
            )
        };
        assert_eq!(status, STATUS_OK);
        assert_eq!(plugin.master_gain, 0.42);
        assert_eq!(plugin.selected_preset_id, "dls.b00000000.p00000000");
    }

    #[test]
    fn version_three_custom_state_migrates_to_its_primary_library_sound() {
        let mut plugin = synthetic_plugin();
        let legacy = serde_json::json!({
            "schema_version": 3,
            "master_gain": 0.5,
            "selected_preset_id": "custom.user.warm-piano",
            "active_program": {
                "slot": 1,
                "gain": 0.75,
                "layers": [{
                    "id": "a",
                    "enabled": true,
                    "source": {
                        "resource_id": "dls-bank",
                        "bank": 0,
                        "program": 0
                    },
                    "parameters": {}
                }],
                "effects": {}
            }
        });
        let mut state = b"RFD3".to_vec();
        state.extend_from_slice(&serde_json::to_vec(&legacy).unwrap());
        let status = unsafe {
            load_state(
                (&mut plugin as *mut RfDls).cast(),
                state.as_ptr(),
                state.len(),
            )
        };
        assert_eq!(status, STATUS_OK);
        assert_eq!(plugin.selected_preset_id, "dls.b00000000.p00000000");
        assert_eq!(plugin.master_gain, 0.5);
    }
}
