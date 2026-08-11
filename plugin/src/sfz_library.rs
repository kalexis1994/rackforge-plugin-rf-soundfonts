//! The instruments installed beside the plugin.
//!
//! Streaming changed what this layer can afford. A resident instrument used to
//! cost 1.7 GiB, which forced a choice between holding one and swapping on
//! every sound change — and a swap costs half a minute, which is not a thing
//! that can happen between two songs. At 75 MiB an instrument, holding all of
//! them is cheaper than the machinery to swap them, so that is what happens:
//! everything loads once at start-up and switching sounds only changes which
//! instrument receives the notes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rf_soundfonts::Voice;
use rf_soundfonts::sfz::instrument::{CcState, SampledInstrument};
use rf_soundfonts::streamer::Streamer;

/// Extensions identifying an instrument definition.
///
/// Both describe the same thing — a keyboard map over a folder of samples —
/// and both build the same instrument, so a player installing a library does
/// not need to know which kind it is.
const INSTRUMENT_EXTENSIONS: [&str; 2] = ["sfz", "nki"];

/// Instruments discovered in the library directory, all resident.
pub struct SfzLibrary {
    instruments: Vec<LoadedInstrument>,
    streamer: Streamer,
}

pub struct LoadedInstrument {
    /// Stable identifier, qualified by library, used by presets and state.
    pub id: String,
    pub name: String,
    /// Library this instrument came from, which groups it in the catalog.
    pub library: String,
    pub instrument: SampledInstrument,
    /// Live controller values, seeded from the document's own defaults.
    pub controllers: CcState,
}

impl SfzLibrary {
    /// Loads the instruments installed under `root`.
    ///
    /// Two levels are searched and no more. A library arrives as a folder
    /// containing its definitions and a samples directory, so the instruments
    /// sit one level down; searching deeper would start opening whatever is
    /// inside the samples folder, and searching only the top level would find
    /// nothing at all in the layout libraries actually ship in.
    ///
    /// One instrument failing does not fail the rest. A library with a broken
    /// file should still offer the ones that work, and the failure is reported
    /// rather than swallowed.
    pub fn load(root: &Path) -> (Self, Vec<String>) {
        let mut failures = Vec::new();
        let mut found: Vec<(String, PathBuf)> = Vec::new();

        // Loose definitions at the top level belong to no folder, so they are
        // grouped under the library root's own name.
        let root_label = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Library".to_string());
        collect(root, &root_label, &mut found);

        match fs::read_dir(root) {
            Ok(entries) => {
                let mut directories: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect();
                directories.sort();
                for directory in directories {
                    let label = directory
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // Skip our own derived data rather than reporting it.
                    if label.starts_with('.') {
                        continue;
                    }
                    collect(&directory, &label, &mut found);
                }
            }
            Err(error) => failures.push(format!("{}: {error}", root.display())),
        }

        let mut instruments = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (library, path) in found {
            // A Kontakt map reports what it could not use — a zone with no
            // mapping, a sample that is not where the author left it — while
            // still playing everything else. Those are carried alongside the
            // outright failures so the player sees them, because an instrument
            // missing a handful of notes is easy to miss on stage.
            let opened = if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("nki"))
            {
                rf_soundfonts::nki::instrument::open(&path).map(|(instrument, reports)| {
                    failures.extend(reports);
                    instrument
                })
            } else {
                SampledInstrument::open(&path)
            };
            match opened {
                Ok(instrument) => {
                    let name = instrument.name.clone();
                    let id = unique_id(&format!("{library} {name}"), &mut seen);
                    let controllers = instrument.defaults.clone();
                    instruments.push(LoadedInstrument {
                        id,
                        name,
                        library,
                        instrument,
                        controllers,
                    });
                }
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }
        (
            Self {
                instruments,
                streamer: Streamer::start(),
            },
            failures,
        )
    }

    /// Library names in catalog order, each appearing once.
    pub fn libraries(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for loaded in &self.instruments {
            if !names.contains(&loaded.library.as_str()) {
                names.push(&loaded.library);
            }
        }
        names
    }

    pub fn is_empty(&self) -> bool {
        self.instruments.is_empty()
    }

    pub fn instruments(&self) -> &[LoadedInstrument] {
        &self.instruments
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.instruments.iter().position(|loaded| loaded.id == id)
    }

    /// Total memory the loaded instruments hold resident.
    pub fn resident_bytes(&self) -> usize {
        self.instruments
            .iter()
            .map(|loaded| loaded.instrument.resident_bytes())
            .sum()
    }

    /// Records a control change for every instrument.
    ///
    /// Applied to all of them, not only the selected one: an instrument that
    /// missed the pedal going down while another was playing would resolve its
    /// next note against a damper the player already pressed.
    pub fn set_controller(&mut self, controller: u8, value: u8) {
        for loaded in &mut self.instruments {
            loaded.controllers.set_midi(controller, value);
        }
    }

    /// Seconds a displaced voice of one instrument should take to fade.
    pub fn off_time(&self, index: usize) -> f32 {
        self.instruments
            .get(index)
            .map_or(0.005, |loaded| loaded.instrument.off_time())
    }

    /// Builds the voices one instrument should start for a key press.
    pub fn voices_for_note(&self, index: usize, note: u8, velocity: u8, rate: u32) -> Vec<Voice> {
        let Some(loaded) = self.instruments.get(index) else {
            return Vec::new();
        };
        loaded
            .instrument
            .voices_for_note(note, velocity, &loaded.controllers, rate, &self.streamer)
            .unwrap_or_default()
    }

    /// Streams currently in use. Surfaced for diagnosis from a host that
    /// wants it; nothing in the audio path depends on it.
    #[allow(dead_code)]
    pub fn active_streams(&self) -> usize {
        self.streamer.active_streams()
    }
}

/// Gathers the `.sfz` files directly inside one directory.
///
/// Sorted, because directory order is not stable across filesystems and
/// identifiers derived from position would move under a saved performance.
fn collect(directory: &Path, library: &str, found: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(directory) else {
        // Unreadable directories are silent here. The root's failure is
        // reported by the caller, and a subdirectory that vanished between
        // being listed and being opened is not worth a message.
        return;
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_instrument = path.extension().is_some_and(|extension| {
            INSTRUMENT_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
        if is_instrument && path.is_file() {
            paths.push(path);
        }
    }

    paths.sort();
    found.extend(paths.into_iter().map(|path| (library.to_string(), path)));
}

/// Derives a stable identifier, disambiguating repeats.
///
/// Two files can reduce to the same slug — `Piano.sfz` and `piano.sfz`, or two
/// mixes named alike. A collision would make one instrument unreachable and
/// silently redirect a saved preset to the other.
fn unique_id(name: &str, seen: &mut BTreeMap<String, usize>) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut separator = false;
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(char::from(byte.to_ascii_lowercase()));
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        slug.push_str("instrument");
    }
    let count = seen.entry(slug.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        slug
    } else {
        format!("{slug}-{count}")
    }
}

/// Preset identifier for an instrument, as published to the host.
///
/// Kept distinguishable from the DLS identifier without naming the file
/// format in anything a player reads: the prefix is a namespace, and
/// the display name comes from the instrument itself.
pub fn preset_id(instrument_id: &str) -> String {
    format!("sfz.{instrument_id}")
}

/// Recovers the instrument identifier from a preset identifier.
pub fn instrument_id(preset: &str) -> Option<&str> {
    preset.strip_prefix("sfz.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_identifier_round_trips() {
        let id = preset_id("headroom-piano-nofx");
        assert_eq!(instrument_id(&id), Some("headroom-piano-nofx"));
    }

    #[test]
    fn a_dls_preset_is_not_mistaken_for_an_sfz_one() {
        assert_eq!(instrument_id("dls.b00000000.p00000000"), None);
    }

    #[test]
    fn a_name_becomes_a_stable_slug() {
        let mut seen = BTreeMap::new();
        assert_eq!(
            unique_id("Headroom Piano (NoFX)", &mut seen),
            "headroom-piano-nofx"
        );
    }

    #[test]
    fn colliding_names_stay_distinguishable() {
        let mut seen = BTreeMap::new();
        assert_eq!(unique_id("Piano", &mut seen), "piano");
        assert_eq!(unique_id("piano", &mut seen), "piano-2");
        assert_eq!(unique_id("PIANO!", &mut seen), "piano-3");
    }

    #[test]
    fn a_nameless_file_still_gets_an_identifier() {
        let mut seen = BTreeMap::new();
        assert_eq!(unique_id("", &mut seen), "instrument");
        assert_eq!(unique_id("...", &mut seen), "instrument-2");
    }

    #[test]
    fn a_missing_directory_reports_instead_of_panicking() {
        let (library, failures) = SfzLibrary::load(Path::new("/definitely/not/here"));
        assert!(library.is_empty());
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn a_directory_without_instruments_loads_empty() {
        let root = std::env::temp_dir().join(format!("rf-soundfonts-lib-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("readme.txt"), "not an instrument").unwrap();
        let (library, failures) = SfzLibrary::load(&root);
        assert!(library.is_empty());
        assert!(
            failures.is_empty(),
            "a stray text file was treated as an error"
        );
    }

    /// Writes an instrument that loads: one region over one real sample.
    fn install(directory: &Path, name: &str) {
        fs::create_dir_all(directory.join("Samples")).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::create(directory.join("Samples/tone.wav"), spec).unwrap();
        for index in 0..256 {
            writer.write_sample((index as i16) * 64).unwrap();
        }
        writer.finalize().unwrap();
        fs::write(
            directory.join(format!("{name}.sfz")),
            "<control> default_path=Samples/\n<region> sample=tone.wav key=60",
        )
        .unwrap();
    }

    #[test]
    fn instruments_are_found_inside_installed_library_folders() {
        // The layout libraries actually ship in: a folder per library, with
        // definitions beside a samples directory. Searching only the top level
        // would find nothing here.
        let root =
            std::env::temp_dir().join(format!("rf-soundfonts-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        install(&root.join("HeadroomPiano"), "Headroom Piano");
        install(&root.join("HeadroomPiano"), "Intimate Piano");
        install(&root.join("VSCO2"), "Strings");

        let (library, failures) = SfzLibrary::load(&root);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(library.instruments().len(), 3);
        assert_eq!(library.libraries(), vec!["HeadroomPiano", "VSCO2"]);
    }

    #[test]
    fn two_libraries_may_hold_instruments_of_the_same_name() {
        let root =
            std::env::temp_dir().join(format!("rf-soundfonts-samename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        install(&root.join("LibraryA"), "Piano");
        install(&root.join("LibraryB"), "Piano");

        let (library, _) = SfzLibrary::load(&root);
        let ids: Vec<&str> = library
            .instruments()
            .iter()
            .map(|loaded| loaded.id.as_str())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1], "a saved preset would reach the wrong piano");
    }

    #[test]
    fn the_cache_directory_is_not_mistaken_for_a_library() {
        let root =
            std::env::temp_dir().join(format!("rf-soundfonts-cachedir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        install(&root.join("Real"), "Piano");
        fs::create_dir_all(root.join(".rf-soundfonts-cache")).unwrap();

        let (library, failures) = SfzLibrary::load(&root);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(library.libraries(), vec!["Real"]);
    }

    #[test]
    fn a_kontakt_instrument_is_discovered_alongside_an_sfz() {
        // Only discovery is checked here: a real `.nki` needs its samples, and
        // those live on the Pi. What matters is that the file is picked up and
        // handed to the Kontakt reader, which the error proves — an unreadable
        // header is a different complaint than never having been looked at.
        let root =
            std::env::temp_dir().join(format!("rf-soundfonts-lib-nki-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("map.nki"), b"not a kontakt file at all").unwrap();
        let (_, failures) = SfzLibrary::load(&root);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("Kontakt"), "{:?}", failures);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn one_broken_instrument_does_not_hide_the_others() {
        let root =
            std::env::temp_dir().join(format!("rf-soundfonts-lib-mixed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("broken.sfz"), "<region> sample=missing.wav").unwrap();
        let (library, failures) = SfzLibrary::load(&root);
        assert!(library.is_empty());
        assert_eq!(failures.len(), 1, "the failure was not reported");
        assert!(failures[0].contains("broken.sfz"), "{:?}", failures);
    }
}

#[cfg(test)]
mod real_library {
    use super::*;

    /// Reports what every installed instrument says about itself.
    ///
    /// ```text
    /// RF_SOUNDFONTS_ROOT=/path/to/sfz cargo test -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires an installed library root"]
    fn describes_every_installed_instrument() {
        let root = std::env::var("RF_SOUNDFONTS_ROOT")
            .expect("set RF_SOUNDFONTS_ROOT to the installed sfz directory");
        let (library, failures) = SfzLibrary::load(std::path::Path::new(&root));
        for failure in &failures {
            eprintln!("  aviso: {failure}");
        }
        let mut current = String::new();
        for loaded in library.instruments() {
            if loaded.library != current {
                current = loaded.library.clone();
                eprintln!("{current}");
            }
            let summary = loaded.instrument.summary();
            eprintln!(
                "   {:26} grabado {:>3}-{:<3} alcanza {:>3}-{:<3} {:>4} zonas {:>4} samples {:>2} capas {:>4} MiB{}",
                loaded.name,
                summary.root_low,
                summary.root_high,
                summary.key_low,
                summary.key_high,
                summary.regions,
                summary.samples,
                summary.velocity_layers,
                summary.resident_bytes / 1_048_576,
                if summary.looping { " loop" } else { "" },
            );
            assert!(summary.key_low <= summary.key_high, "{}", loaded.name);
            assert!(summary.regions > 0, "{} has no regions", loaded.name);
        }
        assert!(!library.is_empty());
    }
}
