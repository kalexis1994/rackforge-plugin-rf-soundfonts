//! Loading an instrument's samples regardless of how they were exported.
//!
//! An SFZ file names its samples but says nothing about their container, and a
//! single library may mix formats after conversion. Dispatching on the
//! extension keeps that detail out of the instrument builder.

use crate::{SoundfontError, Wave, flac, wav};
use std::path::Path;

/// Decodes a sample file, choosing a decoder by extension.
pub fn load(path: impl AsRef<Path>) -> Result<Wave, SoundfontError> {
    let path = path.as_ref();
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "wav" | "wave" => wav::load_wave(path),
        "flac" => flac::load_wave(path),
        other => Err(SoundfontError::Unsupported(format!(
            "sample {} is a {other:?} file",
            path.display()
        ))),
    }
}

/// Resolves a sample path the way an SFZ document means it.
///
/// Paths are written relative to the instrument, prefixed by `default_path`,
/// and by convention use backslashes because most libraries are authored on
/// Windows. A host that took them literally would fail every lookup elsewhere.
pub fn resolve(root: &Path, default_path: &str, sample: &str) -> std::path::PathBuf {
    let mut resolved = root.to_path_buf();
    for part in default_path.replace('\\', "/").split('/') {
        if !part.is_empty() {
            resolved.push(part);
        }
    }
    for part in sample.replace('\\', "/").split('/') {
        if !part.is_empty() {
            resolved.push(part);
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_extension_is_named_in_the_error() {
        let error = load(Path::new("piano.ogg")).unwrap_err();
        assert!(error.to_string().contains("ogg"), "{error}");
    }

    #[test]
    fn a_default_path_is_prefixed_to_the_sample() {
        let resolved = resolve(Path::new("/lib"), "Samples/", "PIANO 60.flac");
        assert!(resolved.ends_with("Samples/PIANO 60.flac"), "{resolved:?}");
    }

    #[test]
    fn backslashes_resolve_the_same_as_forward_slashes() {
        let windows = resolve(Path::new("/lib"), "Samples\\", "Close\\a.wav");
        let posix = resolve(Path::new("/lib"), "Samples/", "Close/a.wav");
        assert_eq!(windows, posix);
    }

    #[test]
    fn an_absent_default_path_leaves_the_sample_relative_to_the_instrument() {
        let resolved = resolve(Path::new("/lib"), "", "a.wav");
        assert_eq!(resolved, Path::new("/lib").join("a.wav"));
    }

    #[test]
    fn spaces_in_a_sample_name_survive_resolution() {
        let resolved = resolve(Path::new("/lib"), "Samples/", "HEADROOM PIANO LEVEL1 CLOSE 60.flac");
        assert!(
            resolved.to_string_lossy().contains("HEADROOM PIANO LEVEL1 CLOSE 60.flac"),
            "{resolved:?}"
        );
    }
}
