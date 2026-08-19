//! Decompressing the FastLZ streams inside a Kontakt 5 container.
//!
//! Kontakt stores the branches of its container tree compressed, and FastLZ is
//! what it uses. The algorithm is small — a stream of control bytes that either
//! introduce a run of literals or a back-reference into what has already been
//! written — and only decompression is needed here, since nothing writes these
//! files back out.
//!
//! Two levels exist and both appear in the wild: level one, whose match lengths
//! and distances each fit in a single byte, and level two, which extends both
//! with continuation bytes for longer runs and farther matches. The level is
//! stated in the top three bits of the first byte.
//!
//! The port follows ConvertWithMoss (LGPL-3.0), whose reading of the format is
//! the reference this was written against. Where it trusts the stream, this
//! checks it: a back-reference reaching behind the start of the output, or a
//! copy running past its end, is a corrupt file rather than a panic.

use crate::SoundfontError;

/// Distance subtracted from a level-two far match.
const MAX_DISTANCE_LZ2: usize = 8_191;

/// Length meaning "the real length follows".
///
/// The field is three bits and is stored one above the length it means, so a
/// stored seven — its largest — decrements to six and stands for an escape
/// rather than for six.
const LENGTH_ESCAPE: usize = 6;

/// Expands a FastLZ stream, which must produce exactly `expected` bytes.
///
/// The size is not a hint. It is recorded beside the stream in the container,
/// so a stream that expands to anything else has been misread, and saying so
/// is better than handing back a buffer that is quietly short.
pub fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, SoundfontError> {
    let Some(&first) = input.first() else {
        return Err(SoundfontError::Invalid("FastLZ stream is empty".into()));
    };
    let level = (first >> 5) + 1;
    if level != 1 && level != 2 {
        return Err(SoundfontError::Invalid(format!(
            "FastLZ level {level} is not read here"
        )));
    }

    let mut out: Vec<u8> = Vec::with_capacity(expected);
    let mut reader = Reader { input, at: 1 };
    // The first control byte carries the level in the bits the length would
    // otherwise use, so it is always a literal run.
    let mut control = usize::from(first & 31);

    loop {
        if control >= 32 {
            back_reference(&mut reader, &mut out, control, level, expected)?;
            match reader.next() {
                Some(byte) => control = usize::from(byte),
                None => break,
            }
        } else {
            // A control byte below 32 counts literals, one less than it says.
            let run = control + 1;
            let literals = reader.take(run)?;
            if out.len() + run > expected {
                return Err(overflow(out.len() + run, expected));
            }
            out.extend_from_slice(literals);
            match reader.next() {
                Some(byte) => control = usize::from(byte),
                None => break,
            }
        }
    }

    if out.len() != expected {
        return Err(SoundfontError::Invalid(format!(
            "FastLZ stream expanded to {} bytes, not the {expected} recorded",
            out.len()
        )));
    }
    Ok(out)
}

/// Copies a run that has already been written, appending it to `out`.
///
/// The source may overlap the destination, and deliberately so: a match at
/// distance one is how a long run of the same byte is expressed, so the copy
/// proceeds one byte at a time and reads what it has just written.
fn back_reference(
    reader: &mut Reader<'_>,
    out: &mut Vec<u8>,
    control: usize,
    level: u8,
    expected: usize,
) -> Result<(), SoundfontError> {
    let mut length = (control >> 5) - 1;
    let mut offset = (control & 31) << 8;

    if length == LENGTH_ESCAPE {
        if level == 1 {
            length += usize::from(reader.byte()?);
        } else {
            // Level two spells a long length out in as many bytes as it takes.
            loop {
                let byte = reader.byte()?;
                length += usize::from(byte);
                if byte != 255 {
                    break;
                }
            }
        }
    }

    let low = reader.byte()?;
    let mut distance = offset + usize::from(low) + 1;
    if level == 2 && low == 255 && offset == 31 << 8 {
        // The far-match escape: the distance is restated in two more bytes.
        let high = reader.byte()?;
        let low = reader.byte()?;
        offset = (usize::from(high) << 8) + usize::from(low);
        distance = offset + MAX_DISTANCE_LZ2;
    }

    let total = length + 3;
    let Some(mut source) = out.len().checked_sub(distance) else {
        return Err(SoundfontError::Invalid(format!(
            "FastLZ match reaches {distance} bytes back through only {} written",
            out.len()
        )));
    };
    if out.len() + total > expected {
        return Err(overflow(out.len() + total, expected));
    }
    out.reserve(total);
    for _ in 0..total {
        let byte = out[source];
        out.push(byte);
        source += 1;
    }
    Ok(())
}

fn overflow(would_be: usize, expected: usize) -> SoundfontError {
    SoundfontError::Invalid(format!(
        "FastLZ stream writes {would_be} bytes into a buffer of {expected}"
    ))
}

/// A position in the compressed stream that refuses to read past its end.
struct Reader<'a> {
    input: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    /// The next byte, or nothing when the stream is spent.
    fn next(&mut self) -> Option<u8> {
        let byte = *self.input.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    /// The next byte, where its absence means the stream was cut short.
    fn byte(&mut self) -> Result<u8, SoundfontError> {
        self.next()
            .ok_or_else(|| SoundfontError::Invalid("FastLZ stream ends inside a reference".into()))
    }

    fn take(&mut self, count: usize) -> Result<&[u8], SoundfontError> {
        let end = self.at + count;
        let slice = self.input.get(self.at..end).ok_or_else(|| {
            SoundfontError::Invalid(format!(
                "FastLZ stream ends inside a run of {count} literals"
            ))
        })?;
        self.at = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_literals_is_copied_through() {
        // Control byte 2 means three literals; the top bits being zero make
        // this a level-one stream.
        let out = decompress(&[0x02, b'a', b'b', b'c'], 3).unwrap();
        assert_eq!(out, b"abc");
    }

    #[test]
    fn a_match_repeats_what_came_before() {
        // Four literals, then a match of three bytes at a distance of four.
        let out = decompress(&[0x03, b'a', b'b', b'c', b'd', 0x20, 0x03], 7).unwrap();
        assert_eq!(out, b"abcdabc");
    }

    #[test]
    fn a_match_at_distance_one_becomes_a_run() {
        // The source overlaps the destination, so each byte copied is one the
        // same match has just written. Three literals then a match of three at
        // distance one turns `abc` into `abcccc`.
        let out = decompress(&[0x02, b'a', b'b', b'c', 0x20, 0x00], 6).unwrap();
        assert_eq!(out, b"abcccc");
    }

    #[test]
    fn a_long_match_takes_its_length_from_the_next_byte() {
        // A length field of six says the real length follows: six plus ten,
        // plus the three every match carries, is nineteen bytes.
        let out = decompress(&[0x00, b'x', 0xE0, 0x0A, 0x00], 20).unwrap();
        assert_eq!(out, b"x".repeat(20));
    }

    #[test]
    fn a_match_reaching_behind_the_start_is_refused() {
        // Distance nine into a single byte of output.
        let error = decompress(&[0x00, b'a', 0x20, 0x08], 16).unwrap_err();
        assert!(error.to_string().contains("reaches"), "{error}");
    }

    #[test]
    fn a_stream_that_ends_mid_reference_is_refused() {
        assert!(decompress(&[0x02, b'a', b'b', b'c', 0x20], 8).is_err());
    }

    #[test]
    fn a_run_claiming_more_literals_than_exist_is_refused() {
        assert!(decompress(&[0x1F, b'a', b'b'], 32).is_err());
    }

    #[test]
    fn an_empty_stream_is_refused_rather_than_returning_nothing() {
        assert!(decompress(&[], 0).is_err());
    }

    #[test]
    fn a_stream_expanding_to_the_wrong_size_is_refused() {
        // The container records the size, so a mismatch means the stream was
        // not read the way it was written.
        let error = decompress(&[0x02, b'a', b'b', b'c'], 99).unwrap_err();
        assert!(error.to_string().contains("expanded to 3"), "{error}");
    }

    #[test]
    fn an_unknown_level_is_refused() {
        // Top three bits of the first byte state the level; five is not one.
        let error = decompress(&[0x80, 0x00], 4).unwrap_err();
        assert!(error.to_string().contains("level"), "{error}");
    }
}
