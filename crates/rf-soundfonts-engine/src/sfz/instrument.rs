//! An SFZ instrument resolved against live controller state.
//!
//! The opcode census of a real library explains the shape of this module.
//! The Headroom Piano uses no `volume`, no `pan`, no `tune` and no `ampeg`
//! attack, decay or sustain. Every audible decision it makes — the balance
//! between its two microphone positions, the master level, the stereo
//! placement, the dynamic response, the length of the release tail — is
//! expressed as a controller modulation:
//!
//! ```text
//! locc74=1  amplitude_oncc74=100  pan_oncc75=100  pan_curvecc75=1
//! ```
//!
//! So a region cannot be reduced to fixed values when the file is read. It has
//! to keep its modulation and resolve at note-on against whatever the
//! controllers hold at that instant. That is the difference between this and
//! the DLS path, where a region's level was decided when the bank was parsed.

use std::collections::BTreeMap;
use std::path::Path;

use crate::sfz::parse::{Curve, OpcodeMap, SfzDocument};
use crate::sample_store::{SampleStore, StreamedSample};
use crate::streamer::Streamer;
use crate::{SoundfontError, EnvelopeSpec, SampleParams, Voice, VoiceConfig};

/// Controller values, normalised to `0.0..=1.0`.
///
/// Normalised rather than 0-127 because SFZ has two ways to set the same
/// control: `set_cc74=127` and `set_hdcc74=1` mean the same thing, and the
/// high-definition form carries fractions a 7-bit integer cannot hold.
#[derive(Clone, Debug)]
pub struct CcState {
    values: [f32; 128],
}

impl Default for CcState {
    fn default() -> Self {
        Self {
            values: [0.0; 128],
        }
    }
}

impl CcState {
    pub fn get(&self, controller: u8) -> f32 {
        self.values
            .get(usize::from(controller))
            .copied()
            .unwrap_or(0.0)
    }

    pub fn set(&mut self, controller: u8, value: f32) {
        if let Some(slot) = self.values.get_mut(usize::from(controller)) {
            *slot = value.clamp(0.0, 1.0);
        }
    }

    /// Records an ordinary 7-bit MIDI control change.
    pub fn set_midi(&mut self, controller: u8, value: u8) {
        self.set(controller, f32::from(value.min(127)) / 127.0);
    }
}

/// One `*_oncc` modulation and the curve that shapes it.
#[derive(Clone, Copy, Debug)]
pub struct CcModulation {
    pub controller: u8,
    /// Depth in the unit of whatever it modulates.
    pub depth: f32,
    pub curve: Option<u32>,
}

/// A `locc`/`hicc` window a region must fall inside to sound at all.
#[derive(Clone, Copy, Debug)]
pub struct CcGate {
    pub controller: u8,
    pub low: f32,
    pub high: f32,
}

/// A region with its modulation intact.
#[derive(Clone, Debug)]
pub struct SampledRegion {
    pub key_low: u8,
    pub key_high: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    pub pitch_keycenter: u8,
    pub wave_index: usize,
    pub tune_cents: f32,
    /// Fixed level from `volume` and `group_volume`, in decibels.
    pub volume_db: f32,
    pub pan: f32,
    /// `amp_veltrack` as a fraction: 1.0 means full dynamic range.
    pub amp_veltrack: f32,
    pub group: u32,
    pub off_by: Option<u32>,
    pub note_polyphony: Option<u32>,
    /// Seconds over which a voice this region displaces should fade.
    pub off_time: f32,
    pub envelope: EnvelopeSpec,
    pub sample_loop: Option<crate::SampleLoop>,
    pub gates: Vec<CcGate>,
    pub amplitude_cc: Vec<CcModulation>,
    pub pan_cc: Vec<CcModulation>,
    pub veltrack_cc: Vec<CcModulation>,
    pub release_cc: Vec<CcModulation>,
}

/// A loaded SFZ instrument.
#[derive(Debug)]
pub struct SampledInstrument {
    pub name: String,
    pub samples: Vec<StreamedSample>,
    pub regions: Vec<SampledRegion>,
    pub curves: BTreeMap<u32, Curve>,
    /// Controller values the document declares through `set_cc`/`set_hdcc`.
    pub defaults: CcState,
    /// Gain that brings this instrument to the shared reference level.
    ///
    /// Libraries are mastered independently and arrive far apart: a piano and
    /// a Rhodes measured here peaked 10.4 dB apart, which means the master
    /// fader has to be moved on every sound change. That is not a thing a
    /// player can do between two songs, so the instruments are levelled to
    /// each other when they load.
    pub normalisation: f32,
}

/// Peak a single note is levelled to.
///
/// Deliberately far below full scale. This is the loudest one note may reach,
/// and notes are played together: a reference near unity would leave a chord
/// nowhere to go. A quarter leaves roughly four notes of linear headroom, and
/// real chords sum well below linearly because their partials do not align.
const REFERENCE_PEAK: f32 = 0.25;

/// Bounds on the correction, so a measurement cannot produce a surprise.
///
/// A library that is already close needs almost nothing; one that is silent
/// through its own settings must not be amplified into noise.
const MIN_NORMALISATION: f32 = 0.05;
const MAX_NORMALISATION: f32 = 8.0;

impl SampledInstrument {
    /// Reads and loads an instrument, decoding every sample it references.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SoundfontError> {
        let path = path.as_ref();
        let expanded = super::preprocess::expand(path)?;
        let document = super::parse::parse(&expanded)?;
        let root = path.parent().unwrap_or(Path::new("."));
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::build(&document, root, name)
    }

    fn build(document: &SfzDocument, root: &Path, name: String) -> Result<Self, SoundfontError> {
        let default_path = document
            .control
            .get("default_path")
            .cloned()
            .unwrap_or_default();
        let defaults = control_defaults(&document.control);

        // Samples are shared between regions: five velocity layers of one key
        // reference five files, but two microphone groups of the same layer do
        // not. Collecting distinct paths first keeps one copy of each and lets
        // the store transcode them all in parallel.
        let mut indices: BTreeMap<String, usize> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for opcodes in &document.regions {
            if let Some(relative) = opcodes.get("sample")
                && !indices.contains_key(relative)
            {
                indices.insert(relative.clone(), order.len());
                order.push(relative.clone());
            }
        }
        if order.is_empty() {
            return Err(SoundfontError::Invalid(format!(
                "SFZ instrument {name:?} references no samples"
            )));
        }

        let store = SampleStore::beside(root);
        let samples = store.load_all(root, &default_path, &order, &Default::default())?;

        let mut regions = Vec::new();
        for opcodes in &document.regions {
            let Some(relative) = opcodes.get("sample") else {
                continue;
            };
            let Some(index) = indices.get(relative).copied() else {
                continue;
            };
            regions.push(region_from(opcodes, index, &samples[index]));
        }
        let mut instrument = Self {
            name,
            samples,
            regions,
            curves: document.curves.clone(),
            defaults,
            normalisation: 1.0,
        };
        instrument.normalisation = instrument.measure_normalisation();
        Ok(instrument)
    }

    /// Derives the gain that brings this instrument to [`REFERENCE_PEAK`].
    ///
    /// Measured from the resident heads, which cost nothing to read because
    /// they are already in memory and which contain the attack — where a
    /// plucked or struck instrument reaches its peak. Each region's sample is
    /// weighted by the gain that region would actually apply, so a library
    /// that quietens a layer in its own settings is not treated as loud.
    pub fn renormalise(&mut self) {
        self.normalisation = self.measure_normalisation();
    }

    fn measure_normalisation(&self) -> f32 {
        let curves = &self.curves;
        let mut peak = 0.0_f32;
        for region in &self.regions {
            let Some(sample) = self.samples.get(region.wave_index) else {
                continue;
            };
            let loudest = sample
                .preload
                .iter()
                .fold(0.0_f32, |loudest, value| loudest.max(value.abs()));
            if loudest <= 0.0 {
                continue;
            }
            // Resolved at full velocity, because that is the loudest the
            // region can be asked to play.
            let config = region.resolve(&self.defaults, curves);
            peak = peak.max(loudest * config.gain);
        }
        if peak <= 0.0 {
            return 1.0;
        }
        (REFERENCE_PEAK / peak).clamp(MIN_NORMALISATION, MAX_NORMALISATION)
    }

    /// Bytes of memory the instrument holds resident.
    pub fn resident_bytes(&self) -> usize {
        self.samples.iter().map(StreamedSample::resident_bytes).sum()
    }

    /// Seconds a displaced voice should take to fade.
    ///
    /// The longest any region asks for, because a fade shorter than the
    /// instrument intends is audible as a cut while a longer one is merely a
    /// gentler tail. Libraries state it uniformly in practice; this piano asks
    /// for half a second on all three hundred regions.
    pub fn off_time(&self) -> f32 {
        self.regions
            .iter()
            .map(|region| region.off_time)
            .fold(0.005_f32, f32::max)
    }

    /// Builds the voices a key press should start.
    ///
    /// Gating happens here, not at load time: a region silenced by a
    /// controller today must still be able to sound when the player moves that
    /// controller, so nothing is discarded when the instrument is read.
    pub fn voices_for_note(
        &self,
        note: u8,
        velocity: u8,
        cc: &CcState,
        output_rate: u32,
        streamer: &Streamer,
    ) -> Result<Vec<Voice>, SoundfontError> {
        let mut voices = Vec::new();
        for region in &self.regions {
            if !region.accepts(note, velocity, cc) {
                continue;
            }
            let sample = &self.samples[region.wave_index];
            let mut config = region.resolve(cc, &self.curves);
            config.gain *= self.normalisation;
            let params = SampleParams {
                unity_note: u16::from(region.pitch_keycenter),
                fine_tune: region.tune_cents as i16,
                attenuation_db: 0.0,
                sample_loop: region.sample_loop,
            };
            // A sample short enough to be wholly resident needs no stream, and
            // taking one would occupy a slot another voice may need.
            let reader = if sample.is_fully_resident() {
                None
            } else {
                streamer.claim(sample, sample.preload_frames)
            };
            voices.push(Voice::from_streamed(
                sample,
                reader,
                params,
                note,
                velocity,
                output_rate,
                config,
            )?);
        }
        Ok(voices)
    }
}

impl SampledRegion {
    /// Whether this region should sound for a key press.
    pub fn accepts(&self, note: u8, velocity: u8, cc: &CcState) -> bool {
        (self.key_low..=self.key_high).contains(&note)
            && (self.velocity_low..=self.velocity_high).contains(&velocity)
            && self.gates.iter().all(|gate| {
                let value = cc.get(gate.controller);
                value >= gate.low && value <= gate.high
            })
    }

    /// Collapses the region's modulation into the voice parameters.
    fn resolve(&self, cc: &CcState, curves: &BTreeMap<u32, Curve>) -> VoiceConfig {
        let mut gain_db = self.volume_db;
        for modulation in &self.amplitude_cc {
            // `amplitude_oncc` is a percentage of full level, so a controller
            // at half its travel through the default linear curve halves the
            // amplitude rather than subtracting decibels.
            let position = shape(cc.get(modulation.controller), modulation.curve, curves);
            let amplitude = (modulation.depth / 100.0) * position;
            gain_db += 20.0 * amplitude.max(1e-4).log10();
        }

        let mut pan = self.pan;
        for modulation in &self.pan_cc {
            let position = shape(cc.get(modulation.controller), modulation.curve, curves);
            // Pan modulation is expressed over -100..100 and centred at half
            // travel, which is why a resting wheel leaves the image alone.
            pan += (modulation.depth / 100.0) * (position * 2.0 - 1.0);
        }

        let mut veltrack = self.amp_veltrack;
        for modulation in &self.veltrack_cc {
            let position = shape(cc.get(modulation.controller), modulation.curve, curves);
            veltrack += (modulation.depth / 100.0) * position;
        }

        let mut envelope = self.envelope;
        for modulation in &self.release_cc {
            let position = shape(cc.get(modulation.controller), modulation.curve, curves);
            envelope.release_seconds += modulation.depth * position;
        }

        VoiceConfig {
            amplitude_envelope: envelope,
            pitch_envelope: crate::PitchEnvelopeSpec::default(),
            lfo: crate::LfoSpec::default(),
            pitch_offset_cents: 0.0,
            pitch_bend_range_cents: 200.0,
            modulation_depth: 1.0,
            gain: 10.0_f32.powf(gain_db / 20.0),
            pan: pan.clamp(-1.0, 1.0),
            velocity_tracking: veltrack.clamp(0.0, 1.0),
        }
    }
}

/// Applies a curve to a normalised controller position.
fn shape(position: f32, curve: Option<u32>, curves: &BTreeMap<u32, Curve>) -> f32 {
    match curve.and_then(|index| curves.get(&index)) {
        Some(curve) => curve.value(position),
        // Curve 0 through 6 are predefined; only the linear default is needed
        // by the libraries seen so far, and inventing the rest would be worse
        // than passing the controller through unchanged.
        None => position,
    }
}

/// Reads `set_cc` and `set_hdcc` into the starting controller state.
fn control_defaults(control: &OpcodeMap) -> CcState {
    let mut state = CcState::default();
    for (name, value) in control {
        if let Some(number) = name.strip_prefix("set_hdcc") {
            if let (Ok(controller), Ok(value)) = (number.parse::<u8>(), value.parse::<f32>()) {
                state.set(controller, value);
            }
        } else if let Some(number) = name.strip_prefix("set_cc")
            && let (Ok(controller), Ok(value)) = (number.parse::<u8>(), value.parse::<f32>())
        {
            state.set(controller, value / 127.0);
        }
    }
    state
}

fn region_from(opcodes: &OpcodeMap, wave_index: usize, sample: &StreamedSample) -> SampledRegion {
    let number = |name: &str| opcodes.get(name).and_then(|value| value.parse::<f32>().ok());
    let key = |name: &str| opcodes.get(name).and_then(|value| parse_note(value));

    let key_low = key("lokey").unwrap_or(0);
    let key_high = key("hikey").unwrap_or(127);
    // `key=` sets range and root together, which is how one-shot and drum
    // mappings are usually written.
    let (key_low, key_high, default_center) = match key("key") {
        Some(single) => (single, single, single),
        None => (key_low, key_high, key_low),
    };

    SampledRegion {
        key_low,
        key_high,
        velocity_low: number("lovel").unwrap_or(0.0) as u8,
        velocity_high: number("hivel").unwrap_or(127.0).min(127.0) as u8,
        pitch_keycenter: key("pitch_keycenter").unwrap_or(default_center),
        wave_index,
        tune_cents: number("tune").or_else(|| number("pitch")).unwrap_or(0.0)
            + number("transpose").unwrap_or(0.0) * 100.0,
        volume_db: number("volume").unwrap_or(0.0) + number("group_volume").unwrap_or(0.0),
        pan: number("pan").unwrap_or(0.0) / 100.0,
        amp_veltrack: number("amp_veltrack").unwrap_or(100.0) / 100.0,
        group: number("group").unwrap_or(0.0) as u32,
        off_by: number("off_by").map(|value| value as u32),
        note_polyphony: number("note_polyphony").map(|value| value as u32),
        // A short default rather than zero: an abrupt cut is a click whatever
        // the document says, and five milliseconds is inaudible as a fade.
        off_time: number("off_time").unwrap_or(0.005).max(0.001),
        envelope: envelope_from(opcodes),
        sample_loop: loop_from(opcodes, sample),
        gates: gates_from(opcodes),
        amplitude_cc: modulations_from(opcodes, "amplitude_oncc", "amplitude_curvecc"),
        pan_cc: modulations_from(opcodes, "pan_oncc", "pan_curvecc"),
        veltrack_cc: modulations_from(opcodes, "amp_veltrack_oncc", "amp_veltrack_curvecc"),
        release_cc: modulations_from(opcodes, "ampeg_release_oncc", "ampeg_release_curvecc"),
    }
}

fn envelope_from(opcodes: &OpcodeMap) -> EnvelopeSpec {
    let seconds = |name: &str, fallback: f32| {
        opcodes
            .get(name)
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(fallback)
            .max(0.0)
    };
    EnvelopeSpec {
        attack_seconds: seconds("ampeg_attack", 0.0),
        // A region without a decay sustains, which is what an unspecified
        // amplitude envelope means in SFZ.
        decay_seconds: seconds("ampeg_decay", 1_000.0),
        sustain_level: opcodes
            .get("ampeg_sustain")
            .and_then(|value| value.parse::<f32>().ok())
            .map_or(1.0, |percent| (percent / 100.0).clamp(0.0, 1.0)),
        release_seconds: seconds("ampeg_release", 0.0),
    }
}

fn loop_from(opcodes: &OpcodeMap, sample: &StreamedSample) -> Option<crate::SampleLoop> {
    let mode = opcodes.get("loop_mode").map(String::as_str);
    if matches!(mode, Some("no_loop") | Some("one_shot")) {
        return None;
    }
    let frames = |name: &str| {
        opcodes
            .get(name)
            .and_then(|value| value.parse::<usize>().ok())
    };
    let explicit = frames("loop_start")
        .or_else(|| frames("loopstart"))
        .zip(frames("loop_end").or_else(|| frames("loopend")))
        // `loop_end` names the last frame inside the loop, as `smpl` does.
        .and_then(|(start, last)| Some((start, last.checked_add(1)?)));

    // A region that asks to loop and states no points expects the sample's
    // own. That is how SoundFont conversions are written: `loop_mode` in the
    // document, the markers in the file. Ignoring the fallback turns a
    // sustaining instrument into one that stops when the recording ends.
    let (start, end) = match explicit {
        Some(points) => points,
        None if mode.is_some() => {
            let inherited = sample.header.sample_loop?;
            (inherited.start, inherited.end)
        }
        None => return None,
    };
    (start < end && end <= sample.frame_count).then_some(crate::SampleLoop { start, end })
}

fn gates_from(opcodes: &OpcodeMap) -> Vec<CcGate> {
    let mut gates: BTreeMap<u8, (f32, f32)> = BTreeMap::new();
    for (name, value) in opcodes {
        let (prefix, is_low) = match () {
            _ if name.starts_with("locc") => ("locc", true),
            _ if name.starts_with("hicc") => ("hicc", false),
            _ => continue,
        };
        let Ok(controller) = name[prefix.len()..].parse::<u8>() else {
            continue;
        };
        let Ok(bound) = value.parse::<f32>() else {
            continue;
        };
        let entry = gates.entry(controller).or_insert((0.0, 1.0));
        if is_low {
            entry.0 = bound / 127.0;
        } else {
            entry.1 = bound / 127.0;
        }
    }
    gates
        .into_iter()
        .map(|(controller, (low, high))| CcGate {
            controller,
            low,
            high,
        })
        .collect()
}

fn modulations_from(opcodes: &OpcodeMap, depth: &str, curve: &str) -> Vec<CcModulation> {
    let mut modulations = Vec::new();
    for (name, value) in opcodes {
        let Some(number) = name.strip_prefix(depth) else {
            continue;
        };
        let (Ok(controller), Ok(amount)) = (number.parse::<u8>(), value.parse::<f32>()) else {
            continue;
        };
        modulations.push(CcModulation {
            controller,
            depth: amount,
            curve: opcodes
                .get(&format!("{curve}{controller}"))
                .and_then(|index| index.parse().ok()),
        });
    }
    modulations
}

/// Parses a key as either a MIDI number or a note name such as `c#4`.
///
/// Both spellings appear in the wild, sometimes in the same library, and a
/// parser that accepted only numbers would silently map every named region to
/// the bottom of the keyboard.
pub fn parse_note(text: &str) -> Option<u8> {
    let text = text.trim();
    if let Ok(number) = text.parse::<i32>() {
        return u8::try_from(number.clamp(0, 127)).ok();
    }
    let bytes = text.as_bytes();
    let step = match bytes.first()?.to_ascii_lowercase() {
        b'c' => 0,
        b'd' => 2,
        b'e' => 4,
        b'f' => 5,
        b'g' => 7,
        b'a' => 9,
        b'b' => 11,
        _ => return None,
    };
    let mut index = 1;
    let mut accidental = 0_i32;
    while index < bytes.len() {
        match bytes[index] {
            b'#' => accidental += 1,
            b'b' => accidental -= 1,
            _ => break,
        }
        index += 1;
    }
    let octave: i32 = text[index..].parse().ok()?;
    // Middle C is C4 = 60, the convention SFZ inherited from its tooling.
    let value = (octave + 1) * 12 + step + accidental;
    u8::try_from(value.clamp(0, 127)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opcodes(pairs: &[(&str, &str)]) -> OpcodeMap {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    /// A sample of the right shape for building regions.
    ///
    /// `region_from` consults only the frame count, so nothing here needs a
    /// cache file behind it.
    fn silent_wave(frames: usize) -> StreamedSample {
        StreamedSample {
            name: "fixture".into(),
            sample_rate: 44_100,
            channels: 2,
            frame_count: frames,
            preload: std::sync::Arc::from(vec![0.0; frames * 2]),
            preload_frames: frames,
            cache_path: std::path::PathBuf::from("fixture.pcm"),
            header: crate::pcm_cache::CacheHeader {
                sample_rate: 44_100,
                channels: 2,
                format: crate::pcm_cache::CacheFormat::Int16,
                frame_count: frames,
                sample_loop: None,
            },
        }
    }

    #[test]
    fn a_numeric_key_is_read_as_a_midi_note() {
        assert_eq!(parse_note("60"), Some(60));
        assert_eq!(parse_note(" 21 "), Some(21));
    }

    #[test]
    fn middle_c_is_sixty_by_name_and_by_number() {
        assert_eq!(parse_note("c4"), Some(60));
        assert_eq!(parse_note("C4"), parse_note("60"));
    }

    #[test]
    fn accidentals_shift_a_named_note() {
        assert_eq!(parse_note("c#4"), Some(61));
        assert_eq!(parse_note("db4"), Some(61));
    }

    #[test]
    fn a_nonsense_key_is_rejected_rather_than_defaulted() {
        assert_eq!(parse_note("banana"), None);
        assert_eq!(parse_note(""), None);
    }

    #[test]
    fn the_two_ways_of_declaring_a_default_agree() {
        // set_cc is 0-127; set_hdcc is normalised. Both mean full travel here.
        let state = control_defaults(&opcodes(&[("set_cc7", "127"), ("set_hdcc74", "1")]));
        assert!((state.get(7) - 1.0).abs() < 1e-6);
        assert!((state.get(74) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_high_definition_default_keeps_its_fraction() {
        // set_hdcc77=0.5 is the Decca microphone at half level, which a 7-bit
        // control could only approximate.
        let state = control_defaults(&opcodes(&[("set_hdcc77", "0.5")]));
        assert!((state.get(77) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_region_below_its_gate_stays_silent() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("locc74", "1")]),
            0,
            &silent_wave(8),
        );
        let mut cc = CcState::default();
        cc.set(74, 0.0);
        assert!(!region.accepts(60, 100, &cc), "a closed gate let a note through");
        cc.set(74, 1.0);
        assert!(region.accepts(60, 100, &cc));
    }

    #[test]
    fn key_and_velocity_windows_are_inclusive() {
        let region = region_from(
            &opcodes(&[
                ("sample", "a.flac"),
                ("lokey", "60"),
                ("hikey", "62"),
                ("lovel", "60"),
                ("hivel", "89"),
            ]),
            0,
            &silent_wave(8),
        );
        let cc = CcState::default();
        assert!(region.accepts(60, 60, &cc));
        assert!(region.accepts(62, 89, &cc));
        assert!(!region.accepts(59, 70, &cc));
        assert!(!region.accepts(61, 90, &cc));
    }

    #[test]
    fn a_single_key_opcode_sets_the_range_and_the_root_together() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("key", "36")]),
            0,
            &silent_wave(8),
        );
        assert_eq!((region.key_low, region.key_high), (36, 36));
        assert_eq!(region.pitch_keycenter, 36);
    }

    #[test]
    fn a_controller_at_rest_leaves_the_image_centred() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("pan_oncc10", "100")]),
            0,
            &silent_wave(8),
        );
        let mut cc = CcState::default();
        cc.set(10, 0.5);
        let config = region.resolve(&cc, &BTreeMap::new());
        assert!(config.pan.abs() < 1e-6, "pan drifted to {}", config.pan);
    }

    #[test]
    fn a_controller_at_full_travel_pans_hard() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("pan_oncc10", "100")]),
            0,
            &silent_wave(8),
        );
        let mut cc = CcState::default();
        cc.set(10, 1.0);
        assert!((region.resolve(&cc, &BTreeMap::new()).pan - 1.0).abs() < 1e-6);
    }

    #[test]
    fn amplitude_modulation_scales_the_level_rather_than_offsetting_decibels() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("amplitude_oncc74", "100")]),
            0,
            &silent_wave(8),
        );
        let mut cc = CcState::default();
        cc.set(74, 1.0);
        let full = region.resolve(&cc, &BTreeMap::new()).gain;
        cc.set(74, 0.5);
        let half = region.resolve(&cc, &BTreeMap::new()).gain;
        assert!((full - 1.0).abs() < 1e-3, "full travel was {full}");
        assert!((half - 0.5).abs() < 1e-3, "half travel was {half}");
    }

    #[test]
    fn the_two_microphones_resolve_to_the_balance_the_author_set() {
        // Close at 1.0 and Decca at 0.5 is the shipped default of the
        // Headroom Piano; the mix must not come out equal.
        let close = region_from(
            &opcodes(&[("sample", "c.flac"), ("amplitude_oncc74", "100")]),
            0,
            &silent_wave(8),
        );
        let decca = region_from(
            &opcodes(&[("sample", "d.flac"), ("amplitude_oncc77", "100")]),
            0,
            &silent_wave(8),
        );
        let state = control_defaults(&opcodes(&[("set_hdcc74", "1"), ("set_hdcc77", "0.5")]));
        let curves = BTreeMap::new();
        let close_gain = close.resolve(&state, &curves).gain;
        let decca_gain = decca.resolve(&state, &curves).gain;
        assert!(
            close_gain > decca_gain * 1.5,
            "close {close_gain} vs decca {decca_gain}"
        );
    }

    #[test]
    fn group_volume_and_volume_both_reach_the_level() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("group_volume", "-6"), ("volume", "-6")]),
            0,
            &silent_wave(8),
        );
        assert!((region.volume_db - -12.0).abs() < 1e-6);
    }

    #[test]
    fn zero_veltrack_ignores_how_hard_the_key_was_struck() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("amp_veltrack", "0")]),
            0,
            &silent_wave(8),
        );
        let config = region.resolve(&CcState::default(), &BTreeMap::new());
        assert_eq!(config.velocity_tracking, 0.0);
    }

    #[test]
    fn a_release_modulation_lengthens_the_tail() {
        let region = region_from(
            &opcodes(&[("sample", "a.flac"), ("ampeg_release_oncc72", "2")]),
            0,
            &silent_wave(8),
        );
        let mut cc = CcState::default();
        cc.set(72, 0.5);
        let config = region.resolve(&cc, &BTreeMap::new());
        assert!((config.amplitude_envelope.release_seconds - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_curve_reshapes_a_modulation() {
        let mut curve = Curve::default();
        curve.points.insert(0, 1.0);
        curve.points.insert(127, 0.0);
        let mut curves = BTreeMap::new();
        curves.insert(7_u32, curve);
        // The curve inverts the controller, so full travel means silence.
        assert!((shape(1.0, Some(7), &curves) - 0.0).abs() < 1e-6);
        assert!((shape(0.0, Some(7), &curves) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_one_shot_region_never_loops() {
        let region = region_from(
            &opcodes(&[
                ("sample", "a.flac"),
                ("loop_mode", "one_shot"),
                ("loop_start", "0"),
                ("loop_end", "4"),
            ]),
            0,
            &silent_wave(8),
        );
        assert!(region.sample_loop.is_none());
    }

    #[test]
    fn a_loop_end_names_the_last_frame_inside_the_loop() {
        let region = region_from(
            &opcodes(&[
                ("sample", "a.flac"),
                ("loop_start", "2"),
                ("loop_end", "5"),
            ]),
            0,
            &silent_wave(8),
        );
        let looping = region.sample_loop.unwrap();
        assert_eq!((looping.start, looping.end), (2, 6));
    }

    #[test]
    fn a_loop_past_the_end_of_the_audio_is_discarded() {
        let region = region_from(
            &opcodes(&[
                ("sample", "a.flac"),
                ("loop_start", "0"),
                ("loop_end", "99"),
            ]),
            0,
            &silent_wave(8),
        );
        assert!(region.sample_loop.is_none());
    }

    /// Renders one held note and reports every discontinuity in it.
    ///
    /// A click is a step between adjacent output frames, so it can be found
    /// exactly rather than described. Positions are printed in milliseconds so
    /// they can be compared against the moment the resident head runs out.
    ///
    /// ```text
    /// RF_SOUNDFONTS_SFZ="..." RF_DLS_NOTE=72 cargo test --release -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied SFZ library"]
    fn finds_discontinuities_in_a_held_note() {
        let path = std::env::var("RF_SOUNDFONTS_SFZ").expect("set RF_SOUNDFONTS_SFZ to an .sfz file");
        let note: u8 = std::env::var("RF_DLS_NOTE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(72);
        let rate = 48_000_u32;
        let instrument = SampledInstrument::open(&path).unwrap();
        let streamer = Streamer::start();
        let mut voices = instrument
            .voices_for_note(note, 100, &instrument.defaults, rate, &streamer)
            .unwrap();
        assert!(!voices.is_empty());

        let seconds = 3;
        let mut output = Vec::with_capacity(rate as usize * seconds);
        for _ in 0..rate as usize * seconds {
            let mut left = 0.0;
            for voice in &mut voices {
                left += voice.next_frame()[0];
            }
            output.push(left);
        }

        let starved: usize = voices.iter().map(Voice::starved_frames).sum();
        let peak = output.iter().fold(0.0_f32, |peak, value| peak.max(value.abs()));
        eprintln!("note {note}, {} voices, peak {peak:.4}", voices.len());
        eprintln!("resident head ends near {:.1} ms", 32_768.0 / 44_100.0 * 1000.0);
        eprintln!("starved frames: {starved}");

        // A click is a step that stands out from its surroundings, not one
        // that crosses a threshold chosen in advance. Bucketing the largest
        // step per 50 ms shows the shape of the signal and lets an anomaly
        // announce itself against its own neighbours.
        let bucket_frames = rate as usize / 20;
        let mut buckets: Vec<f32> = Vec::new();
        for chunk in output.windows(2).collect::<Vec<_>>().chunks(bucket_frames) {
            let worst = chunk
                .iter()
                .fold(0.0_f32, |worst, pair| worst.max((pair[1] - pair[0]).abs()));
            buckets.push(worst);
        }
        let median = {
            let mut sorted = buckets.clone();
            sorted.sort_by(f32::total_cmp);
            sorted[sorted.len() / 2]
        };
        eprintln!("median 50 ms peak step: {median:.5}");
        eprintln!("buckets more than 4x the median:");
        let mut flagged = 0;
        for (index, worst) in buckets.iter().enumerate() {
            if *worst > median * 4.0 && *worst > 1e-5 {
                eprintln!("   {:6} ms   step {worst:.5}", index * 50);
                flagged += 1;
            }
        }
        if flagged == 0 {
            eprintln!("   none");
        }
    }

    /// Loads a library the user supplies locally and plays a note through it.
    ///
    /// ```text
    /// RF_SOUNDFONTS_SFZ="/path/to/instrument.sfz" cargo test -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied SFZ library"]
    fn loads_and_plays_a_real_library() {
        let path = std::env::var("RF_SOUNDFONTS_SFZ").expect("set RF_SOUNDFONTS_SFZ to an .sfz file");
        let started = std::time::Instant::now();
        let instrument = SampledInstrument::open(&path).unwrap();
        let load = started.elapsed();
        let whole: usize = instrument
            .samples
            .iter()
            .map(|sample| {
                sample.frame_count * usize::from(sample.channels) * size_of::<f32>()
            })
            .sum();
        eprintln!(
            "{}: {} regions, {} distinct samples",
            instrument.name,
            instrument.regions.len(),
            instrument.samples.len()
        );
        eprintln!("load:      {:.2} s", load.as_secs_f32());
        eprintln!("resident:  {} MiB", instrument.resident_bytes() / 1_048_576);
        eprintln!("whole:     {} MiB", whole / 1_048_576);

        let streamer = Streamer::start();
        let cc = instrument.defaults.clone();
        let voices = instrument
            .voices_for_note(60, 100, &cc, 48_000, &streamer)
            .unwrap();
        eprintln!("middle C at velocity 100 started {} voices", voices.len());
        assert!(!voices.is_empty(), "no region accepted middle C");
        // Give the reader a moment to fill, as the resident head would.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let mut peak = 0.0_f32;
        let mut voices = voices;
        for _ in 0..48_000 {
            let mut left = 0.0;
            for voice in &mut voices {
                left += voice.next_frame()[0];
            }
            peak = peak.max(left.abs());
        }
        let starved: usize = voices.iter().map(Voice::starved_frames).sum();
        eprintln!("peak over one second: {peak:.4}");
        eprintln!("starved frames:       {starved}");
        assert!(peak > 1e-3, "the instrument rendered silence");
        assert!(peak.is_finite());
        // One second of audio is well past the 0.74 s resident head, so this
        // only passes if the streamer supplied the tail.
        assert_eq!(starved, 0, "the reader could not keep up with one note");
    }
}
