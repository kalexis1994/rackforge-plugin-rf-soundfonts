//! The container Kontakt 5 and later wrap their presets in.
//!
//! Where a Kontakt 2 instrument is a header and a zlib stream holding XML, a
//! Kontakt 5 one is a tree. Each node is a length-prefixed block introduced by
//! the ASCII tag `hsin`, carrying a UUID, a stack of typed chunks, and any
//! number of child nodes. The instrument itself is one chunk deep inside that,
//! and the branches on the way are usually FastLZ-compressed.
//!
//! The chunk stack nests rather than following one after another: a chunk's
//! block begins with the whole of the next chunk, and only what remains after
//! that belongs to the chunk itself. The stack ends at a terminator. It is read
//! into a plain list here, because nothing downstream cares how it was folded.
//!
//! Only the two payloads that lead somewhere are interpreted — the compressed
//! subtree and the preset — and everything else is kept as bytes. A format this
//! large will always have chunks nobody has named yet, and refusing a file over
//! one of them would mean refusing instruments that play perfectly well.
//!
//! Ported from ConvertWithMoss (LGPL-3.0).

use crate::SoundfontError;

/// Tag every node begins with.
const DOMAIN: &[u8; 4] = b"hsin";

/// Chunk type ending a stack.
const TERMINATOR: u32 = 1;

/// Chunk type holding a Kontakt preset.
pub const PRESET_CHUNK_ITEM: u32 = 109;

/// Chunk type holding a nested, usually compressed, tree.
const SUB_TREE_ITEM: u32 = 115;

/// Chunk type naming the application that wrote the file.
pub const AUTHORING_APPLICATION: u32 = 101;

/// Value closing a preset chunk, checked to confirm it was read whole.
const PRESET_MAGIC: u32 = 0x8565_620D;

/// Deepest nesting followed before giving up.
///
/// Real instruments nest a handful of levels. This is only here so that a
/// malformed file describing a tree inside itself is refused rather than
/// running the stack out.
const MAX_DEPTH: usize = 32;

/// Largest block accepted, as a guard against a corrupt length.
const MAX_BLOCK_BYTES: u64 = 512 * 1_048_576;

/// One node of the container tree.
#[derive(Debug)]
pub struct Item {
    pub version: u32,
    /// The node's chunk stack, in the order it unfolds.
    pub chunks: Vec<Chunk>,
    pub children: Vec<Item>,
}

/// One typed payload within a node.
#[derive(Debug)]
pub struct Chunk {
    pub kind: u32,
    pub payload: Payload,
}

#[derive(Debug)]
pub enum Payload {
    /// A nested tree, already decompressed.
    SubTree(Box<Item>),
    /// A nested tree belonging to a Player library, sealed with a key that
    /// belongs to its publisher. Recorded rather than treated as an error, so
    /// an instrument whose other branches are readable still loads.
    Encrypted,
    /// A Kontakt preset, to be read by [`super::chunk`].
    Preset(Vec<u8>),
    /// Anything not interpreted here.
    Raw(Vec<u8>),
}

impl Item {
    /// Reads the tree rooted at the start of `bytes`.
    pub fn parse(bytes: &[u8]) -> Result<Self, SoundfontError> {
        let mut reader = Reader::new(bytes);
        let block = reader.block()?;
        Self::from_block(block, 0)
    }

    /// The first chunk of the given type anywhere in the tree.
    pub fn find(&self, kind: u32) -> Option<&Chunk> {
        for chunk in &self.chunks {
            if chunk.kind == kind {
                return Some(chunk);
            }
            if let Payload::SubTree(subtree) = &chunk.payload
                && let Some(found) = subtree.find(kind)
            {
                return Some(found);
            }
        }
        self.children.iter().find_map(|child| child.find(kind))
    }

    /// Whether any branch of the tree could not be opened.
    pub fn has_encrypted_branch(&self) -> bool {
        self.chunks.iter().any(|chunk| match &chunk.payload {
            Payload::Encrypted => true,
            Payload::SubTree(subtree) => subtree.has_encrypted_branch(),
            _ => false,
        }) || self.children.iter().any(Self::has_encrypted_branch)
    }

    fn from_block(block: &[u8], depth: usize) -> Result<Self, SoundfontError> {
        if depth > MAX_DEPTH {
            return Err(SoundfontError::Invalid(
                "Kontakt container nests deeper than any instrument does".into(),
            ));
        }
        let mut reader = Reader::new(block);

        let header_version = reader.u32()?;
        if header_version != 1 {
            return Err(SoundfontError::Unsupported(format!(
                "Kontakt container node version {header_version} is not read here"
            )));
        }
        if reader.take(4)? != DOMAIN {
            return Err(SoundfontError::Invalid(
                "Kontakt container node does not begin with \"hsin\"".into(),
            ));
        }
        reader.u32()?;
        reader.u32()?;
        reader.take(16)?;

        let mut chunks = Vec::new();
        read_chunk_stack(&mut reader, &mut chunks, depth)?;

        let version = reader.u32()?;
        let count = reader.u32()?;
        let mut children = Vec::with_capacity(count.min(1_024) as usize);
        for _ in 0..count {
            // A child is introduced by its index, a domain tag and a type
            // before its node begins. None of the three is needed: the node
            // states its own kind through the chunks it carries.
            reader.u32()?;
            reader.take(4)?;
            reader.u32()?;
            // The node announces its own length, so a count that overstates the
            // truth runs out of bytes rather than reading into whatever
            // follows.
            let child = reader.block()?;
            children.push(Self::from_block(child, depth + 1)?);
        }

        Ok(Self {
            version,
            chunks,
            children,
        })
    }
}

/// Unfolds one chunk and, nested inside it, all those that follow.
fn read_chunk_stack(
    reader: &mut Reader<'_>,
    chunks: &mut Vec<Chunk>,
    depth: usize,
) -> Result<(), SoundfontError> {
    if depth > MAX_DEPTH {
        return Err(SoundfontError::Invalid(
            "Kontakt chunk stack is deeper than any instrument's".into(),
        ));
    }
    let block = reader.block()?;
    let mut inner = Reader::new(block);

    inner.take(4)?;
    let kind = inner.u32()?;
    let version = inner.u32()?;
    if version != 1 {
        return Err(SoundfontError::Unsupported(format!(
            "Kontakt chunk version {version} is not read here"
        )));
    }

    // The rest of the stack sits inside this chunk, ahead of its own data, so
    // it is taken first and the chunks come out in the order they unfold.
    if kind != TERMINATOR {
        read_chunk_stack(&mut inner, chunks, depth + 1)?;
    }

    let payload = match kind {
        SUB_TREE_ITEM => read_subtree(&mut inner, depth)?,
        PRESET_CHUNK_ITEM => Payload::Preset(read_preset(&mut inner)?),
        _ => Payload::Raw(inner.rest().to_vec()),
    };
    chunks.push(Chunk { kind, payload });
    Ok(())
}

fn read_subtree(reader: &mut Reader<'_>, depth: usize) -> Result<Payload, SoundfontError> {
    reader.u32()?;
    let compressed = reader.u8()? > 0;
    if !compressed {
        let block = reader.block()?;
        return Ok(Payload::SubTree(Box::new(Item::from_block(
            block,
            depth + 1,
        )?)));
    }

    let expected = reader.u32()? as usize;
    let packed = reader.u32()? as usize;
    let body = reader.take(packed)?;
    // A Player library encrypts its subtree, and what reaches here is then not
    // a FastLZ stream at all. Failing to expand is how that is recognised:
    // nothing in the container says which it is.
    let Ok(expanded) = crate::fastlz::decompress(body, expected) else {
        return Ok(Payload::Encrypted);
    };
    let mut inner = Reader::new(&expanded);
    let block = inner.block()?;
    Ok(Payload::SubTree(Box::new(Item::from_block(
        block,
        depth + 1,
    )?)))
}

fn read_preset(reader: &mut Reader<'_>) -> Result<Vec<u8>, SoundfontError> {
    reader.u32()?;
    reader.u32()?;
    let items = reader.u32()?;
    if items != 1 {
        return Err(SoundfontError::Invalid(format!(
            "Kontakt preset chunk holds {items} entries, not one"
        )));
    }
    let size = reader.u32()? as usize;
    reader.u32()?;
    let data = reader.take(size)?.to_vec();
    reader.u32()?;
    let magic = reader.u32()?;
    if magic != PRESET_MAGIC {
        return Err(SoundfontError::Invalid(format!(
            "Kontakt preset chunk ends with {magic:#010x}, not {PRESET_MAGIC:#010x}"
        )));
    }
    Ok(data)
}

/// A position in a buffer that refuses to read past its end.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], SoundfontError> {
        let end = self.at.checked_add(count).ok_or_else(short)?;
        let slice = self.bytes.get(self.at..end).ok_or_else(short)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, SoundfontError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SoundfontError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, SoundfontError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn rest(&mut self) -> &'a [u8] {
        let slice = &self.bytes[self.at..];
        self.at = self.bytes.len();
        slice
    }

    /// Reads a block whose leading length counts itself.
    fn block(&mut self) -> Result<&'a [u8], SoundfontError> {
        let size = self.u64()?;
        if !(8..=MAX_BLOCK_BYTES).contains(&size) {
            return Err(SoundfontError::Invalid(format!(
                "Kontakt container block claims {size} bytes"
            )));
        }
        self.take(size as usize - 8)
    }
}

fn short() -> SoundfontError {
    SoundfontError::Invalid("Kontakt container ends in the middle of a field".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps `body` in the length prefix a block carries.
    fn block(body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u64).to_le_bytes().to_vec();
        out.extend_from_slice(body);
        out
    }

    /// The chunk stack of a node holding a single terminator.
    fn terminator() -> Vec<u8> {
        let mut chunk = b"hsin".to_vec();
        chunk.extend_from_slice(&TERMINATOR.to_le_bytes());
        chunk.extend_from_slice(&1u32.to_le_bytes());
        block(&chunk)
    }

    /// A node with the given chunk stack and children.
    fn node_with(stack: Vec<u8>, children: Vec<Vec<u8>>) -> Vec<u8> {
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(DOMAIN);
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&[0; 16]);
        body.extend_from_slice(&stack);
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(&(children.len() as u32).to_le_bytes());
        for (index, child) in children.into_iter().enumerate() {
            body.extend_from_slice(&(index as u32).to_le_bytes());
            body.extend_from_slice(b"DSIN");
            body.extend_from_slice(&117u32.to_le_bytes());
            body.extend_from_slice(&child);
        }
        block(&body)
    }

    /// A node with the given chunk stack and no children.
    fn node(stack: Vec<u8>) -> Vec<u8> {
        node_with(stack, Vec::new())
    }

    #[test]
    fn a_bare_node_carries_its_version_and_terminator() {
        let item = Item::parse(&node(terminator())).unwrap();
        assert_eq!(item.version, 7);
        assert_eq!(item.chunks.len(), 1);
        assert_eq!(item.chunks[0].kind, TERMINATOR);
        assert!(item.children.is_empty());
    }

    #[test]
    fn a_child_is_found_behind_its_index_and_tag() {
        // A child node is preceded by three fields the format inserts before
        // the node itself. Reading the node without stepping over them lands
        // in the middle of nothing.
        let tree = node_with(terminator(), vec![node(terminator())]);
        let item = Item::parse(&tree).unwrap();
        assert_eq!(item.children.len(), 1);
        assert_eq!(item.children[0].version, 7);
    }

    #[test]
    fn a_node_not_tagged_hsin_is_refused() {
        let mut body = node(terminator());
        body[12..16].copy_from_slice(b"nope");
        let error = Item::parse(&body).unwrap_err();
        assert!(error.to_string().contains("hsin"), "{error}");
    }

    #[test]
    fn the_chunk_stack_unfolds_in_order() {
        // A preset chunk whose block contains the terminator ahead of its own
        // data, which is how the format folds a stack.
        let mut stack = b"hsin".to_vec();
        stack.extend_from_slice(&PRESET_CHUNK_ITEM.to_le_bytes());
        stack.extend_from_slice(&1u32.to_le_bytes());
        stack.extend_from_slice(&terminator());
        for value in [0u32, 0, 1, 3, 0] {
            stack.extend_from_slice(&value.to_le_bytes());
        }
        stack.extend_from_slice(b"abc");
        stack.extend_from_slice(&0u32.to_le_bytes());
        stack.extend_from_slice(&PRESET_MAGIC.to_le_bytes());

        let item = Item::parse(&node(block(&stack))).unwrap();
        let kinds: Vec<u32> = item.chunks.iter().map(|chunk| chunk.kind).collect();
        assert_eq!(kinds, vec![TERMINATOR, PRESET_CHUNK_ITEM]);
        let found = item.find(PRESET_CHUNK_ITEM).expect("the preset is found");
        match &found.payload {
            Payload::Preset(data) => assert_eq!(data, b"abc"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_preset_not_closed_by_its_magic_is_refused() {
        let mut stack = b"hsin".to_vec();
        stack.extend_from_slice(&PRESET_CHUNK_ITEM.to_le_bytes());
        stack.extend_from_slice(&1u32.to_le_bytes());
        stack.extend_from_slice(&terminator());
        for value in [0u32, 0, 1, 0, 0, 0, 0xDEAD_BEEF] {
            stack.extend_from_slice(&value.to_le_bytes());
        }
        let error = Item::parse(&node(block(&stack))).unwrap_err();
        assert!(error.to_string().contains("0xdeadbeef"), "{error}");
    }

    #[test]
    fn a_block_claiming_more_than_it_holds_is_refused() {
        let mut body = node(terminator());
        body[0..8].copy_from_slice(&9_999u64.to_le_bytes());
        assert!(Item::parse(&body).is_err());
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_panicking() {
        let full = node(terminator());
        for cut in 1..full.len() {
            let _ = Item::parse(&full[..cut]);
        }
    }
}

#[cfg(test)]
mod real_files {
    use super::*;

    /// Walks a real container and reports what it holds.
    ///
    /// Point `RF_SOUNDFONTS_KONTAKT5` at a folder of `.nkm` or `.nki` files.
    #[test]
    #[ignore = "needs a Kontakt 5 library on disk"]
    fn reads_real_containers() {
        let Ok(root) = std::env::var("RF_SOUNDFONTS_KONTAKT5") else {
            panic!("set RF_SOUNDFONTS_KONTAKT5 to a folder of Kontakt files");
        };
        let mut seen = 0;
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(directory) = stack.pop() {
            for entry in std::fs::read_dir(&directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let matches = path.extension().is_some_and(|extension| {
                    let extension = extension.to_string_lossy().to_ascii_lowercase();
                    extension == "nkm" || extension == "nki"
                });
                if !matches {
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap();
                let item = match Item::parse(&bytes) {
                    Ok(item) => item,
                    Err(error) => {
                        eprintln!("{}: {error}", path.display());
                        continue;
                    }
                };
                let preset = item
                    .find(PRESET_CHUNK_ITEM)
                    .map(|chunk| match &chunk.payload {
                        Payload::Preset(data) => data.len(),
                        _ => 0,
                    });
                eprintln!(
                    "{:34} arbol v{} chunks={} hijos={} preset={:?} cifrado={}",
                    path.file_name().unwrap().to_string_lossy(),
                    item.version,
                    item.chunks.len(),
                    item.children.len(),
                    preset,
                    item.has_encrypted_branch(),
                );
                assert!(preset.is_some(), "no preset in {}", path.display());

                // The preset is a tree of its own; walking it here proves the
                // container handed over something whole rather than a slice
                // that merely looked plausible.
                let Some(super::Payload::Preset(data)) =
                    item.find(PRESET_CHUNK_ITEM).map(|chunk| &chunk.payload)
                else {
                    unreachable!()
                };
                if let Ok(dump) = std::env::var("RF_SOUNDFONTS_DUMP") {
                    std::fs::write(&dump, data).unwrap();
                    eprintln!("    preset volcado en {dump}");
                }
                let chunks = crate::kontakt5::chunk::PresetChunk::parse_all(data).unwrap();
                let programs = match crate::kontakt5::program::Program::read_all(&chunks) {
                    Ok(programs) => programs,
                    Err(error) => {
                        eprintln!("    programas: {error}");
                        Vec::new()
                    }
                };
                let paths = match crate::kontakt5::program::file_paths(&chunks) {
                    Ok(paths) => paths,
                    Err(error) => {
                        eprintln!("    rutas: {error}");
                        Vec::new()
                    }
                };
                eprintln!(
                    "    preset: {} chunks, {} programas, {} rutas",
                    chunks.len(),
                    programs.len(),
                    paths.len(),
                );
                for path in paths.iter().take(3) {
                    eprintln!("      ruta: {path}");
                }
                for program in &programs {
                    let placed = program.zones.iter().filter(|z| z.file.is_some()).count();
                    let looped: usize = program.zones.iter().map(|z| z.loops.len()).sum();
                    eprintln!(
                        "      {:22} {:4} zonas ({placed} con sample, {looped} loops)",
                        program.name,
                        program.zones.len(),
                    );
                    for zone in program.zones.iter().take(2) {
                        eprintln!(
                            "         teclas {}..{} vel {}..{} raiz {} archivo {:?} {}Hz {}ch",
                            zone.key_low,
                            zone.key_high,
                            zone.velocity_low,
                            zone.velocity_high,
                            zone.root_key,
                            zone.file,
                            zone.sample_rate,
                            zone.channels,
                        );
                    }
                }
                seen += 1;
            }
        }
        eprintln!("contenedores leidos: {seen}");
        assert!(seen > 0, "found no Kontakt files");
    }
}
