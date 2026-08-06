pub mod kontakt5;
pub mod fastlz;
pub mod nki;
pub mod smpl;
pub mod streamer;
pub mod spsc;
pub mod sample_store;
pub mod pcm_cache;
pub mod flac;
pub mod sample;
pub mod sfz;
pub mod wav;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

/// Fade applied as a sample runs out, so its final frame is never a step.
///
/// Short enough to be inaudible as a fade and long enough to remove the
/// discontinuity: five milliseconds is roughly two cycles of the lowest note
/// a keyboard produces.
const END_DECLICK_SECONDS: f32 = 0.005;

const DLS_DRUM_BANK: u32 = 0x8000_0000;
const CONN_SRC_NONE: u16 = 0x0000;
const CONN_SRC_LFO: u16 = 0x0001;
const CONN_SRC_EG2: u16 = 0x0005;
const CONN_SRC_CC1: u16 = 0x0081;
const CONN_DST_ATTENUATION: u16 = 0x0001;
const CONN_DST_PITCH: u16 = 0x0003;
const CONN_DST_LFO_FREQUENCY: u16 = 0x0104;
const CONN_DST_LFO_STARTDELAY: u16 = 0x0105;
const CONN_DST_EG1_ATTACKTIME: u16 = 0x0206;
const CONN_DST_EG1_DECAYTIME: u16 = 0x0207;
const CONN_DST_EG1_RELEASETIME: u16 = 0x0209;
const CONN_DST_EG1_SUSTAINLEVEL: u16 = 0x020a;
const CONN_DST_EG2_ATTACKTIME: u16 = 0x030a;
const CONN_DST_EG2_DECAYTIME: u16 = 0x030b;
const CONN_DST_EG2_RELEASETIME: u16 = 0x030d;
const CONN_DST_EG2_SUSTAINLEVEL: u16 = 0x030e;

#[derive(Debug, Error)]
pub enum SoundfontError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid DLS structure: {0}")]
    Invalid(String),
    #[error("unsupported DLS feature: {0}")]
    Unsupported(String),
}

#[derive(Clone, Debug)]
struct Chunk {
    id: [u8; 4],
    kind: Option<[u8; 4]>,
    payload_start: usize,
    payload_len: usize,
    children: Vec<Chunk>,
}

impl Chunk {
    fn is(&self, id: &[u8; 4]) -> bool {
        &self.id == id
    }

    fn is_list(&self, kind: &[u8; 4]) -> bool {
        self.kind.as_ref() == Some(kind)
    }

    fn child(&self, id: &[u8; 4]) -> Option<&Self> {
        self.children.iter().find(|child| child.is(id))
    }

    fn list(&self, kind: &[u8; 4]) -> Option<&Self> {
        self.children.iter().find(|child| child.is_list(kind))
    }

    fn data<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        &bytes[self.payload_start..self.payload_start + self.payload_len]
    }
}

fn fourcc(bytes: &[u8], offset: usize) -> [u8; 4] {
    bytes[offset..offset + 4].try_into().unwrap()
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn i16_at(bytes: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn i32_at(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn parse_chunks(
    bytes: &[u8],
    start: usize,
    end: usize,
    depth: usize,
) -> Result<Vec<Chunk>, SoundfontError> {
    if depth > 12 {
        return Err(SoundfontError::Invalid("RIFF nesting exceeds 12 levels".into()));
    }
    let mut chunks = Vec::new();
    let mut cursor = start;
    while cursor < end {
        if end - cursor < 8 {
            return Err(SoundfontError::Invalid(format!(
                "truncated chunk header at 0x{cursor:x}"
            )));
        }
        let id = fourcc(bytes, cursor);
        let size = u32_at(bytes, cursor + 4) as usize;
        let data_start = cursor + 8;
        let data_end = data_start
            .checked_add(size)
            .filter(|value| *value <= end)
            .ok_or_else(|| {
                SoundfontError::Invalid(format!(
                    "chunk {:?} at 0x{cursor:x} exceeds its container",
                    String::from_utf8_lossy(&id)
                ))
            })?;
        let (kind, payload_start, payload_len, children) = if id == *b"RIFF" || id == *b"LIST" {
            if size < 4 {
                return Err(SoundfontError::Invalid(format!(
                    "container at 0x{cursor:x} is shorter than its form type"
                )));
            }
            let kind = fourcc(bytes, data_start);
            (
                Some(kind),
                data_start + 4,
                size - 4,
                parse_chunks(bytes, data_start + 4, data_end, depth + 1)?,
            )
        } else {
            (None, data_start, size, Vec::new())
        };
        chunks.push(Chunk {
            id,
            kind,
            payload_start,
            payload_len,
            children,
        });
        cursor = data_end + (size & 1);
        if cursor > end {
            return Err(SoundfontError::Invalid(format!(
                "padding after chunk at 0x{:x} exceeds its container",
                data_start - 8
            )));
        }
    }
    Ok(chunks)
}

fn require_size(data: &[u8], minimum: usize, label: &str) -> Result<(), SoundfontError> {
    if data.len() < minimum {
        return Err(SoundfontError::Invalid(format!(
            "{label} has {} bytes; expected at least {minimum}",
            data.len()
        )));
    }
    Ok(())
}

fn info_name(container: &Chunk, bytes: &[u8]) -> String {
    container
        .list(b"INFO")
        .and_then(|info| info.child(b"INAM"))
        .map(|chunk| {
            let data = chunk.data(bytes);
            let end = data
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(data.len());
            String::from_utf8_lossy(&data[..end]).trim().to_owned()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleLoop {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct SampleParams {
    pub unity_note: u16,
    pub fine_tune: i16,
    pub attenuation_db: f32,
    pub sample_loop: Option<SampleLoop>,
}

fn parse_wsmp(chunk: &Chunk, bytes: &[u8]) -> Result<SampleParams, SoundfontError> {
    let data = chunk.data(bytes);
    require_size(data, 20, "wsmp")?;
    let header_size = u32_at(data, 0) as usize;
    if !(20..=data.len()).contains(&header_size) {
        return Err(SoundfontError::Invalid(format!(
            "wsmp header size {header_size} is invalid"
        )));
    }
    let loop_count = u32_at(data, 16) as usize;
    let sample_loop = if loop_count == 0 {
        None
    } else {
        require_size(&data[header_size..], 16, "wsmp loop")?;
        let loop_size = u32_at(data, header_size) as usize;
        if loop_size < 16 || header_size + loop_size > data.len() {
            return Err(SoundfontError::Invalid("wsmp loop size is invalid".into()));
        }
        let loop_type = u32_at(data, header_size + 4);
        if loop_type > 1 {
            return Err(SoundfontError::Unsupported(format!(
                "sample loop type {loop_type}"
            )));
        }
        let start = u32_at(data, header_size + 8) as usize;
        let length = u32_at(data, header_size + 12) as usize;
        Some(SampleLoop {
            start,
            end: start
                .checked_add(length)
                .ok_or_else(|| SoundfontError::Invalid("sample loop overflows usize".into()))?,
        })
    };
    let attenuation_units = i32_at(data, 8) as f32 / 65_536.0;
    Ok(SampleParams {
        unity_note: u16_at(data, 4),
        fine_tune: i16_at(data, 6),
        attenuation_db: attenuation_units / 10.0,
        sample_loop,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct EnvelopeSpec {
    pub attack_seconds: f32,
    pub decay_seconds: f32,
    pub sustain_level: f32,
    pub release_seconds: f32,
}

impl Default for EnvelopeSpec {
    fn default() -> Self {
        Self {
            attack_seconds: 0.0,
            decay_seconds: 1_000.0,
            sustain_level: 1.0,
            release_seconds: 0.0,
        }
    }
}

impl EnvelopeSpec {
    fn pitch_default() -> Self {
        Self {
            attack_seconds: 0.0,
            decay_seconds: 1_000.0,
            sustain_level: 1.0,
            release_seconds: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PitchEnvelopeSpec {
    pub envelope: EnvelopeSpec,
    pub depth_cents: f32,
}

impl Default for PitchEnvelopeSpec {
    fn default() -> Self {
        Self {
            envelope: EnvelopeSpec::pitch_default(),
            depth_cents: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LfoSpec {
    pub frequency_hz: f32,
    pub delay_seconds: f32,
    pub pitch_depth_cents: f32,
    pub mod_wheel_pitch_depth_cents: f32,
    pub attenuation_depth_centibels: f32,
    pub mod_wheel_attenuation_depth_centibels: f32,
}

impl Default for LfoSpec {
    fn default() -> Self {
        Self {
            frequency_hz: 5.0,
            delay_seconds: 0.01,
            pitch_depth_cents: 0.0,
            mod_wheel_pitch_depth_cents: 0.0,
            attenuation_depth_centibels: 0.0,
            mod_wheel_attenuation_depth_centibels: 0.0,
        }
    }
}

fn timecents_to_seconds(raw: i32) -> f32 {
    if raw == i32::MIN {
        0.0
    } else {
        let timecents = raw as f32 / 65_536.0;
        2.0_f32.powf(timecents / 1_200.0).clamp(0.0, 1_000.0)
    }
}

fn absolute_pitch_cents_to_hz(raw: i32) -> f32 {
    let cents = raw as f32 / 65_536.0;
    440.0 * 2.0_f32.powf((cents - 6_900.0) / 1_200.0)
}

fn sustain_level(raw: i32) -> f32 {
    (raw as f32 / 65_536.0 / 1_000.0).clamp(0.0, 1.0)
}

fn apply_articulation_connection(
    amplitude: &mut EnvelopeSpec,
    pitch: &mut PitchEnvelopeSpec,
    lfo: &mut LfoSpec,
    source: u16,
    control: u16,
    destination: u16,
    scale: i32,
) {
    match (source, control, destination) {
        (CONN_SRC_NONE, 0, CONN_DST_EG1_ATTACKTIME) => {
            amplitude.attack_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG1_DECAYTIME) => {
            amplitude.decay_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG1_RELEASETIME) => {
            amplitude.release_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG1_SUSTAINLEVEL) => {
            amplitude.sustain_level = sustain_level(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG2_ATTACKTIME) => {
            pitch.envelope.attack_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG2_DECAYTIME) => {
            pitch.envelope.decay_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG2_RELEASETIME) => {
            pitch.envelope.release_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_EG2_SUSTAINLEVEL) => {
            pitch.envelope.sustain_level = sustain_level(scale)
        }
        (CONN_SRC_EG2, 0, CONN_DST_PITCH) => pitch.depth_cents = scale as f32 / 65_536.0,
        (CONN_SRC_NONE, 0, CONN_DST_LFO_FREQUENCY) => {
            lfo.frequency_hz = absolute_pitch_cents_to_hz(scale)
        }
        (CONN_SRC_NONE, 0, CONN_DST_LFO_STARTDELAY) => {
            lfo.delay_seconds = timecents_to_seconds(scale)
        }
        (CONN_SRC_LFO, 0, CONN_DST_PITCH) => lfo.pitch_depth_cents = scale as f32 / 65_536.0,
        (CONN_SRC_LFO, CONN_SRC_CC1, CONN_DST_PITCH) => {
            lfo.mod_wheel_pitch_depth_cents = scale as f32 / 65_536.0
        }
        (CONN_SRC_LFO, 0, CONN_DST_ATTENUATION) => {
            lfo.attenuation_depth_centibels = scale as f32 / 65_536.0
        }
        (CONN_SRC_LFO, CONN_SRC_CC1, CONN_DST_ATTENUATION) => {
            lfo.mod_wheel_attenuation_depth_centibels = scale as f32 / 65_536.0
        }
        _ => {}
    }
}

fn parse_articulation(
    container: &Chunk,
    bytes: &[u8],
) -> Result<(EnvelopeSpec, PitchEnvelopeSpec, LfoSpec), SoundfontError> {
    let mut amplitude = EnvelopeSpec::default();
    let mut pitch = PitchEnvelopeSpec::default();
    let mut lfo = LfoSpec::default();
    let Some(articulators) = container.list(b"lar2").or_else(|| container.list(b"lart")) else {
        return Ok((amplitude, pitch, lfo));
    };
    let Some(art) = articulators
        .child(b"art2")
        .or_else(|| articulators.child(b"art1"))
    else {
        return Ok((amplitude, pitch, lfo));
    };
    let data = art.data(bytes);
    require_size(data, 8, "articulator")?;
    let header_size = u32_at(data, 0) as usize;
    let count = u32_at(data, 4) as usize;
    let required = header_size
        .checked_add(count.saturating_mul(12))
        .ok_or_else(|| SoundfontError::Invalid("articulator size overflows".into()))?;
    if header_size < 8 || required > data.len() {
        return Err(SoundfontError::Invalid(
            "articulator connection table is truncated".into(),
        ));
    }
    for connection in 0..count {
        let offset = header_size + connection * 12;
        let source = u16_at(data, offset);
        let control = u16_at(data, offset + 2);
        let destination = u16_at(data, offset + 4);
        let scale = i32_at(data, offset + 8);
        apply_articulation_connection(
            &mut amplitude,
            &mut pitch,
            &mut lfo,
            source,
            control,
            destination,
            scale,
        );
    }
    Ok((amplitude, pitch, lfo))
}

#[derive(Clone, Debug)]
pub struct Region {
    pub key_low: u16,
    pub key_high: u16,
    pub velocity_low: u16,
    pub velocity_high: u16,
    pub wave_index: usize,
    pub key_group: u16,
    pub sample_params: Option<SampleParams>,
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub name: String,
    pub bank: u32,
    pub program: u32,
    pub regions: Vec<Region>,
    pub envelope: EnvelopeSpec,
    pub pitch_envelope: PitchEnvelopeSpec,
    pub lfo: LfoSpec,
}

impl Instrument {
    pub fn is_drum(&self) -> bool {
        self.bank & DLS_DRUM_BANK != 0
    }

    pub fn matching_regions(&self, note: u8, velocity: u8) -> impl Iterator<Item = &Region> {
        let note = u16::from(note);
        let velocity = u16::from(velocity);
        self.regions.iter().filter(move |region| {
            (region.key_low..=region.key_high).contains(&note)
                && (region.velocity_low..=region.velocity_high).contains(&velocity)
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VoiceConfig {
    pub amplitude_envelope: EnvelopeSpec,
    pub pitch_envelope: PitchEnvelopeSpec,
    pub lfo: LfoSpec,
    pub pitch_offset_cents: f32,
    pub pitch_bend_range_cents: f32,
    pub modulation_depth: f32,
    pub gain: f32,
    /// Stereo placement from `-1.0` (hard left) to `1.0` (hard right).
    pub pan: f32,
    /// How much of the level follows key velocity, from `0.0` to `1.0`.
    ///
    /// A library that has already split its dynamics into separate velocity
    /// layers asks for `0.0`, so each layer plays at its recorded level
    /// instead of being scaled a second time. This piano does exactly that,
    /// five times over.
    pub velocity_tracking: f32,
}

impl VoiceConfig {
    pub fn inherit(instrument: &Instrument) -> Self {
        Self {
            amplitude_envelope: instrument.envelope,
            pitch_envelope: instrument.pitch_envelope,
            lfo: instrument.lfo,
            pitch_offset_cents: 0.0,
            pitch_bend_range_cents: 200.0,
            modulation_depth: 1.0,
            gain: 1.0,
            pan: 0.0,
            velocity_tracking: 1.0,
        }
    }
}

/// Per-channel gains for a stereo position.
///
/// Centre is unity on both channels rather than the -3 dB of a constant-power
/// law. That choice is deliberate: every existing DLS voice pans centre, and a
/// constant-power centre would quietly drop every bank by 3 dB the moment
/// stereo landed.
fn pan_gains(pan: f32) -> [f32; 2] {
    let pan = pan.clamp(-1.0, 1.0);
    [(1.0 - pan).min(1.0), (1.0 + pan).min(1.0)]
}

/// Decoded audio for one sample.
///
/// `samples` is interleaved by frame, so a stereo wave stores `L R L R`.
/// Positions and loop points are counted in *frames* everywhere in this crate,
/// never in individual samples: a loop expressed in samples would mean two
/// different things for mono and stereo material and would silently halve a
/// stereo loop.
#[derive(Clone, Debug)]
pub struct Wave {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u8,
    /// Bit depth of the file this was decoded from.
    ///
    /// Kept after decoding so a cache can store the audio at a width that
    /// cannot lose anything: most libraries are 16-bit, and writing those as
    /// 32-bit float would double both the cache and the traffic a streaming
    /// voice generates for no gain at all.
    pub source_bits: u16,
    pub samples: Arc<[f32]>,
    pub sample_params: Option<SampleParams>,
}

impl Wave {
    /// Frames available, independent of channel count.
    pub fn frame_count(&self) -> usize {
        self.samples.len() / usize::from(self.channels).max(1)
    }
}

#[derive(Debug)]
pub struct DlsBank {
    pub instruments: Vec<Instrument>,
    pub waves: Vec<Wave>,
}

impl DlsBank {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SoundfontError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| SoundfontError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, SoundfontError> {
        let roots = parse_chunks(bytes, 0, bytes.len(), 0)?;
        if roots.len() != 1 || !roots[0].is(b"RIFF") || !roots[0].is_list(b"DLS ") {
            return Err(SoundfontError::Invalid(
                "file is not a RIFF DLS collection".into(),
            ));
        }
        let root = &roots[0];
        let pool_table = root
            .child(b"ptbl")
            .ok_or_else(|| SoundfontError::Invalid("missing ptbl".into()))?;
        let pool = pool_table.data(bytes);
        require_size(pool, 8, "ptbl")?;
        let pool_header = u32_at(pool, 0) as usize;
        let cue_count = u32_at(pool, 4) as usize;
        if pool_header < 8 || pool_header + cue_count.saturating_mul(4) > pool.len() {
            return Err(SoundfontError::Invalid("ptbl cue table is truncated".into()));
        }
        let cues = (0..cue_count)
            .map(|index| u32_at(pool, pool_header + index * 4) as usize)
            .collect::<Vec<_>>();

        let wave_pool = root
            .list(b"wvpl")
            .ok_or_else(|| SoundfontError::Invalid("missing LIST wvpl".into()))?;
        let wave_pool_start = wave_pool.payload_start;
        let wave_chunks = wave_pool
            .children
            .iter()
            .filter(|chunk| chunk.is_list(b"wave"))
            .collect::<Vec<_>>();
        let mut waves_by_offset = std::collections::BTreeMap::new();
        for wave in wave_chunks {
            let relative = wave
                .payload_start
                .checked_sub(12)
                .and_then(|start| start.checked_sub(wave_pool_start))
                .ok_or_else(|| SoundfontError::Invalid("wave pool relative offset underflows".into()))?;
            waves_by_offset.insert(relative, parse_wave(wave, bytes)?);
        }
        let waves = cues
            .iter()
            .map(|cue| {
                waves_by_offset.get(cue).cloned().ok_or_else(|| {
                    SoundfontError::Invalid(format!("ptbl cue 0x{cue:x} does not select a wave"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let instrument_list = root
            .list(b"lins")
            .ok_or_else(|| SoundfontError::Invalid("missing LIST lins".into()))?;
        let instruments = instrument_list
            .children
            .iter()
            .filter(|chunk| chunk.is_list(b"ins "))
            .map(|chunk| parse_instrument(chunk, bytes, waves.len()))
            .collect::<Result<Vec<_>, _>>()?;
        if instruments.is_empty() || waves.is_empty() {
            return Err(SoundfontError::Invalid(
                "DLS collection has no instruments or waves".into(),
            ));
        }
        Ok(Self { instruments, waves })
    }

    pub fn instrument(&self, bank: u32, program: u32) -> Option<&Instrument> {
        self.instruments
            .iter()
            .find(|instrument| instrument.bank == bank && instrument.program == program)
    }
}

fn parse_wave(chunk: &Chunk, bytes: &[u8]) -> Result<Wave, SoundfontError> {
    let format = chunk
        .child(b"fmt ")
        .ok_or_else(|| SoundfontError::Invalid("wave is missing fmt".into()))?
        .data(bytes);
    require_size(format, 16, "wave fmt")?;
    let format_tag = u16_at(format, 0);
    let channels = u16_at(format, 2);
    let sample_rate = u32_at(format, 4);
    let block_align = u16_at(format, 12);
    let bits = u16_at(format, 14);
    // DLS-1 collections are mono in practice, but nothing in the renderer
    // requires that any more, and rejecting a stereo wave a bank happens to
    // carry would fail the whole collection over one sample.
    if format_tag != 1 || !(1..=2).contains(&channels) || bits != 16 {
        return Err(SoundfontError::Unsupported(format!(
            "wave format tag={format_tag} channels={channels} bits={bits} align={block_align}"
        )));
    }
    if usize::from(block_align) != usize::from(channels) * 2 {
        return Err(SoundfontError::Invalid(format!(
            "wave block align {block_align} does not match {channels} channels of PCM16"
        )));
    }
    if sample_rate == 0 {
        return Err(SoundfontError::Invalid("wave sample rate is zero".into()));
    }
    let raw = chunk
        .child(b"data")
        .ok_or_else(|| SoundfontError::Invalid("wave is missing data".into()))?
        .data(bytes);
    if raw.len() % 2 != 0 {
        return Err(SoundfontError::Invalid("PCM16 wave has an odd data size".into()));
    }
    let samples = raw
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32_768.0)
        .collect::<Vec<_>>();
    if samples.is_empty() {
        return Err(SoundfontError::Invalid("wave contains no samples".into()));
    }
    if samples.len() % usize::from(channels) != 0 {
        return Err(SoundfontError::Invalid(format!(
            "PCM16 wave holds a partial frame for {channels} channels"
        )));
    }
    let frames = samples.len() / usize::from(channels);
    let sample_params = chunk
        .child(b"wsmp")
        .map(|wsmp| parse_wsmp(wsmp, bytes))
        .transpose()?;
    if let Some(sample_loop) = sample_params.and_then(|params| params.sample_loop)
        && (sample_loop.start >= sample_loop.end || sample_loop.end > frames)
    {
        return Err(SoundfontError::Invalid(format!(
            "wave loop {}..{} exceeds {frames} frames",
            sample_loop.start, sample_loop.end,
        )));
    }
    Ok(Wave {
        name: info_name(chunk, bytes),
        sample_rate,
        channels: channels as u8,
        source_bits: bits,
        samples: Arc::from(samples),
        sample_params,
    })
}

fn parse_instrument(
    chunk: &Chunk,
    bytes: &[u8],
    wave_count: usize,
) -> Result<Instrument, SoundfontError> {
    let header = chunk
        .child(b"insh")
        .ok_or_else(|| SoundfontError::Invalid("instrument is missing insh".into()))?
        .data(bytes);
    require_size(header, 12, "insh")?;
    let declared_regions = u32_at(header, 0) as usize;
    let bank = u32_at(header, 4);
    let program = u32_at(header, 8);
    let region_list = chunk
        .list(b"lrgn")
        .ok_or_else(|| SoundfontError::Invalid("instrument is missing LIST lrgn".into()))?;
    let regions = region_list
        .children
        .iter()
        .filter(|region| region.is_list(b"rgn ") || region.is_list(b"rgn2"))
        .map(|region| parse_region(region, bytes, wave_count))
        .collect::<Result<Vec<_>, _>>()?;
    if regions.len() != declared_regions {
        return Err(SoundfontError::Invalid(format!(
            "instrument declares {declared_regions} regions but contains {}",
            regions.len()
        )));
    }
    let (envelope, pitch_envelope, lfo) = parse_articulation(chunk, bytes)?;
    Ok(Instrument {
        name: info_name(chunk, bytes),
        bank,
        program,
        regions,
        envelope,
        pitch_envelope,
        lfo,
    })
}

fn parse_region(chunk: &Chunk, bytes: &[u8], wave_count: usize) -> Result<Region, SoundfontError> {
    let header = chunk
        .child(b"rgnh")
        .ok_or_else(|| SoundfontError::Invalid("region is missing rgnh".into()))?
        .data(bytes);
    require_size(header, 12, "rgnh")?;
    let wave_link = chunk
        .child(b"wlnk")
        .ok_or_else(|| SoundfontError::Invalid("region is missing wlnk".into()))?
        .data(bytes);
    require_size(wave_link, 12, "wlnk")?;
    let wave_index = u32_at(wave_link, 8) as usize;
    if wave_index >= wave_count {
        return Err(SoundfontError::Invalid(format!(
            "region selects wave {wave_index}, but collection has {wave_count}"
        )));
    }
    let sample_params = chunk
        .child(b"wsmp")
        .map(|wsmp| parse_wsmp(wsmp, bytes))
        .transpose()?;
    Ok(Region {
        key_low: u16_at(header, 0),
        key_high: u16_at(header, 2),
        velocity_low: u16_at(header, 4),
        velocity_high: u16_at(header, 6),
        key_group: u16_at(header, 10),
        wave_index,
        sample_params,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvelopePhase {
    Attack,
    Decay,
    Sustain,
    Release,
    Finished,
}

#[derive(Clone, Debug)]
struct Envelope {
    spec: EnvelopeSpec,
    phase: EnvelopePhase,
    level: f32,
    release_step: f32,
    sample_rate: f32,
}

impl Envelope {
    fn new(spec: EnvelopeSpec, sample_rate: u32) -> Self {
        let phase = if spec.attack_seconds <= 1.0 / sample_rate as f32 {
            EnvelopePhase::Decay
        } else {
            EnvelopePhase::Attack
        };
        Self {
            spec,
            phase,
            level: if phase == EnvelopePhase::Attack {
                0.0
            } else {
                1.0
            },
            release_step: 0.0,
            sample_rate: sample_rate as f32,
        }
    }

    fn note_off(&mut self) {
        if self.phase != EnvelopePhase::Finished && self.phase != EnvelopePhase::Release {
            let frames = (self.spec.release_seconds * self.sample_rate).max(1.0);
            self.release_step = self.level / frames;
            self.phase = EnvelopePhase::Release;
        }
    }

    fn next_gain(&mut self) -> f32 {
        match self.phase {
            EnvelopePhase::Attack => {
                self.level += 1.0 / (self.spec.attack_seconds * self.sample_rate).max(1.0);
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.phase = EnvelopePhase::Decay;
                }
            }
            EnvelopePhase::Decay => {
                let step = (1.0 - self.spec.sustain_level)
                    / (self.spec.decay_seconds * self.sample_rate).max(1.0);
                self.level -= step;
                if self.level <= self.spec.sustain_level {
                    self.level = self.spec.sustain_level;
                    self.phase = EnvelopePhase::Sustain;
                }
            }
            EnvelopePhase::Sustain => {}
            EnvelopePhase::Release => {
                self.level -= self.release_step;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.phase = EnvelopePhase::Finished;
                }
            }
            EnvelopePhase::Finished => {}
        }
        self.level
    }
}

const AMPLITUDE_SILENCE_CENTIBELS: f32 = 960.0;

#[derive(Clone, Debug)]
struct AmplitudeEnvelope {
    spec: EnvelopeSpec,
    phase: EnvelopePhase,
    phase_frame: usize,
    phase_frames: usize,
    attenuation_centibels: f32,
    release_start_centibels: f32,
    sample_rate: f32,
}

impl AmplitudeEnvelope {
    fn new(spec: EnvelopeSpec, sample_rate: u32) -> Self {
        let mut envelope = Self {
            spec,
            phase: EnvelopePhase::Attack,
            phase_frame: 0,
            phase_frames: 0,
            attenuation_centibels: AMPLITUDE_SILENCE_CENTIBELS,
            release_start_centibels: AMPLITUDE_SILENCE_CENTIBELS,
            sample_rate: sample_rate as f32,
        };
        envelope.enter_attack();
        envelope
    }

    fn frames(&self, seconds: f32) -> usize {
        (seconds * self.sample_rate).max(0.0).round() as usize
    }

    fn enter_attack(&mut self) {
        self.phase_frames = self.frames(self.spec.attack_seconds);
        if self.phase_frames == 0 {
            self.attenuation_centibels = 0.0;
            self.enter_decay();
        }
    }

    fn enter_decay(&mut self) {
        self.phase = EnvelopePhase::Decay;
        self.phase_frame = 0;
        let sustain = self.sustain_centibels();
        self.phase_frames =
            self.frames(self.spec.decay_seconds * sustain / AMPLITUDE_SILENCE_CENTIBELS);
        if self.phase_frames == 0 || sustain == 0.0 {
            self.attenuation_centibels = sustain;
            self.phase = EnvelopePhase::Sustain;
        }
    }

    fn sustain_centibels(&self) -> f32 {
        ((1.0 - self.spec.sustain_level) * 1_000.0).clamp(0.0, AMPLITUDE_SILENCE_CENTIBELS)
    }

    fn note_off(&mut self) {
        if self.phase == EnvelopePhase::Finished || self.phase == EnvelopePhase::Release {
            return;
        }
        self.release_start_centibels = self.attenuation_centibels;
        let remaining = (AMPLITUDE_SILENCE_CENTIBELS - self.release_start_centibels).max(0.0);
        self.phase_frames =
            self.frames(self.spec.release_seconds * remaining / AMPLITUDE_SILENCE_CENTIBELS);
        self.phase_frame = 0;
        self.phase = if self.phase_frames == 0 {
            EnvelopePhase::Finished
        } else {
            EnvelopePhase::Release
        };
    }

    fn is_finished(&self) -> bool {
        self.phase == EnvelopePhase::Finished
    }

    fn next_gain(&mut self) -> f32 {
        match self.phase {
            EnvelopePhase::Attack => {
                self.phase_frame += 1;
                let gain = (self.phase_frame as f32 / self.phase_frames as f32).min(1.0);
                self.attenuation_centibels = if gain <= 0.0 {
                    AMPLITUDE_SILENCE_CENTIBELS
                } else {
                    (-200.0 * gain.log10()).clamp(0.0, AMPLITUDE_SILENCE_CENTIBELS)
                };
                if self.phase_frame >= self.phase_frames {
                    self.attenuation_centibels = 0.0;
                    self.enter_decay();
                }
                return gain;
            }
            EnvelopePhase::Decay => {
                self.phase_frame += 1;
                let progress = (self.phase_frame as f32 / self.phase_frames as f32).min(1.0);
                self.attenuation_centibels = self.sustain_centibels() * progress;
                if self.phase_frame >= self.phase_frames {
                    self.attenuation_centibels = self.sustain_centibels();
                    self.phase = EnvelopePhase::Sustain;
                }
            }
            EnvelopePhase::Sustain => {
                self.attenuation_centibels = self.sustain_centibels();
            }
            EnvelopePhase::Release => {
                self.phase_frame += 1;
                let progress = (self.phase_frame as f32 / self.phase_frames as f32).min(1.0);
                self.attenuation_centibels = self.release_start_centibels
                    + (AMPLITUDE_SILENCE_CENTIBELS - self.release_start_centibels) * progress;
                if self.phase_frame >= self.phase_frames {
                    self.attenuation_centibels = AMPLITUDE_SILENCE_CENTIBELS;
                    self.phase = EnvelopePhase::Finished;
                }
            }
            EnvelopePhase::Finished => {
                return 0.0;
            }
        }
        10.0_f32.powf(-self.attenuation_centibels / 200.0)
    }
}

/// A sounding note.
///
/// Deliberately not `Clone`. A streaming voice owns one slot out of a fixed
/// pool, and a copy would hand two voices the same reader: both would drain
/// the same ring and each would hear half the audio. Voices are moved, never
/// duplicated.
#[derive(Debug)]
pub struct Voice {
    pub note: u8,
    /// Audio held in memory: the whole sample, or only its head when the rest
    /// is streamed.
    samples: Arc<[f32]>,
    /// Supplies frames past the resident head. Absent for resident samples.
    stream: Option<streamer::StreamWindow>,
    channels: usize,
    frame_count: usize,
    /// Playback cursor in frames, not samples.
    position: f64,
    base_increment: f64,
    sample_loop: Option<SampleLoop>,
    gain: f32,
    pan_gains: [f32; 2],
    envelope: AmplitudeEnvelope,
    pitch_envelope: Envelope,
    pitch_depth_cents: f32,
    lfo: LfoSpec,
    lfo_phase: f32,
    lfo_delay_frames: usize,
    pitch_offset_cents: f32,
    pitch_bend_range_cents: f32,
    modulation_depth: f32,
    output_rate: f32,
    /// Level of a forced fade, and how much of it to remove per frame.
    ///
    /// Separate from the amplitude envelope because it answers a different
    /// question. The envelope is what the instrument sounds like; this is what
    /// happens when a voice has to stop for a reason the music did not ask
    /// for — a repeated key taking over from the note still ringing, or the
    /// voice pool running out.
    fade_level: f32,
    fade_step: f32,
    finished: bool,
}

impl Voice {
    pub fn new(
        bank: &DlsBank,
        instrument: &Instrument,
        region: &Region,
        note: u8,
        velocity: u8,
        output_rate: u32,
    ) -> Result<Self, SoundfontError> {
        Self::new_with_envelope(
            bank,
            instrument,
            region,
            note,
            velocity,
            output_rate,
            instrument.envelope,
        )
    }

    pub fn new_with_envelope(
        bank: &DlsBank,
        instrument: &Instrument,
        region: &Region,
        note: u8,
        velocity: u8,
        output_rate: u32,
        envelope: EnvelopeSpec,
    ) -> Result<Self, SoundfontError> {
        let mut config = VoiceConfig::inherit(instrument);
        config.amplitude_envelope = envelope;
        Self::new_with_config(
            bank,
            instrument,
            region,
            note,
            velocity,
            output_rate,
            config,
        )
    }

    pub fn new_with_config(
        bank: &DlsBank,
        instrument: &Instrument,
        region: &Region,
        note: u8,
        velocity: u8,
        output_rate: u32,
        config: VoiceConfig,
    ) -> Result<Self, SoundfontError> {
        let wave = &bank.waves[region.wave_index];
        let params = region.sample_params.or(wave.sample_params).ok_or_else(|| {
            SoundfontError::Unsupported(format!(
                "region for instrument {:?} has no wsmp parameters",
                instrument.name
            ))
        })?;
        Self::from_wave(wave, params, note, velocity, output_rate, config)
    }

    /// Builds a voice from a wave and the parameters to play it with.
    ///
    /// This is the format-neutral entry point. DLS arrives through a bank and
    /// an instrument; SFZ arrives here directly, because an SFZ region resolves
    /// its own root note, level and stereo position from controller state at
    /// the moment the key goes down rather than from anything a collection
    /// stored ahead of time.
    pub fn from_wave(
        wave: &Wave,
        params: SampleParams,
        note: u8,
        velocity: u8,
        output_rate: u32,
        config: VoiceConfig,
    ) -> Result<Self, SoundfontError> {
        if params.unity_note > 127 {
            return Err(SoundfontError::Invalid(format!(
                "unity note {} is outside MIDI range",
                params.unity_note
            )));
        }
        let frame_count = wave.frame_count();
        if let Some(sample_loop) = params.sample_loop
            && (sample_loop.start >= sample_loop.end || sample_loop.end > frame_count)
        {
            return Err(SoundfontError::Invalid(format!(
                "loop {}..{} exceeds wave {:?} length {frame_count} frames",
                sample_loop.start, sample_loop.end, wave.name,
            )));
        }
        let pitch_cents =
            (f64::from(note) - f64::from(params.unity_note)) * 100.0 + f64::from(params.fine_tune);
        let pitch = 2.0_f64.powf(pitch_cents / 1_200.0);
        let base_increment = f64::from(wave.sample_rate) / f64::from(output_rate) * pitch;
        let attenuation = 10.0_f32.powf(params.attenuation_db / 20.0);
        // Blended rather than switched: `velocity_tracking` scales how far the
        // curve is allowed to pull the level down, so 0.0 leaves a
        // pre-layered sample at its recorded loudness.
        let tracking = config.velocity_tracking.clamp(0.0, 1.0);
        let velocity_gain =
            1.0 - tracking + tracking * (f32::from(velocity) / 127.0).powf(0.7);
        Ok(Self {
            note,
            samples: Arc::clone(&wave.samples),
            stream: None,
            channels: usize::from(wave.channels).max(1),
            frame_count,
            position: 0.0,
            base_increment,
            sample_loop: params.sample_loop,
            gain: attenuation * velocity_gain * config.gain,
            pan_gains: pan_gains(config.pan),
            envelope: AmplitudeEnvelope::new(config.amplitude_envelope, output_rate),
            pitch_envelope: Envelope::new(config.pitch_envelope.envelope, output_rate),
            pitch_depth_cents: config.pitch_envelope.depth_cents,
            lfo: config.lfo,
            lfo_phase: 0.0,
            lfo_delay_frames: (config.lfo.delay_seconds * output_rate as f32)
                .max(0.0)
                .round() as usize,
            pitch_offset_cents: config.pitch_offset_cents,
            pitch_bend_range_cents: config.pitch_bend_range_cents,
            modulation_depth: config.modulation_depth,
            output_rate: output_rate as f32,
            fade_level: 1.0,
            fade_step: 0.0,
            finished: false,
        })
    }

    pub fn note_off(&mut self) {
        self.envelope.note_off();
        self.pitch_envelope.note_off();
    }

    /// Silences the voice over `seconds` instead of cutting it dead.
    ///
    /// Removing a sounding voice from the pool drops its output from whatever
    /// amplitude the waveform happened to be at straight to zero. That step is
    /// a click, and it is heard most on a repeated key, where the note still
    /// ringing is taken over by its own retrigger — which is exactly what
    /// `note_polyphony=1` describes and what `off_time` is there to soften.
    ///
    /// A fade already under way is never made slower, so a voice asked to stop
    /// twice does not linger.
    pub fn fade_out(&mut self, seconds: f32) {
        let frames = (seconds.max(0.0) * self.output_rate).round().max(1.0);
        let step = self.fade_level / frames;
        if self.fade_step == 0.0 || step > self.fade_step {
            self.fade_step = step;
        }
    }

    /// Whether the voice is being faded out rather than playing normally.
    pub fn is_fading(&self) -> bool {
        self.fade_step > 0.0
    }

    pub fn is_finished(&self) -> bool {
        self.finished || self.envelope.is_finished()
    }

    pub fn next_sample(&mut self) -> f32 {
        self.next_sample_modulated(0.0, 0.0)
    }

    /// Mono downmix of [`Voice::next_frame_modulated`].
    ///
    /// Kept so callers that genuinely want one number do not have to average a
    /// frame themselves. A centre-panned mono wave returns exactly what this
    /// method returned before stereo existed.
    pub fn next_sample_modulated(&mut self, pitch_bend_cents: f32, modulation_wheel: f32) -> f32 {
        let [left, right] = self.next_frame_modulated(pitch_bend_cents, modulation_wheel);
        (left + right) * 0.5
    }

    pub fn next_frame(&mut self) -> [f32; 2] {
        self.next_frame_modulated(0.0, 0.0)
    }

    pub fn next_frame_modulated(
        &mut self,
        pitch_bend_cents: f32,
        modulation_wheel: f32,
    ) -> [f32; 2] {
        if self.is_finished() {
            return [0.0, 0.0];
        }
        let end = self.sample_loop.map_or(self.frame_count, |looping| looping.end);
        // A sample that simply runs out must not cut mid-waveform. Material
        // recorded to decay ends near silence and costs nothing here, but a
        // looped library is cut at its loop point instead: 94 of the 130
        // samples in the Rhodes measured here end above 0.01, the worst at
        // half of full scale. Dropping that to zero in one frame is a click,
        // and it is heard whether or not the loop is ever honoured.
        if self.sample_loop.is_none() && self.fade_step == 0.0 {
            let remaining = (end as f64 - self.position).max(0.0);
            let frames_left = remaining / self.base_increment.max(f64::MIN_POSITIVE);
            if frames_left <= f64::from(END_DECLICK_SECONDS * self.output_rate) {
                self.fade_out(END_DECLICK_SECONDS);
            }
        }
        if self.position >= end as f64 {
            if let Some(looping) = self.sample_loop {
                let length = (looping.end - looping.start) as f64;
                self.position =
                    looping.start as f64 + (self.position - looping.end as f64) % length;
            } else {
                self.finished = true;
                return [0.0, 0.0];
            }
        }
        let index = self.position.floor() as usize;
        let fraction = (self.position - index as f64) as f32;
        let next = if index + 1 < end {
            index + 1
        } else {
            self.sample_loop.map_or(index, |looping| looping.start)
        };
        // Read the source frame before any gain is applied. A mono wave feeds
        // both channels so panning behaves the same for either source.
        let (source_left, source_right) = self.interpolate_frame(index, next, fraction);
        let mut pitch_cents =
            pitch_bend_cents + self.pitch_depth_cents * self.pitch_envelope.next_gain();
        let mut lfo_attenuation_centibels = 0.0;
        if self.lfo_delay_frames > 0 {
            self.lfo_delay_frames -= 1;
        } else {
            let lfo_value = 1.0 - 4.0 * (self.lfo_phase - 0.5).abs();
            self.lfo_phase = (self.lfo_phase + self.lfo.frequency_hz / self.output_rate).fract();
            let modulation_wheel = modulation_wheel.clamp(0.0, 1.0);
            pitch_cents += lfo_value
                * (self.lfo.pitch_depth_cents
                    + modulation_wheel * self.lfo.mod_wheel_pitch_depth_cents);
            lfo_attenuation_centibels = -lfo_value
                * (self.lfo.attenuation_depth_centibels
                    + modulation_wheel * self.lfo.mod_wheel_attenuation_depth_centibels);
        }
        let pitch_ratio = 2.0_f64.powf(f64::from(pitch_cents) / 1_200.0);
        self.position += self.base_increment * pitch_ratio;
        let lfo_gain = 10.0_f32.powf(-lfo_attenuation_centibels / 200.0);
        if self.fade_step > 0.0 {
            self.fade_level -= self.fade_step;
            if self.fade_level <= 0.0 {
                self.fade_level = 0.0;
                self.finished = true;
            }
        }
        // The envelope advances once per frame, not once per channel, or a
        // stereo voice would decay at twice the rate of a mono one.
        let level = self.gain * self.envelope.next_gain() * lfo_gain * self.fade_level;
        [
            source_left * level * self.pan_gains[0],
            source_right * level * self.pan_gains[1],
        ]
    }

    /// Builds a voice whose head is resident and whose tail arrives from disk.
    ///
    /// `stream` may be absent when every stream in the pool is busy. The note
    /// then plays from its resident head and stops there, which is a shorter
    /// note under extreme polyphony rather than a missing one.
    pub fn from_streamed(
        sample: &crate::sample_store::StreamedSample,
        stream: Option<crate::streamer::StreamReader>,
        params: SampleParams,
        note: u8,
        velocity: u8,
        output_rate: u32,
        config: VoiceConfig,
    ) -> Result<Self, SoundfontError> {
        let channels = usize::from(sample.channels).max(1);
        // The head is presented to the renderer as an ordinary resident wave,
        // so everything below the read path is unchanged whether the audio is
        // streamed or not.
        let head = Wave {
            name: sample.name.clone(),
            sample_rate: sample.sample_rate,
            channels: sample.channels,
            source_bits: 16,
            samples: Arc::clone(&sample.preload),
            sample_params: None,
        };
        let mut voice = Self::from_wave(&head, params, note, velocity, output_rate, config)?;
        // `from_wave` sized the voice to the head; the sample is longer.
        voice.frame_count = sample.frame_count;
        voice.stream = stream.map(|reader| {
            crate::streamer::StreamWindow::new(reader, channels, sample.preload_frames)
        });
        Ok(voice)
    }

    /// Frames wanted before the reader could supply them.
    ///
    /// Zero on a healthy system. Anything else is the streaming equivalent of
    /// an underrun and is worth surfacing rather than hiding.
    pub fn starved_frames(&self) -> usize {
        self.stream
            .as_ref()
            .map_or(0, crate::streamer::StreamWindow::starved_frames)
    }

    /// Linearly interpolates one frame, widening mono sources to both channels.
    fn interpolate_frame(&mut self, index: usize, next: usize, fraction: f32) -> (f32, f32) {
        let Some(here) = self.frame_at(index) else {
            return (0.0, 0.0);
        };
        let Some(ahead) = self.frame_at(next) else {
            // The following frame has not arrived. Holding this one is better
            // than interpolating towards a zero that is not in the audio.
            return (here[0], here[1]);
        };
        (
            here[0] + (ahead[0] - here[0]) * fraction,
            here[1] + (ahead[1] - here[1]) * fraction,
        )
    }

    /// One frame widened to stereo, from resident audio or from the stream.
    ///
    /// Returns `None` only when a streamed frame has not arrived yet. The
    /// caller emits silence for it rather than repeating the previous frame,
    /// which would leave a DC step that clicks once the stream catches up.
    fn frame_at(&mut self, frame: usize) -> Option<[f32; 2]> {
        let resident_frames = self.samples.len() / self.channels;
        if frame < resident_frames {
            let base = frame * self.channels;
            let left = self.samples[base];
            let right = if self.channels < 2 {
                left
            } else {
                self.samples[base + 1]
            };
            return Some([left, right]);
        }
        let channels = self.channels;
        let samples = self.stream.as_mut()?.frame(frame)?;
        let left = samples[0];
        let right = if channels < 2 { left } else { samples[1] };
        Some([left, right])
    }

    pub fn next_sample_controlled(
        &mut self,
        pitch_bend_normalized: f32,
        modulation_wheel: f32,
    ) -> f32 {
        let [left, right] = self.next_frame_controlled(pitch_bend_normalized, modulation_wheel);
        (left + right) * 0.5
    }

    pub fn next_frame_controlled(
        &mut self,
        pitch_bend_normalized: f32,
        modulation_wheel: f32,
    ) -> [f32; 2] {
        self.next_frame_modulated(
            self.pitch_offset_cents
                + pitch_bend_normalized.clamp(-1.0, 1.0) * self.pitch_bend_range_cents,
            modulation_wheel.clamp(0.0, 1.0) * self.modulation_depth,
        )
    }
}

pub fn voices_for_note(
    bank: &DlsBank,
    instrument: &Instrument,
    note: u8,
    velocity: u8,
    output_rate: u32,
) -> Result<Vec<Voice>, SoundfontError> {
    instrument
        .matching_regions(note, velocity)
        .map(|region| Voice::new(bank, instrument, region, note, velocity, output_rate))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecents_conversion_has_known_landmarks() {
        assert!((timecents_to_seconds(0) - 1.0).abs() < 1e-6);
        assert!((timecents_to_seconds(1_200 * 65_536) - 2.0).abs() < 1e-5);
        assert_eq!(timecents_to_seconds(i32::MIN), 0.0);
    }

    #[test]
    fn default_envelope_is_finite() {
        let mut envelope = Envelope::new(EnvelopeSpec::default(), 48_000);
        for _ in 0..48_000 {
            assert!(envelope.next_gain().is_finite());
        }
        envelope.note_off();
        for _ in 0..48_000 {
            assert!(envelope.next_gain().is_finite());
        }
        assert_eq!(envelope.phase, EnvelopePhase::Finished);
    }

    #[test]
    fn parses_eg2_pitch_depth_in_cents() {
        let mut amplitude = EnvelopeSpec::default();
        let mut pitch = PitchEnvelopeSpec::default();
        let mut lfo = LfoSpec::default();
        apply_articulation_connection(
            &mut amplitude,
            &mut pitch,
            &mut lfo,
            CONN_SRC_EG2,
            CONN_SRC_NONE,
            CONN_DST_PITCH,
            102 * 65_536,
        );
        assert_eq!(pitch.depth_cents, 102.0);
    }

    #[test]
    fn parses_mod_wheel_lfo_pitch_depth() {
        let mut amplitude = EnvelopeSpec::default();
        let mut pitch = PitchEnvelopeSpec::default();
        let mut lfo = LfoSpec::default();
        apply_articulation_connection(
            &mut amplitude,
            &mut pitch,
            &mut lfo,
            CONN_SRC_LFO,
            CONN_SRC_CC1,
            CONN_DST_PITCH,
            50 * 65_536,
        );
        assert_eq!(lfo.mod_wheel_pitch_depth_cents, 50.0);
    }

    #[test]
    fn pitch_envelope_modulates_voice_playback_rate() {
        let bank = DlsBank {
            instruments: vec![Instrument {
                name: "Pitch envelope".into(),
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
                        sample_loop: None,
                    }),
                }],
                envelope: EnvelopeSpec::default(),
                pitch_envelope: PitchEnvelopeSpec {
                    envelope: EnvelopeSpec::pitch_default(),
                    depth_cents: 1_200.0,
                },
                lfo: LfoSpec::default(),
            }],
            waves: vec![Wave {
                name: "Synthetic".into(),
                sample_rate: 48_000,
                channels: 1,
                source_bits: 16,
                samples: Arc::from(vec![0.0; 32]),
                sample_params: None,
            }],
        };
        let instrument = &bank.instruments[0];
        let mut voice =
            Voice::new(&bank, instrument, &instrument.regions[0], 60, 127, 48_000).unwrap();
        voice.next_sample();
        assert!((voice.position - 2.0).abs() < 1e-6);
    }

    #[test]
    fn positive_sample_fine_tune_raises_playback_rate() {
        let bank = DlsBank {
            instruments: vec![Instrument {
                name: "Fine tune".into(),
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
                        fine_tune: 100,
                        attenuation_db: 0.0,
                        sample_loop: None,
                    }),
                }],
                envelope: EnvelopeSpec::default(),
                pitch_envelope: PitchEnvelopeSpec::default(),
                lfo: LfoSpec::default(),
            }],
            waves: vec![Wave {
                name: "Synthetic".into(),
                sample_rate: 48_000,
                channels: 1,
                source_bits: 16,
                samples: Arc::from(vec![0.0; 32]),
                sample_params: None,
            }],
        };
        let instrument = &bank.instruments[0];
        let mut voice =
            Voice::new(&bank, instrument, &instrument.regions[0], 60, 127, 48_000).unwrap();
        voice.next_sample();
        assert!((voice.position - 2.0_f64.powf(1.0 / 12.0)).abs() < 1e-6);
    }

    #[test]
    fn live_pitch_bend_modulates_an_existing_voice() {
        let bank = DlsBank {
            instruments: vec![Instrument {
                name: "Pitch bend".into(),
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
                        sample_loop: None,
                    }),
                }],
                envelope: EnvelopeSpec::default(),
                pitch_envelope: PitchEnvelopeSpec::default(),
                lfo: LfoSpec::default(),
            }],
            waves: vec![Wave {
                name: "Synthetic".into(),
                sample_rate: 48_000,
                channels: 1,
                source_bits: 16,
                samples: Arc::from(vec![0.0; 32]),
                sample_params: None,
            }],
        };
        let instrument = &bank.instruments[0];
        let mut voice =
            Voice::new(&bank, instrument, &instrument.regions[0], 60, 127, 48_000).unwrap();
        voice.next_sample_modulated(1_200.0, 0.0);
        assert!((voice.position - 2.0).abs() < 1e-6);
    }

    #[test]
    fn mod_wheel_scales_dls_lfo_pitch_depth() {
        let bank = DlsBank {
            instruments: vec![Instrument {
                name: "Mod wheel".into(),
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
                        sample_loop: None,
                    }),
                }],
                envelope: EnvelopeSpec::default(),
                pitch_envelope: PitchEnvelopeSpec::default(),
                lfo: LfoSpec {
                    frequency_hz: 0.0,
                    delay_seconds: 0.0,
                    pitch_depth_cents: 0.0,
                    mod_wheel_pitch_depth_cents: 1_200.0,
                    attenuation_depth_centibels: 0.0,
                    mod_wheel_attenuation_depth_centibels: 0.0,
                },
            }],
            waves: vec![Wave {
                name: "Synthetic".into(),
                sample_rate: 48_000,
                channels: 1,
                source_bits: 16,
                samples: Arc::from(vec![0.0; 32]),
                sample_params: None,
            }],
        };
        let instrument = &bank.instruments[0];
        let mut voice =
            Voice::new(&bank, instrument, &instrument.regions[0], 60, 127, 48_000).unwrap();
        voice.next_sample_modulated(0.0, 1.0);
        assert!((voice.position - 0.5).abs() < 1e-6);
    }

    #[test]
    fn amplitude_release_falls_linearly_in_centibels() {
        let mut envelope = AmplitudeEnvelope::new(
            EnvelopeSpec {
                attack_seconds: 0.0,
                decay_seconds: 0.0,
                sustain_level: 1.0,
                release_seconds: 1.0,
            },
            100,
        );
        assert_eq!(envelope.next_gain(), 1.0);
        envelope.note_off();
        let mut halfway_gain = 1.0;
        for _ in 0..50 {
            halfway_gain = envelope.next_gain();
        }
        assert!((halfway_gain - 10.0_f32.powf(-2.4)).abs() < 1e-5);
    }

    /// One region covering the whole keyboard, played at its unity note so the
    /// playback rate is exactly one frame per output frame.
    fn stereo_fixture(channels: u8, samples: Vec<f32>, sample_loop: Option<SampleLoop>) -> DlsBank {
        DlsBank {
            instruments: vec![Instrument {
                name: "Fixture".into(),
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
                        sample_loop,
                    }),
                }],
                envelope: EnvelopeSpec::default(),
                pitch_envelope: PitchEnvelopeSpec::default(),
                lfo: LfoSpec::default(),
            }],
            waves: vec![Wave {
                name: "Fixture".into(),
                sample_rate: 48_000,
                channels,
                source_bits: 16,
                samples: Arc::from(samples),
                sample_params: None,
            }],
        }
    }

    fn fixture_voice(bank: &DlsBank, pan: f32) -> Voice {
        let instrument = &bank.instruments[0];
        let mut config = VoiceConfig::inherit(instrument);
        config.pan = pan;
        Voice::new_with_config(
            bank,
            instrument,
            &instrument.regions[0],
            60,
            127,
            48_000,
            config,
        )
        .unwrap()
    }

    #[test]
    fn frame_count_is_independent_of_channel_count() {
        let mono = stereo_fixture(1, vec![0.0; 8], None);
        let stereo = stereo_fixture(2, vec![0.0; 8], None);
        assert_eq!(mono.waves[0].frame_count(), 8);
        assert_eq!(stereo.waves[0].frame_count(), 4);
    }

    #[test]
    fn a_mono_source_feeds_both_channels_equally() {
        let bank = stereo_fixture(1, vec![0.5, 0.5, 0.5, 0.5], None);
        let [left, right] = fixture_voice(&bank, 0.0).next_frame();
        assert!((left - right).abs() < 1e-6, "mono must stay centred");
        assert!(left > 0.0);
    }

    #[test]
    fn a_stereo_source_keeps_its_channels_apart() {
        // Frames are interleaved: left is always 1.0, right always -1.0.
        let bank = stereo_fixture(2, vec![1.0, -1.0, 1.0, -1.0], None);
        let [left, right] = fixture_voice(&bank, 0.0).next_frame();
        assert!(left > 0.0, "left channel lost its sign");
        assert!(right < 0.0, "right channel was overwritten by the left");
    }

    #[test]
    fn centre_pan_does_not_change_the_level_of_an_existing_bank() {
        // The regression this guards: adopting a constant-power pan law would
        // drop every DLS voice ever rendered by 3 dB.
        assert_eq!(pan_gains(0.0), [1.0, 1.0]);
    }

    #[test]
    fn panning_hard_silences_the_opposite_channel() {
        assert_eq!(pan_gains(-1.0), [1.0, 0.0]);
        assert_eq!(pan_gains(1.0), [0.0, 1.0]);
        let bank = stereo_fixture(1, vec![0.5; 8], None);
        let [left, right] = fixture_voice(&bank, -1.0).next_frame();
        assert!(left > 0.0);
        assert_eq!(right, 0.0);
    }

    #[test]
    fn a_pan_beyond_the_legal_range_is_clamped_rather_than_inverted() {
        assert_eq!(pan_gains(-4.0), pan_gains(-1.0));
        assert_eq!(pan_gains(4.0), pan_gains(1.0));
    }

    #[test]
    fn a_stereo_loop_counts_frames_rather_than_samples() {
        // Four frames of stereo. Looping 0..4 must replay all of them; a loop
        // read as samples would turn back after two and halve the loop.
        let bank = stereo_fixture(
            2,
            vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4],
            Some(SampleLoop { start: 0, end: 4 }),
        );
        let mut voice = fixture_voice(&bank, 0.0);
        let lefts: Vec<f32> = (0..4).map(|_| voice.next_frame()[0]).collect();
        assert!(
            lefts[3] > lefts[2] && lefts[2] > lefts[1],
            "the fourth frame was not reached: {lefts:?}"
        );
    }

    #[test]
    fn a_stereo_voice_decays_at_the_same_rate_as_a_mono_one() {
        // The envelope must advance once per frame, not once per channel.
        let release = EnvelopeSpec {
            attack_seconds: 0.0,
            decay_seconds: 0.0,
            sustain_level: 1.0,
            release_seconds: 1.0,
        };
        let mut mono = AmplitudeEnvelope::new(release, 100);
        let mut stereo = AmplitudeEnvelope::new(release, 100);
        mono.note_off();
        stereo.note_off();
        for _ in 0..25 {
            mono.next_gain();
            stereo.next_gain();
        }
        assert_eq!(mono.next_gain(), stereo.next_gain());
    }

    #[test]
    fn a_faded_voice_reaches_silence_without_a_step() {
        // The click this fixes: a displaced voice used to drop from whatever
        // amplitude it held straight to zero in one frame.
        let bank = stereo_fixture(1, vec![0.5; 4_000], None);
        let mut voice = fixture_voice(&bank, 0.0);
        voice.next_frame();
        voice.fade_out(0.01);
        let mut previous = voice.next_frame()[0];
        let mut frames = 1;
        while !voice.is_finished() && frames < 48_000 {
            let value = voice.next_frame()[0];
            assert!(
                (value - previous).abs() < 0.02,
                "step of {} at frame {frames}",
                value - previous
            );
            previous = value;
            frames += 1;
        }
        assert!(voice.is_finished(), "the fade never reached silence");
        assert!(previous.abs() < 1e-3, "it stopped at {previous}");
    }

    #[test]
    fn a_fade_takes_roughly_the_time_it_was_given() {
        let bank = stereo_fixture(1, vec![0.5; 96_000], None);
        let mut voice = fixture_voice(&bank, 0.0);
        voice.fade_out(0.1);
        let mut frames = 0;
        while !voice.is_finished() && frames < 96_000 {
            voice.next_frame();
            frames += 1;
        }
        // 0.1 s at 48 kHz is 4800 frames; allow for rounding on either side.
        assert!(
            (4_700..=4_900).contains(&frames),
            "a 0.1 s fade took {frames} frames"
        );
    }

    #[test]
    fn asking_twice_never_makes_a_fade_slower() {
        // A voice told to stop urgently must not be rescued by a later, gentler
        // request; the second key press still needs the first voice gone.
        let bank = stereo_fixture(1, vec![0.5; 96_000], None);
        let mut voice = fixture_voice(&bank, 0.0);
        voice.fade_out(0.005);
        voice.fade_out(1.0);
        let mut frames = 0;
        while !voice.is_finished() && frames < 96_000 {
            voice.next_frame();
            frames += 1;
        }
        assert!(frames < 1_000, "the slower request won: {frames} frames");
    }

    #[test]
    fn a_sample_that_ends_loud_still_stops_quietly() {
        // A looped library is cut at its loop point rather than faded, so its
        // last frame can sit at half of full scale. Reaching the end of one
        // must not produce a step.
        let bank = stereo_fixture(1, vec![0.5; 2_000], None);
        let mut voice = fixture_voice(&bank, 0.0);
        let mut previous = 0.0;
        let mut worst_step = 0.0_f32;
        let mut frames = 0;
        while !voice.is_finished() && frames < 10_000 {
            let value = voice.next_frame()[0];
            if frames > 0 {
                worst_step = worst_step.max((value - previous).abs());
            }
            previous = value;
            frames += 1;
        }
        assert!(voice.is_finished(), "the voice never ended");
        assert!(previous.abs() < 0.01, "it stopped at {previous}, which is a click");
        assert!(worst_step < 0.05, "a step of {worst_step} remains at the end");
    }

    #[test]
    fn a_looping_sample_is_not_faded_at_its_loop_point() {
        // The declick must not mistake a loop for the end of the audio.
        let bank = stereo_fixture(1, vec![0.5; 2_000], Some(SampleLoop { start: 0, end: 1_000 }));
        let mut voice = fixture_voice(&bank, 0.0);
        for _ in 0..4_000 {
            voice.next_frame();
        }
        assert!(!voice.is_finished(), "a looping voice ended");
        assert!(!voice.is_fading(), "a looping voice was faded out");
    }

    #[test]
    fn a_voice_nobody_faded_keeps_its_full_level() {
        let bank = stereo_fixture(1, vec![0.5; 100], None);
        let mut voice = fixture_voice(&bank, 0.0);
        assert!(!voice.is_fading());
        assert!(voice.next_frame()[0] > 0.0);
    }

    #[test]
    fn the_mono_downmix_matches_a_centred_frame() {
        let bank = stereo_fixture(1, vec![0.5; 8], None);
        let expected = fixture_voice(&bank, 0.0).next_frame()[0];
        let actual = fixture_voice(&bank, 0.0).next_sample();
        assert!((expected - actual).abs() < 1e-6);
    }
}
