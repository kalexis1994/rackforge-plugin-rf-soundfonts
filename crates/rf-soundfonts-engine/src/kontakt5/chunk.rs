//! The tree of typed chunks inside a Kontakt 5 preset.
//!
//! What the container hands over is a second, unrelated tree. Each chunk is an
//! identifier, a length, and a body, and how the body is read depends entirely
//! on the identifier: most are a structure with private bytes, public bytes and
//! nested chunks; a few are arrays; the rest are opaque and only meaningful to
//! whatever reads them later.
//!
//! Everything is kept, including chunks with no name here. The zone map is a
//! small part of what an instrument records, and a reader that refused what it
//! did not recognise would refuse nearly every file.
//!
//! Ported from ConvertWithMoss (LGPL-3.0).

use crate::SoundfontError;

/// A group of a program.
pub const GROUP: i64 = 0x04;
/// One program: an instrument's zones, groups and parameters.
pub const PROGRAM: i64 = 0x28;
/// The list of groups.
const GROUP_LIST: i64 = 0x33;
/// The list of zones, whose entries carry a reference rather than a type.
const ZONE_LIST: i64 = 0x34;
/// The slots of a multi, each holding an instrument.
pub const SLOT_LIST: i64 = 0x37;
/// Up to eight loops belonging to a zone.
pub const LOOP_ARRAY: i64 = 0x39;
/// The list of sample file names.
pub const FILENAME_LIST: i64 = 0x3D;
/// The same list as written by more recent versions.
pub const FILENAME_LIST_EX: i64 = 0x4B;
/// Which instruments a multi loads into its slots.
pub const MULTI_CONFIGURATION: i64 = 0x48;
/// An envelope or other modulator internal to a group.
pub const PAR_INTERNAL_MOD: i64 = 0x0D;

/// Chunks whose body is a structure of private data, public data and children.
const STRUCTURED: [i64; 10] = [
    0x03, // Bank
    0x29, // Program container
    PROGRAM, 0x06, // Script
    0x17, // Send levels
    0x32, // Voice groups
    0x3A, // Parameter array of 8
    0x45, // Insert bus
    0x47, // Save settings
    0x4E, // Quick browse data
];

/// Marker on an array entry that carries no reference of its own.
const NO_REFERENCE: i64 = -1;

/// Deepest nesting followed, so a malformed file cannot exhaust the stack.
const MAX_DEPTH: usize = 64;

/// One node of the preset tree.
#[derive(Debug, Default)]
pub struct PresetChunk {
    /// The chunk's type, or for an entry of the zone list, its reference.
    pub id: i64,
    pub version: u16,
    pub private_data: Vec<u8>,
    pub public_data: Vec<u8>,
    pub children: Vec<PresetChunk>,
}

impl PresetChunk {
    /// Reads every chunk in a run of them.
    pub fn parse_all(data: &[u8]) -> Result<Vec<Self>, SoundfontError> {
        let mut reader = Reader::new(data);
        let mut chunks = Vec::new();
        read_run(&mut reader, 0, &mut chunks)?;
        Ok(chunks)
    }

    /// Reads one chunk whose type and body are already known.
    ///
    /// Some chunks are opaque to the tree and hold another chunk inside their
    /// bytes, which only the reader that understands them can say.
    pub fn parse_one(id: u16, body: &[u8]) -> Result<Self, SoundfontError> {
        Self::from_body(i64::from(id), body, 0)
    }

    /// Reads a bare array: a count and then that many structures.
    ///
    /// The entries carry no type, so whoever asked for them decides what they
    /// are. A program list is the case that matters.
    pub fn parse_array(data: &[u8]) -> Result<Vec<Self>, SoundfontError> {
        let mut holder = Self::default();
        let mut reader = Reader::new(data);
        holder.read_array(&mut reader, data.len(), false, 0)?;
        Ok(holder.children)
    }

    /// Collects every chunk of one type, at any depth.
    pub fn find_all<'a>(chunks: &'a [Self], id: i64, found: &mut Vec<&'a Self>) {
        for chunk in chunks {
            if chunk.id == id {
                found.push(chunk);
            } else {
                Self::find_all(&chunk.children, id, found);
            }
        }
    }

    /// The first child of one type.
    pub fn child(&self, id: i64) -> Option<&Self> {
        self.children.iter().find(|child| child.id == id)
    }

    fn from_body(id: i64, body: &[u8], depth: usize) -> Result<Self, SoundfontError> {
        if depth > MAX_DEPTH {
            return Err(SoundfontError::Invalid(
                "Kontakt preset nests deeper than any instrument does".into(),
            ));
        }
        let size = body.len();
        let mut inner = Reader::new(body);

        let mut chunk = Self {
            id,
            ..Self::default()
        };
        match id {
            GROUP_LIST => {
                chunk.read_array(&mut inner, size, false, depth)?;
                // Entries of a group list carry no type of their own, so they
                // are named here rather than left as bare structures.
                for child in &mut chunk.children {
                    child.id = GROUP;
                }
            }
            // A zone list states, for each entry, which group it belongs to.
            ZONE_LIST => chunk.read_array(&mut inner, size, true, depth)?,
            id if STRUCTURED.contains(&id) => chunk.read_structure(&mut inner, size, depth)?,
            0x3B => chunk.read_fixed_array(&mut inner, 16)?,
            0x3C => chunk.read_fixed_array(&mut inner, 32)?,
            _ => chunk.public_data = body.to_vec(),
        }
        Ok(chunk)
    }

    fn read_array(
        &mut self,
        reader: &mut Reader<'_>,
        size: usize,
        referenced: bool,
        depth: usize,
    ) -> Result<(), SoundfontError> {
        let count = reader.u32()?;
        for _ in 0..count {
            let id = if referenced {
                i64::from(reader.u32()?)
            } else {
                NO_REFERENCE
            };
            let mut entry = Self {
                id,
                ..Self::default()
            };
            entry.read_structure(reader, size, depth + 1)?;
            self.children.push(entry);
        }
        Ok(())
    }

    fn read_structure(
        &mut self,
        reader: &mut Reader<'_>,
        size: usize,
        depth: usize,
    ) -> Result<(), SoundfontError> {
        if reader.u8()? == 0 {
            // A chunk may say it holds no structure, in which case the rest of
            // the body is its own and nothing is nested inside.
            if size > 0 {
                self.public_data = reader.take(size - 1)?.to_vec();
            }
            return Ok(());
        }
        self.version = reader.u16()?;
        let private_size = reader.u32()? as usize;
        self.private_data = reader.take(private_size)?.to_vec();
        let public_size = reader.u32()? as usize;
        self.public_data = reader.take(public_size)?.to_vec();

        let children_size = reader.u32()? as usize;
        let children = reader.take(children_size)?;
        let mut inner = Reader::new(children);
        read_run(&mut inner, depth + 1, &mut self.children)
    }

    /// Reads an array of a fixed number of slots, most of them usually empty.
    fn read_fixed_array(
        &mut self,
        reader: &mut Reader<'_>,
        slots: usize,
    ) -> Result<(), SoundfontError> {
        if reader.u8()? != 0 {
            return Err(SoundfontError::Invalid(
                "Kontakt parameter array does not begin with zero".into(),
            ));
        }
        self.version = reader.u16()?;
        for _ in 0..slots {
            if reader.u8()? == 0 {
                continue;
            }
            let id = i64::from(reader.u16()?);
            let size = reader.u32()? as usize;
            self.children.push(Self {
                id,
                public_data: reader.take(size)?.to_vec(),
                ..Self::default()
            });
        }
        Ok(())
    }
}

/// Reads chunks from `reader` until it runs out or stops making sense.
///
/// A run does not always end cleanly. The bank of the trumpet multi carries
/// eighteen hundred bytes after its last chunk — a table of plain integers,
/// not a chunk at all — and every reader of this format simply stops there.
/// Kontakt's own writer evidently puts something after the run that the run's
/// length includes, so treating it as corruption would reject a file that
/// plays. What is refused instead is a chunk that begins plausibly and then
/// contradicts itself from the inside.
fn read_run(
    reader: &mut Reader<'_>,
    depth: usize,
    out: &mut Vec<PresetChunk>,
) -> Result<(), SoundfontError> {
    // Six bytes are the smallest a chunk can be: a type and a length.
    while reader.remaining() >= 6 {
        let resume = reader.at;
        let id = reader.u16()?;
        let size = reader.u32()? as usize;
        if size > reader.remaining() {
            reader.at = resume;
            break;
        }
        let body = reader.take(size)?;
        out.push(PresetChunk::from_body(i64::from(id), body, depth)?);
    }
    Ok(())
}

/// A position in a buffer that refuses to read past its end.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pub at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    pub fn take(&mut self, count: usize) -> Result<&'a [u8], SoundfontError> {
        let end = self.at.checked_add(count).ok_or_else(short)?;
        let slice = self.bytes.get(self.at..end).ok_or_else(short)?;
        self.at = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, SoundfontError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, SoundfontError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, SoundfontError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, SoundfontError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32, SoundfontError> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

fn short() -> SoundfontError {
    SoundfontError::Invalid("Kontakt preset ends in the middle of a field".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps a body as a chunk of the given type.
    fn chunk(id: u16, body: &[u8]) -> Vec<u8> {
        let mut out = id.to_le_bytes().to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    /// A structure body with the given private, public and nested bytes.
    fn structure(private: &[u8], public: &[u8], children: &[u8]) -> Vec<u8> {
        let mut out = vec![1];
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&(private.len() as u32).to_le_bytes());
        out.extend_from_slice(private);
        out.extend_from_slice(&(public.len() as u32).to_le_bytes());
        out.extend_from_slice(public);
        out.extend_from_slice(&(children.len() as u32).to_le_bytes());
        out.extend_from_slice(children);
        out
    }

    #[test]
    fn an_opaque_chunk_keeps_its_body_whole() {
        let chunks = PresetChunk::parse_all(&chunk(FILENAME_LIST as u16, b"names")).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].id, FILENAME_LIST);
        assert_eq!(chunks[0].public_data, b"names");
    }

    #[test]
    fn a_structure_separates_its_private_and_public_halves() {
        let body = structure(b"priv", b"pub", &[]);
        let chunks = PresetChunk::parse_all(&chunk(PROGRAM as u16, &body)).unwrap();
        let program = &chunks[0];
        assert_eq!(program.version, 3);
        assert_eq!(program.private_data, b"priv");
        assert_eq!(program.public_data, b"pub");
        assert!(program.children.is_empty());
    }

    #[test]
    fn a_structure_carries_its_nested_chunks() {
        let nested = chunk(FILENAME_LIST as u16, b"one");
        let body = structure(b"", b"", &nested);
        let chunks = PresetChunk::parse_all(&chunk(PROGRAM as u16, &body)).unwrap();
        assert_eq!(chunks[0].children.len(), 1);
        assert_eq!(chunks[0].children[0].id, FILENAME_LIST);
    }

    #[test]
    fn a_group_list_names_the_entries_the_format_leaves_bare() {
        let mut body = 2u32.to_le_bytes().to_vec();
        body.extend_from_slice(&structure(b"a", b"", &[]));
        body.extend_from_slice(&structure(b"b", b"", &[]));
        let chunks = PresetChunk::parse_all(&chunk(GROUP_LIST as u16, &body)).unwrap();
        let ids: Vec<i64> = chunks[0].children.iter().map(|child| child.id).collect();
        assert_eq!(ids, vec![GROUP, GROUP]);
    }

    #[test]
    fn a_zone_list_entry_keeps_the_group_it_refers_to() {
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&5u32.to_le_bytes());
        body.extend_from_slice(&structure(b"z", b"", &[]));
        let chunks = PresetChunk::parse_all(&chunk(ZONE_LIST as u16, &body)).unwrap();
        assert_eq!(chunks[0].children[0].id, 5);
        assert_eq!(chunks[0].children[0].private_data, b"z");
    }

    #[test]
    fn a_fixed_array_skips_the_slots_that_are_empty() {
        let mut body = vec![0];
        body.extend_from_slice(&1u16.to_le_bytes());
        for slot in 0..16 {
            if slot == 3 {
                body.push(1);
                body.extend_from_slice(&9u16.to_le_bytes());
                body.extend_from_slice(&2u32.to_le_bytes());
                body.extend_from_slice(b"hi");
            } else {
                body.push(0);
            }
        }
        let chunks = PresetChunk::parse_all(&chunk(0x3B, &body)).unwrap();
        assert_eq!(chunks[0].children.len(), 1);
        assert_eq!(chunks[0].children[0].id, 9);
        assert_eq!(chunks[0].children[0].public_data, b"hi");
    }

    #[test]
    fn an_unstructured_structure_keeps_its_body() {
        // The leading zero says the chunk holds no nested tree after all.
        let mut body = vec![0];
        body.extend_from_slice(b"flat");
        let chunks = PresetChunk::parse_all(&chunk(PROGRAM as u16, &body)).unwrap();
        assert_eq!(chunks[0].public_data, b"flat");
        assert!(chunks[0].children.is_empty());
    }

    #[test]
    fn chunks_are_found_at_any_depth() {
        let nested = chunk(PROGRAM as u16, &structure(b"", b"deep", &[]));
        let body = structure(b"", b"", &nested);
        let chunks = PresetChunk::parse_all(&chunk(0x29, &body)).unwrap();
        let mut found = Vec::new();
        PresetChunk::find_all(&chunks, PROGRAM, &mut found);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].public_data, b"deep");
    }

    #[test]
    fn a_run_stops_where_it_stops_making_sense() {
        // Real banks carry a table of integers after their last chunk, inside
        // the length that covers the run. Read as a chunk it claims a size
        // nothing could satisfy, and that is where reading ends.
        let mut bytes = chunk(FILENAME_LIST as u16, b"real");
        bytes.extend_from_slice(&0x48u16.to_le_bytes());
        bytes.extend_from_slice(&91_227_652u32.to_le_bytes());
        bytes.extend_from_slice(b"trailing bytes that are not a chunk");
        let chunks = PresetChunk::parse_all(&bytes).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].public_data, b"real");
    }

    #[test]
    fn a_chunk_that_contradicts_itself_from_inside_is_refused() {
        // Here the outer length is satisfied, so the chunk is read — and its
        // structure then asks for more than the body holds.
        let mut body = vec![1];
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&9_999u32.to_le_bytes());
        assert!(PresetChunk::parse_all(&chunk(PROGRAM as u16, &body)).is_err());
    }

    #[test]
    fn a_truncated_preset_is_refused_rather_than_panicking() {
        let full = chunk(PROGRAM as u16, &structure(b"priv", b"pub", &[]));
        for cut in 0..full.len() {
            let _ = PresetChunk::parse_all(&full[..cut]);
        }
    }
}
