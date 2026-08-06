//! Samples held as a resident head plus a seekable tail on disk.
//!
//! Loading a library whole costs what the Headroom Piano costs: 1669 MiB of
//! resident memory and 27 seconds of start-up, of which 23 were spent
//! decoding FLAC on one core. Neither number is about how much audio the
//! instrument actually plays — a chord uses ten samples out of three hundred.
//!
//! So only the head of each sample stays in memory, enough for a note to
//! start and keep sounding while a reader fetches the rest. The size follows
//! LinuxSampler's `CONFIG_PRELOAD_SAMPLES`: 32768 frames, about three quarters
//! of a second, chosen there to cover disk latency plus the time to fill the
//! first buffer rather than just the first access.
//!
//! The tail lives in the PCM cache, which is written once and reused, so the
//! cost of decoding a compressed library is paid on first sight of it and
//! never again.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use crate::pcm_cache::{self, CacheHeader};
use crate::{SoundfontError, sample};

/// Frames of every sample kept resident, following LinuxSampler's default.
pub const PRELOAD_FRAMES: usize = 32_768;

/// Largest looped sample held whole rather than streamed.
///
/// A loop needs the audio in memory because playback moves backwards through
/// it. This bounds what that can cost: a sample beyond the limit is streamed
/// without its loop, which shortens the note but cannot exhaust the machine.
pub const MAX_LOOPED_RESIDENT_BYTES: usize = 32 * 1_048_576;

/// One sample: a resident head and a path to the rest.
#[derive(Clone, Debug)]
pub struct StreamedSample {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_count: usize,
    /// The first [`PRELOAD_FRAMES`] frames, interleaved.
    pub preload: Arc<[f32]>,
    /// Frames actually held in [`StreamedSample::preload`].
    pub preload_frames: usize,
    pub cache_path: PathBuf,
    pub header: CacheHeader,
}

impl StreamedSample {
    /// Whether the whole sample is resident and no reader is ever needed.
    ///
    /// True of most short material — drum hits, release noises, key clicks —
    /// which is worth knowing because such a voice can skip the streaming
    /// machinery entirely.
    pub fn is_fully_resident(&self) -> bool {
        self.preload_frames >= self.frame_count
    }

    /// Bytes of resident memory this sample occupies.
    pub fn resident_bytes(&self) -> usize {
        self.preload.len() * size_of::<f32>()
    }
}

/// Loads and caches the samples an instrument references.
pub struct SampleStore {
    cache_root: PathBuf,
}

impl SampleStore {
    /// Caches beside the library, in a directory that can be deleted wholesale.
    pub fn new(cache_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
        }
    }

    /// Default cache location for a library.
    pub fn beside(library_root: &Path) -> Self {
        Self::new(library_root.join(".rf-soundfonts-cache"))
    }

    /// Loads every distinct sample, transcoding on all available cores.
    ///
    /// Transcoding is the one-time cost and it is the expensive one, so it is
    /// the part worth parallelising: the files are independent and a Pi has
    /// four cores sitting idle while an instrument loads. Reading a head back
    /// from an existing cache is cheap enough to stay sequential.
    pub fn load_all(
        &self,
        library_root: &Path,
        default_path: &str,
        relatives: &[String],
        resident: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<StreamedSample>, SoundfontError> {
        fs::create_dir_all(&self.cache_root).map_err(|source| SoundfontError::Read {
            path: self.cache_root.display().to_string(),
            source,
        })?;

        let workers = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(relatives.len().max(1));
        if workers <= 1 {
            return relatives
                .iter()
                .map(|relative| self.load_one(library_root, default_path, relative, resident.contains(relative)))
                .collect();
        }

        // Indices are handed out round-robin and results carry their index
        // back, so the returned order matches the requested order however the
        // threads interleave.
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            for worker in 0..workers {
                let sender = sender.clone();
                scope.spawn(move || {
                    for index in (worker..relatives.len()).step_by(workers) {
                        let loaded =
                            self.load_one(
                            library_root,
                            default_path,
                            &relatives[index],
                            resident.contains(&relatives[index]),
                        );
                        // A closed receiver means another worker already
                        // failed and the load is being abandoned.
                        if sender.send((index, loaded)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(sender);

            let mut slots: Vec<Option<StreamedSample>> = vec![None; relatives.len()];
            for (index, loaded) in receiver {
                slots[index] = Some(loaded?);
            }
            slots
                .into_iter()
                .enumerate()
                .map(|(index, slot)| {
                    slot.ok_or_else(|| {
                        SoundfontError::Invalid(format!(
                            "sample {:?} was never loaded",
                            relatives[index]
                        ))
                    })
                })
                .collect()
        })
    }

    /// Loads one sample, transcoding it first if the cache is missing or stale.
    ///
    /// `force_resident` holds the whole sample in memory even when the file
    /// declares no loop of its own. A Kontakt zone states its loop in the
    /// instrument document rather than in the audio, so the file cannot be
    /// asked whether playback will move backwards through it.
    pub fn load_one(
        &self,
        library_root: &Path,
        default_path: &str,
        relative: &str,
        force_resident: bool,
    ) -> Result<StreamedSample, SoundfontError> {
        let source = sample::resolve(library_root, default_path, relative);
        let cache_path = pcm_cache::cache_path(&self.cache_root, relative);

        let header = match self.reusable_cache(&cache_path, &source) {
            Some(header) => header,
            None => {
                let wave = sample::load(&source)?;
                // Stored at whatever width the source used, so the cache can
                // never be the reason an instrument loses resolution.
                let format = pcm_cache::format_for(wave.source_bits);
                pcm_cache::write(&cache_path, &wave, format)?
            }
        };

        let mut file = File::open(&cache_path).map_err(|source| SoundfontError::Read {
            path: cache_path.display().to_string(),
            source,
        })?;
        let header = pcm_cache::read_header(&mut file).unwrap_or(header);
        let channels = usize::from(header.channels).max(1);
        // A looped sample is held whole. The loop of a converted library sits
        // at the very end — the Rhodes measured here loops the last 69 ms of a
        // seven-second recording — so playback jumps backwards there, and the
        // streaming window only moves forward. Holding the sample costs memory
        // that looped libraries do not have much of: they loop precisely
        // because they are built from short recordings.
        let resident_whole = (force_resident || header.sample_loop.is_some())
            && header.frame_count * channels * size_of::<f32>() <= MAX_LOOPED_RESIDENT_BYTES;
        let preload_frames = if resident_whole {
            header.frame_count
        } else {
            PRELOAD_FRAMES.min(header.frame_count)
        };
        let mut preload = vec![0.0_f32; preload_frames * channels];
        let mut scratch = Vec::new();
        pcm_cache::read_frames(&file, &header, 0, &mut preload, &mut scratch)?;

        Ok(StreamedSample {
            name: relative.to_string(),
            sample_rate: header.sample_rate,
            channels: header.channels,
            frame_count: header.frame_count,
            preload: Arc::from(preload),
            preload_frames,
            cache_path,
            header,
        })
    }

    /// Whether an existing cache can be trusted for this source.
    ///
    /// A cache older than its source is stale: the library was replaced or
    /// re-exported underneath it. Rebuilding is cheap relative to playing the
    /// wrong audio for a whole performance.
    fn reusable_cache(&self, cache_path: &Path, source: &Path) -> Option<CacheHeader> {
        let cache_time = fs::metadata(cache_path).ok()?.modified().ok()?;
        if let Ok(source_time) = fs::metadata(source).and_then(|meta| meta.modified())
            && source_time > cache_time
        {
            return None;
        }
        let mut file = File::open(cache_path).ok()?;
        pcm_cache::read_header(&mut file).ok()
    }

    /// Removes the cache directory.
    pub fn clear(&self) -> Result<(), SoundfontError> {
        match fs::remove_dir_all(&self.cache_root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SoundfontError::Read {
                path: self.cache_root.display().to_string(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rf-soundfonts-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    /// Writes a mono 16-bit WAV of `frames` ascending samples.
    fn write_source(root: &Path, name: &str, frames: usize) -> String {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(root.join(name), spec).unwrap();
        for index in 0..frames {
            writer
                .write_sample(((index % 1000) as f32 / 2000.0 * 32_767.0) as i16)
                .unwrap();
        }
        writer.finalize().unwrap();
        name.to_string()
    }

    #[test]
    fn a_short_sample_ends_up_fully_resident() {
        let root = temp_root();
        let name = write_source(&root, "short.wav", 128);
        let store = SampleStore::beside(&root);
        let loaded = store.load_one(&root, "", &name, false).unwrap();
        assert_eq!(loaded.frame_count, 128);
        assert!(loaded.is_fully_resident(), "a short sample needs no reader");
    }

    #[test]
    fn a_long_sample_keeps_only_its_head() {
        let root = temp_root();
        let frames = PRELOAD_FRAMES * 3;
        let name = write_source(&root, "long.wav", frames);
        let store = SampleStore::beside(&root);
        let loaded = store.load_one(&root, "", &name, false).unwrap();
        assert_eq!(loaded.frame_count, frames);
        assert_eq!(loaded.preload_frames, PRELOAD_FRAMES);
        assert!(!loaded.is_fully_resident());
        // The point of the exercise: memory is a third of the audio.
        assert!(loaded.resident_bytes() < frames * size_of::<f32>() / 2);
    }

    #[test]
    fn the_resident_head_is_the_beginning_of_the_audio() {
        let root = temp_root();
        let name = write_source(&root, "head.wav", PRELOAD_FRAMES * 2);
        let store = SampleStore::beside(&root);
        let loaded = store.load_one(&root, "", &name, false).unwrap();
        // Source ramps from zero, so the first frame must be near silence and
        // a later one must not be.
        assert!(loaded.preload[0].abs() < 1e-3);
        assert!(loaded.preload[500].abs() > 1e-3);
    }

    #[test]
    fn a_second_load_reuses_the_cache_instead_of_decoding_again() {
        let root = temp_root();
        let name = write_source(&root, "reuse.wav", 4_096);
        let store = SampleStore::beside(&root);
        let first = store.load_one(&root, "", &name, false).unwrap();

        // Removing the source proves the second load never touched it.
        fs::remove_file(root.join(&name)).unwrap();
        let second = store.load_one(&root, "", &name, false).unwrap();
        assert_eq!(first.frame_count, second.frame_count);
        assert_eq!(first.preload.len(), second.preload.len());
    }

    #[test]
    fn a_source_newer_than_its_cache_is_decoded_again() {
        let root = temp_root();
        let name = write_source(&root, "stale.wav", 1_024);
        let store = SampleStore::beside(&root);
        let first = store.load_one(&root, "", &name, false).unwrap();
        assert_eq!(first.frame_count, 1_024);

        // Re-export the sample at a different length and touch it forward, as
        // a library update would.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_source(&root, "stale.wav", 2_048);
        let second = store.load_one(&root, "", &name, false).unwrap();
        assert_eq!(
            second.frame_count, 2_048,
            "a replaced library kept playing the old audio"
        );
    }

    #[test]
    fn clearing_the_cache_costs_only_a_slow_reload() {
        let root = temp_root();
        let name = write_source(&root, "clear.wav", 1_024);
        let store = SampleStore::beside(&root);
        store.load_one(&root, "", &name, false).unwrap();
        store.clear().unwrap();
        assert!(store.load_one(&root, "", &name, false).is_ok());
    }

    #[test]
    fn clearing_an_absent_cache_is_not_an_error() {
        let root = temp_root();
        SampleStore::new(root.join("never-created")).clear().unwrap();
    }

    #[test]
    fn loading_many_in_parallel_preserves_the_requested_order() {
        let root = temp_root();
        let names: Vec<String> = (0..16)
            .map(|index| write_source(&root, &format!("voice{index}.wav"), 256 + index * 8))
            .collect();
        let store = SampleStore::beside(&root);
        let loaded = store.load_all(&root, "", &names, &Default::default()).unwrap();
        assert_eq!(loaded.len(), names.len());
        for (index, sample) in loaded.iter().enumerate() {
            assert_eq!(sample.name, names[index], "results came back reordered");
            assert_eq!(sample.frame_count, 256 + index * 8);
        }
    }

    #[test]
    fn one_unreadable_sample_fails_the_load_rather_than_yielding_silence() {
        let root = temp_root();
        let mut names = vec![write_source(&root, "good.wav", 128)];
        names.push("missing.wav".to_string());
        let store = SampleStore::beside(&root);
        assert!(store.load_all(&root, "", &names, &Default::default()).is_err());
    }

    #[test]
    fn an_empty_request_is_not_an_error() {
        let root = temp_root();
        let store = SampleStore::beside(&root);
        assert!(store.load_all(&root, "", &[], &Default::default()).unwrap().is_empty());
    }

    /// Checks every cached sample against the file it was made from.
    ///
    /// A click heard on some notes and not others points at the samples those
    /// notes use rather than at timing, so this compares the transcode against
    /// its source frame by frame and reports the worst disagreement, plus any
    /// step inside the audio large enough to be heard as a click.
    ///
    /// ```text
    /// RF_SOUNDFONTS_SFZ="/path/to/instrument.sfz" cargo test --release -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied SFZ library"]
    fn audits_every_cached_sample() {
        let path = std::env::var("RF_SOUNDFONTS_SFZ").expect("set RF_SOUNDFONTS_SFZ to an .sfz file");
        let path = Path::new(&path);
        let root = path.parent().unwrap_or(Path::new("."));
        let expanded = crate::sfz::preprocess::expand(path).unwrap();
        let document = crate::sfz::parse::parse(&expanded).unwrap();
        let default_path = document
            .control
            .get("default_path")
            .cloned()
            .unwrap_or_default();
        let mut relatives: Vec<String> = document
            .regions
            .iter()
            .filter_map(|region| region.get("sample").cloned())
            .collect();
        relatives.sort();
        relatives.dedup();

        let store = SampleStore::beside(root);
        let mut worst_error = 0.0_f32;
        let mut worst_sample = String::new();
        let mut suspects = Vec::new();

        for relative in &relatives {
            let source = crate::sample::load(&crate::sample::resolve(
                root,
                &default_path,
                relative,
            ))
            .unwrap();
            let loaded = store.load_one(root, &default_path, relative, false).unwrap();

            if loaded.frame_count != source.frame_count() {
                suspects.push(format!(
                    "{relative}: cache holds {} frames, source has {}",
                    loaded.frame_count,
                    source.frame_count()
                ));
                continue;
            }

            // Read the whole cache back and compare against the decode.
            let file = File::open(&loaded.cache_path).unwrap();
            let channels = usize::from(loaded.channels).max(1);
            let mut out = vec![0.0_f32; loaded.frame_count * channels];
            let mut scratch = Vec::new();
            pcm_cache::read_frames(&file, &loaded.header, 0, &mut out, &mut scratch).unwrap();

            let mut error = 0.0_f32;
            for (cached, original) in out.iter().zip(source.samples.iter()) {
                error = error.max((cached - original).abs());
            }
            if error > worst_error {
                worst_error = error;
                worst_sample = relative.clone();
            }
            // A 16-bit round trip cannot differ by more than one step.
            if error > 1.0 / 32_000.0 {
                suspects.push(format!("{relative}: differs from its source by {error}"));
            }

            // Steps this large do not occur inside recorded piano audio.
            let mut worst_step = 0.0_f32;
            let mut step_at = 0;
            for (index, pair) in out.chunks_exact(channels).collect::<Vec<_>>().windows(2).enumerate()
            {
                let step = (pair[1][0] - pair[0][0]).abs();
                if step > worst_step {
                    worst_step = step;
                    step_at = index;
                }
            }
            if worst_step > 0.25 {
                suspects.push(format!(
                    "{relative}: jumps {worst_step:.3} at frame {step_at}"
                ));
            }
        }

        eprintln!("samples audited: {}", relatives.len());
        eprintln!("worst round-trip error: {worst_error:.8} ({worst_sample})");
        eprintln!("suspects: {}", suspects.len());
        for suspect in suspects.iter().take(20) {
            eprintln!("  {suspect}");
        }
        assert!(suspects.is_empty(), "{} samples look wrong", suspects.len());
    }

    /// Loads a real library through the store and reports what it cost.
    ///
    /// Run twice: the first pass transcodes and the second reuses the cache,
    /// which is the number that matters for start-up on stage.
    ///
    /// ```text
    /// RF_SOUNDFONTS_SFZ="/path/to/instrument.sfz" cargo test --release -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied SFZ library"]
    fn measures_a_real_library() {
        use std::time::Instant;

        let path = std::env::var("RF_SOUNDFONTS_SFZ").expect("set RF_SOUNDFONTS_SFZ to an .sfz file");
        let path = Path::new(&path);
        let root = path.parent().unwrap_or(Path::new("."));

        let expanded = crate::sfz::preprocess::expand(path).unwrap();
        let document = crate::sfz::parse::parse(&expanded).unwrap();
        let default_path = document
            .control
            .get("default_path")
            .cloned()
            .unwrap_or_default();
        let mut relatives: Vec<String> = document
            .regions
            .iter()
            .filter_map(|region| region.get("sample").cloned())
            .collect();
        relatives.sort();
        relatives.dedup();

        let store = SampleStore::beside(root);
        let started = Instant::now();
        let samples = store.load_all(root, &default_path, &relatives, &Default::default()).unwrap();
        let elapsed = started.elapsed();

        let resident: usize = samples.iter().map(StreamedSample::resident_bytes).sum();
        let whole: usize = samples
            .iter()
            .map(|sample| {
                sample.frame_count * usize::from(sample.channels) * size_of::<f32>()
            })
            .sum();
        let streaming = samples
            .iter()
            .filter(|sample| !sample.is_fully_resident())
            .count();

        eprintln!("samples:          {}", samples.len());
        eprintln!("load:             {:.2} s", elapsed.as_secs_f32());
        eprintln!("resident:         {} MiB", resident / 1_048_576);
        eprintln!("if loaded whole:  {} MiB", whole / 1_048_576);
        eprintln!(
            "saved:            {:.1}x",
            whole as f32 / resident.max(1) as f32
        );
        eprintln!("need a reader:    {streaming} of {}", samples.len());

        assert!(!samples.is_empty());
        assert!(
            resident < whole,
            "preloading saved nothing over loading the library whole"
        );
    }
}
