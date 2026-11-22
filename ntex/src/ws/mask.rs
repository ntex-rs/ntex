//! WebSocket payload masking, RFC 6455 section 5.3.

/// Applies the masking key `mask_u32`, in native byte order, to `buf`.
///
/// The buffer is processed in unaligned 8-byte words, so branches depend
/// only on the length. Payloads packed back to back in a read buffer each
/// start at a different alignment, and splitting at aligned boundaries
/// mispredicted for small frames.
#[inline]
pub(crate) fn apply_mask(buf: &mut [u8], mask_u32: u32) {
    let mask_u64 = (u64::from(mask_u32) << 32) | u64::from(mask_u32);
    let (words, mut rest) = buf.as_chunks_mut::<8>();
    for word in words {
        *word = (u64::from_ne_bytes(*word) ^ mask_u64).to_ne_bytes();
    }

    // the remainder starts at a multiple of the key length
    if rest.len() >= 4 {
        let (word, tail) = rest.split_first_chunk_mut::<4>().unwrap();
        *word = (u32::from_ne_bytes(*word) ^ mask_u32).to_ne_bytes();
        rest = tail;
    }
    for (byte, m) in rest.iter_mut().zip(mask_u32.to_ne_bytes()) {
        *byte ^= m;
    }
}

#[cfg(test)]
mod tests {
    use super::apply_mask;

    /// A safe unoptimized mask application.
    fn apply_mask_fallback(buf: &mut [u8], mask: [u8; 4]) {
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte ^= mask[i & 3];
        }
    }

    /// STATEMENT: Optimized WebSocket masking matches bytewise XOR for aligned and unaligned buffers.
    ///
    /// HOW:
    /// - Apply a native-endian mask to a complete buffer and an offset slice.
    /// - Compare each result with the safe bytewise reference implementation.
    #[test]
    fn test_apply_mask() {
        let mask = [0x6d, 0xb6, 0xb2, 0x80];
        let mask_u32 = u32::from_ne_bytes(mask);

        let unmasked = vec![
            0xf3, 0x00, 0x01, 0x02, 0x03, 0x80, 0x81, 0x82, 0xff, 0xfe, 0x00, 0x17, 0x74, 0xf9,
            0x12, 0x03,
        ];

        // Check masking with proper alignment.
        {
            let mut masked = unmasked.clone();
            apply_mask_fallback(&mut masked, mask);

            let mut masked_fast = unmasked.clone();
            apply_mask(&mut masked_fast, mask_u32);

            assert_eq!(masked, masked_fast);
        }

        // Check masking without alignment.
        {
            let mut masked = unmasked.clone();
            apply_mask_fallback(&mut masked[1..], mask);

            let mut masked_fast = unmasked;
            apply_mask(&mut masked_fast[1..], mask_u32);

            assert_eq!(masked, masked_fast);
        }
    }

    /// STATEMENT: Masking matches bytewise XOR for every short length and alignment.
    ///
    /// HOW:
    /// - Mask slices of lengths 0 to 24 at offsets 0 to 7 of an aligned buffer, so
    ///   buffers with up to three words and every 0 to 7 byte remainder occur.
    /// - Compare each result with the safe bytewise reference implementation.
    #[test]
    fn test_apply_mask_short_segments() {
        let mask = [0x6d, 0xb6, 0xb2, 0x80];
        let mask_u32 = u32::from_ne_bytes(mask);
        let mut buf = [0u8; 48];
        let base = buf.as_ptr().align_offset(8);
        let bytes = &mut buf[base..base + 40];

        for offset in 0..8 {
            for len in 0..=24 {
                for (i, b) in bytes.iter_mut().enumerate() {
                    *b = i as u8;
                }
                let mut expected = bytes[offset..offset + len].to_vec();
                apply_mask_fallback(&mut expected, mask);

                apply_mask(&mut bytes[offset..offset + len], mask_u32);
                assert_eq!(bytes[offset..offset + len], expected[..], "{offset}/{len}");
            }
        }
    }
}
