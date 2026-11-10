//! Decoding FLAC samples referenced by an instrument definition.
//!
//! Sample libraries increasingly ship FLAC rather than WAV: the Headroom
//! Piano is 156 MB compressed against 875 MB uncompressed, which is the
//! difference between fitting comfortably on an SD card and not. Decoded
//! audio is identical either way, so this costs disk space only, never
//! fidelity.
//!
//! FLAC carries no `smpl` chunk, so a converted library loses whatever loop
//! markers its WAV originals held. Instruments that loop are expected to state
//! `loop_start` and `loop_end` in the SFZ itself; sustained-to-silence
//! material such as piano never needed them.

use crate::{SoundfontError, Wave};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// Channels beyond this are refused rather than silently folded down.
const MAX_CHANNELS: u32 = 2;

/// Decodes a FLAC file into the engine's shared [`Wave`] representation.
pub fn load_wave(path: impl AsRef<Path>) -> Result<Wave, SoundfontError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| SoundfontError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    decode(&bytes, name)
}

/// Decodes FLAC bytes already held in memory.
pub fn decode(bytes: &[u8], name: String) -> Result<Wave, SoundfontError> {
    decode_limited(bytes, name, usize::MAX)
}

/// Samples reserved up front per byte of FLAC when the stream declares its
/// length. Ordinary audio compresses two- to threefold; reserving for far more
/// than that would let a header that lies about its length ask for gigabytes.
/// Streams that genuinely compress harder simply grow as they decode.
const RESERVED_SAMPLES_PER_BYTE: usize = 16;

/// Decodes FLAC bytes, refusing a stream that would yield more than
/// `max_samples` interleaved samples.
///
/// A declared length over the limit is refused before anything is allocated,
/// and the limit is enforced again while decoding, so a stream that does not
/// declare its length, or misdeclares it, cannot grow past it either.
pub fn decode_limited(
    bytes: &[u8],
    name: String,
    max_samples: usize,
) -> Result<Wave, SoundfontError> {
    let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes))
        .map_err(|error| SoundfontError::Invalid(format!("cannot read FLAC {name:?}: {error}")))?;
    let info = reader.streaminfo();
    if info.channels == 0 || info.channels > MAX_CHANNELS {
        return Err(SoundfontError::Unsupported(format!(
            "FLAC {name:?} has {} channels; only mono and stereo are supported",
            info.channels
        )));
    }
    if info.sample_rate == 0 {
        return Err(SoundfontError::Invalid(format!(
            "FLAC {name:?} has no sample rate"
        )));
    }
    if !(1..=32).contains(&info.bits_per_sample) {
        return Err(SoundfontError::Unsupported(format!(
            "FLAC {name:?} is {}-bit",
            info.bits_per_sample
        )));
    }

    // claxon yields samples sign-extended into i32 regardless of the stored
    // width, so one divisor derived from the declared depth normalises every
    // case the format allows.
    let scale = 1.0_f32 / (1_i64 << (info.bits_per_sample - 1)) as f32;
    let declared = info
        .samples
        .and_then(|frames| frames.checked_mul(u64::from(info.channels)))
        .map(|samples| usize::try_from(samples).unwrap_or(usize::MAX));
    if declared.is_some_and(|declared| declared > max_samples) {
        return Err(SoundfontError::TooLarge {
            name,
            limit: max_samples,
        });
    }
    let too_large = |name: &str| SoundfontError::TooLarge {
        name: name.to_string(),
        limit: max_samples,
    };
    let mut samples: Vec<f32> = Vec::new();
    samples
        .try_reserve_exact(
            declared
                .unwrap_or(0)
                .min(max_samples)
                .min(bytes.len().saturating_mul(RESERVED_SAMPLES_PER_BYTE)),
        )
        .map_err(|_| too_large(&name))?;
    for sample in reader.samples() {
        let value =
            sample.map_err(|error| SoundfontError::Invalid(format!("FLAC {name:?}: {error}")))?;
        if samples.len() >= max_samples {
            return Err(too_large(&name));
        }
        if samples.len() == samples.capacity() {
            // Grow by hand rather than letting `push` double: doubling could
            // reserve twice the limit just before reaching it.
            let room = max_samples - samples.len();
            samples
                .try_reserve_exact(samples.capacity().max(4_096).min(room))
                .map_err(|_| too_large(&name))?;
        }
        samples.push(value as f32 * scale);
    }

    if samples.is_empty() {
        return Err(SoundfontError::Invalid(format!(
            "FLAC {name:?} contains no audio"
        )));
    }
    if !samples.len().is_multiple_of(info.channels as usize) {
        return Err(SoundfontError::Invalid(format!(
            "FLAC {name:?} ends on a partial frame"
        )));
    }

    let frames = samples.len() / info.channels as usize;
    let sample_loop =
        read_loop(bytes).filter(|looping| looping.start < looping.end && looping.end <= frames);

    Ok(Wave {
        name,
        sample_rate: info.sample_rate,
        channels: info.channels as u8,
        source_bits: info.bits_per_sample as u16,
        samples: Arc::from(samples),
        sample_params: sample_loop.map(|sample_loop| crate::SampleParams {
            // The instrument decides the root note; the file contributes only
            // its loop.
            unity_note: 60,
            fine_tune: 0,
            attenuation_db: 0.0,
            sample_loop: Some(sample_loop),
        }),
    })
}

/// Finds loop markers a converter preserved from the original WAV.
///
/// FLAC has nowhere of its own to keep them, so a converted library carries
/// the source RIFF chunks verbatim inside `APPLICATION` blocks tagged `riff`.
/// A library that declares `loop_mode` and no explicit points — which is how
/// SoundFont conversions are written — depends entirely on these.
fn read_loop(bytes: &[u8]) -> Option<crate::SampleLoop> {
    const STREAM_MARKER: usize = 4;
    const APPLICATION: u8 = 2;

    if bytes.len() < STREAM_MARKER || &bytes[0..4] != b"fLaC" {
        return None;
    }
    let mut cursor = STREAM_MARKER;
    while cursor + 4 <= bytes.len() {
        let header = bytes[cursor];
        let last = header & 0x80 != 0;
        let kind = header & 0x7f;
        let length =
            u32::from_be_bytes([0, bytes[cursor + 1], bytes[cursor + 2], bytes[cursor + 3]])
                as usize;
        let body = cursor + 4;
        let end = body.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if kind == APPLICATION
            && length > 4
            && &bytes[body..body + 4] == b"riff"
            && let Some(looping) = crate::smpl::loop_in_riff(&bytes[body + 4..end], 0)
        {
            return Some(looping);
        }
        cursor = end;
        if last {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a FLAC file the user supplies locally.
    ///
    /// FLAC has no minimal hand-written fixture the way RIFF does, and no
    /// third-party audio is committed here, so the real check runs against a
    /// library on disk.
    ///
    /// ```text
    /// RF_SOUNDFONTS_FLAC=/path/to/sample.flac cargo test -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied FLAC sample"]
    fn decodes_a_real_sample() {
        let path =
            std::env::var("RF_SOUNDFONTS_FLAC").expect("set RF_SOUNDFONTS_FLAC to a .flac file");
        let wave = load_wave(&path).unwrap();
        eprintln!(
            "{}: {} Hz, {} channels, {} frames",
            wave.name,
            wave.sample_rate,
            wave.channels,
            wave.frame_count()
        );
        assert!(wave.frame_count() > 0);
        assert!((1..=2).contains(&wave.channels));
        assert!(
            wave.samples.iter().all(|sample| sample.is_finite()),
            "decoded audio contains non-finite samples"
        );
        assert!(
            wave.samples.iter().any(|sample| sample.abs() > 1e-4),
            "decoded audio is silent"
        );
        assert!(
            wave.samples.iter().all(|sample| sample.abs() <= 1.0),
            "decoded audio exceeds full scale"
        );
    }

    /// A stream holding only its STREAMINFO block: enough for every check
    /// that runs before the first frame is decoded.
    fn header_only_flac(total_frames: u64, channels: u8) -> Vec<u8> {
        let mut bytes = b"fLaC".to_vec();
        bytes.push(0x80); // last metadata block, type STREAMINFO
        bytes.extend_from_slice(&[0, 0, 34]);
        bytes.extend_from_slice(&4_096_u16.to_be_bytes());
        bytes.extend_from_slice(&4_096_u16.to_be_bytes());
        bytes.extend_from_slice(&[0; 6]); // frame sizes unknown
        let packed = (48_000_u64 << 44)
            | (u64::from(channels - 1) << 41)
            | (15_u64 << 36) // 16-bit
            | (total_frames & ((1 << 36) - 1));
        bytes.extend_from_slice(&packed.to_be_bytes());
        bytes.extend_from_slice(&[0; 16]); // MD5 unknown
        bytes
    }

    #[test]
    fn a_declared_length_over_the_limit_is_refused_before_decoding() {
        let bytes = header_only_flac(1_000, 2);
        assert!(matches!(
            decode_limited(&bytes, "big".into(), 1_999),
            Err(SoundfontError::TooLarge { limit: 1_999, .. })
        ));
    }

    #[test]
    fn an_absurd_declared_length_does_not_reserve_it() {
        // 2^36 stereo frames would be half a terabyte of f32. Reserving it up
        // front used to abort the process before any frame was read.
        let bytes = header_only_flac((1 << 36) - 1, 2);
        assert!(decode(&bytes, "liar".into()).is_err());
    }

    #[test]
    fn rubbish_bytes_are_refused_rather_than_panicking() {
        assert!(decode(b"not a flac file at all", "junk".into()).is_err());
    }

    #[test]
    fn an_empty_input_is_refused() {
        assert!(decode(&[], "empty".into()).is_err());
    }
}
