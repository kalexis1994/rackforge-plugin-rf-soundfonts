mod custom_program;
mod program_editor;
mod sfz_library;
use custom_program::{CustomProgram, CustomProgramPayload, DlsSource, ProgramLayer, SharedEffects};

use rackforge_dsp::{Chorus, Exciter, Reverb, StereoFrame};
use rackforge_plugin_api::abi::{
    HostApiV1, LOG_LEVEL_ERROR, LOG_LEVEL_INFO, LOG_LEVEL_WARN, MidiEventV1,
    PROGRAM_EXTENSION_VERSION, ParameterEventV1, PluginApiV1, ProcessBlockV1,
    ProgramExtensionApiV1, STATUS_INVALID_ARGUMENT, STATUS_INVALID_STATE, STATUS_OK,
    STATUS_UNKNOWN_PARAMETER, SURFACE_EXTENSION_VERSION, SurfaceExtensionApiV1,
    copy_to_host_buffer, pack_version, version_major, version_minor,
};
use rackforge_plugin_api::{
    BankDescriptor, PreparedProgram, PresetCatalog, PresetDescriptor, ProgramDocument,
    ProgramEditRequest, ProgramEditorView, ProgramFieldEditRequest, SurfaceActivationRequest,
    SurfaceActivationResponse,
};
use rf_dls::{DlsBank, Voice};
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

const RUNTIME_DESCRIPTOR: &[u8] = br#"{
  "schema_version": 1,
  "id": "org.rackforge.rf-dls",
  "version": "0.1.0",
  "state_version": 3
}"#;

const PARAMETER_SCHEMA: &[u8] = br#"{
  "schema_version": 1,
  "pages": [
    { "id": "sound", "name": "Sound", "order": 0, "header": "RF-DLS" }
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
    program_gain: f32,
    program_gain_target: f32,
    active_layers: Vec<ProgramLayer>,
    active_effects: SharedEffects,
    exciter: Exciter,
    chorus: Chorus,
    reverb: Reverb,
    selected_bank: u32,
    selected_program: u32,
    selected_preset_id: String,
    custom_programs: Vec<CustomProgram>,
    /// Every installed SFZ instrument, all resident. Absent when no library
    /// resource is installed.
    sfz: Option<sfz_library::SfzLibrary>,
    /// Which of them currently receives notes. `None` means the DLS path.
    selected_sfz: Option<usize>,
}

impl RfDls {
    fn new(
        host: HostApiV1,
        bank: DlsBank,
        selected_bank: u32,
        selected_program: u32,
        custom_programs: Vec<CustomProgram>,
    ) -> Self {
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
            program_gain: 1.0,
            program_gain_target: 1.0,
            active_layers: vec![ProgramLayer::inherited(
                "a",
                DlsSource {
                    resource_id: "dls-bank".into(),
                    bank: selected_bank,
                    program: selected_program,
                },
            )],
            active_effects: SharedEffects::default(),
            exciter: Exciter::new(48_000.0).expect("48 kHz is a supported DSP sample rate"),
            chorus: Chorus::new(48_000.0).expect("48 kHz is a supported DSP sample rate"),
            reverb: Reverb::new(48_000.0).expect("48 kHz is a supported DSP sample rate"),
            selected_bank,
            selected_program,
            selected_preset_id: dynamic_preset_id(selected_bank, selected_program),
            custom_programs,
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
        self.exciter.reset();
        self.chorus.reset();
        self.reverb.reset();
    }

    fn select_instrument(&mut self, bank: u32, program: u32) -> bool {
        if self.bank.instrument(bank, program).is_none() {
            return false;
        }
        self.selected_bank = bank;
        self.selected_program = program;
        self.selected_preset_id = dynamic_preset_id(bank, program);
        self.program_gain = 1.0;
        self.program_gain_target = 1.0;
        self.active_layers = vec![ProgramLayer::inherited(
            "a",
            DlsSource {
                resource_id: "dls-bank".into(),
                bank,
                program,
            },
        )];
        self.active_effects = SharedEffects::default();
        self.exciter
            .set_parameters(self.active_effects.exciter.parameters())
            .expect("default exciter settings are valid");
        self.chorus
            .set_parameters(self.active_effects.chorus.parameters())
            .expect("default chorus settings are valid");
        self.reverb
            .set_parameters(self.active_effects.reverb.parameters())
            .expect("default reverb settings are valid");
        self.reset();
        true
    }

    fn select_custom(&mut self, preset_id: &str) -> bool {
        let Some(program) = self
            .custom_programs
            .iter()
            .find(|program| program.preset_id() == preset_id)
            .cloned()
        else {
            return false;
        };
        self.apply_custom_program(program, preset_id)
    }

    fn apply_custom_program(&mut self, program: CustomProgram, preset_id: &str) -> bool {
        self.activate_custom_program(program, preset_id, true)
    }

    fn activate_custom_program(
        &mut self,
        program: CustomProgram,
        preset_id: &str,
        reset_voices: bool,
    ) -> bool {
        if program.layers.iter().any(|layer| {
            self.bank
                .instrument(layer.source.bank, layer.source.program)
                .is_none()
        }) {
            return false;
        }
        let Some(primary) = program.layers.iter().find(|layer| layer.enabled) else {
            return false;
        };
        self.selected_bank = primary.source.bank;
        self.selected_program = primary.source.program;
        self.selected_preset_id = preset_id.to_owned();
        self.program_gain_target = program.gain;
        if reset_voices {
            self.program_gain = program.gain;
        }
        if self
            .exciter
            .set_parameters(program.effects.exciter.parameters())
            .is_err()
            || self
                .chorus
                .set_parameters(program.effects.chorus.parameters())
                .is_err()
            || self
                .reverb
                .set_parameters(program.effects.reverb.parameters())
                .is_err()
        {
            return false;
        }
        self.active_effects = program.effects;
        self.active_layers = program.layers;
        if reset_voices {
            self.reset();
        }
        true
    }

    fn preview_program(&mut self, prepared: PreparedProgram) -> Result<(), String> {
        prepared.validate().map_err(|error| error.to_string())?;
        let canonical = self.prepare_program_save(prepared.document)?;
        if canonical.storage_path != prepared.storage_path
            || canonical.preview_sound_id != prepared.preview_sound_id
        {
            return Err("prepared RF-DLS preview metadata is not canonical".into());
        }
        let program = CustomProgram::from_document(canonical.document)?;
        let preset_id = format!("preview.{}", program.id);
        if !self.activate_custom_program(program, &preset_id, false) {
            return Err("RF-DLS could not activate the prepared preview".into());
        }
        Ok(())
    }

    fn select_preset(&mut self, preset_id: &str) -> bool {
        if let Some(id) = sfz_library::instrument_id(preset_id) {
            return self.select_sfz(id, preset_id);
        }
        // Leaving an SFZ instrument returns the plugin to the DLS path, whose
        // layers the SFZ instruments never used.
        self.selected_sfz = None;
        if preset_id.starts_with("custom.") {
            return self.select_custom(preset_id);
        }
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
        // Layers belong to the DLS side. An SFZ instrument carries its own
        // regions and must not inherit a split left over from another sound.
        self.active_layers.clear();
        self.reset();
        true
    }

    fn has_preset(&self, preset_id: &str) -> bool {
        if let Some(id) = preset_id.strip_prefix("custom.") {
            return self.custom_programs.iter().any(|program| program.id == id);
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

    fn begin_program_edit(&self, request: &ProgramEditRequest) -> Result<PreparedProgram, String> {
        request.validate().map_err(|error| error.to_string())?;
        if let Some(program_id) = &request.program_id {
            let id = program_id.strip_prefix("custom.").unwrap_or(program_id);
            let program = self
                .custom_programs
                .iter()
                .find(|program| program.id == id)
                .ok_or_else(|| format!("unknown CUSTOM program {program_id:?}"))?;
            return program.prepared();
        }

        let slot = (1..=999)
            .find(|slot| {
                self.custom_programs
                    .iter()
                    .all(|program| program.slot != *slot)
            })
            .ok_or_else(|| "RF-DLS has no free CUSTOM slots".to_owned())?;
        let source = self
            .bank
            .instruments
            .iter()
            .filter(|instrument| !instrument.is_drum())
            .min_by_key(|instrument| (instrument.bank, instrument.program))
            .or_else(|| self.bank.instruments.first())
            .ok_or_else(|| "DLS bank contains no source instruments".to_owned())?;
        CustomProgram {
            id: format!("user.custom-{slot:03}"),
            name: format!("CUSTOM {slot:03}"),
            category: None,
            slot,
            gain: 1.0,
            layers: vec![ProgramLayer::inherited(
                "a",
                DlsSource {
                    resource_id: "dls-bank".into(),
                    bank: source.bank,
                    program: source.program,
                },
            )],
            effects: SharedEffects::default(),
        }
        .prepared()
    }

    fn prepare_program_save(&self, document: ProgramDocument) -> Result<PreparedProgram, String> {
        let program = CustomProgram::from_document(document)?;
        for layer in &program.layers {
            if self
                .bank
                .instrument(layer.source.bank, layer.source.program)
                .is_none()
            {
                return Err(format!(
                    "DLS bank does not contain layer {:?} source bank={} program={}",
                    layer.id, layer.source.bank, layer.source.program
                ));
            }
        }
        if self
            .custom_programs
            .iter()
            .any(|candidate| candidate.id != program.id && candidate.slot == program.slot)
        {
            return Err(format!("CUSTOM slot {} is already in use", program.slot));
        }
        program.prepared()
    }

    fn program_editor_view(&self, document: ProgramDocument) -> Result<ProgramEditorView, String> {
        let program = CustomProgram::from_document(document)?;
        let view = program_editor::view(&program);
        view.validate().map_err(|error| error.to_string())?;
        Ok(view)
    }

    fn apply_program_edit(
        &self,
        request: ProgramFieldEditRequest,
    ) -> Result<PreparedProgram, String> {
        request.validate().map_err(|error| error.to_string())?;
        let mut program = CustomProgram::from_document(request.document)?;
        program_editor::apply(&mut program, &request.field_id, request.value)?;
        self.prepare_program_save(program.to_document()?)
    }

    fn install_program(&mut self, prepared: PreparedProgram) -> Result<(), String> {
        prepared.validate().map_err(|error| error.to_string())?;
        let canonical = self.prepare_program_save(prepared.document)?;
        if canonical.storage_path != prepared.storage_path
            || canonical.preview_sound_id != prepared.preview_sound_id
        {
            return Err("prepared RF-DLS program metadata is not canonical".into());
        }
        let program = CustomProgram::from_document(canonical.document)?;
        if let Some(current) = self
            .custom_programs
            .iter_mut()
            .find(|current| current.id == program.id)
        {
            *current = program;
        } else {
            self.custom_programs.push(program);
        }
        self.custom_programs
            .sort_by_key(|program| (program.slot, program.id.clone()));
        publish_dynamic_catalog(&self.host, &self.bank, &self.custom_programs, self.sfz.as_ref())
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
            // placement from controller state, so it bypasses the layer and
            // program machinery entirely rather than being squeezed into it.
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
        for layer in &self.active_layers {
            if !layer.accepts(note, velocity) {
                continue;
            }
            let Some(instrument) = bank.instrument(layer.source.bank, layer.source.program) else {
                continue;
            };
            let config = layer.voice_config(instrument);
            for region in instrument.matching_regions(note, velocity) {
                if let Ok(voice) = Voice::new_with_config(
                    bank,
                    instrument,
                    region,
                    note,
                    velocity,
                    self.sample_rate,
                    config,
                ) {
                    if self.voices.len() >= MAX_VOICES {
                        self.voices.remove(0);
                    }
                    self.voices.push(voice);
                }
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
    fn render_frame(&mut self) -> StereoFrame {
        self.program_gain += (self.program_gain_target - self.program_gain) * 0.005;
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
        let gain = self.master_gain * self.program_gain;
        StereoFrame::new(left * gain, right * gain)
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

fn dynamic_catalog(
    bank: &DlsBank,
    custom_programs: &[CustomProgram],
    sfz: Option<&sfz_library::SfzLibrary>,
) -> PresetCatalog {
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
    for program in custom_programs {
        presets.push(PresetDescriptor {
            id: program.preset_id(),
            name: program.name.clone(),
            description: Some(format!("CUSTOM {:03}", program.slot)),
            bank: Some("custom".into()),
            category: program.category.clone(),
            order: i32::from(program.slot),
            tags: vec!["custom".into()],
            editable: true,
        });
    }
    // Only banks with something in them are published. An installation driven
    // by SFZ alone should not make the player scroll past an empty DLS bank
    // and an empty CUSTOM bank to reach the instrument they installed.
    let mut banks = Vec::new();
    if !bank.instruments.is_empty() {
        banks.push(BankDescriptor {
            id: "dls".into(),
            name: "DLS".into(),
            order: 0,
        });
    }
    if !custom_programs.is_empty() {
        banks.push(BankDescriptor {
            id: "custom".into(),
            name: "CUSTOM".into(),
            order: 1,
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
                order: 2 + offset as i32,
            });
        }
        for (offset, loaded) in library.instruments().iter().enumerate() {
            presets.push(PresetDescriptor {
                id: sfz_library::preset_id(&loaded.id),
                name: loaded.name.clone(),
                description: Some(format!(
                    "{} MiB",
                    loaded.instrument.resident_bytes() / 1_048_576
                )),
                bank: Some(sfz_bank_id(&loaded.library)),
                category: Some("Instrument".into()),
                order: offset as i32,
                tags: vec!["sfz".into()],
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
        assert_eq!(sfz_bank_id("Virtual Playing  Orchestra"), "virtual-playing-orchestra");
        assert_eq!(sfz_bank_id("VSCO-2 CE"), "vsco-2-ce");
    }

    #[test]
    fn a_library_cannot_take_over_a_bank_this_plugin_owns() {
        assert_ne!(sfz_bank_id("DLS"), "dls");
        assert_ne!(sfz_bank_id("custom"), "custom");
    }

    #[test]
    fn a_nameless_folder_still_yields_an_identifier() {
        assert_eq!(sfz_bank_id("!!!"), "library");
    }
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
    if id == "dls" || id == "custom" {
        id.push_str("-library");
    }
    id
}

fn publish_dynamic_catalog(
    host: &HostApiV1,
    bank: &DlsBank,
    custom_programs: &[CustomProgram],
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
    let bytes = serde_json::to_vec(&dynamic_catalog(bank, custom_programs, sfz))
        .map_err(|error| error.to_string())?;
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

fn plugin_data_path(host: &HostApiV1) -> Result<Option<PathBuf>, String> {
    if version_major(host.api_version) != 1 || version_minor(host.api_version) < 2 {
        return Ok(None);
    }
    if host.struct_size < size_of::<HostApiV1>() as u32 {
        return Err("host API 1.2 table is truncated".into());
    }
    let Some(callback) = host.get_plugin_data_path else {
        return Ok(None);
    };
    // SAFETY: a null destination queries the required path size.
    let required = unsafe { callback(host.context, ptr::null_mut(), 0) };
    if required == 0 {
        return Ok(None);
    }
    if required > 32 * 1024 {
        return Err("plugin data path is unreasonably large".into());
    }
    let mut bytes = vec![0_u8; required];
    // SAFETY: bytes is writable for exactly the reported size.
    let reported = unsafe { callback(host.context, bytes.as_mut_ptr(), bytes.len()) };
    if reported != required {
        return Err("plugin data path changed while being read".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "plugin data path is not UTF-8".to_owned())?;
    Ok(Some(PathBuf::from(text)))
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
    let result = Ok::<DlsBank, String>(bank)
        .and_then(|bank| plugin_data_path(host).map(|data_root| (bank, data_root)))
        .and_then(|(bank, data_root)| {
            let (mut custom_programs, warnings) =
                custom_program::load_programs(data_root.as_deref())?;
            for warning in warnings {
                log_host(host, LOG_LEVEL_WARN, &warning);
            }
            custom_programs.retain(|program| {
                let missing = program.layers.iter().find(|layer| {
                    bank.instrument(layer.source.bank, layer.source.program)
                        .is_none()
                });
                if let Some(layer) = missing {
                    log_host(
                        host,
                        LOG_LEVEL_WARN,
                        &format!(
                            "ignoring CUSTOM {:?}: layer {:?} DLS bank {} program {} is missing",
                            program.id, layer.id, layer.source.bank, layer.source.program
                        ),
                    );
                    false
                } else {
                    true
                }
            });
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
            publish_dynamic_catalog(host, &bank, &custom_programs, None)?;
            Ok((bank, default, custom_programs))
        });
    match result {
        Ok((bank, (selected_bank, selected_program), custom_programs)) => {
            log_host(
                host,
                LOG_LEVEL_INFO,
                &format!(
                    "RF-DLS bank loaded with {} CUSTOM programs",
                    custom_programs.len()
                ),
            );
            let mut plugin = RfDls::new(
                *host,
                bank,
                selected_bank,
                selected_program,
                custom_programs,
            );
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
                    let _ = publish_dynamic_catalog(
                        host,
                        &plugin.bank,
                        &plugin.custom_programs,
                        plugin.sfz.as_ref(),
                    );
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
    let Ok(mut exciter) = Exciter::new(sample_rate as f32) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if exciter
        .set_parameters(plugin.active_effects.exciter.parameters())
        .is_err()
    {
        return STATUS_INVALID_STATE;
    }
    let Ok(mut chorus) = Chorus::new(sample_rate as f32) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if chorus
        .set_parameters(plugin.active_effects.chorus.parameters())
        .is_err()
    {
        return STATUS_INVALID_STATE;
    }
    let Ok(mut reverb) = Reverb::new(sample_rate as f32) else {
        return STATUS_INVALID_ARGUMENT;
    };
    if reverb
        .set_parameters(plugin.active_effects.reverb.parameters())
        .is_err()
    {
        return STATUS_INVALID_STATE;
    }
    plugin.exciter = exciter;
    plugin.chorus = chorus;
    plugin.reverb = reverb;
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
struct SavedStateV3 {
    schema_version: u32,
    master_gain: f32,
    selected_preset_id: String,
    active_program: CustomProgramPayload,
}

fn state_bytes(plugin: &RfDls) -> Vec<u8> {
    let snapshot = SavedStateV3 {
        schema_version: 3,
        master_gain: plugin.master_gain,
        selected_preset_id: plugin.selected_preset_id.clone(),
        active_program: CustomProgramPayload {
            slot: 1,
            gain: plugin.program_gain_target,
            layers: plugin.active_layers.clone(),
            effects: plugin.active_effects,
        },
    };
    let payload = serde_json::to_vec(&snapshot).expect("validated RF-DLS state serializes");
    let mut state = Vec::with_capacity(4 + payload.len());
    state.extend_from_slice(b"RFD3");
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
        let program = CustomProgram {
            id: "restored-state".into(),
            name: "Restored state".into(),
            category: None,
            slot: snapshot.active_program.slot,
            gain: snapshot.active_program.gain,
            layers: snapshot.active_program.layers,
            effects: snapshot.active_program.effects,
        };
        if program.validate().is_err() {
            return STATUS_INVALID_STATE;
        }
        if program.layers.iter().any(|layer| {
            plugin
                .bank
                .instrument(layer.source.bank, layer.source.program)
                .is_none()
        }) {
            return STATUS_INVALID_STATE;
        }
        let status = plugin.set_parameter(MASTER_GAIN_PARAMETER, f64::from(snapshot.master_gain));
        if status != STATUS_OK {
            return status;
        }
        if !plugin.activate_custom_program(program, &snapshot.selected_preset_id, true) {
            return STATUS_INVALID_STATE;
        }
        return STATUS_OK;
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
        let mixed = plugin.render_frame();
        let excited = plugin.exciter.process(mixed);
        let chorused = plugin.chorus.process(excited);
        let processed = plugin.reverb.process(chorused);
        let start = frame as usize * block.output_channels as usize;
        output[start] = if block.output_channels == 1 {
            processed.mono()
        } else {
            processed.left
        };
        if block.output_channels > 1 {
            output[start + 1] = processed.right;
        }
        for channel in 2..block.output_channels as usize {
            output[start + channel] = processed.mono();
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

unsafe extern "C" fn begin_program_edit_json(
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
    let prepared = serde_json::from_slice::<ProgramEditRequest>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|request| plugin.begin_program_edit(&request))
        .and_then(|prepared| serde_json::to_vec(&prepared).map_err(|error| error.to_string()));
    match prepared {
        Ok(bytes) => unsafe { copy_to_host_buffer(&bytes, destination, capacity) },
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            0
        }
    }
}

unsafe extern "C" fn prepare_program_save_json(
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
    let prepared = serde_json::from_slice::<ProgramDocument>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|document| plugin.prepare_program_save(document))
        .and_then(|prepared| serde_json::to_vec(&prepared).map_err(|error| error.to_string()));
    match prepared {
        Ok(bytes) => unsafe { copy_to_host_buffer(&bytes, destination, capacity) },
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            0
        }
    }
}

unsafe extern "C" fn program_editor_view_json(
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
    let view = serde_json::from_slice::<ProgramDocument>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|document| plugin.program_editor_view(document))
        .and_then(|view| serde_json::to_vec(&view).map_err(|error| error.to_string()));
    match view {
        Ok(bytes) => unsafe { copy_to_host_buffer(&bytes, destination, capacity) },
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            0
        }
    }
}

unsafe extern "C" fn apply_program_edit_json(
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
    let prepared = serde_json::from_slice::<ProgramFieldEditRequest>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|request| plugin.apply_program_edit(request))
        .and_then(|prepared| serde_json::to_vec(&prepared).map_err(|error| error.to_string()));
    match prepared {
        Ok(bytes) => unsafe { copy_to_host_buffer(&bytes, destination, capacity) },
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            0
        }
    }
}

unsafe extern "C" fn install_program_json(
    instance: *mut c_void,
    source: *const u8,
    source_length: usize,
) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let Some(bytes) = (unsafe { read_extension_source(source, source_length) }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    match serde_json::from_slice::<PreparedProgram>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|prepared| plugin.install_program(prepared))
    {
        Ok(()) => STATUS_OK,
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            STATUS_INVALID_ARGUMENT
        }
    }
}

unsafe extern "C" fn preview_program_json(
    instance: *mut c_void,
    source: *const u8,
    source_length: usize,
) -> i32 {
    let Some(plugin) = (unsafe { instance.cast::<RfDls>().as_mut() }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    let Some(bytes) = (unsafe { read_extension_source(source, source_length) }) else {
        return STATUS_INVALID_ARGUMENT;
    };
    match serde_json::from_slice::<PreparedProgram>(bytes)
        .map_err(|error| error.to_string())
        .and_then(|prepared| plugin.preview_program(prepared))
    {
        Ok(()) => STATUS_OK,
        Err(error) => {
            log_host(&plugin.host, LOG_LEVEL_WARN, &error);
            STATUS_INVALID_ARGUMENT
        }
    }
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

static PROGRAM_EXTENSION_API: ProgramExtensionApiV1 = ProgramExtensionApiV1 {
    struct_size: size_of::<ProgramExtensionApiV1>() as u32,
    api_version: PROGRAM_EXTENSION_VERSION,
    begin_edit: begin_program_edit_json,
    prepare_save: prepare_program_save_json,
    install: install_program_json,
    preview: Some(preview_program_json),
    editor_view: Some(program_editor_view_json),
    apply_edit: Some(apply_program_edit_json),
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
pub extern "C" fn rackforge_program_extension_entry_v1() -> *const ProgramExtensionApiV1 {
    ptr::addr_of!(PROGRAM_EXTENSION_API)
}

#[unsafe(no_mangle)]
pub extern "C" fn rackforge_surface_extension_entry_v1() -> *const SurfaceExtensionApiV1 {
    ptr::addr_of!(SURFACE_EXTENSION_API)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rackforge_plugin_api::{ParameterSchema, PresetCatalog, RuntimeDescriptor, SurfaceMode};
    use rf_dls::{
        EnvelopeSpec, Instrument, LfoSpec, PitchEnvelopeSpec, Region, SampleLoop, SampleParams,
        Wave,
    };
    use std::sync::Arc;

    fn custom_program() -> CustomProgram {
        let document = serde_json::from_slice(include_bytes!(
            "../examples/custom.warm-piano.rackforge-program.json"
        ))
        .unwrap();
        CustomProgram::from_document(document).unwrap()
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
            Vec::new(),
        )
    }

    #[test]
    fn surface_activation_only_focuses_existing_programs() {
        let mut plugin = synthetic_plugin();
        plugin.custom_programs.push(custom_program());
        let response = plugin
            .activate_surface(SurfaceActivationRequest::return_to(
                "little@1",
                SurfaceMode::Play,
                Some("custom.user.warm-piano".into()),
            ))
            .unwrap();
        assert_eq!(
            response.focus_item_id.as_deref(),
            Some("custom.user.warm-piano")
        );
        let response = plugin
            .activate_surface(SurfaceActivationRequest::return_to(
                "little@1",
                SurfaceMode::Play,
                Some("custom.missing".into()),
            ))
            .unwrap();
        assert_eq!(response.focus_item_id, None);
    }

    #[test]
    fn exports_valid_metadata() {
        let descriptor: RuntimeDescriptor = serde_json::from_slice(RUNTIME_DESCRIPTOR).unwrap();
        assert_eq!(descriptor.id, "org.rackforge.rf-dls");
        let parameters: ParameterSchema = serde_json::from_slice(PARAMETER_SCHEMA).unwrap();
        assert_eq!(parameters.validate(), Ok(()));
        let presets: PresetCatalog = serde_json::from_slice(PRESET_CATALOG).unwrap();
        assert_eq!(presets.validate(), Ok(()));
    }

    #[test]
    fn dynamic_catalog_uses_opaque_stable_ids() {
        let plugin = synthetic_plugin();
        let catalog = dynamic_catalog(&plugin.bank, &[], plugin.sfz.as_ref());
        assert_eq!(catalog.validate(), Ok(()));
        assert_eq!(catalog.presets.len(), 1);
        assert_eq!(catalog.presets[0].id, "dls.b00000000.p00000000");
        assert_eq!(catalog.presets[0].description.as_deref(), Some("B000 P000"));
        assert_eq!(
            parse_dynamic_preset_id(&catalog.presets[0].id),
            Some((0, 0))
        );
    }

    #[test]
    fn custom_programs_are_separate_and_survive_state_round_trip() {
        let mut plugin = synthetic_plugin();
        plugin.custom_programs.push(custom_program());
        let catalog = dynamic_catalog(&plugin.bank, &plugin.custom_programs, plugin.sfz.as_ref());
        assert_eq!(catalog.validate(), Ok(()));
        assert_eq!(catalog.banks[0].name, "DLS");
        assert_eq!(catalog.banks[1].name, "CUSTOM");
        assert_eq!(catalog.presets[1].id, "custom.user.warm-piano");
        assert_eq!(catalog.presets[1].bank.as_deref(), Some("custom"));

        assert!(plugin.select_custom("custom.user.warm-piano"));
        let state = state_bytes(&plugin);
        assert!(plugin.select_instrument(0, 0));
        let status = unsafe {
            load_state(
                (&mut plugin as *mut RfDls).cast(),
                state.as_ptr(),
                state.len(),
            )
        };
        assert_eq!(status, STATUS_OK);
        assert_eq!(plugin.selected_preset_id, "custom.user.warm-piano");
        let instrument = plugin.bank.instrument(0, 0).unwrap();
        assert_eq!(
            plugin.active_layers[0]
                .voice_config(instrument)
                .amplitude_envelope
                .release_seconds,
            1.2
        );
    }

    #[test]
    fn program_extension_creates_and_reopens_canonical_drafts() {
        let mut plugin = synthetic_plugin();
        let created = plugin
            .begin_program_edit(&ProgramEditRequest::new(None))
            .unwrap();
        assert_eq!(created.document.id, "user.custom-001");
        assert_eq!(created.document.name, "CUSTOM 001");
        assert_eq!(
            created.storage_path,
            "custom/user.custom-001.rackforge-program.json"
        );
        assert_eq!(
            plugin
                .prepare_program_save(created.document.clone())
                .unwrap(),
            created
        );

        plugin.custom_programs.push(custom_program());
        let reopened = plugin
            .begin_program_edit(&ProgramEditRequest::new(Some(
                "custom.user.warm-piano".into(),
            )))
            .unwrap();
        assert_eq!(reopened.document.id, "user.warm-piano");
        assert_eq!(reopened.preview_sound_id, "dls.b00000000.p00000000");
    }

    #[test]
    fn declarative_editor_owns_rf_dls_payload_mutations() {
        let plugin = synthetic_plugin();
        let created = plugin
            .begin_program_edit(&ProgramEditRequest::new(None))
            .unwrap();
        let view = plugin
            .program_editor_view(created.document.clone())
            .unwrap();
        assert_eq!(view.validate(), Ok(()));
        assert_eq!(view.title, "RF-DLS");
        assert_eq!(
            view.pages
                .iter()
                .map(|page| page.label.as_str())
                .collect::<Vec<_>>(),
            ["LAYER A", "LAYER B", "FX", "OUTPUT"]
        );

        let enabled = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: created.document,
                field_id: "layer.b.enabled".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Boolean(true),
            })
            .unwrap();
        let program = CustomProgram::from_document(enabled.document.clone()).unwrap();
        assert_eq!(program.layers.len(), 2);
        assert!(program.layers[1].enabled);

        let quieter = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: enabled.document,
                field_id: "layer.b.gain".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Integer(75),
            })
            .unwrap();
        let program = CustomProgram::from_document(quieter.document).unwrap();
        assert_eq!(program.layers[0].parameters.gain, 1.0);
        assert_eq!(program.layers[1].parameters.gain, 0.75);

        let exciter = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: program.to_document().unwrap(),
                field_id: "fx.exciter.enabled".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Boolean(true),
            })
            .unwrap();
        let exciter = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: exciter.document,
                field_id: "fx.exciter.emphatic-point".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Integer(7),
            })
            .unwrap();
        let program = CustomProgram::from_document(exciter.document).unwrap();
        assert!(program.effects.exciter.enabled);
        assert_eq!(program.effects.exciter.emphatic_point, 7);

        let chorus = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: program.to_document().unwrap(),
                field_id: "fx.chorus.enabled".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Boolean(true),
            })
            .unwrap();
        let chorus = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: chorus.document,
                field_id: "fx.chorus.rate".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Integer(125),
            })
            .unwrap();
        let program = CustomProgram::from_document(chorus.document).unwrap();
        assert!(program.effects.chorus.enabled);
        assert_eq!(program.effects.chorus.rate_hz, 1.25);

        let reverb = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: program.to_document().unwrap(),
                field_id: "fx.reverb.enabled".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Boolean(true),
            })
            .unwrap();
        let reverb = plugin
            .apply_program_edit(ProgramFieldEditRequest {
                schema_version: rackforge_plugin_api::PROGRAM_EDITOR_SCHEMA_VERSION,
                document: reverb.document,
                field_id: "fx.reverb.decay".into(),
                value: rackforge_plugin_api::ProgramEditorValue::Integer(375),
            })
            .unwrap();
        let program = CustomProgram::from_document(reverb.document).unwrap();
        assert!(program.effects.reverb.enabled);
        assert_eq!(program.effects.reverb.decay_seconds, 3.75);
    }

    #[test]
    fn draft_preview_applies_layers_without_installing_a_catalog_program() {
        let mut plugin = synthetic_plugin();
        plugin.note_on(60, 127);
        assert_eq!(plugin.voices.len(), 1);
        let mut program = custom_program();
        let mut layer_b = program.layers[0].clone();
        layer_b.id = "b".into();
        layer_b.parameters.gain = 0.5;
        layer_b.parameters.lfo.delay_seconds = Some(0.25);
        program.layers.push(layer_b);
        let prepared = program.prepared().unwrap();

        plugin.preview_program(prepared).unwrap();
        assert_eq!(plugin.active_layers.len(), 2);
        assert!(plugin.selected_preset_id.starts_with("preview."));
        assert!(plugin.custom_programs.is_empty());
        assert_eq!(plugin.voices.len(), 1);

        plugin.note_on(62, 127);
        assert_eq!(plugin.voices.len(), 3);
    }

    #[test]
    fn note_on_and_note_off_drive_a_voice() {
        let mut plugin = synthetic_plugin();
        plugin.note_on(60, 127);
        assert_eq!(plugin.voices.len(), 1);
        assert_ne!(plugin.render_frame().mono(), 0.0);
        plugin.note_off(60);
        assert!(!plugin.held_notes[60]);
        for _ in 0..600 {
            plugin.render_frame();
        }
        assert!(plugin.voices.is_empty());
    }

    #[test]
    fn sustain_defers_release_until_the_pedal_lifts() {
        let mut plugin = synthetic_plugin();
        plugin.note_on(60, 100);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xb0, 64, 127],
        });
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0x80, 60, 0],
        });
        for _ in 0..600 {
            plugin.render_frame();
        }
        assert_eq!(plugin.voices.len(), 1);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xb0, 64, 0],
        });
        for _ in 0..600 {
            plugin.render_frame();
        }
        assert!(plugin.voices.is_empty());
    }

    #[test]
    fn all_notes_off_respects_sustain_but_all_sound_off_does_not() {
        let mut plugin = synthetic_plugin();
        plugin.note_on(60, 100);
        plugin.set_sustain(true);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xb0, 123, 0],
        });
        assert_eq!(plugin.voices.len(), 1);
        assert!(!plugin.held_notes[60]);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xb0, 120, 0],
        });
        assert!(plugin.voices.is_empty());
        assert!(!plugin.sustain);
    }

    #[test]
    fn midi_note_on_with_zero_velocity_is_note_off() {
        let mut plugin = synthetic_plugin();
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0x90, 60, 127],
        });
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0x90, 60, 0],
        });
        assert!(!plugin.held_notes[60]);
    }

    #[test]
    fn pitch_bend_uses_the_full_fourteen_bit_range() {
        let mut plugin = synthetic_plugin();
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xe0, 0, 0],
        });
        assert_eq!(plugin.pitch_bend_normalized, -1.0);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xe0, 0, 64],
        });
        assert_eq!(plugin.pitch_bend_normalized, 0.0);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xe0, 127, 127],
        });
        assert_eq!(plugin.pitch_bend_normalized, 1.0);
    }

    #[test]
    fn mod_wheel_and_reset_controllers_update_plugin_state() {
        let mut plugin = synthetic_plugin();
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xb0, 1, 127],
        });
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xe0, 127, 127],
        });
        assert_eq!(plugin.modulation_wheel, 1.0);
        assert_eq!(plugin.pitch_bend_normalized, 1.0);
        plugin.handle_midi(MidiEventV1 {
            frame: 0,
            length: 3,
            data: [0xb0, 121, 0],
        });
        assert_eq!(plugin.modulation_wheel, 0.0);
        assert_eq!(plugin.pitch_bend_normalized, 0.0);
    }

    #[test]
    fn state_round_trip_preserves_gain_and_program() {
        let mut plugin = synthetic_plugin();
        plugin.master_gain = 0.25;
        let mut program = custom_program();
        program.gain = 0.72;
        program.layers[0].parameters.transpose_semitones = 7;
        program.effects.chorus.enabled = true;
        program.effects.chorus.mix = 0.33;
        assert!(plugin.apply_custom_program(program, "preview.unsaved"));
        let state = state_bytes(&plugin);
        plugin.master_gain = 1.0;
        assert!(plugin.select_instrument(0, 0));
        let status = unsafe {
            load_state(
                (&mut plugin as *mut RfDls).cast(),
                state.as_ptr(),
                state.len(),
            )
        };
        assert_eq!(status, STATUS_OK);
        assert_eq!(plugin.master_gain, 0.25);
        assert_eq!(plugin.selected_preset_id, "preview.unsaved");
        assert_eq!(plugin.program_gain_target, 0.72);
        assert_eq!(plugin.active_layers[0].parameters.transpose_semitones, 7);
        assert!(plugin.active_effects.chorus.enabled);
        assert_eq!(plugin.active_effects.chorus.mix, 0.33);
    }
}
