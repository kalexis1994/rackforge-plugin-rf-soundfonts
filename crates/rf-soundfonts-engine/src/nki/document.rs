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
    pub zones: Vec<NkiZone>,
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

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                depth += 1;
                if element.name().as_ref() == b"K2_Zone" {
                    current = Some(BTreeMap::new());
                    depth_of_zone = depth;
                }
            }
            Ok(Event::Empty(element)) => {
                if element.name().as_ref() != b"V" {
                    continue;
                }
                let Some(values) = current.as_mut() else {
                    continue;
                };
                let mut name = None;
                let mut value = None;
                // `unescape_value` is deprecated in favour of an API that
                // needs an entity resolver this document does not use. Its
                // behaviour — resolving the five predefined XML entities — is
                // exactly what a file name containing an ampersand needs.
                #[allow(deprecated)]
                for attribute in element.attributes().flatten() {
                    match attribute.key.as_ref() {
                        b"name" => {
                            name = attribute.unescape_value().ok().map(|text| text.into_owned())
                        }
                        b"value" => {
                            value = attribute.unescape_value().ok().map(|text| text.into_owned())
                        }
                        _ => {}
                    }
                }
                if let (Some(name), Some(value)) = (name, value) {
                    // First writer wins. A zone's own parameters appear before
                    // the nested modulator tables that reuse names like
                    // `volume`, and taking the last would read a modulation
                    // depth as the zone's level.
                    values.entry(name).or_insert(value);
                }
            }
            Ok(Event::End(element)) => {
                if element.name().as_ref() == b"K2_Zone" && depth == depth_of_zone
                    && let Some(values) = current.take() {
                        match zone_from(&values) {
                            Some(zone) => document.zones.push(zone),
                            None => skipped += 1,
                        }
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
    Ok((document, skipped))
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

    Some(NkiZone {
        sample,
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
        tune: number("zoneTune").filter(|ratio| *ratio > 0.0).unwrap_or(1.0),
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
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Segment names may contain slashes on neither platform, so the last of
    // either separator is a safe boundary for a plainly written path too.
    let tail = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed);
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

                let lowest = document.zones.iter().map(|zone| zone.key_low).min().unwrap();
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
                    "{:34} zonas={:3} saltadas={} teclas={}..{} looped={}",
                    path.file_stem().unwrap().to_string_lossy(),
                    document.zones.len(),
                    skipped,
                    lowest,
                    highest,
                    looped
                );
                for zone in &document.zones {
                    assert!(!zone.sample.contains('@'), "path marker survived: {}", zone.sample);
                    assert!(zone.key_low <= zone.key_high);
                    assert!(zone.root_key <= 127);
                    assert!(zone.volume.is_finite() && zone.tune > 0.0);
                }
            }
        }
        assert!(found > 0, "no .nki files were found");
        eprintln!("instrumentos leídos: {found}");
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
