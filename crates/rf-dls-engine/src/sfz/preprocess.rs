//! Macro and include expansion for SFZ documents.
//!
//! SFZ libraries of any size are written as templates rather than as flat
//! lists of regions. The Headroom Piano defines one region body once and
//! includes it sixty times per velocity layer per microphone, redefining
//! `$KEY` before each include:
//!
//! ```text
//! <region> #define $KEY 96 lokey=95 hikey=97 #include "Data/sample.txt"
//! ```
//!
//! Two consequences drive the design.
//!
//! Expansion has to run in **document order**, not as a gather-then-substitute
//! pass: the include on that line must observe the `$KEY` defined earlier on
//! the same line, and the next region redefines it. A macro table collected up
//! front would give every region the last key in the file.
//!
//! Included text has to keep its **line structure**. A `sample=` value runs to
//! the end of its line, so splicing a file's contents as one long line would
//! swallow whatever followed the include into the sample path.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::DlsError;

/// Includes may nest; a cycle would otherwise recurse until the stack ends.
const MAX_INCLUDE_DEPTH: usize = 16;

/// Expands `#define` and `#include` into a single self-contained document.
pub fn expand(root: &Path) -> Result<String, DlsError> {
    let base = root.parent().unwrap_or(Path::new(".")).to_path_buf();
    let source = read(root)?;
    let mut macros = BTreeMap::new();
    let mut output = String::with_capacity(source.len() * 2);
    expand_into(&source, &base, &mut macros, &mut output, 0)?;
    Ok(output)
}

/// Expands text that is already in memory, resolving includes against `base`.
pub fn expand_text(
    source: &str,
    base: &Path,
    macros: &mut BTreeMap<String, String>,
) -> Result<String, DlsError> {
    let mut output = String::with_capacity(source.len() * 2);
    expand_into(source, base, macros, &mut output, 0)?;
    Ok(output)
}

fn read(path: &Path) -> Result<String, DlsError> {
    let bytes = fs::read(path).map_err(|source| DlsError::Read {
        path: path.display().to_string(),
        source,
    })?;
    // Libraries are authored on every platform and are not reliably UTF-8;
    // a stray byte in a comment must not cost the whole instrument.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn expand_into(
    source: &str,
    base: &Path,
    macros: &mut BTreeMap<String, String>,
    output: &mut String,
    depth: usize,
) -> Result<(), DlsError> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(DlsError::Invalid(format!(
            "SFZ includes nest deeper than {MAX_INCLUDE_DEPTH} levels"
        )));
    }
    for line in source.lines() {
        expand_line(line, base, macros, output, depth)?;
        output.push('\n');
    }
    Ok(())
}

fn expand_line(
    line: &str,
    base: &Path,
    macros: &mut BTreeMap<String, String>,
    output: &mut String,
    depth: usize,
) -> Result<(), DlsError> {
    let line = strip_comment(line);
    let mut rest = line;
    loop {
        let Some(directive) = next_directive(rest) else {
            output.push_str(&substitute(rest, macros));
            return Ok(());
        };
        output.push_str(&substitute(&rest[..directive.start], macros));
        match directive.kind {
            Directive::Define => {
                let after = &rest[directive.end..];
                let (name, after) = take_token(after);
                let (value, after) = take_token(after);
                if name.is_empty() {
                    return Err(DlsError::Invalid("#define is missing a macro name".into()));
                }
                // The value is substituted now so a macro defined in terms of
                // another resolves against what was in scope at definition.
                macros.insert(name.to_string(), substitute(value, macros));
                rest = after;
            }
            Directive::Include => {
                let after = &rest[directive.end..];
                let (path, after) = take_quoted(after).ok_or_else(|| {
                    DlsError::Invalid("#include is missing a quoted path".into())
                })?;
                let resolved = resolve(base, &substitute(path, macros));
                let included = read(&resolved)?;
                // A newline before and after keeps the included file's own line
                // structure intact, which is what terminates a `sample=` value.
                output.push('\n');
                expand_into(&included, base, macros, output, depth + 1)?;
                rest = after;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Directive {
    Define,
    Include,
}

struct Found {
    kind: Directive,
    start: usize,
    end: usize,
}

fn next_directive(text: &str) -> Option<Found> {
    let define = text.find("#define").map(|start| Found {
        kind: Directive::Define,
        start,
        end: start + "#define".len(),
    });
    let include = text.find("#include").map(|start| Found {
        kind: Directive::Include,
        start,
        end: start + "#include".len(),
    });
    match (define, include) {
        (Some(define), Some(include)) if include.start < define.start => Some(include),
        (Some(define), _) => Some(define),
        (None, include) => include,
    }
}

/// Removes a `//` comment, respecting that `//` also opens a URL-like path.
fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(index) => &line[..index],
        None => line,
    }
}

fn take_token(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start();
    match trimmed.find(char::is_whitespace) {
        Some(index) => (&trimmed[..index], &trimmed[index..]),
        None => (trimmed, ""),
    }
}

fn take_quoted(text: &str) -> Option<(&str, &str)> {
    let start = text.find('"')?;
    let rest = &text[start + 1..];
    let end = rest.find('"')?;
    Some((&rest[..end], &rest[end + 1..]))
}

/// Resolves an include path, accepting the backslashes Windows-authored
/// libraries write and the host may not.
fn resolve(base: &Path, path: &str) -> PathBuf {
    let normalised = path.replace('\\', "/");
    base.join(normalised)
}

/// Replaces every `$NAME` occurrence, longest name first.
///
/// Longest-first matters: `$VEL` is a prefix of `$VELTRACK`, and substituting
/// the shorter one first would leave `LEVEL1TRACK` behind.
fn substitute(text: &str, macros: &BTreeMap<String, String>) -> String {
    if !text.contains('$') || macros.is_empty() {
        return text.to_string();
    }
    let mut names: Vec<&String> = macros.keys().collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    let mut result = text.to_string();
    for name in names {
        if result.contains(name.as_str()) {
            result = result.replace(name.as_str(), &macros[name]);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(source: &str) -> String {
        let mut macros = BTreeMap::new();
        expand_text(source, Path::new("."), &mut macros).unwrap()
    }

    #[test]
    fn a_macro_is_substituted_after_it_is_defined() {
        let output = expand_str("#define $KEY 60\npitch_keycenter=$KEY");
        assert!(output.contains("pitch_keycenter=60"), "{output}");
    }

    #[test]
    fn a_macro_redefined_later_applies_only_from_that_point() {
        // The whole reason expansion runs in document order: each region in a
        // real library redefines $KEY before including the shared body.
        let output = expand_str(
            "#define $KEY 21 a=$KEY\n#define $KEY 96 b=$KEY",
        );
        assert!(output.contains("a=21"), "{output}");
        assert!(output.contains("b=96"), "{output}");
    }

    #[test]
    fn a_definition_takes_effect_within_the_same_line() {
        let output = expand_str("<region> #define $KEY 96 lokey=$KEY hikey=$KEY");
        assert!(output.contains("lokey=96"), "{output}");
        assert!(output.contains("hikey=96"), "{output}");
    }

    #[test]
    fn a_longer_macro_name_wins_over_a_prefix_of_it() {
        // $VEL is a prefix of $VELTRACK in the Headroom Piano.
        let output = expand_str("#define $VEL LEVEL1\n#define $VELTRACK 73\na=$VELTRACK b=$VEL");
        assert!(output.contains("a=73"), "{output}");
        assert!(output.contains("b=LEVEL1"), "{output}");
    }

    #[test]
    fn macros_expand_inside_a_file_name() {
        let output = expand_str(
            "#define $VEL LEVEL1\n#define $MIC CLOSE\n#define $KEY 60\n#define $EXT flac\n\
             sample=HEADROOM PIANO $VEL $MIC $KEY.$EXT",
        );
        assert!(
            output.contains("sample=HEADROOM PIANO LEVEL1 CLOSE 60.flac"),
            "{output}"
        );
    }

    #[test]
    fn a_comment_is_removed_before_substitution() {
        let output = expand_str("#define $KEY 60\nlokey=$KEY // $KEY is the root");
        assert!(output.contains("lokey=60"), "{output}");
        assert!(!output.contains("root"), "{output}");
    }

    #[test]
    fn text_without_macros_survives_untouched() {
        let output = expand_str("<region> lokey=0 hikey=127");
        assert!(output.contains("<region> lokey=0 hikey=127"), "{output}");
    }

    #[test]
    fn a_define_without_a_name_is_refused() {
        let mut macros = BTreeMap::new();
        assert!(expand_text("#define", Path::new("."), &mut macros).is_err());
    }

    #[test]
    fn an_include_without_a_path_is_refused() {
        let mut macros = BTreeMap::new();
        assert!(expand_text("#include", Path::new("."), &mut macros).is_err());
    }

    #[test]
    fn an_include_splices_the_file_and_keeps_its_lines() {
        let directory = tempdir();
        fs::write(directory.join("body.txt"), "sample=piano.wav\nvolume=-3").unwrap();
        let mut macros = BTreeMap::new();
        let output = expand_text(
            "<region> #include \"body.txt\" pan=20",
            &directory,
            &mut macros,
        )
        .unwrap();
        // `pan=20` must not end up inside the sample path.
        let sample_line = output
            .lines()
            .find(|line| line.contains("sample="))
            .unwrap();
        assert!(!sample_line.contains("pan=20"), "{output}");
        assert!(output.contains("pan=20"), "{output}");
    }

    #[test]
    fn an_include_observes_a_macro_defined_earlier_on_the_same_line() {
        let directory = tempdir();
        fs::write(directory.join("body.txt"), "sample=note $KEY.flac").unwrap();
        let mut macros = BTreeMap::new();
        let output = expand_text(
            "<region> #define $KEY 96 #include \"body.txt\"",
            &directory,
            &mut macros,
        )
        .unwrap();
        assert!(output.contains("sample=note 96.flac"), "{output}");
    }

    #[test]
    fn a_backslash_include_path_resolves_on_any_platform() {
        let directory = tempdir();
        fs::create_dir_all(directory.join("Data")).unwrap();
        fs::write(directory.join("Data/body.txt"), "volume=0").unwrap();
        let mut macros = BTreeMap::new();
        let output =
            expand_text("#include \"Data\\body.txt\"", &directory, &mut macros).unwrap();
        assert!(output.contains("volume=0"), "{output}");
    }

    #[test]
    fn a_self_including_file_is_refused_rather_than_overflowing_the_stack() {
        let directory = tempdir();
        fs::write(directory.join("loop.txt"), "#include \"loop.txt\"").unwrap();
        let mut macros = BTreeMap::new();
        let error = expand_text("#include \"loop.txt\"", &directory, &mut macros).unwrap_err();
        assert!(error.to_string().contains("nest"), "{error}");
    }

    /// Expands a library the user supplies locally.
    ///
    /// Ignored by default and driven by an environment variable because no
    /// third-party instrument is committed here, for the same reason no ROM
    /// is. Synthetic tests prove the rules; this proves the rules were the
    /// right ones, against a document nobody on this project wrote.
    ///
    /// ```text
    /// RF_DLS_SFZ="/path/to/Headroom Piano (NoFX).sfz" cargo test -- --ignored
    /// ```
    #[test]
    #[ignore = "requires a locally supplied SFZ library"]
    fn expands_a_real_library() {
        let Ok(path) = std::env::var("RF_DLS_SFZ") else {
            panic!("set RF_DLS_SFZ to an .sfz file");
        };
        let expanded = expand(Path::new(&path)).unwrap();
        assert!(!expanded.contains('$'), "unexpanded macros remain");
        assert!(
            !expanded.contains("#include"),
            "unresolved include remains"
        );
        let regions = expanded.matches("<region>").count();
        let samples = expanded.matches("sample=").count();
        assert!(regions > 0, "no regions were produced");
        assert_eq!(
            regions, samples,
            "every region must resolve exactly one sample"
        );
        eprintln!("expanded {regions} regions from {path}");
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "rf-dls-sfz-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }
}
