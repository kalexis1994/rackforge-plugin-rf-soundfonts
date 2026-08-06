//! Loading standalone WAV files referenced by an instrument definition.
//!
//! DLS carries its audio inside the collection, so the engine could decode it
//! while walking the RIFF tree. SFZ instead points at files on disk, in
//! whatever format the library author exported: 16-, 24- or 32-bit integer,
//! 32-bit float, mono or stereo. Decoding is delegated to `hound`, which
//! already handles that matrix correctly.
//!
//! What `hound` does not expose is the `smpl` chunk. It matters because the
//! SFZ specification says a region loops at the sample's own markers unless
//! the instrument overrides them, and a large share of free libraries rely on
//! exactly that rather than writing `loop_start` and `loop_end` by hand. A
//! loader that ignored `smpl` would play those sustained instruments as
//! one-shots that stop mid-note.

use crate::{DlsError, SampleLoop, Wave};
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// Channels beyond this are refused rather than silently folded down.
const MAX_CHANNELS: u16 = 2;

/// Decodes a WAV file into the engine's shared [`Wave`] representation.
///
/// The returned wave is interleaved and normalised to `-1.0..=1.0`. Loop
/// points found in `smpl` are reported in frames, matching the rest of the
/// engine.
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

/// Decodes WAV bytes already held in memory.
pub fn decode(bytes: &[u8], name: String) -> Result<Wave, DlsError> {
    let reader = hound::WavReader::new(bytes)
        .map_err(|error| DlsError::Invalid(format!("cannot read WAV {name:?}: {error}")))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.channels > MAX_CHANNELS {
        return Err(DlsError::Unsupported(format!(
            "WAV {name:?} has {} channels; only mono and stereo are supported",
            spec.channels
        )));
    }
    if spec.sample_rate == 0 {
        return Err(DlsError::Invalid(format!("WAV {name:?} has no sample rate")));
    }

    let samples = decode_samples(reader, &name)?;
    if samples.is_empty() {
        return Err(DlsError::Invalid(format!("WAV {name:?} contains no audio")));
    }
    if samples.len() % usize::from(spec.channels) != 0 {
        return Err(DlsError::Invalid(format!(
            "WAV {name:?} ends on a partial frame"
        )));
    }
    let frames = samples.len() / usize::from(spec.channels);

    // A loop that runs past the audio is treated as absent rather than fatal:
    // truncated markers are common in converted libraries, and refusing the
    // file would lose an instrument over metadata the renderer can do without.
    let sample_loop = read_smpl_loop(bytes).filter(|looping| {
        looping.start < looping.end && looping.end <= frames
    });

    Ok(Wave {
        name,
        sample_rate: spec.sample_rate,
        channels: spec.channels as u8,
        source_bits: spec.bits_per_sample,
        samples: Arc::from(samples),
        sample_params: sample_loop.map(|sample_loop| crate::SampleParams {
            // The instrument decides the root note; the file only contributes
            // its loop. Leaving unity at middle C keeps a wave usable on its
            // own while any SFZ region overrides it anyway.
            unity_note: 60,
            fine_tune: 0,
            attenuation_db: 0.0,
            sample_loop: Some(sample_loop),
        }),
    })
}

fn decode_samples(
    reader: hound::WavReader<impl std::io::Read>,
    name: &str,
) -> Result<Vec<f32>, DlsError> {
    let spec = reader.spec();
    let invalid = |error: hound::Error| DlsError::Invalid(format!("WAV {name:?}: {error}"));
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => reader
            .into_samples::<f32>()
            .map(|sample| sample.map_err(invalid))
            .collect(),
        (hound::SampleFormat::Int, bits @ (8 | 16 | 24 | 32)) => {
            // hound yields integer samples sign-extended into i32, so one
            // divisor derived from the declared width normalises every case.
            let scale = 1.0_f32 / (1_i64 << (bits - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 * scale).map_err(invalid))
                .collect()
        }
        (format, bits) => Err(DlsError::Unsupported(format!(
            "WAV {name:?} is {bits}-bit {format:?}"
        ))),
    }
}

/// Extracts the first sustaining loop from a `smpl` chunk, in frames.
///
/// Deliberately a flat scan of top-level chunks rather than a full RIFF walk:
/// `smpl` always sits beside `fmt ` and `data`, and a shallow reader cannot be
/// led into deep recursion by a malformed file.
fn read_smpl_loop(bytes: &[u8]) -> Option<SampleLoop> {
    const HEADER: usize = 12;
    if bytes.len() < HEADER || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    crate::smpl::loop_in_riff(bytes, HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(spec: hound::WavSpec, samples: &[f32]) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut bytes, spec).unwrap();
            for sample in samples {
                match spec.sample_format {
                    hound::SampleFormat::Float => writer.write_sample(*sample).unwrap(),
                    hound::SampleFormat::Int => {
                        let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
                        writer.write_sample((sample * scale) as i32).unwrap();
                    }
                }
            }
            writer.finalize().unwrap();
        }
        bytes.into_inner()
    }

    fn spec(channels: u16, bits: u16, format: hound::SampleFormat) -> hound::WavSpec {
        hound::WavSpec {
            channels,
            sample_rate: 44_100,
            bits_per_sample: bits,
            sample_format: format,
        }
    }

    #[test]
    fn decodes_sixteen_bit_mono() {
        let bytes = write_wav(spec(1, 16, hound::SampleFormat::Int), &[0.5, -0.5]);
        let wave = decode(&bytes, "mono".into()).unwrap();
        assert_eq!(wave.channels, 1);
        assert_eq!(wave.frame_count(), 2);
        assert!((wave.samples[0] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn decodes_stereo_and_keeps_frames_interleaved() {
        // Kept below full scale: +1.0 is one step past what a signed 16-bit
        // sample can hold, and the writer rejects it.
        let bytes = write_wav(spec(2, 16, hound::SampleFormat::Int), &[0.5, -0.5, 0.5, -0.5]);
        let wave = decode(&bytes, "stereo".into()).unwrap();
        assert_eq!(wave.channels, 2);
        assert_eq!(wave.frame_count(), 2, "frames must not count each channel");
        assert!(wave.samples[0] > 0.0 && wave.samples[1] < 0.0);
    }

    #[test]
    fn every_supported_depth_normalises_to_the_same_amplitude() {
        // A library that mixes export depths must not change level per sample.
        let mut amplitudes = Vec::new();
        for (bits, format) in [
            (16, hound::SampleFormat::Int),
            (24, hound::SampleFormat::Int),
            (32, hound::SampleFormat::Int),
            (32, hound::SampleFormat::Float),
        ] {
            let bytes = write_wav(spec(1, bits, format), &[0.5]);
            let wave = decode(&bytes, format!("{bits}")).unwrap();
            amplitudes.push(wave.samples[0]);
        }
        for amplitude in &amplitudes {
            assert!(
                (amplitude - 0.5).abs() < 1e-3,
                "depths disagree: {amplitudes:?}"
            );
        }
    }

    #[test]
    fn a_file_without_smpl_reports_no_loop() {
        let bytes = write_wav(spec(1, 16, hound::SampleFormat::Int), &[0.1, 0.2, 0.3]);
        assert!(decode(&bytes, "dry".into()).unwrap().sample_params.is_none());
    }

    #[test]
    fn a_smpl_loop_end_is_exclusive() {
        // smpl stores the last frame inside the loop; the engine wants one past.
        let mut bytes = write_wav(spec(1, 16, hound::SampleFormat::Int), &[0.0; 8]);
        append_smpl(&mut bytes, 2, 5);
        let wave = decode(&bytes, "looped".into()).unwrap();
        let looping = wave.sample_params.unwrap().sample_loop.unwrap();
        assert_eq!(looping.start, 2);
        assert_eq!(looping.end, 6);
    }

    #[test]
    fn a_loop_running_past_the_audio_is_dropped_rather_than_fatal() {
        let mut bytes = write_wav(spec(1, 16, hound::SampleFormat::Int), &[0.0; 4]);
        append_smpl(&mut bytes, 0, 99);
        let wave = decode(&bytes, "bad-loop".into()).unwrap();
        assert!(wave.sample_params.is_none(), "instrument was lost to metadata");
    }

    #[test]
    fn more_than_two_channels_is_refused() {
        let bytes = write_wav(spec(3, 16, hound::SampleFormat::Int), &[0.0; 6]);
        assert!(decode(&bytes, "surround".into()).is_err());
    }

    #[test]
    fn empty_audio_is_refused() {
        let bytes = write_wav(spec(1, 16, hound::SampleFormat::Int), &[]);
        assert!(decode(&bytes, "silent".into()).is_err());
    }

    /// Appends a minimal `smpl` chunk carrying one loop.
    fn append_smpl(bytes: &mut Vec<u8>, start: u32, last: u32) {
        let mut chunk = vec![0_u8; 36 + 24];
        chunk[28..32].copy_from_slice(&1_u32.to_le_bytes());
        chunk[36 + 8..36 + 12].copy_from_slice(&start.to_le_bytes());
        chunk[36 + 12..36 + 16].copy_from_slice(&last.to_le_bytes());
        bytes.extend_from_slice(b"smpl");
        bytes.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&chunk);
        let total = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&total.to_le_bytes());
    }
}
