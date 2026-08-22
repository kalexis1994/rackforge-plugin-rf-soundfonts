//! Reading the zone map out of a Kontakt instrument file.
//!
//! The format is far more open than its reputation. A `.nki` is a fixed
//! header, a signature naming the Kontakt generation, and a zlib stream; the
//! stream inflates to a plain XML document that describes itself. The
//! accordions this was written against carry no scripts and no encryption, and
//! their samples sit beside them as ordinary WAV files.
//!
//! That is worth stating precisely, because the reputation is not wrong about
//! the rest of the ecosystem: a Kontakt Player library is encrypted with keys
//! that belong to its publisher, and a scripted one puts its behaviour in KSP
//! rather than in the map. Neither is read here, and neither can be. What this
//! module handles is the plain case — which is what an instrument saved from
//! Kontakt without those features actually is.
//!
//! Only the mapping is taken: which sample sounds for which keys and
//! velocities, where its root is, how loud and how detuned. Everything the
//! renderer needs, and nothing it would have to guess at.

use std::collections::BTreeMap;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::SoundfontError;

/// Identifies a Kontakt instrument file.
const MAGIC: u32 = 0x7fa8_9012;

/// Offset of the signature naming the format generation.
const SIGNATURE_OFFSET: usize = 20;

/// Offset at which the compressed document begins.
///
/// Constant across every file examined. Verified rather than assumed: the
/// reader checks for the zlib header there and says so plainly if it is
/// missing, because a silently wrong offset would look like a corrupt file.
const PAYLOAD_OFFSET: usize = 170;

/// Signatures this reader understands, stored as they appear on disk.
const KONTAKT_4: &[u8; 4] = b"4noK";
const KONTAKT_2: &[u8; 4] = b"2noK";

/// Ceiling on the inflated document.
///
/// A zone map is tens of kilobytes. This bounds what a malformed or hostile
/// file can make the loader allocate, without being anywhere near a real
/// instrument's size.
const MAX_DOCUMENT_BYTES: usize = 64 * 1_048_576;

/// The amplitude envelope a group applies to its zones.
///
/// Kontakt keeps this in a modulator rather than in the zone: a group holds a
/// list of them, each naming what it drives, and the one driving `volume` is
/// the note's shape. The others — a filter sweep, a pitch wobble — are read
/// past, because the renderer has nothing to do with them.
///
/// Times are seconds, converted from the milliseconds the document states.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NkiEnvelope {
    pub attack_seconds: f32,
    /// Kept for fidelity to the document. Every library examined states zero,
    /// and the renderer's envelope has no hold stage to put it in.
    pub hold_seconds: f32,
    pub decay_seconds: f32,
    pub sustain_level: f32,
    pub release_seconds: f32,
}

/// One sample placed on the keyboard.
#[derive(Clone, Debug, PartialEq)]
pub struct NkiZone {
    /// File name as written in the instrument, without its serialised path.
    pub sample: String,
    pub key_low: u8,
    pub key_high: u8,
    pub root_key: u8,
    pub velocity_low: u8,
    pub velocity_high: u8,
    /// Frame the sample starts from, for a trimmed attack.
    pub sample_start: usize,
    /// Group whose envelope shapes this zone, by position in the document.
    pub group: usize,
    /// Linear volume, where 1.0 is unity.
    pub volume: f32,
    /// Stereo position from -1.0 to 1.0.
    pub pan: f32,
    /// Tuning as a frequency ratio, where 1.0 is unaltered.
    pub tune: f32,
    pub sample_loop: Option<crate::SampleLoop>,
}

/// An instrument's zone map.
#[derive(Clone, Debug, Default)]
pub struct NkiDocument {
    pub name: String,
    /// Kontakt performance-view wallpaper, reduced to its portable file name.
    pub wallpaper: Option<String>,
    pub zones: Vec<NkiZone>,
    /// Envelope of each group, in document order. A group that declares no
    /// modulator driving volume holds `None`, and its zones take the
    /// renderer's default rather than a shape invented here.
    pub groups: Vec<Option<NkiEnvelope>>,
    /// Audible DSP translated from Kontakt's group and program insert racks.
    pub effects: NkiEffects,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NkiEffects {
    /// Ordered insert chain for each Kontakt group.
    pub group_filters: BTreeMap<usize, Vec<NkiFilter>>,
    pub program: Vec<NkiProgramEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NkiFilter {
    LowPass2 {
        /// Kontakt's normalised cutoff value, from 0.0 to 1.0.
        cutoff: f32,
        resonance: f32,
    },
    HighPass2 {
        /// Kontakt's normalised cutoff value, from 0.0 to 1.0.
        cutoff: f32,
        resonance: f32,
    },
    PeakEq {
        frequency_hz: f32,
        bandwidth_octaves: f32,
        gain_db: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NkiProgramEffect {
    Reverb(NkiReverb),
    Delay(NkiDelay),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NkiReverb {
    pub pre_delay_ms: f32,
    pub room_size: f32,
    pub width: f32,
    pub color: f32,
    pub damping: f32,
    pub wet_gain: f32,
    pub dry_gain: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NkiDelay {
    pub time_ms: f32,
    pub feedback: f32,
    pub panning: f32,
    pub damping: f32,
    pub wet_gain: f32,
    pub dry_gain: f32,
}

/// Inflates the document inside a `.nki`.
pub fn inflate(bytes: &[u8]) -> Result<String, SoundfontError> {
    use std::io::Read;

    if bytes.len() < PAYLOAD_OFFSET + 2 {
        return Err(SoundfontError::Invalid(
            "Kontakt instrument is shorter than its header".into(),
        ));
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(SoundfontError::Invalid(format!(
            "not a Kontakt instrument: magic {magic:#010x}"
        )));
    }
    let signature: &[u8] = &bytes[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4];
    if signature != KONTAKT_4 && signature != KONTAKT_2 {
        return Err(SoundfontError::Unsupported(format!(
            "Kontakt format {:?} is not read here",
            String::from_utf8_lossy(signature)
        )));
    }
    // The header is a known length in every file seen, but a wrong guess would
    // surface as a confusing decompression failure, so it is checked directly.
    if bytes[PAYLOAD_OFFSET] != 0x78 {
        return Err(SoundfontError::Unsupported(
            "Kontakt instrument is not a plain zlib document; it may be from \
             a Player library, which is encrypted and not read here"
                .into(),
        ));
    }

    let decoder = flate2::read::ZlibDecoder::new(&bytes[PAYLOAD_OFFSET..]);
    let mut text = String::new();
    decoder
        .take(MAX_DOCUMENT_BYTES as u64)
        .read_to_string(&mut text)
        .map_err(|error| {
            SoundfontError::Invalid(format!("Kontakt document does not inflate: {error}"))
        })?;
    if text.is_empty() {
        return Err(SoundfontError::Invalid(
            "Kontakt document inflated to nothing".into(),
        ));
    }
    Ok(text)
}

/// Reads the zone map from an inflated document.
///
/// Zones missing what the renderer needs are skipped rather than failing the
/// instrument: a map with one unusable zone is still worth playing, and the
/// count of what was dropped is returned so it can be reported.
pub fn parse(text: &str) -> Result<(NkiDocument, usize), SoundfontError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut document = NkiDocument::default();
    let mut skipped = 0;
    // Values are gathered per zone regardless of which container they sit in,
    // because the document nests them differently in different generations and
    // the names themselves are unambiguous.
    let mut current: Option<BTreeMap<String, String>> = None;
    let mut depth_of_zone = 0_usize;
    let mut depth = 0_usize;

    // A group's envelope arrives in pieces spread across three nested
    // elements: which parameter the modulator drives, and separately the
    // stage times. They are collected as they pass and joined when the
    // modulator closes.
    let mut group_envelope: Option<NkiEnvelope> = None;
    let mut in_group = false;
    let mut in_modulator = false;
    let mut modulator_kind: Option<String> = None;
    let mut modulator_target: Option<String> = None;
    let mut in_envelope = false;
    let mut stages: BTreeMap<String, String> = BTreeMap::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                depth += 1;
                match element.name().as_ref() {
                    b"K2_Zone" => {
                        let mut values = BTreeMap::new();
                        // The group reference is an attribute of the zone
                        // rather than one of its listed values, so it is put
                        // in with them here and read back like the rest.
                        #[allow(deprecated)]
                        for attribute in element.attributes().flatten() {
                            if attribute.key.as_ref() == b"groupIdx"
                                && let Ok(text) = attribute.unescape_value()
                            {
                                values.insert("groupIdx".to_string(), text.into_owned());
                            }
                        }
                        current = Some(values);
                        depth_of_zone = depth;
                    }
                    b"K2_Group" => {
                        in_group = true;
                        group_envelope = None;
                    }
                    b"K2_IntMod" => {
                        in_modulator = true;
                        modulator_kind = None;
                        modulator_target = None;
                        stages.clear();
                    }
                    b"Envelope" => {
                        in_envelope = true;
                        // What kind of modulator this is sits on the envelope,
                        // not on the modulator: `K2_IntMod` carries only an
                        // index and a version. A pitch wobble has no
                        // `<Envelope>` at all, so its absence is the answer.
                        #[allow(deprecated)]
                        for attribute in element.attributes().flatten() {
                            if attribute.key.as_ref() == b"type"
                                && let Ok(text) = attribute.unescape_value()
                            {
                                modulator_kind = Some(text.into_owned());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(element)) => {
                if element.name().as_ref() != b"V" {
                    continue;
                }
                let pair = value_pair(&element);
                if pair.0.as_deref() == Some("wallpaperFile") {
                    if document.wallpaper.is_none() {
                        document.wallpaper = pair.1.as_deref().and_then(resource_name);
                    }
                    continue;
                }
                if in_envelope || in_modulator {
                    let (Some(name), Some(value)) = pair else {
                        continue;
                    };
                    if in_envelope {
                        stages.entry(name).or_insert(value);
                    } else if name == "target" {
                        // The first target wins: a modulator may drive several
                        // parameters, and the volume one names it first.
                        modulator_target.get_or_insert(value);
                    }
                    continue;
                }
                let Some(values) = current.as_mut() else {
                    continue;
                };
                let (name, value) = pair;
                if let (Some(name), Some(value)) = (name, value) {
                    // First writer wins. A zone's own parameters appear before
                    // the nested modulator tables that reuse names like
                    // `volume`, and taking the last would read a modulation
                    // depth as the zone's level.
                    values.entry(name).or_insert(value);
                }
            }
            Ok(Event::End(element)) => {
                match element.name().as_ref() {
                    b"K2_Zone" if depth == depth_of_zone => {
                        if let Some(values) = current.take() {
                            match zone_from(&values) {
                                Some(zone) => document.zones.push(zone),
                                None => skipped += 1,
                            }
                        }
                    }
                    b"Envelope" => in_envelope = false,
                    b"K2_IntMod" => {
                        let drives_volume = modulator_kind.as_deref() == Some("ahdsr")
                            && modulator_target.as_deref() == Some("volume");
                        // First one wins, so a group listing two volume
                        // envelopes takes the one it applies first.
                        if drives_volume && in_group && group_envelope.is_none() {
                            group_envelope = envelope_from(&stages);
                        }
                        // Cleared so the zone values that follow are not read
                        // as though they were still inside a modulator.
                        in_modulator = false;
                        modulator_kind = None;
                        modulator_target = None;
                        stages.clear();
                    }
                    b"K2_Group" => {
                        document.groups.push(group_envelope.take());
                        in_group = false;
                    }
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(SoundfontError::Invalid(format!(
                    "Kontakt document is not valid XML: {error}"
                )));
            }
            _ => {}
        }
    }

    if document.zones.is_empty() {
        return Err(SoundfontError::Invalid(
            "Kontakt instrument declares no playable zones".into(),
        ));
    }
    document.effects = parse_effects(text)?;
    Ok((document, skipped))
}

/// Reads the `name` and `value` attributes of a `<V>` element.
fn value_pair(element: &quick_xml::events::BytesStart<'_>) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut value = None;
    #[allow(deprecated)]
    for attribute in element.attributes().flatten() {
        match attribute.key.as_ref() {
            b"name" => {
                name = attribute
                    .unescape_value()
                    .ok()
                    .map(|text| text.into_owned())
            }
            b"value" => {
                value = attribute
                    .unescape_value()
                    .ok()
                    .map(|text| text.into_owned())
            }
            _ => {}
        }
    }
    (name, value)
}

#[derive(Clone, Copy)]
enum EffectScope {
    ProgramInsert,
    ProgramSend,
    GroupInsert(usize),
}

struct EffectDraft {
    scope: EffectScope,
    kind: Option<String>,
    kind_type: Option<String>,
    values: BTreeMap<String, String>,
}

/// Reads the bounded subset of Kontakt DSP that the resident player can
/// reproduce faithfully. Routing-only `SendLevels` nodes are intentionally
/// ignored; program sends are retained by Kontakt as separate buses and are
/// not inserts unless another node actually feeds them.
fn parse_effects(text: &str) -> Result<NkiEffects, SoundfontError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut effects = NkiEffects::default();
    let mut scope = None;
    let mut draft: Option<EffectDraft> = None;
    let mut current_group = None;
    let mut next_group = 0_usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.name().as_ref() {
                b"K2_Group" => {
                    #[allow(deprecated)]
                    let explicit_index = element
                        .attributes()
                        .flatten()
                        .find(|attribute| attribute.key.as_ref() == b"index")
                        .and_then(|attribute| attribute.unescape_value().ok())
                        .and_then(|value| value.parse::<usize>().ok());
                    let group = explicit_index.unwrap_or(next_group);
                    current_group = Some(group);
                    next_group = next_group.max(group.saturating_add(1));
                }
                b"ProgramInsertFX" => scope = Some(EffectScope::ProgramInsert),
                b"ProgramSendFX" => scope = Some(EffectScope::ProgramSend),
                b"GroupInsertFX" => {
                    scope = current_group.map(EffectScope::GroupInsert);
                }
                b"K2_Effect" => {
                    draft = scope.map(|scope| EffectDraft {
                        scope,
                        kind: None,
                        kind_type: None,
                        values: BTreeMap::new(),
                    });
                }
                b"Reverb" | b"Delay" | b"Filter" if draft.is_some() => {
                    let value = draft.as_mut().unwrap();
                    value.kind =
                        Some(String::from_utf8_lossy(element.name().as_ref()).into_owned());
                    #[allow(deprecated)]
                    for attribute in element.attributes().flatten() {
                        if attribute.key.as_ref() == b"type"
                            && let Ok(text) = attribute.unescape_value()
                        {
                            value.kind_type = Some(text.into_owned());
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(element)) if element.name().as_ref() == b"V" => {
                if let Some(draft) = draft.as_mut()
                    && let (Some(name), Some(value)) = value_pair(&element)
                {
                    draft.values.entry(name).or_insert(value);
                }
            }
            Ok(Event::End(element)) => match element.name().as_ref() {
                b"K2_Effect" => {
                    if let Some(draft) = draft.take() {
                        append_effect(&mut effects, draft);
                    }
                }
                b"ProgramInsertFX" | b"ProgramSendFX" | b"GroupInsertFX" => scope = None,
                b"K2_Group" => current_group = None,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(SoundfontError::Invalid(format!(
                    "Kontakt effects are not valid XML: {error}"
                )));
            }
            _ => {}
        }
    }
    Ok(effects)
}

fn append_effect(effects: &mut NkiEffects, draft: EffectDraft) {
    if draft
        .values
        .get("bypass")
        .is_some_and(|value| value == "yes")
    {
        return;
    }
    let number = |name: &str, fallback: f32| {
        draft
            .values
            .get(name)
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|value| value.is_finite())
            .unwrap_or(fallback)
    };
    match (
        draft.scope,
        draft.kind.as_deref(),
        draft.kind_type.as_deref(),
    ) {
        (EffectScope::ProgramInsert, Some("Reverb"), _) => {
            effects.program.push(NkiProgramEffect::Reverb(NkiReverb {
                pre_delay_ms: number("preDelay", 0.0).clamp(0.0, 200.0),
                room_size: number("roomsize", 0.5).clamp(0.0, 1.0),
                width: number("stereo", 0.8).clamp(0.0, 1.0),
                color: number("color", 0.5).clamp(0.0, 1.0),
                damping: number("filter", 0.5).clamp(0.0, 1.0),
                wet_gain: number("outLevel", 1.0).clamp(0.0, 4.0),
                dry_gain: number("outLevelDry", 1.0).clamp(0.0, 4.0),
            }));
        }
        (EffectScope::ProgramInsert, Some("Delay"), _) => {
            effects.program.push(NkiProgramEffect::Delay(NkiDelay {
                time_ms: number("time", 250.0).clamp(1.0, 2_000.0),
                feedback: number("feedback", 0.25).clamp(0.0, 0.95),
                panning: number("panning", 0.0).clamp(-1.0, 1.0),
                damping: number("damping", 0.0).clamp(0.0, 1.0),
                wet_gain: number("outLevel", 1.0).clamp(0.0, 4.0),
                dry_gain: number("outLevelDry", 1.0).clamp(0.0, 4.0),
            }));
        }
        (EffectScope::GroupInsert(group), Some("Filter"), Some("lp2pole")) => {
            effects
                .group_filters
                .entry(group)
                .or_default()
                .push(NkiFilter::LowPass2 {
                    cutoff: number("cutoff", 1.0).clamp(0.0, 1.0),
                    resonance: number("resonance", 0.0).clamp(0.0, 1.0),
                });
        }
        (EffectScope::GroupInsert(group), Some("Filter"), Some("hp2pole")) => {
            effects
                .group_filters
                .entry(group)
                .or_default()
                .push(NkiFilter::HighPass2 {
                    cutoff: number("cutoff", 0.0).clamp(0.0, 1.0),
                    resonance: number("resonance", 0.0).clamp(0.0, 1.0),
                });
        }
        (EffectScope::GroupInsert(group), Some("Filter"), Some("eq1band")) => {
            effects
                .group_filters
                .entry(group)
                .or_default()
                .push(NkiFilter::PeakEq {
                    frequency_hz: number("freq_1", 1_000.0).clamp(20.0, 20_000.0),
                    bandwidth_octaves: number("bandWidth_1", 1.0).clamp(0.05, 8.0),
                    gain_db: number("gain_1", 0.0).clamp(-24.0, 24.0),
                });
        }
        // Program sends need a send-level source to become audible. Merely
        // declaring the destination reverb does not put it in the insert path.
        _ => {}
    }
}

/// Builds an envelope from the stage times a modulator states.
///
/// The times are milliseconds. Nothing in the document says so, but the values
/// only make sense that way: the accordions here state decays of several
/// thousand against sustains near unity, which is a reed holding steady, and
/// releases in the tens, which is a reed stopping.
fn envelope_from(stages: &BTreeMap<String, String>) -> Option<NkiEnvelope> {
    let milliseconds = |name: &str| {
        stages
            .get(name)
            .and_then(|text| text.parse::<f32>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| value / 1_000.0)
    };
    // Without a release there is no envelope worth taking: it is the one stage
    // the renderer cannot infer, and its absence means this modulator is not
    // shaped the way this reader assumes.
    let release_seconds = milliseconds("release")?;
    Some(NkiEnvelope {
        attack_seconds: milliseconds("attack").unwrap_or(0.0),
        hold_seconds: milliseconds("hold").unwrap_or(0.0),
        decay_seconds: milliseconds("decay").unwrap_or(0.0),
        sustain_level: stages
            .get("sustain")
            .and_then(|text| text.parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0),
        release_seconds,
    })
}

fn zone_from(values: &BTreeMap<String, String>) -> Option<NkiZone> {
    let sample = sample_name(values.get("file_ex2").or_else(|| values.get("file"))?)?;
    let number = |name: &str| values.get(name).and_then(|text| text.parse::<f32>().ok());
    let key = |name: &str, fallback: f32| number(name).unwrap_or(fallback).clamp(0.0, 127.0) as u8;

    let key_low = key("lowKey", 0.0);
    let key_high = key("highKey", 127.0);
    if key_low > key_high {
        return None;
    }
    let velocity_low = key("lowVelocity", 1.0);
    let velocity_high = key("highVelocity", 127.0);

    let sample_start = number("sampleStart").unwrap_or(0.0).max(0.0) as usize;
    // A loop is described by its start and length; a zero length is Kontakt's
    // way of saying the zone does not loop.
    let loop_start = number("loopStart").unwrap_or(0.0).max(0.0) as usize;
    let loop_length = number("loopLength").unwrap_or(0.0).max(0.0) as usize;
    let sample_loop = (loop_length > 0).then(|| crate::SampleLoop {
        start: loop_start,
        end: loop_start + loop_length,
    });

    let group = number("groupIdx").unwrap_or(0.0).max(0.0) as usize;

    Some(NkiZone {
        sample,
        group,
        key_low,
        key_high,
        root_key: key("rootKey", 60.0),
        velocity_low,
        velocity_high: velocity_high.max(velocity_low),
        sample_start,
        volume: number("zoneVolume").unwrap_or(1.0).max(0.0),
        pan: number("zonePan").unwrap_or(0.0).clamp(-1.0, 1.0),
        // Kontakt stores tuning as a ratio; anything at or below zero would
        // stop the sample dead, so it is treated as unset.
        tune: number("zoneTune")
            .filter(|ratio| *ratio > 0.0)
            .unwrap_or(1.0),
        sample_loop,
    })
}

/// Recovers a file name from Kontakt's serialised path.
///
/// A path is written as `@` followed by typed, length-prefixed segments:
/// `@d025Hohner Colombiano SamplesF00000017000ACORDEON REAL.wav`. Only the
/// final name is taken. Resolving the recorded directories would be worse than
/// useless — they are absolute paths from the machine that saved the
/// instrument, and will not exist on the one playing it.
fn sample_name(raw: &str) -> Option<String> {
    resource_name(raw)
}

/// Recovers the final portable name from either a Kontakt serialised path or
/// a conventional path. Samples and wallpapers use the same encoding.
fn resource_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Segment names may contain slashes on neither platform, so the last of
    // either separator is a safe boundary for a plainly written path too.
    let tail = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    // The serialised form has no separator, so fall back to the last position
    // where a known audio extension ends and walk back to the segment start.
    let name = strip_length_prefix(tail);
    (!name.is_empty()).then(|| name.to_string())
}

/// Removes the type and length markers preceding the final segment.
///
/// Kontakt writes a relative path as a run of segments, each introduced by a
/// letter naming its kind and a number giving the length of the text that
/// follows: `@d025Hohner Student 72 SamplesF00000012000smpl0190.wav` is a
/// directory of twenty-five characters and then a file of twelve. Nothing
/// separates the segments, so the marker is the only boundary there is.
///
/// That length is what makes a marker recognisable. Treating any letter
/// followed by digits as one is not enough: `smpl0190.wav` contains `l0190`,
/// which reads as a marker and leaves the name as `.wav`. So a candidate is
/// accepted only when the number it states matches the length of what remains
/// — a claim the text either satisfies or does not.
fn strip_length_prefix(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut boundary = 0;
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_alphabetic() {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        // Where the marker ends cannot simply be read off, for two reasons: the
        // file marker pads its count out to a fixed width with trailing zeroes,
        // and a name beginning with a digit — `01 - F1.wav` — runs straight on
        // from that padding with nothing to say where one stops.
        //
        // So the length is used the other way round. Each prefix of the digits
        // is a candidate count, and a count of N means the marker must end N
        // characters from the end of the text. When that position is consistent
        // with the prefix that proposed it, the reading is self-supporting.
        let matched = (start..end).find_map(|split| {
            let declared = text[start..=split].parse::<usize>().ok()?;
            let boundary = bytes.len().checked_sub(declared)?;
            (boundary > split && boundary <= end).then_some(boundary)
        });
        if let Some(end) = matched {
            // The earliest marker that describes the rest of the text is the
            // one introducing the final segment; markers for the directories
            // before it describe only their own span and so never match here.
            // Scanning stops there because a later match could only come from
            // the file name itself, which must be kept whole.
            boundary = end;
            break;
        }
        index += 1;
    }
    text[boundary..].trim_start_matches('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn a_serialised_path_yields_its_file_name() {
        assert_eq!(
            sample_name("@d025Hohner Colombiano SamplesF00000017000ACORDEON REAL.wav").as_deref(),
            Some("ACORDEON REAL.wav")
        );
    }

    #[test]
    fn a_plainly_written_path_yields_its_file_name() {
        assert_eq!(
            sample_name("C:\\Libraries\\Accordion\\Samples\\note60.wav").as_deref(),
            Some("note60.wav")
        );
        assert_eq!(
            sample_name("/home/player/samples/note60.wav").as_deref(),
            Some("note60.wav")
        );
    }

    #[test]
    fn a_name_containing_digits_is_not_mistaken_for_a_marker() {
        // `NewSample_0001.wav` is eighteen characters, which is what the marker
        // states; `e_0001` inside it reads as a marker too but describes a
        // length nothing in the text has.
        assert_eq!(
            sample_name("@F00000018000NewSample_0001.wav").as_deref(),
            Some("NewSample_0001.wav")
        );
    }

    #[test]
    fn a_name_whose_digits_follow_a_letter_survives() {
        // The Hohner library names every sample `smplNNNN.wav`, so the marker
        // heuristic that only looked for a letter and digits cut at `l0190`
        // and left `.wav` behind.
        assert_eq!(
            sample_name("@d025Hohner Student 72 SamplesF00000012000smpl0190.wav").as_deref(),
            Some("smpl0190.wav")
        );
    }

    #[test]
    fn a_name_beginning_with_digits_is_not_eaten_by_the_marker() {
        // The Giulietti library numbers its samples, so the marker's padding
        // runs straight into the name with nothing between them. Only the
        // stated length says where one ends and the other begins.
        assert_eq!(
            sample_name("@d028Sanfona - Original-2 SamplesF0000001100001 - F1.wav").as_deref(),
            Some("01 - F1.wav")
        );
    }

    #[test]
    fn a_marker_that_misstates_its_length_is_left_alone() {
        // Better to hand back a name that will plainly not be found than to
        // cut a real one at a boundary the text does not support.
        assert_eq!(
            sample_name("@F00000099000note60.wav").as_deref(),
            Some("F00000099000note60.wav")
        );
    }

    #[test]
    fn an_empty_reference_is_refused() {
        assert!(sample_name("").is_none());
        assert!(sample_name("@").is_none());
    }

    #[test]
    fn a_zone_carries_its_placement() {
        let built = zone_from(&zone(&[
            ("file_ex2", "@d004baseF00000008000note.wav"),
            ("lowKey", "48"),
            ("highKey", "59"),
            ("rootKey", "55"),
            ("lowVelocity", "1"),
            ("highVelocity", "100"),
        ]))
        .unwrap();
        assert_eq!(built.sample, "note.wav");
        assert_eq!((built.key_low, built.key_high), (48, 59));
        assert_eq!(built.root_key, 55);
        assert_eq!((built.velocity_low, built.velocity_high), (1, 100));
    }

    #[test]
    fn a_zone_without_a_sample_is_dropped() {
        assert!(zone_from(&zone(&[("lowKey", "0"), ("highKey", "127")])).is_none());
    }

    #[test]
    fn an_inverted_key_range_is_dropped_rather_than_played_silently() {
        assert!(
            zone_from(&zone(&[
                ("file_ex2", "note.wav"),
                ("lowKey", "80"),
                ("highKey", "20"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn a_zero_length_loop_means_no_loop() {
        let built = zone_from(&zone(&[
            ("file_ex2", "note.wav"),
            ("loopStart", "100"),
            ("loopLength", "0"),
        ]))
        .unwrap();
        assert!(built.sample_loop.is_none());
    }

    #[test]
    fn a_loop_is_read_as_a_start_and_a_length() {
        let built = zone_from(&zone(&[
            ("file_ex2", "note.wav"),
            ("loopStart", "1000"),
            ("loopLength", "845"),
        ]))
        .unwrap();
        let looping = built.sample_loop.unwrap();
        assert_eq!((looping.start, looping.end), (1_000, 1_845));
    }

    #[test]
    fn a_zone_falls_back_to_sensible_placement() {
        let built = zone_from(&zone(&[("file_ex2", "note.wav")])).unwrap();
        assert_eq!((built.key_low, built.key_high), (0, 127));
        assert_eq!(built.root_key, 60);
        assert_eq!(built.volume, 1.0);
        assert_eq!(built.tune, 1.0);
    }

    #[test]
    fn a_zero_tuning_ratio_is_treated_as_unset() {
        // A ratio of zero would stop the sample rather than detune it.
        let built = zone_from(&zone(&[("file_ex2", "note.wav"), ("zoneTune", "0.")])).unwrap();
        assert_eq!(built.tune, 1.0);
    }

    #[test]
    fn a_zone_takes_its_own_level_rather_than_a_modulator_depth() {
        // The document repeats names like `volume` inside modulator tables
        // that follow the zone's own parameters.
        let mut values = zone(&[("file_ex2", "note.wav"), ("zoneVolume", "0.5")]);
        values.entry("zoneVolume".into()).or_insert("9.9".into());
        assert_eq!(zone_from(&values).unwrap().volume, 0.5);
    }

    #[test]
    fn a_document_without_zones_is_refused() {
        let error = parse("<?xml version=\"1.0\"?><K2_Container/>").unwrap_err();
        assert!(error.to_string().contains("no playable zones"), "{error}");
    }

    #[test]
    fn malformed_xml_is_refused_rather_than_guessed_at() {
        assert!(parse("<K2_Zone><Parameters>").is_err());
    }

    #[test]
    fn zones_are_read_in_document_order() {
        let text = r#"<?xml version="1.0"?>
<K2_Container>
  <Parameters>
    <V name="wallpaperFile" value="@d018AcordClari Samplesd009WallpaperF00000016000Acordeon III.tga"/>
  </Parameters>
  <Zones>
    <K2_Zone index="0">
      <Parameters><V name="lowKey" value="0"/><V name="highKey" value="59"/></Parameters>
      <Sample><V name="file_ex2" value="@F00000007000low.wav"/></Sample>
    </K2_Zone>
    <K2_Zone index="1">
      <Parameters><V name="lowKey" value="60"/><V name="highKey" value="127"/></Parameters>
      <Sample><V name="file_ex2" value="@F00000008000high.wav"/></Sample>
    </K2_Zone>
  </Zones>
</K2_Container>"#;
        let (document, skipped) = parse(text).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(document.zones.len(), 2);
        assert_eq!(document.zones[0].sample, "low.wav");
        assert_eq!(document.zones[1].sample, "high.wav");
        assert_eq!(document.wallpaper.as_deref(), Some("Acordeon III.tga"));
    }

    #[test]
    fn audible_insert_effects_keep_their_scope_and_order() {
        let text = r#"<K2_Container>
  <ProgramInsertFX>
    <K2_Effect index="0"><V name="outLevel" value="0.4"/><V name="outLevelDry" value="1"/>
      <Reverb><V name="preDelay" value="25"/><V name="roomsize" value="0.75"/>
        <V name="stereo" value="0.8"/><V name="color" value="0.5"/><V name="filter" value="0.5"/></Reverb>
    </K2_Effect>
    <K2_Effect index="1"><V name="outLevel" value="0.1"/><V name="outLevelDry" value="1"/>
      <Delay><V name="time" value="321"/><V name="feedback" value="0.4"/>
        <V name="panning" value="0.2"/><V name="damping" value="0.2"/></Delay>
    </K2_Effect>
  </ProgramInsertFX>
  <Groups><K2_Group index="0"><GroupInsertFX><K2_Effect index="0">
    <Filter type="lp2pole"><V name="cutoff" value="0.125"/><V name="resonance" value="0.1"/></Filter>
  </K2_Effect><K2_Effect index="1">
    <Filter type="hp2pole"><V name="cutoff" value="0.05"/><V name="resonance" value="0.2"/></Filter>
  </K2_Effect></GroupInsertFX></K2_Group></Groups>
  <Zones><K2_Zone groupIdx="0"><Sample><V name="file_ex2" value="note.wav"/></Sample></K2_Zone></Zones>
</K2_Container>"#;
        let (document, _) = parse(text).unwrap();
        assert_eq!(document.effects.program.len(), 2);
        assert!(matches!(
            document.effects.program[0],
            NkiProgramEffect::Reverb(NkiReverb {
                pre_delay_ms: 25.0,
                room_size: 0.75,
                ..
            })
        ));
        assert!(matches!(
            document.effects.program[1],
            NkiProgramEffect::Delay(NkiDelay {
                time_ms: 321.0,
                feedback: 0.4,
                ..
            })
        ));
        assert_eq!(
            document.effects.group_filters.get(&0),
            Some(&vec![
                NkiFilter::LowPass2 {
                    cutoff: 0.125,
                    resonance: 0.1,
                },
                NkiFilter::HighPass2 {
                    cutoff: 0.05,
                    resonance: 0.2,
                }
            ])
        );
    }

    /// A document of two groups, each with its own volume envelope, plus the
    /// other modulators a real instrument carries alongside them.
    ///
    /// Copied from the accordions rather than composed: `K2_IntMod` carries
    /// only an index and a version, and what kind of modulator it is appears
    /// on the envelope inside it. A fixture that put the kind on the
    /// modulator passed while the real files read as having no envelope.
    const TWO_GROUPS: &str = r#"<?xml version="1.0"?>
<K2_Container>
  <Groups>
    <K2_Group index="0">
      <IntModulators>
        <K2_IntMod index="0" version="0.80">
          <Targets><Target index="0"><V name="target" value="volume"/></Target></Targets>
          <Envelope type="ahdsr">
            <V name="attack" value="0."/><V name="hold" value="0."/>
            <V name="decay" value="9538."/><V name="sustain" value="0.94666701555252075"/>
            <V name="release" value="33."/>
          </Envelope>
        </K2_IntMod>
        <K2_IntMod index="0" version="0.80">
          <Targets><Target index="0"><V name="target" value="filterCutoff"/></Target></Targets>
          <Envelope type="ahdsr">
            <V name="attack" value="1."/><V name="decay" value="4350."/>
            <V name="sustain" value="0.43"/><V name="release" value="25000."/>
          </Envelope>
        </K2_IntMod>
      </IntModulators>
    </K2_Group>
    <K2_Group index="1">
      <IntModulators>
        <K2_IntMod index="0" version="0.80">
          <Targets><Target index="0"><V name="target" value="volume"/></Target></Targets>
          <Envelope type="ahdsr">
            <V name="attack" value="52."/><V name="decay" value="25000."/>
            <V name="sustain" value="0.949"/><V name="release" value="88."/>
          </Envelope>
        </K2_IntMod>
      </IntModulators>
    </K2_Group>
  </Groups>
  <Zones>
    <K2_Zone index="0" groupIdx="1">
      <Parameters><V name="lowKey" value="0"/><V name="highKey" value="127"/></Parameters>
      <Sample><V name="file_ex2" value="@F00000007000low.wav"/></Sample>
    </K2_Zone>
  </Zones>
</K2_Container>"#;

    #[test]
    fn the_volume_envelope_of_each_group_is_read() {
        let (document, _) = parse(TWO_GROUPS).unwrap();
        assert_eq!(document.groups.len(), 2);
        let first = document.groups[0].expect("the first group states an envelope");
        // Milliseconds in the document, seconds here.
        assert!((first.release_seconds - 0.033).abs() < 1e-6, "{first:?}");
        assert!((first.decay_seconds - 9.538).abs() < 1e-4, "{first:?}");
        assert!((first.sustain_level - 0.946_667).abs() < 1e-5, "{first:?}");
        let second = document.groups[1].expect("the second group states an envelope");
        assert!((second.release_seconds - 0.088).abs() < 1e-6, "{second:?}");
        assert!((second.attack_seconds - 0.052).abs() < 1e-6, "{second:?}");
    }

    #[test]
    fn a_modulator_driving_something_else_is_not_taken_for_the_envelope() {
        // The filter sweep in the first group releases over twenty-five
        // seconds. Reading it as the note's shape would leave every key
        // ringing long after it was let go.
        let (document, _) = parse(TWO_GROUPS).unwrap();
        let first = document.groups[0].unwrap();
        assert!(first.release_seconds < 1.0, "{first:?}");
    }

    #[test]
    fn a_zone_names_the_group_that_shapes_it() {
        let (document, _) = parse(TWO_GROUPS).unwrap();
        assert_eq!(document.zones[0].group, 1);
    }

    #[test]
    fn modulator_values_do_not_leak_into_the_zones_that_follow() {
        // Groups are written before zones and are not their parents, so a
        // reader that kept collecting after a modulator closed would read an
        // envelope's `attack` as though the zone had stated it.
        let (document, _) = parse(TWO_GROUPS).unwrap();
        assert_eq!(document.zones[0].sample, "low.wav");
        assert_eq!(document.zones[0].volume, 1.0);
        assert_eq!(document.zones[0].key_high, 127);
    }

    #[test]
    fn a_group_without_a_volume_envelope_states_none() {
        let text = r#"<?xml version="1.0"?>
<K2_Container>
  <Groups><K2_Group index="0"><IntModulators/></K2_Group></Groups>
  <Zones>
    <K2_Zone index="0" groupIdx="0">
      <Sample><V name="file_ex2" value="@F00000007000low.wav"/></Sample>
    </K2_Zone>
  </Zones>
</K2_Container>"#;
        let (document, _) = parse(text).unwrap();
        assert_eq!(document.groups, vec![None]);
    }

    #[test]
    fn one_unusable_zone_does_not_lose_the_others() {
        let text = r#"<K2_Container><Zones>
    <K2_Zone index="0"><Parameters><V name="lowKey" value="0"/></Parameters></K2_Zone>
    <K2_Zone index="1">
      <Parameters><V name="lowKey" value="60"/></Parameters>
      <Sample><V name="file_ex2" value="@F00000008000high.wav"/></Sample>
    </K2_Zone>
  </Zones></K2_Container>"#;
        let (document, skipped) = parse(text).unwrap();
        assert_eq!(document.zones.len(), 1);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn a_file_that_is_not_a_kontakt_instrument_is_refused() {
        let error = inflate(&[0_u8; 256]).unwrap_err();
        assert!(error.to_string().contains("magic"), "{error}");
    }

    #[test]
    fn a_truncated_file_is_refused() {
        assert!(inflate(&[0_u8; 8]).is_err());
    }

    /// Reads instruments the user supplies locally and reports their maps.
    ///
    /// No third-party instrument is committed here, for the same reason no ROM
    /// or sample is. The synthetic tests prove the rules; this proves they
    /// were the right ones against files nobody on this project wrote.
    ///
    /// ```text
    /// RF_SOUNDFONTS_NKI=/path/to/library cargo test -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires locally supplied Kontakt instruments"]
    fn reads_real_instruments() {
        let root = std::env::var("RF_SOUNDFONTS_NKI")
            .expect("set RF_SOUNDFONTS_NKI to a directory of .nki files");
        let mut found = 0;
        let mut walk = vec![std::path::PathBuf::from(root)];
        while let Some(directory) = walk.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            let mut paths: Vec<std::path::PathBuf> =
                entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    walk.push(path);
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "nki") {
                    continue;
                }
                found += 1;
                let bytes = std::fs::read(&path).unwrap();
                let text = inflate(&bytes).unwrap_or_else(|error| {
                    panic!("{}: {error}", path.display());
                });
                let (document, skipped) = parse(&text).unwrap_or_else(|error| {
                    panic!("{}: {error}", path.display());
                });

                let lowest = document
                    .zones
                    .iter()
                    .map(|zone| zone.key_low)
                    .min()
                    .unwrap();
                let highest = document
                    .zones
                    .iter()
                    .map(|zone| zone.key_high)
                    .max()
                    .unwrap();
                let looped = document
                    .zones
                    .iter()
                    .filter(|zone| zone.sample_loop.is_some())
                    .count();
                eprintln!(
                    "{:34} zones={:3} skipped={} keys={}..{} looped={}",
                    path.file_stem().unwrap().to_string_lossy(),
                    document.zones.len(),
                    skipped,
                    lowest,
                    highest,
                    looped
                );
                for zone in &document.zones {
                    assert!(
                        !zone.sample.contains('@'),
                        "path marker survived: {}",
                        zone.sample
                    );
                    assert!(zone.key_low <= zone.key_high);
                    assert!(zone.root_key <= 127);
                    assert!(zone.volume.is_finite() && zone.tune > 0.0);
                }
            }
        }
        assert!(found > 0, "no .nki files were found");
        eprintln!("instruments read: {found}");
    }

    #[test]
    fn an_encrypted_instrument_says_so_rather_than_failing_obscurely() {
        let mut bytes = vec![0_u8; 256];
        bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 4].copy_from_slice(KONTAKT_4);
        bytes[PAYLOAD_OFFSET] = 0x00;
        let error = inflate(&bytes).unwrap_err();
        assert!(error.to_string().contains("Player"), "{error}");
    }
}
