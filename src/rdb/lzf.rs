//! LZF decompression — the only compression scheme RDB applies to strings.
//!
//! Decompress-only on purpose: an uncompressed string is always a legal
//! encoding, so meebis never needs to compress on the way out, and skipping
//! the compressor removes the half of liblzf that has to make choices.
//!
//! Every copy is bounds-checked and every failure returns `None`. This parses
//! a file we did not write, so a malformed length must not panic the server.

/// Expand an LZF block to exactly `expected_len` bytes, or fail.
///
/// The length comes from the RDB header rather than the block itself, so a
/// short or long result means the file disagrees with itself — treat it as
/// corruption instead of trusting whichever value looks more plausible.
pub fn decompress(input: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(expected_len);
    let mut i = 0usize;

    while i < input.len() {
        let ctrl = input[i] as usize;
        i += 1;

        if ctrl < 32 {
            // A literal run: the control byte is `count - 1`, followed by that
            // many bytes copied straight through.
            let run = ctrl + 1;
            let end = i.checked_add(run)?;
            if end > input.len() {
                return None;
            }
            out.extend_from_slice(&input[i..end]);
            i = end;
        } else {
            // A back reference. The top 3 bits hold the length, the low 5 the
            // high bits of the distance. Note the order: a length of 7 means
            // "one more length byte follows", and that byte comes *before* the
            // low byte of the distance.
            let mut len = ctrl >> 5;
            if len == 7 {
                len += *input.get(i)? as usize;
                i += 1;
            }
            let dist = ((ctrl & 0x1f) << 8) | *input.get(i)? as usize;
            i += 1;

            let mut src = out.len().checked_sub(dist + 1)?;
            // Overlapping references are normal (that is how LZF encodes runs),
            // so copy a byte at a time rather than slicing.
            for _ in 0..len + 2 {
                let b = *out.get(src)?;
                out.push(b);
                src += 1;
            }
        }
    }

    if out.len() == expected_len {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_run_only() {
        // ctrl=4 => a five-byte literal run.
        assert_eq!(
            decompress(&[4, b'h', b'e', b'l', b'l', b'o'], 5).unwrap(),
            b"hello"
        );
    }

    /// A back reference whose distance is shorter than its length, which is how
    /// LZF spells a repeating run — the classic off-by-one trap in a naive
    /// slice-based copy.
    #[test]
    fn overlapping_back_reference_expands_a_run() {
        // Literal "a", then a reference back 1 byte for 5 bytes total.
        // ctrl = (3 << 5) | 0 => len 3 (+2 = 5), distance high bits 0; next
        // byte 0 => distance 1.
        let out = decompress(&[0, b'a', (3 << 5), 0], 6).unwrap();
        assert_eq!(out, b"aaaaaa");
    }

    #[test]
    fn truncated_input_fails_instead_of_panicking() {
        assert!(decompress(&[10, b'a'], 11).is_none());
        assert!(decompress(&[(3 << 5)], 5).is_none());
    }

    #[test]
    fn length_mismatch_is_rejected() {
        assert!(decompress(&[4, b'h', b'e', b'l', b'l', b'o'], 99).is_none());
    }

    /// A distance pointing before the start of the output is corruption, not a
    /// wrap-around.
    #[test]
    fn reference_before_start_is_rejected() {
        assert!(decompress(&[(3 << 5), 200], 5).is_none());
    }
}
