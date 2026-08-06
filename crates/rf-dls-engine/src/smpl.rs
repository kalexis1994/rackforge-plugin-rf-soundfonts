//! Loop markers carried in a RIFF `smpl` chunk.
//!
//! Two formats need this and neither is RIFF all the way down. A WAV holds the
//! chunk beside its audio; a FLAC converted from one holds the original RIFF
//! chunks verbatim inside `APPLICATION` blocks tagged `riff`, which is how a
//! converted library keeps loop points the compressed format has nowhere else
//! to put.
//!
//! It matters because a looped instrument states `loop_mode` in its SFZ and
//! then says nothing about where the loop is: the points are expected to come
//! from the sample. Ignoring them turns a sustaining instrument into one that
//! stops when the recording runs out, which for the Rhodes measured here is
//! seven seconds in, at half of full scale.

use crate::SampleLoop;

/// Bytes of `smpl` before the first loop record.
const SMPL_HEADER: usize = 36;

/// Bytes in one loop record.
const LOOP_RECORD: usize = 24;

/// Offset of the loop count within the chunk body.
const LOOP_COUNT_OFFSET: usize = 28;

/// Reads the first sustaining loop from a bare `smpl` chunk body.
///
/// The body excludes the four-character identifier and the size word.
pub fn loop_from_body(body: &[u8]) -> Option<SampleLoop> {
    if body.len() < SMPL_HEADER + LOOP_RECORD {
        return None;
    }
    let count = u32::from_le_bytes(
        body[LOOP_COUNT_OFFSET..LOOP_COUNT_OFFSET + 4]
            .try_into()
            .ok()?,
    );
    if count == 0 {
        return None;
    }
    let record = &body[SMPL_HEADER..SMPL_HEADER + LOOP_RECORD];
    let kind = u32::from_le_bytes(record[4..8].try_into().ok()?);
    // Forward loops only. A ping-pong or reverse loop played forward would be
    // audibly wrong, and silently so; better to leave the sample unlooped.
    if kind != 0 {
        return None;
    }
    let start = u32::from_le_bytes(record[8..12].try_into().ok()?) as usize;
    let last = u32::from_le_bytes(record[12..16].try_into().ok()?) as usize;
    // `smpl` names the last frame *inside* the loop; the engine's end is
    // exclusive. Off by one here detunes every sustained note.
    Some(SampleLoop {
        start,
        end: last.checked_add(1)?,
    })
}

/// Finds a `smpl` chunk among top-level RIFF chunks and reads its loop.
///
/// A flat scan rather than a recursive walk: `smpl` sits beside `fmt ` and
/// `data`, and a shallow reader cannot be led into deep recursion by a
/// malformed file.
pub fn loop_in_riff(bytes: &[u8], skip_header: usize) -> Option<SampleLoop> {
    let mut cursor = skip_header;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().ok()?) as usize;
        let body = cursor + 8;
        let end = body.checked_add(size)?;
        if end > bytes.len() {
            return None;
        }
        if id == b"smpl" {
            return loop_from_body(&bytes[body..end]);
        }
        cursor = end + (size & 1);
    }
    None
}

/// Builds a `smpl` chunk body holding one forward loop, for tests.
#[cfg(test)]
pub fn body_with_loop(start: u32, last: u32) -> Vec<u8> {
    let mut body = vec![0_u8; SMPL_HEADER + LOOP_RECORD];
    body[LOOP_COUNT_OFFSET..LOOP_COUNT_OFFSET + 4].copy_from_slice(&1_u32.to_le_bytes());
    body[SMPL_HEADER + 8..SMPL_HEADER + 12].copy_from_slice(&start.to_le_bytes());
    body[SMPL_HEADER + 12..SMPL_HEADER + 16].copy_from_slice(&last.to_le_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loop_end_names_the_last_frame_inside_the_loop() {
        let looping = loop_from_body(&body_with_loop(319_390, 322_438)).unwrap();
        assert_eq!(looping.start, 319_390);
        assert_eq!(looping.end, 322_439);
    }

    #[test]
    fn a_chunk_without_loops_reports_none() {
        let body = vec![0_u8; SMPL_HEADER + LOOP_RECORD];
        assert!(loop_from_body(&body).is_none());
    }

    #[test]
    fn a_truncated_chunk_is_refused_rather_than_read_past() {
        assert!(loop_from_body(&[0_u8; 8]).is_none());
    }

    #[test]
    fn a_backward_loop_is_left_unlooped_rather_than_played_forward() {
        let mut body = body_with_loop(10, 20);
        body[SMPL_HEADER + 4..SMPL_HEADER + 8].copy_from_slice(&1_u32.to_le_bytes());
        assert!(loop_from_body(&body).is_none());
    }

    #[test]
    fn a_smpl_chunk_is_found_among_its_neighbours() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; 16]);
        let body = body_with_loop(100, 200);
        bytes.extend_from_slice(b"smpl");
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&body);
        let looping = loop_in_riff(&bytes, 0).unwrap();
        assert_eq!((looping.start, looping.end), (100, 201));
    }

    #[test]
    fn a_chunk_claiming_more_than_it_holds_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"smpl");
        bytes.extend_from_slice(&9_999_u32.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; 8]);
        assert!(loop_in_riff(&bytes, 0).is_none());
    }
}
