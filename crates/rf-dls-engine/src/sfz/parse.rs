//! Turning an expanded SFZ document into merged opcode maps, one per region.
//!
//! The format has no separators: headers and opcodes run together on a line,
//! and a value ends only where the next opcode begins. That rule is what makes
//! both of these parse correctly from the same scanner:
//!
//! ```text
//! sample=HEADROOM PIANO LEVEL1 CLOSE 96.flac
//! region_label=96 ampeg_release_oncc67=10
//! ```
//!
//! The first value keeps its spaces; the second stops before `ampeg_…`. A
//! scanner that split on whitespace would truncate every sample path that
//! contains a space, and one that ran to end of line would swallow the opcode
//! following a label.

use std::collections::BTreeMap;

use crate::DlsError;

/// Opcodes gathered for one scope, in name order.
pub type OpcodeMap = BTreeMap<String, String>;

/// A `<curve>` block, indexed by `curve_index`.
#[derive(Clone, Debug, Default)]
pub struct Curve {
    /// Control points by input step, `v000` through `v127`.
    pub points: BTreeMap<u8, f32>,
}

impl Curve {
    /// Evaluates the curve at a normalised input, interpolating between the
    /// points the author supplied.
    ///
    /// Real curves are sparse: a library commonly gives only `v000` and `v127`
    /// and expects a straight line between them.
    pub fn value(&self, position: f32) -> f32 {
        if self.points.is_empty() {
            return position;
        }
        let step = (position.clamp(0.0, 1.0) * 127.0).round() as u8;
        if let Some(exact) = self.points.get(&step) {
            return *exact;
        }
        let below = self.points.range(..step).next_back();
        let above = self.points.range(step..).next();
        match (below, above) {
            (Some((low, low_value)), Some((high, high_value))) => {
                let span = f32::from(*high - *low);
                let offset = f32::from(step - *low);
                low_value + (high_value - low_value) * (offset / span)
            }
            (Some((_, value)), None) | (None, Some((_, value))) => *value,
            (None, None) => position,
        }
    }
}

/// An SFZ document reduced to what the engine needs.
#[derive(Clone, Debug, Default)]
pub struct SfzDocument {
    /// `<control>` opcodes, notably `default_path` and the `set_cc` defaults.
    pub control: OpcodeMap,
    /// One fully merged map per `<region>`, ancestors already folded in.
    pub regions: Vec<OpcodeMap>,
    /// Author-defined curves by index.
    pub curves: BTreeMap<u32, Curve>,
}

/// Scopes an opcode can belong to, outermost first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Control,
    Global,
    Master,
    Group,
    Region,
    Curve,
    /// A header this engine does not model; its opcodes are discarded rather
    /// than leaking into the enclosing scope.
    Ignored,
}

fn scope_of(header: &str) -> Scope {
    match header {
        "control" => Scope::Control,
        "global" => Scope::Global,
        "master" => Scope::Master,
        "group" => Scope::Group,
        "region" => Scope::Region,
        "curve" => Scope::Curve,
        _ => Scope::Ignored,
    }
}

/// Parses an already-expanded document.
pub fn parse(source: &str) -> Result<SfzDocument, DlsError> {
    let mut document = SfzDocument::default();
    let mut global = OpcodeMap::new();
    let mut master = OpcodeMap::new();
    let mut group = OpcodeMap::new();
    let mut region = OpcodeMap::new();
    let mut curve = Curve::default();
    let mut curve_index: Option<u32> = None;
    let mut scope = Scope::Global;

    // Closing a scope has to happen when the *next* header arrives, because a
    // region is only complete once something else starts.
    macro_rules! flush {
        ($scope:expr) => {
            match $scope {
                Scope::Region => {
                    if !region.is_empty() {
                        let mut merged = global.clone();
                        merged.extend(master.clone());
                        merged.extend(group.clone());
                        merged.extend(std::mem::take(&mut region));
                        document.regions.push(merged);
                    }
                }
                Scope::Curve => {
                    if let Some(index) = curve_index.take() {
                        document.curves.insert(index, std::mem::take(&mut curve));
                    }
                }
                _ => {}
            }
        };
    }

    for item in scan(source) {
        match item {
            Item::Header(name) => {
                flush!(scope);
                let next = scope_of(&name);
                match next {
                    // Each level resets everything nested inside it.
                    Scope::Global => {
                        global.clear();
                        master.clear();
                        group.clear();
                    }
                    Scope::Master => {
                        master.clear();
                        group.clear();
                    }
                    Scope::Group => group.clear(),
                    Scope::Curve => curve = Curve::default(),
                    _ => {}
                }
                scope = next;
            }
            Item::Opcode { name, value } => match scope {
                Scope::Control => {
                    document.control.insert(name, value);
                }
                Scope::Global => {
                    global.insert(name, value);
                }
                Scope::Master => {
                    master.insert(name, value);
                }
                Scope::Group => {
                    group.insert(name, value);
                }
                Scope::Region => {
                    region.insert(name, value);
                }
                Scope::Curve => {
                    if name == "curve_index" {
                        curve_index = value.parse().ok();
                    } else if let Some(step) = name.strip_prefix('v')
                        && let Ok(step) = step.parse::<u8>()
                        && let Ok(point) = value.parse::<f32>()
                    {
                        curve.points.insert(step, point);
                    }
                }
                Scope::Ignored => {}
            },
        }
    }
    flush!(scope);

    if document.regions.is_empty() {
        return Err(DlsError::Invalid(
            "SFZ document declares no regions".into(),
        ));
    }
    Ok(document)
}

enum Item {
    Header(String),
    Opcode { name: String, value: String },
}

/// Splits a document into headers and opcodes.
///
/// Works one line at a time because no SFZ value spans a newline, which is
/// exactly the property the preprocessor preserves when splicing includes.
fn scan(source: &str) -> Vec<Item> {
    let mut items = Vec::new();
    for line in source.lines() {
        let bytes = line.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
                continue;
            }
            if bytes[cursor] == b'<' {
                let Some(close) = line[cursor..].find('>') else {
                    break;
                };
                let header = line[cursor + 1..cursor + close].trim().to_ascii_lowercase();
                items.push(Item::Header(header));
                cursor += close + 1;
                continue;
            }
            let Some(name_end) = opcode_name_end(bytes, cursor) else {
                // Not an opcode start; skip this token entirely rather than
                // letting stray text be read as a value.
                while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                continue;
            };
            let name = line[cursor..name_end].to_ascii_lowercase();
            let value_start = name_end + 1;
            let value_end = value_end(bytes, value_start);
            let value = line[value_start..value_end].trim().to_string();
            items.push(Item::Opcode { name, value });
            cursor = value_end;
        }
    }
    items
}

/// Returns the index of the `=` ending an opcode name starting at `start`.
fn opcode_name_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index < bytes.len() && is_name_byte(bytes[index]) {
        index += 1;
    }
    if index > start && index < bytes.len() && bytes[index] == b'=' {
        Some(index)
    } else {
        None
    }
}

/// Finds where a value ends: at the next opcode, the next header, or the line.
fn value_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    let mut candidate = None;
    while index < bytes.len() {
        if bytes[index] == b'<' {
            return candidate.unwrap_or(index);
        }
        if bytes[index].is_ascii_whitespace() {
            candidate = Some(index);
            index += 1;
            continue;
        }
        // A name immediately followed by `=` begins the next opcode, and the
        // value ended at the whitespace before it.
        if candidate.is_some()
            && is_name_byte(bytes[index])
            && let Some(end) = opcode_name_end(bytes, index)
        {
            let _ = end;
            return candidate.unwrap();
        }
        index += 1;
    }
    bytes.len()
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_keeps_the_spaces_inside_a_file_name() {
        let document = parse("<region> sample=HEADROOM PIANO LEVEL1 CLOSE 96.flac").unwrap();
        assert_eq!(
            document.regions[0]["sample"],
            "HEADROOM PIANO LEVEL1 CLOSE 96.flac"
        );
    }

    #[test]
    fn a_value_stops_before_the_next_opcode_on_the_same_line() {
        let document = parse("<region> sample=a.wav\nregion_label=96 volume=-3").unwrap();
        assert_eq!(document.regions[0]["region_label"], "96");
        assert_eq!(document.regions[0]["volume"], "-3");
    }

    #[test]
    fn a_region_inherits_from_group_master_and_global() {
        let document = parse(
            "<global> volume=-6\n<master> pan=10\n<group> lovel=1\n<region> sample=a.wav",
        )
        .unwrap();
        let region = &document.regions[0];
        assert_eq!(region["volume"], "-6");
        assert_eq!(region["pan"], "10");
        assert_eq!(region["lovel"], "1");
    }

    #[test]
    fn a_region_overrides_what_it_inherits() {
        let document =
            parse("<group> volume=-6\n<region> sample=a.wav volume=0").unwrap();
        assert_eq!(document.regions[0]["volume"], "0");
    }

    #[test]
    fn a_new_group_forgets_the_previous_one() {
        let document = parse(
            "<group> lovel=1 hivel=59\n<region> sample=a.wav\n<group> lovel=60\n<region> sample=b.wav",
        )
        .unwrap();
        assert_eq!(document.regions[1]["lovel"], "60");
        assert!(
            !document.regions[1].contains_key("hivel"),
            "the second group inherited the first group's velocity ceiling"
        );
    }

    #[test]
    fn a_new_master_forgets_its_groups() {
        let document = parse(
            "<master> group=1\n<group> lovel=1\n<region> sample=a.wav\n\
             <master> group=2\n<region> sample=b.wav",
        )
        .unwrap();
        assert_eq!(document.regions[1]["group"], "2");
        assert!(
            !document.regions[1].contains_key("lovel"),
            "a group outlived the master that contained it"
        );
    }

    #[test]
    fn global_survives_across_masters() {
        let document = parse(
            "<global> volume=-6\n<master> group=1\n<region> sample=a.wav\n\
             <master> group=2\n<region> sample=b.wav",
        )
        .unwrap();
        assert_eq!(document.regions[1]["volume"], "-6");
    }

    #[test]
    fn control_opcodes_stay_out_of_the_regions() {
        let document =
            parse("<control> default_path=Samples/\n<region> sample=a.wav").unwrap();
        assert_eq!(document.control["default_path"], "Samples/");
        assert!(!document.regions[0].contains_key("default_path"));
    }

    #[test]
    fn an_unknown_header_does_not_leak_into_the_next_region() {
        let document = parse("<effect> type=reverb\n<region> sample=a.wav").unwrap();
        assert!(!document.regions[0].contains_key("type"));
    }

    #[test]
    fn headers_and_opcodes_share_a_line() {
        let document =
            parse("<region> lokey=95 hikey=97 <region> lokey=98 hikey=100").unwrap();
        assert_eq!(document.regions.len(), 2);
        assert_eq!(document.regions[1]["lokey"], "98");
    }

    #[test]
    fn opcode_names_are_case_insensitive() {
        let document = parse("<REGION> LoKey=60 SAMPLE=a.wav").unwrap();
        assert_eq!(document.regions[0]["lokey"], "60");
    }

    #[test]
    fn a_curve_is_collected_by_its_index() {
        let document =
            parse("<curve> curve_index=7 v000=1 v127=0\n<region> sample=a.wav").unwrap();
        let curve = &document.curves[&7];
        assert_eq!(curve.value(0.0), 1.0);
        assert_eq!(curve.value(1.0), 0.0);
    }

    #[test]
    fn a_sparse_curve_interpolates_between_its_points() {
        let mut curve = Curve::default();
        curve.points.insert(0, 0.0);
        curve.points.insert(127, 1.0);
        assert!((curve.value(0.5) - 0.5).abs() < 0.01);
    }

    #[test]
    fn a_curve_without_points_is_the_identity() {
        assert_eq!(Curve::default().value(0.25), 0.25);
    }

    #[test]
    fn a_document_without_regions_is_refused() {
        assert!(parse("<global> volume=0").is_err());
    }

    #[test]
    fn comments_and_blank_lines_are_tolerated() {
        let document = parse("\n\n<region> sample=a.wav\n\n").unwrap();
        assert_eq!(document.regions.len(), 1);
    }

    /// Parses a library the user supplies locally and reports its vocabulary.
    ///
    /// Prints the opcodes actually present so the mapping layer is built from
    /// what libraries write, not from what the specification lists. No
    /// third-party instrument is committed here.
    ///
    /// ```text
    /// RF_DLS_SFZ="/path/to/instrument.sfz" cargo test -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a locally supplied SFZ library"]
    fn parses_a_real_library() {
        let path = std::env::var("RF_DLS_SFZ").expect("set RF_DLS_SFZ to an .sfz file");
        let expanded = super::super::preprocess::expand(std::path::Path::new(&path)).unwrap();
        let document = parse(&expanded).unwrap();

        let mut vocabulary: BTreeMap<&str, usize> = BTreeMap::new();
        for region in &document.regions {
            for name in region.keys() {
                *vocabulary.entry(name.as_str()).or_default() += 1;
            }
        }
        eprintln!("regions: {}", document.regions.len());
        eprintln!("curves: {:?}", document.curves.keys().collect::<Vec<_>>());
        eprintln!("control: {:?}", document.control.keys().collect::<Vec<_>>());
        for (name, count) in &vocabulary {
            eprintln!("  {name} ({count})");
        }
        assert!(document.regions.iter().all(|region| {
            region.contains_key("sample")
        }));
    }
}
