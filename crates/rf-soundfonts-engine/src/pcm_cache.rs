//! A decoded-PCM cache that a stream can seek into by arithmetic.
//!
//! Streaming needs to jump to an arbitrary frame of an arbitrary sample while
//! a note is sounding. Neither shipped format allows that cheaply: FLAC is
//! decoded forward from a sync point, and even WAV forces a decoder per read.
//! Transcoding once into a flat file makes the offset of frame *n* a
//! multiplication, which is what keeps the disk thread's work bounded and
//! predictable.
//!
//! The cache is written beside the library, once, and reused on every later
//! load. It is derived data: deleting it costs one slow start, never an
//! instrument.
//!
//! Sample depth is preserved rather than normalised. A 16-bit source — which
//! most libraries are — stays 16-bit, halving both the cache and the disk
//! traffic a voice generates, and the conversion to `f32` costs one multiply
//! on a path that already scales every sample by a gain.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{SoundfontError, Wave};

/// Identifies the file and guards against reading a cache from a future build.
const MAGIC: &[u8; 8] = b"RFDLSPCM";

/// Bump whenever the layout changes; a stale cache is then rebuilt, not
/// misread.
const VERSION: u32 = 2;

/// Bytes before the first frame.
const HEADER_BYTES: u64 = 48;

/// How samples are stored in the cache body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheFormat {
    /// Signed 16-bit, for sources that were 16-bit or narrower.
    Int16,
    /// 32-bit float, for everything else.
    Float32,
}

impl CacheFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Float32 => 4,
        }
    }

    fn tag(self) -> u32 {
        match self {
            Self::Int16 => 1,
            Self::Float32 => 2,
        }
    }

    fn from_tag(tag: u32) -> Option<Self> {
        match tag {
            1 => Some(Self::Int16),
            2 => Some(Self::Float32),
            _ => None,
        }
    }
}

/// What a reader needs to know about a cached sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheHeader {
    /// Loop the source declared, in frames. Carried through the cache so a
    /// converted library keeps the markers its SFZ never states explicitly.
    pub sample_loop: Option<crate::SampleLoop>,
    pub sample_rate: u32,
    pub channels: u8,
    pub format: CacheFormat,
    pub frame_count: usize,
}

impl CacheHeader {
    /// Bytes occupied by one frame across all channels.
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.channels).max(1) * self.format.bytes_per_sample()
    }

    /// Byte offset of a frame within the file.
    pub fn offset_of(&self, frame: usize) -> u64 {
        HEADER_BYTES + frame as u64 * self.frame_bytes() as u64
    }
}

/// Writes a decoded wave into a cache file, atomically.
///
/// Written to a temporary name and renamed, so an interrupted transcode leaves
/// no half-written cache that a later run would trust.
pub fn write(path: &Path, wave: &Wave, format: CacheFormat) -> Result<CacheHeader, SoundfontError> {
    let header = CacheHeader {
        sample_rate: wave.sample_rate,
        channels: wave.channels,
        format,
        frame_count: wave.frame_count(),
        sample_loop: wave.sample_params.and_then(|params| params.sample_loop),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SoundfontError::Read {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let temporary = path.with_extension("pcm-partial");
    let read_error = |source| SoundfontError::Read {
        path: temporary.display().to_string(),
        source,
    };
    {
        let mut file = File::create(&temporary).map_err(read_error)?;
        let mut prologue = Vec::with_capacity(HEADER_BYTES as usize);
        prologue.extend_from_slice(MAGIC);
        prologue.extend_from_slice(&VERSION.to_le_bytes());
        prologue.extend_from_slice(&header.sample_rate.to_le_bytes());
        prologue.extend_from_slice(&u32::from(header.channels).to_le_bytes());
        prologue.extend_from_slice(&format.tag().to_le_bytes());
        prologue.extend_from_slice(&(header.frame_count as u64).to_le_bytes());
        // A loop of 0..0 means none, which no real loop can be.
        let (loop_start, loop_end) = header.sample_loop.map_or((0_u64, 0_u64), |looping| {
            (looping.start as u64, looping.end as u64)
        });
        prologue.extend_from_slice(&loop_start.to_le_bytes());
        prologue.extend_from_slice(&loop_end.to_le_bytes());
        prologue.resize(HEADER_BYTES as usize, 0);
        file.write_all(&prologue).map_err(read_error)?;

        // Written in blocks rather than sample by sample: a 1.7 GiB transcode
        // through unbuffered writes would be dominated by syscall overhead.
        let mut block = Vec::with_capacity(64 * 1024);
        for chunk in wave.samples.chunks(16 * 1024) {
            block.clear();
            match format {
                CacheFormat::Int16 => {
                    for sample in chunk {
                        let scaled = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
                        block.extend_from_slice(&scaled.to_le_bytes());
                    }
                }
                CacheFormat::Float32 => {
                    for sample in chunk {
                        block.extend_from_slice(&sample.to_le_bytes());
                    }
                }
            }
            file.write_all(&block).map_err(read_error)?;
        }
        file.sync_all().map_err(read_error)?;
    }
    fs::rename(&temporary, path).map_err(|source| SoundfontError::Read {
        path: path.display().to_string(),
        source,
    })?;
    Ok(header)
}

/// Reads a cache header, rejecting anything this build cannot interpret.
pub fn read_header(file: &mut File) -> Result<CacheHeader, SoundfontError> {
    let mut bytes = [0_u8; HEADER_BYTES as usize];
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|source| SoundfontError::Read {
            path: "PCM cache".into(),
            source,
        })?;
    if &bytes[0..8] != MAGIC {
        return Err(SoundfontError::Invalid(
            "PCM cache has the wrong magic".into(),
        ));
    }
    let word = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if word(8) != VERSION {
        return Err(SoundfontError::Invalid(format!(
            "PCM cache is version {} and this build writes {VERSION}",
            word(8)
        )));
    }
    let channels = word(16);
    if !(1..=2).contains(&channels) {
        return Err(SoundfontError::Invalid(format!(
            "PCM cache declares {channels} channels"
        )));
    }
    let format = CacheFormat::from_tag(word(20))
        .ok_or_else(|| SoundfontError::Invalid("PCM cache has an unknown sample format".into()))?;
    let frame_count = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let loop_start = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let loop_end = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    let sample_loop = (loop_start < loop_end).then_some(crate::SampleLoop {
        start: loop_start,
        end: loop_end,
    });
    Ok(CacheHeader {
        sample_rate: word(12),
        channels: channels as u8,
        format,
        frame_count: usize::try_from(frame_count).unwrap_or(0),
        sample_loop,
    })
}

/// Reads frames from an open cache into `out`, returning how many were read.
///
/// `out` is interleaved and must hold `frames * channels` samples. Short reads
/// at the end of the file are reported rather than padded, because a caller
/// that reached the end needs to know it has.
pub fn read_frames(
    file: &File,
    header: &CacheHeader,
    first_frame: usize,
    out: &mut [f32],
    scratch: &mut Vec<u8>,
) -> Result<usize, SoundfontError> {
    let channels = usize::from(header.channels).max(1);
    let wanted = (out.len() / channels).min(header.frame_count.saturating_sub(first_frame));
    if wanted == 0 {
        return Ok(0);
    }
    let bytes = wanted * header.frame_bytes();
    scratch.resize(bytes, 0);
    read_exact_at(file, scratch, header.offset_of(first_frame))?;

    match header.format {
        CacheFormat::Int16 => {
            for (index, value) in scratch.chunks_exact(2).enumerate() {
                let raw = i16::from_le_bytes([value[0], value[1]]);
                out[index] = f32::from(raw) / 32_768.0;
            }
        }
        CacheFormat::Float32 => {
            for (index, value) in scratch.chunks_exact(4).enumerate() {
                out[index] = f32::from_le_bytes(value.try_into().unwrap());
            }
        }
    }
    Ok(wanted)
}

/// Positioned read that does not disturb the file cursor.
///
/// Streams for different voices share one open file, so a read that moved a
/// shared cursor would hand the wrong audio to whichever voice read next.
#[cfg(unix)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<(), SoundfontError> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buffer, offset)
        .map_err(|source| SoundfontError::Read {
            path: "PCM cache".into(),
            source,
        })
}

#[cfg(windows)]
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<(), SoundfontError> {
    use std::os::windows::fs::FileExt;
    let mut read = 0;
    while read < buffer.len() {
        let count = file
            .seek_read(&mut buffer[read..], offset + read as u64)
            .map_err(|source| SoundfontError::Read {
                path: "PCM cache".into(),
                source,
            })?;
        if count == 0 {
            return Err(SoundfontError::Invalid("PCM cache ended early".into()));
        }
        read += count;
    }
    Ok(())
}

/// Where a sample's cache lives, given the library root and the sample path.
///
/// Kept in one directory beside the instrument so the whole cache can be
/// deleted in a single step, and named from the sample's relative path so two
/// samples with the same file name in different folders cannot collide.
pub fn cache_path(cache_root: &Path, relative: &str) -> PathBuf {
    let mut name = String::with_capacity(relative.len() + 4);
    for byte in relative.bytes() {
        match byte {
            b'/' | b'\\' | b':' => name.push('_'),
            _ => name.push(char::from(byte)),
        }
    }
    name.push_str(".pcm");
    cache_root.join(name)
}

/// Chooses a cache format that cannot lose information from the source.
pub fn format_for(source_bits: u16) -> CacheFormat {
    if source_bits <= 16 {
        CacheFormat::Int16
    } else {
        CacheFormat::Float32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rf-soundfonts-pcm-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn wave(channels: u8, samples: Vec<f32>) -> Wave {
        Wave {
            name: "fixture".into(),
            sample_rate: 44_100,
            channels,
            source_bits: 16,
            samples: Arc::from(samples),
            sample_params: None,
        }
    }

    #[test]
    fn a_cache_round_trips_stereo_audio() {
        let root = temp_root();
        let path = root.join("a.pcm");
        let source = wave(2, vec![0.5, -0.5, 0.25, -0.25]);
        let header = write(&path, &source, CacheFormat::Float32).unwrap();
        assert_eq!(header.frame_count, 2);
        assert_eq!(header.channels, 2);

        let mut file = File::open(&path).unwrap();
        let read_back = read_header(&mut file).unwrap();
        assert_eq!(read_back, header);

        let mut out = vec![0.0; 4];
        let mut scratch = Vec::new();
        let frames = read_frames(&file, &read_back, 0, &mut out, &mut scratch).unwrap();
        assert_eq!(frames, 2);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn sixteen_bit_storage_survives_a_round_trip() {
        let root = temp_root();
        let path = root.join("b.pcm");
        write(&path, &wave(1, vec![0.5, -0.5]), CacheFormat::Int16).unwrap();
        let mut file = File::open(&path).unwrap();
        let header = read_header(&mut file).unwrap();
        assert_eq!(header.format, CacheFormat::Int16);
        let mut out = vec![0.0; 2];
        let mut scratch = Vec::new();
        read_frames(&file, &header, 0, &mut out, &mut scratch).unwrap();
        assert!((out[0] - 0.5).abs() < 1e-3);
        assert!((out[1] + 0.5).abs() < 1e-3);
    }

    #[test]
    fn reading_from_the_middle_lands_on_the_right_frame() {
        // The property the whole design rests on: frame n is at a computable
        // offset, so a stream can resume anywhere without decoding.
        let root = temp_root();
        let path = root.join("c.pcm");
        let samples: Vec<f32> = (0..16).map(|index| index as f32 / 100.0).collect();
        write(&path, &wave(2, samples), CacheFormat::Float32).unwrap();
        let mut file = File::open(&path).unwrap();
        let header = read_header(&mut file).unwrap();
        let mut out = vec![0.0; 2];
        let mut scratch = Vec::new();
        read_frames(&file, &header, 5, &mut out, &mut scratch).unwrap();
        assert!((out[0] - 0.10).abs() < 1e-6, "got {}", out[0]);
        assert!((out[1] - 0.11).abs() < 1e-6, "got {}", out[1]);
    }

    #[test]
    fn a_read_past_the_end_is_short_rather_than_padded() {
        let root = temp_root();
        let path = root.join("d.pcm");
        write(&path, &wave(1, vec![0.1, 0.2, 0.3]), CacheFormat::Float32).unwrap();
        let mut file = File::open(&path).unwrap();
        let header = read_header(&mut file).unwrap();
        let mut out = vec![0.0; 8];
        let mut scratch = Vec::new();
        let frames = read_frames(&file, &header, 1, &mut out, &mut scratch).unwrap();
        assert_eq!(frames, 2, "a caller must be able to see the end coming");
    }

    #[test]
    fn a_read_starting_past_the_end_returns_nothing() {
        let root = temp_root();
        let path = root.join("e.pcm");
        write(&path, &wave(1, vec![0.1]), CacheFormat::Float32).unwrap();
        let mut file = File::open(&path).unwrap();
        let header = read_header(&mut file).unwrap();
        let mut out = vec![0.0; 4];
        let mut scratch = Vec::new();
        assert_eq!(
            read_frames(&file, &header, 9, &mut out, &mut scratch).unwrap(),
            0
        );
    }

    #[test]
    fn a_foreign_file_is_refused_rather_than_played_as_noise() {
        let root = temp_root();
        let path = root.join("f.pcm");
        fs::write(&path, b"this is not a cache, it is a text file").unwrap();
        let mut file = File::open(&path).unwrap();
        assert!(read_header(&mut file).is_err());
    }

    #[test]
    fn a_cache_from_another_version_is_refused() {
        let root = temp_root();
        let path = root.join("g.pcm");
        write(&path, &wave(1, vec![0.1]), CacheFormat::Float32).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let mut file = File::open(&path).unwrap();
        let error = read_header(&mut file).unwrap_err();
        assert!(error.to_string().contains("version"), "{error}");
    }

    #[test]
    fn an_interrupted_transcode_leaves_no_usable_cache() {
        // write() renames into place, so the final name never exists in a
        // partial state. Its temporary is what a crash would leave behind.
        let root = temp_root();
        let path = root.join("h.pcm");
        write(&path, &wave(1, vec![0.1]), CacheFormat::Float32).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("pcm-partial").exists());
    }

    #[test]
    fn two_samples_with_the_same_name_do_not_collide() {
        let root = temp_root();
        let close = cache_path(&root, "Close/PIANO 60.flac");
        let decca = cache_path(&root, "Decca/PIANO 60.flac");
        assert_ne!(close, decca);
    }

    #[test]
    fn the_cache_format_never_narrows_the_source() {
        assert_eq!(format_for(16), CacheFormat::Int16);
        assert_eq!(format_for(8), CacheFormat::Int16);
        assert_eq!(format_for(24), CacheFormat::Float32);
        assert_eq!(format_for(32), CacheFormat::Float32);
    }
}
