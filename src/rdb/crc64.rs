//! CRC-64/Jones — the checksum Redis stamps on the last 8 bytes of an RDB.
//!
//! Reflected form (`refin`/`refout`), polynomial `0xad93d23594c935a9`, with a
//! zero init and xor-out. Redis writes a literal zero when `rdbchecksum no` is
//! set and treats that as "not checked" on load, so a zero trailer is a legal
//! file rather than a corrupt one.

const POLY: u64 = 0xad93_d235_94c9_35a9;

/// Bit-reverse a word, so the reflected table can be derived from the
/// polynomial above rather than hardcoding a second, easy-to-mistype form.
const fn reverse(mut v: u64) -> u64 {
    let mut r = 0u64;
    let mut i = 0;
    while i < 64 {
        r = (r << 1) | (v & 1);
        v >>= 1;
        i += 1;
    }
    r
}

const TABLE: [u64; 256] = {
    let rev = reverse(POLY);
    let mut table = [0u64; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut crc = n as u64;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ rev
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
};

/// Fold `data` into a running checksum. Start from `0`.
pub fn update(mut crc: u64, data: &[u8]) -> u64 {
    for &b in data {
        crc = TABLE[((crc ^ b as u64) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// Checksum of a complete buffer.
pub fn checksum(data: &[u8]) -> u64 {
    update(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue check value for CRC-64/Jones (listed as "CRC-64/REDIS"):
    /// the digest of the ASCII string `123456789`. If the table generation or
    /// the reflection is wrong, this is what catches it.
    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(checksum(b"123456789"), 0xe9c6_d914_c4b8_d9ca);
    }

    #[test]
    fn update_is_incremental() {
        let whole = checksum(b"hello world");
        let split = update(update(0, b"hello "), b"world");
        assert_eq!(whole, split);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(checksum(b""), 0);
    }
}
