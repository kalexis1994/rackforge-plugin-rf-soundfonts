//! Building a playable instrument from a Kontakt zone map.
//!
//! Nothing here is new machinery. A Kontakt zone says which sample sounds for
//! which keys and velocities, where its root sits, how loud, how detuned and
//! where it loops — which is field for field what an SFZ region says, and what
//! the renderer already consumes. So this is a translation, and the parts that
//! took work for SFZ, from the sample cache to the disk reader, apply to these
//! instruments unchanged.
//!
//! The one thing that genuinely differs is finding the audio. An SFZ names its
//! samples relative to the document; a Kontakt instrument records the absolute
//! path of the machine that saved it, which will not exist anywhere else. Only
//! the file name survives that, so the file name is what is searched for.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::sample_store::SampleStore;
use crate::sfz::instrument::{CcState, SampledInstrument, SampledRegion};
use crate::{EnvelopeSpec, SoundfontError};

/// Directories descended into when looking for a zone's audio.
///
/// A Kontakt library keeps its samples in a folder beside the instrument,
/// conventionally named after it. Two levels reach that folder without walking
/// into whatever else a collection happens to contain.
const SEARCH_DEPTH: usize = 2;

/// Reads a `.nki` and everything it needs to sound.
pub fn open(path: impl AsRef<Path>) -> Result<(SampledInstrument, Vec<String>), SoundfontError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|source| SoundfontError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let text = super::document::inflate(&bytes)?;
    let (document, skipped_zones) = super::document::parse(&text)?;

    let root = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut reports = Vec::new();
    if skipped_zones > 0 {
        reports.push(format!("{name}: {skipped_zones} zones had no usable mapping"));
    }

    // Every audio file within reach, indexed by name folded to one case.
    // Kontakt records the case its authoring machine used, which on Windows
    // means the recorded name and the name on a Linux disk often differ.
    let available = index_audio_files(root);

    let mut relatives: Vec<String> = Vec::new();
    let mut indices: BTreeMap<String, usize> = BTreeMap::new();
    let mut placements: Vec<(usize, &super::document::NkiZone)> = Vec::new();
    for zone in &document.zones {
        let key = zone.sample.to_ascii_lowercase();
        let Some(relative) = available.get(&key) else {
            reports.push(format!("{name}: sample {:?} was not found", zone.sample));
            continue;
        };
        let index = *indices.entry(relative.clone()).or_insert_with(|| {
            relatives.push(relative.clone());
            relatives.len() - 1
        });
        placements.push((index, zone));
    }
    if placements.is_empty() {
        return Err(SoundfontError::Invalid(format!(
            "Kontakt instrument {name:?} found none of its samples"
        )));
    }

    // A looped sample has to be held whole. The store works that out for itself
    // when the audio carries a `smpl` chunk, but a Kontakt zone states its loop
    // in the instrument document, so nothing in the file itself gives the store
    // any reason to keep the tail: it would preload the head and playback would
    // jump back to a loop that is not in memory.
    let needs_residency: BTreeSet<String> = placements
        .iter()
        .filter(|(_, zone)| zone.sample_loop.is_some())
        .map(|(index, _)| relatives[*index].clone())
        .collect();

    let store = SampleStore::beside(root);
    let samples = store.load_all(root, "", &relatives, &needs_residency)?;

    let regions = placements
        .into_iter()
        .map(|(index, zone)| region_from(zone, index, &samples[index]))
        .collect();

    let mut instrument = SampledInstrument {
        name,
        samples,
        regions,
        curves: BTreeMap::new(),
        // A Kontakt map holds no controller defaults of its own; the plugin's
        // live controller state applies unchanged.
        defaults: CcState::default(),
        normalisation: 1.0,
    };
    instrument.renormalise();
    Ok((instrument, reports))
}

fn region_from(
    zone: &super::document::NkiZone,
    wave_index: usize,
    sample: &crate::sample_store::StreamedSample,
) -> SampledRegion {
    SampledRegion {
        key_low: zone.key_low,
        key_high: zone.key_high,
        velocity_low: zone.velocity_low,
        velocity_high: zone.velocity_high,
        pitch_keycenter: zone.root_key,
        wave_index,
        // Kontakt states tuning as a frequency ratio; the renderer wants cents.
        tune_cents: 1_200.0 * zone.tune.max(f32::MIN_POSITIVE).log2(),
        // And level as a linear factor, which the renderer wants in decibels.
        volume_db: 20.0 * zone.volume.max(1e-4).log10(),
        pan: zone.pan,
        // Kontakt applies velocity through its modulators rather than as a
        // property of the zone, and this reader does not take those. Full
        // tracking matches what an unmodulated instrument sounds like.
        amp_veltrack: 1.0,
        group: 0,
        off_by: None,
        note_polyphony: None,
        off_time: 0.005,
        envelope: EnvelopeSpec::default(),
        // A loop reaching past the audio is dropped rather than trusted: the
        // recorded length belongs to the file the author had, and a converted
        // or replaced sample may be shorter. It is also dropped if the sample
        // did not end up resident after all — a library large enough to exhaust
        // the residency budget should play its samples through once rather than
        // reach backwards into a tail nobody kept.
        sample_loop: zone.sample_loop.filter(|looping| {
            looping.start < looping.end
                && looping.end <= sample.frame_count
                && sample.is_fully_resident()
        }),
        gates: Vec::new(),
        amplitude_cc: Vec::new(),
        pan_cc: Vec::new(),
        veltrack_cc: Vec::new(),
        release_cc: Vec::new(),
    }
}

/// Maps every audio file under `root` from its folded name to its path.
///
/// Later entries do not displace earlier ones, so a shallower file wins over a
/// deeper duplicate: a library that keeps both a working copy and an archived
/// one should play the copy sitting next to the instrument.
fn index_audio_files(root: &Path) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let mut level = vec![root.to_path_buf()];
    for _ in 0..=SEARCH_DEPTH {
        let mut next = Vec::new();
        for directory in level {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    // The engine's own cache holds no source audio.
                    if !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                    {
                        next.push(path);
                    }
                    continue;
                }
                let is_audio = path.extension().is_some_and(|extension| {
                    let extension = extension.to_string_lossy().to_ascii_lowercase();
                    matches!(extension.as_str(), "wav" | "wave" | "flac")
                });
                if !is_audio {
                    continue;
                }
                let Some(name) = path.file_name().map(|name| name.to_string_lossy().into_owned())
                else {
                    continue;
                };
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                found
                    .entry(name.to_ascii_lowercase())
                    .or_insert_with(|| relative.to_string_lossy().replace('\\', "/"));
            }
        }
        level = next;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nki::document::NkiZone;

    fn zone(volume: f32, tune: f32) -> NkiZone {
        NkiZone {
            sample: "note.wav".into(),
            key_low: 48,
            key_high: 59,
            root_key: 55,
            velocity_low: 1,
            velocity_high: 127,
            sample_start: 0,
            volume,
            pan: 0.0,
            tune,
            sample_loop: None,
        }
    }

    /// A sample of `frames`, of which `resident` are in memory.
    fn sample(frames: usize, resident: usize) -> crate::sample_store::StreamedSample {
        crate::sample_store::StreamedSample {
            name: "note.wav".into(),
            sample_rate: 44_100,
            channels: 1,
            frame_count: frames,
            preload: vec![0.0; resident].into(),
            preload_frames: resident,
            cache_path: PathBuf::new(),
            header: crate::pcm_cache::CacheHeader {
                sample_loop: None,
                sample_rate: 44_100,
                channels: 1,
                format: crate::pcm_cache::CacheFormat::Int16,
                frame_count: frames,
            },
        }
    }

    /// A sample held whole, which is what most of these tests want.
    fn whole(frames: usize) -> crate::sample_store::StreamedSample {
        sample(frames, frames)
    }

    #[test]
    fn unity_volume_and_tuning_translate_to_no_change() {
        let region = region_from(&zone(1.0, 1.0), 0, &whole(1_000));
        assert!(region.volume_db.abs() < 1e-3, "{}", region.volume_db);
        assert!(region.tune_cents.abs() < 1e-3, "{}", region.tune_cents);
    }

    #[test]
    fn a_halved_level_becomes_six_decibels_down() {
        let region = region_from(&zone(0.5, 1.0), 0, &whole(1_000));
        assert!((region.volume_db + 6.02).abs() < 0.05, "{}", region.volume_db);
    }

    #[test]
    fn a_doubled_ratio_becomes_an_octave_of_cents() {
        let region = region_from(&zone(1.0, 2.0), 0, &whole(1_000));
        assert!((region.tune_cents - 1_200.0).abs() < 0.1, "{}", region.tune_cents);
    }

    #[test]
    fn a_silent_zone_does_not_become_negative_infinity() {
        let region = region_from(&zone(0.0, 1.0), 0, &whole(1_000));
        assert!(region.volume_db.is_finite(), "{}", region.volume_db);
    }

    #[test]
    fn placement_survives_the_translation() {
        let region = region_from(&zone(1.0, 1.0), 3, &whole(1_000));
        assert_eq!((region.key_low, region.key_high), (48, 59));
        assert_eq!(region.pitch_keycenter, 55);
        assert_eq!(region.wave_index, 3);
    }

    #[test]
    fn a_loop_past_the_end_of_the_audio_is_dropped() {
        let mut source = zone(1.0, 1.0);
        source.sample_loop = Some(crate::SampleLoop {
            start: 10,
            end: 9_999,
        });
        assert!(region_from(&source, 0, &whole(1_000)).sample_loop.is_none());
    }

    #[test]
    fn a_loop_inside_the_audio_is_kept() {
        let mut source = zone(1.0, 1.0);
        source.sample_loop = Some(crate::SampleLoop {
            start: 100,
            end: 900,
        });
        assert!(region_from(&source, 0, &whole(1_000)).sample_loop.is_some());
    }

    #[test]
    fn a_loop_beyond_what_is_resident_is_dropped() {
        // The loop is well inside the audio, so the length check passes, but
        // only the head is in memory and a voice cannot jump back into a tail
        // it would have to read forwards to reach.
        let mut source = zone(1.0, 1.0);
        source.sample_loop = Some(crate::SampleLoop {
            start: 40_609,
            end: 55_489,
        });
        let streamed = sample(60_000, crate::sample_store::PRELOAD_FRAMES);
        assert!(region_from(&source, 0, &streamed).sample_loop.is_none());
    }

    #[test]
    fn samples_are_found_regardless_of_recorded_case() {
        // Kontakt records the case its authoring machine used, and Windows did
        // not care. A Linux disk does.
        let root = std::env::temp_dir().join(format!("rf-nki-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Accordion Samples")).unwrap();
        std::fs::write(root.join("Accordion Samples/ACORDEON REAL.wav"), []).unwrap();

        let index = index_audio_files(&root);
        assert_eq!(
            index.get("acordeon real.wav").map(String::as_str),
            Some("Accordion Samples/ACORDEON REAL.wav")
        );
    }

    #[test]
    fn the_engine_cache_is_not_searched_for_source_audio() {
        let root = std::env::temp_dir().join(format!("rf-nki-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".rf-soundfonts-cache")).unwrap();
        std::fs::write(root.join(".rf-soundfonts-cache/note.wav"), []).unwrap();
        assert!(index_audio_files(&root).is_empty());
    }

    #[test]
    fn a_shallower_copy_wins_over_a_deeper_duplicate() {
        let root = std::env::temp_dir().join(format!("rf-nki-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Samples/Archive")).unwrap();
        std::fs::write(root.join("Samples/note.wav"), []).unwrap();
        std::fs::write(root.join("Samples/Archive/note.wav"), []).unwrap();
        assert_eq!(
            index_audio_files(&root).get("note.wav").map(String::as_str),
            Some("Samples/note.wav")
        );
    }

    /// Loads instruments the user supplies locally and plays one.
    ///
    /// ```text
    /// RF_SOUNDFONTS_NKI=/path/to/library cargo test --release -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires locally supplied Kontakt instruments"]
    fn loads_and_plays_real_instruments() {
        use crate::streamer::Streamer;

        let root = std::env::var("RF_SOUNDFONTS_NKI")
            .expect("set RF_SOUNDFONTS_NKI to a directory of .nki files");
        let mut walk = vec![PathBuf::from(root)];
        let mut played = 0;
        let streamer = Streamer::start();

        while let Some(directory) = walk.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    walk.push(path);
                    continue;
                }
                if path.extension().is_none_or(|extension| extension != "nki") {
                    continue;
                }
                let (instrument, reports) = match open(&path) {
                    Ok(loaded) => loaded,
                    Err(error) => panic!("{}: {error}", path.display()),
                };
                for report in &reports {
                    eprintln!("   aviso: {report}");
                }

                // A note in the middle of whatever the instrument covers.
                let lowest = instrument.regions.iter().map(|r| r.key_low).min().unwrap();
                let highest = instrument.regions.iter().map(|r| r.key_high).max().unwrap();
                let note = ((u16::from(lowest) + u16::from(highest)) / 2) as u8;

                let mut voices = instrument
                    .voices_for_note(note, 100, &instrument.defaults, 48_000, &streamer)
                    .unwrap();
                std::thread::sleep(std::time::Duration::from_millis(80));
                let mut peak = 0.0_f32;
                for _ in 0..48_000 {
                    let mut left = 0.0;
                    for voice in &mut voices {
                        left += voice.next_frame()[0];
                    }
                    peak = peak.max(left.abs());
                }
                let starved: usize = voices.iter().map(crate::Voice::starved_frames).sum();
                eprintln!(
                    "{:34} regiones={:3}  {} MiB  nota {note} -> {} voces, pico {peak:.4}, hambre {starved}",
                    instrument.name,
                    instrument.regions.len(),
                    instrument.resident_bytes() / 1_048_576,
                    voices.len(),
                );
                assert!(!voices.is_empty(), "{} played nothing", instrument.name);
                assert!(peak > 1e-4, "{} rendered silence", instrument.name);
                assert_eq!(starved, 0, "{} starved its reader", instrument.name);
                played += 1;
            }
        }
        assert!(played > 0, "no .nki files were found");
        eprintln!("instrumentos tocados: {played}");
    }
}
