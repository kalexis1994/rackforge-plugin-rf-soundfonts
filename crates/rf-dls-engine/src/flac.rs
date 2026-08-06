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

use crate::{DlsError, Wave};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// Channels beyond this are refused rather than silently folded down.
const MAX_CHANNELS: u32 = 2;

/// Decodes a FLAC file into the engine's shared [`Wave`] representation.
pub fn load_wave(path: impl AsRef<Path>) -> Result<Wave, DlsError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| DlsError::Read {
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
pub fn decode(bytes: &[u8], name: String) -> Result<Wave, DlsError> {
    let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes))
        .map_err(|error| DlsError::Invalid(format!("cannot read FLAC {name:?}: {error}")))?;
    let info = reader.streaminfo();
    if info.channels == 0 || info.channels > MAX_CHANNELS {
        return Err(DlsError::Unsupported(format!(
            "FLAC {name:?} has {} channels; only mono and stereo are supported",
            info.channels
        )));
    }
    if info.sample_rate == 0 {
        return Err(DlsError::Invalid(format!("FLAC {name:?} has no sample rate")));
    }
    if !(1..=32).contains(&info.bits_per_sample) {
        return Err(DlsError::Unsupported(format!(
            "FLAC {name:?} is {}-bit",
            info.bits_per_sample
        )));
    }

    // claxon yields samples sign-extended into i32 regardless of the stored
    // width, so one divisor derived from the declared depth normalises every
    // case the format allows.
    let scale = 1.0_f32 / (1_i64 << (info.bits_per_sample - 1)) as f32;
    let mut samples = Vec::with_capacity(
        usize::try_from(info.samples.unwrap_or(0)).unwrap_or(0) * info.channels as usize,
    );
    for sample in reader.samples() {
        let value =
            sample.map_err(|error| DlsError::Invalid(format!("FLAC {name:?}: {error}")))?;
        samples.push(value as f32 * scale);
    }

    if samples.is_empty() {
        return Err(DlsError::Invalid(format!("FLAC {name:?} contains no audio")));
    }
    if samples.len() % info.channels as usize != 0 {
        return Err(DlsError::Invalid(format!(
            "FLAC {name:?} ends on a partial frame"
        )));
    }

    let frames = samples.len() / info.channels as usize;
    let sample_loop = read_loop(bytes).filter(|looping| {
        looping.start < looping.end && looping.end <= frames
    });

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
        let length = u32::from_be_bytes([0, bytes[cursor + 1], bytes[cursor + 2], bytes[cursor + 3]])
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
    /// RF_DLS_FLAC=/path/to/sample.flac cargo test -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied FLAC sample"]
    fn decodes_a_real_sample() {
        let path = std::env::var("RF_DLS_FLAC").expect("set RF_DLS_FLAC to a .flac file");
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

    #[test]
    fn rubbish_bytes_are_refused_rather_than_panicking() {
        assert!(decode(b"not a flac file at all", "junk".into()).is_err());
    }

    #[test]
    fn an_empty_input_is_refused() {
        assert!(decode(&[], "empty".into()).is_err());
    }
}
