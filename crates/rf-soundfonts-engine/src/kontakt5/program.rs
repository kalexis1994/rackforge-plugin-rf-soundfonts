//! Programs, zones and sample names, read out of the preset tree.
//!
//! A program is what a player calls an instrument: a name, a list of groups,
//! and a list of zones placing samples across the keyboard. A multi holds up to
//! sixty-four of them, one per slot, and reaches them a different way — through
//! the slot list rather than as chunks of the bank — but what comes out is the
//! same shape either way.
//!
//! A zone does not name its sample. It carries an index into a list of paths
//! held once for the whole file, which is how a library with five hundred
//! samples across twenty articulations avoids writing every path over again.
//!
//! Ported from ConvertWithMoss (LGPL-3.0).

use super::chunk::{self, PresetChunk, Reader};
use crate::SoundfontError;

/// Slots a multi can fill.
const MULTI_SLOTS: usize = 64;

/// Zone layout beyond which the reading here no longer applies.
const MAX_ZONE_VERSION: u16 = 0x9C;

/// Zone version from which the layout gains three fields before the sample.
const ZONE_VERSION_PADDED: u16 = 0x9A;

/// Zone version up to which an extra word precedes the root note.
const ZONE_VERSION_EXTRA_WORD: u16 = 0x93;

/// One sample placed on the keyboard.
#[derive(Clone, Debug, Default)]
pub struct Zone {
    /// Which group's settings apply, by reference rather than position.
    pub group: i64,
    pub sample_start: u32,
    pub sample_end: u32,
    pub velocity_low: u16,
    pub velocity_high: u16,
    pub key_low: u16,
    pub key_high: u16,
    pub root_key: u16,
    /// Level as a linear factor, where one is unity.
    pub volume: f32,
    pub pan: f32,
    /// Tuning as a frequency ratio, where one is unaltered.
    pub tune: f32,
    /// Index into the file list, or nothing when the zone names no sample.
    pub file: Option<usize>,
    pub sample_rate: u32,
    pub channels: u8,
    pub frames: u32,
    pub loops: Vec<ZoneLoop>,
}

/// One of a zone's loops.
#[derive(Clone, Copy, Debug)]
pub struct ZoneLoop {
    pub mode: i32,
    pub start: u32,
    pub length: u32,
    /// Zero meaning the loop repeats for as long as the note is held.
    pub count: u32,
    pub alternating: bool,
    pub crossfade: u32,
}

/// An instrument: its zones and what they are called.
#[derive(Debug, Default)]
pub struct Program {
    pub name: String,
    pub volume: f32,
    pub pan: f32,
    pub tune: f32,
    pub zones: Vec<Zone>,
}

impl Program {
    /// Reads the programs of a preset, whether it is an instrument or a multi.
    ///
    /// An instrument states its program among the chunks directly. A multi
    /// hides them one level further down, in the public bytes of its slot list,
    /// so both places are looked in and whichever answers is used.
    pub fn read_all(chunks: &[PresetChunk]) -> Result<Vec<Self>, SoundfontError> {
        let mut found = Vec::new();
        PresetChunk::find_all(chunks, chunk::PROGRAM, &mut found);
        let mut programs = Vec::new();
        for program in found {
            programs.push(Self::from_chunk(program)?);
        }
        if programs.is_empty() {
            // A multi keeps its slot list inside the bank rather than beside
            // it, so the search has to descend.
            let mut slots = Vec::new();
            PresetChunk::find_all(chunks, chunk::SLOT_LIST, &mut slots);
            for slot_list in slots {
                programs.extend(read_slot_list(slot_list)?);
            }
        }
        Ok(programs)
    }

    fn from_chunk(chunk: &PresetChunk) -> Result<Self, SoundfontError> {
        let mut program = Self {
            volume: 1.0,
            tune: 1.0,
            ..Self::default()
        };
        program.read_header(&chunk.public_data)?;
        for child in &chunk.children {
            if child.id == ZONE_LIST {
                program.read_zones(child)?;
            }
        }
        Ok(program)
    }

    fn read_header(&mut self, data: &[u8]) -> Result<(), SoundfontError> {
        let mut reader = Reader::new(data);
        self.name = utf16(&mut reader)?;
        // The size of all samples, as a double, which nothing here needs.
        reader.take(8)?;
        reader.u8()?;
        self.volume = reader.f32()?;
        self.pan = reader.f32()?;
        self.tune = reader.f32()?;
        Ok(())
    }

    fn read_zones(&mut self, list: &PresetChunk) -> Result<(), SoundfontError> {
        for entry in &list.children {
            let mut zone = Zone::read(&entry.public_data, entry.version)?;
            zone.group = entry.id;
            if let Some(loops) = entry.child(chunk::LOOP_ARRAY) {
                zone.loops = ZoneLoop::read_all(&loops.public_data)?;
            }
            self.zones.push(zone);
        }
        Ok(())
    }
}

/// The list of zones belonging to a program.
const ZONE_LIST: i64 = 0x34;

impl Zone {
    fn read(data: &[u8], version: u16) -> Result<Self, SoundfontError> {
        if version > MAX_ZONE_VERSION {
            return Err(SoundfontError::Unsupported(format!(
                "Kontakt zone layout {version:#x} is newer than this reads"
            )));
        }
        let mut reader = Reader::new(data);
        let mut zone = Self {
            sample_start: reader.u32()?,
            sample_end: reader.u32()?,
            ..Self::default()
        };
        reader.u32()?;
        zone.velocity_low = reader.u16()?;
        zone.velocity_high = reader.u16()?;
        zone.key_low = reader.u16()?;
        zone.key_high = reader.u16()?;
        // Four crossfade widths, which the renderer does not express.
        for _ in 0..4 {
            reader.u16()?;
        }
        zone.root_key = reader.u16()?;
        zone.volume = reader.f32()?;
        zone.pan = reader.f32()?;
        zone.tune = reader.f32()?;

        if version >= ZONE_VERSION_PADDED {
            reader.u8()?;
            reader.u8()?;
            reader.u32()?;
            // A zone may stop here. One that does places no sample, which is
            // not a fault: an instrument can hold an empty slot.
            if reader.remaining() == 0 {
                return Ok(zone);
            }
        }

        zone.file = Some(reader.u32()? as usize);
        // The sample's own resolution, which the decoder reads from the audio.
        reader.u32()?;
        zone.sample_rate = reader.u32()?;
        zone.channels = reader.u8()?;
        zone.frames = reader.u32()?;
        reader.u32()?;
        if version <= ZONE_VERSION_EXTRA_WORD {
            reader.u32()?;
        }
        Ok(zone)
    }
}

impl ZoneLoop {
    /// Reads however many loops the array holds.
    ///
    /// The count is not stated: the array is as long as it is, and each loop
    /// occupies a fixed span, so reading stops when the bytes do.
    fn read_all(data: &[u8]) -> Result<Vec<Self>, SoundfontError> {
        const LOOP_BYTES: usize = 25;
        let mut reader = Reader::new(data);
        let mut loops = Vec::new();
        while reader.remaining() >= LOOP_BYTES {
            let entry = Self {
                mode: reader.i32()?,
                start: reader.u32()?,
                length: reader.u32()?,
                count: reader.u32()?,
                alternating: reader.u8()? > 0,
                crossfade: {
                    // Tuning sits between the flag and the crossfade.
                    reader.f32()?;
                    reader.u32()?
                },
            };
            if reader.remaining() > 0 {
                reader.take(1)?;
            }
            loops.push(entry);
        }
        Ok(loops)
    }
}

/// Reads the programs a multi loads into its slots.
fn read_slot_list(list: &PresetChunk) -> Result<Vec<Program>, SoundfontError> {
    let mut reader = Reader::new(&list.public_data);
    let filled = u64::from_le_bytes(reader.take(8)?.try_into().unwrap());
    let mut programs = Vec::new();
    for slot in 0..MULTI_SLOTS {
        if filled & (1 << slot) == 0 {
            continue;
        }
        let id = reader.u16()?;
        let size = reader.u32()? as usize;
        let body = reader.take(size)?;
        if id != PROGRAM_CONTAINER {
            continue;
        }
        let container = PresetChunk::parse_one(PROGRAM_CONTAINER, body)?;
        for child in &container.children {
            if child.id == PROGRAM_LIST {
                programs.extend(read_program_list(child)?);
            }
        }
    }
    Ok(programs)
}

/// A slot's instrument, wrapping its program list.
const PROGRAM_CONTAINER: u16 = 0x29;
/// The programs within a slot's container.
const PROGRAM_LIST: i64 = 0x36;

/// Reads the programs held in a slot's list.
///
/// The list is an array of bare structures: they carry no type of their own,
/// so each is read as a program because that is what the list is.
fn read_program_list(list: &PresetChunk) -> Result<Vec<Program>, SoundfontError> {
    let entries = PresetChunk::parse_array(&list.public_data)?;
    let mut programs = Vec::new();
    for entry in &entries {
        programs.push(Program::from_chunk(entry)?);
    }
    Ok(programs)
}

/// Reads a string stored as a length in characters and then UTF-16.
fn utf16(reader: &mut Reader<'_>) -> Result<String, SoundfontError> {
    let length = reader.u32()? as usize;
    let bytes = reader.take(length * 2)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

/// The sample paths a preset refers to by index.
///
/// A path is written as a run of segments rather than as text, so that the
/// drive, the folders above it and the file itself stay separable. Only the
/// joined result is wanted here.
pub fn file_paths(chunks: &[PresetChunk]) -> Result<Vec<String>, SoundfontError> {
    let mut lists = Vec::new();
    PresetChunk::find_all(chunks, chunk::FILENAME_LIST, &mut lists);
    if lists.is_empty() {
        PresetChunk::find_all(chunks, chunk::FILENAME_LIST_EX, &mut lists);
    }
    let Some(list) = lists.first() else {
        return Ok(Vec::new());
    };
    let extended = list.id == chunk::FILENAME_LIST_EX;
    let mut reader = Reader::new(&list.public_data);

    if extended {
        let version = reader.u16()?;
        if !(2..=3).contains(&version) {
            return Err(SoundfontError::Unsupported(format!(
                "Kontakt file list version {version} is not read here"
            )));
        }
        if version == 3 {
            return read_dated_files(&mut reader);
        }
    }
    // The first run names files the instrument needs that are not samples.
    read_files(&mut reader)?;
    read_files(&mut reader)
}

fn read_files(reader: &mut Reader<'_>) -> Result<Vec<String>, SoundfontError> {
    if reader.remaining() == 0 {
        return Ok(Vec::new());
    }
    let count = reader.i32()?.max(0) as usize;
    let mut files = Vec::with_capacity(count.min(4_096));
    for _ in 0..count {
        files.push(read_path(reader)?);
    }
    Ok(files)
}

/// Reads the newer list, which follows each path with when it was last written.
fn read_dated_files(reader: &mut Reader<'_>) -> Result<Vec<String>, SoundfontError> {
    if reader.remaining() == 0 {
        return Ok(Vec::new());
    }
    let count = reader.i32()?.max(0) as usize;
    reader.take(8)?;
    let mut files = Vec::with_capacity(count.min(4_096));
    // The count includes an entry that carries no path of its own.
    for _ in 0..count.saturating_sub(1) {
        files.push(read_path(reader)?);
        if reader.remaining() > 0 {
            reader.take(4 + 8 + 20)?;
        }
    }
    Ok(files)
}

fn read_path(reader: &mut Reader<'_>) -> Result<String, SoundfontError> {
    let segments = reader.u32()?;
    let mut path = String::new();
    for _ in 0..segments {
        match reader.u8()? {
            // A drive letter, written as two characters in the oldest form.
            0 => {
                let drive = String::from_utf8_lossy(reader.take(2)?).trim().to_string();
                if drive.is_empty() {
                    path.push('/');
                } else {
                    path.push_str(&drive);
                    path.push_str(":/");
                }
            }
            1 => {
                let drive = utf16(reader)?;
                if drive.is_empty() {
                    path.push('/');
                } else {
                    path.push_str(&drive);
                    path.push_str(":/");
                }
            }
            2 => {
                path.push_str(&utf16(reader)?);
                path.push('/');
            }
            3 => path.push_str("../"),
            4 | 8 | 9 => path.push_str(&utf16(reader)?),
            6 => {}
            other => {
                return Err(SoundfontError::Unsupported(format!(
                    "Kontakt path segment {other} is not read here"
                )));
            }
        }
    }
    Ok(path)
}
