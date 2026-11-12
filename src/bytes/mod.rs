//! Small byte searches shared by text-oriented applets.
//!
//! Case mapping uses the standard library's auto-vectorized implementation.
//! Custom ISA kernels must demonstrate a kernel and end-to-end win before
//! introducing architecture-specific code or runtime dispatch here.

/// Count newline bytes.
pub(crate) fn count_newlines(input: &[u8]) -> usize {
    input.iter().filter(|&&byte| byte == b'\n').count()
}

/// Find the first byte that belongs to `set`.
pub(crate) fn find_any(input: &[u8], set: &[u8]) -> Option<usize> {
    match set {
        [] => None,
        [one] => memchr::memchr(*one, input),
        [one, two] => memchr::memchr2(*one, *two, input),
        [one, two, three] => memchr::memchr3(*one, *two, *three, input),
        [one, two, three, four] => {
            let first = memchr::memchr2(*one, *two, input);
            let second = memchr::memchr2(*three, *four, input);
            match (first, second) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (Some(position), None) | (None, Some(position)) => Some(position),
                (None, None) => None,
            }
        }
        _ => input.iter().position(|byte| set.contains(byte)),
    }
}

/// Bytes that CSV and text fields reject outside quoting: C0 controls other
/// than TAB, and DEL.
#[inline]
fn is_unsupported_control(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t') || byte == 0x7f
}

/// Find the first byte that is an unsupported control (see
/// [`is_unsupported_control`]). Each 32-byte block is reduced without an
/// early exit so the compiler can vectorise the test; the position is only
/// resolved in a block that contains a hit.
pub(crate) fn find_unsupported_control(input: &[u8]) -> Option<usize> {
    const LANES: usize = 32;
    let (blocks, rest) = input.as_chunks::<LANES>();
    for (index, block) in blocks.iter().enumerate() {
        let mut hit = 0u8;
        for &byte in block {
            hit |= u8::from(is_unsupported_control(byte));
        }
        if hit != 0 {
            return block
                .iter()
                .position(|&byte| is_unsupported_control(byte))
                .map(|position| index * LANES + position);
        }
    }
    let base = blocks.len() * LANES;
    rest.iter()
        .position(|&byte| is_unsupported_control(byte))
        .map(|position| base + position)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic generator for test inputs (xorshift64).
    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// Bytes weighted toward the values the kernels classify differently.
    fn interesting_byte(random: u64) -> u8 {
        const SPECIAL: [u8; 12] = [
            b'\n', b'\r', b'\t', b',', b'"', 0x00, 0x1f, 0x20, 0x7e, 0x7f, 0x80, 0xff,
        ];
        if random.is_multiple_of(3) {
            SPECIAL[(random as usize >> 2) % SPECIAL.len()]
        } else {
            (random >> 8) as u8
        }
    }

    fn reference_position(input: &[u8], predicate: impl Fn(u8) -> bool) -> Option<usize> {
        input.iter().position(|&byte| predicate(byte))
    }

    #[test]
    fn byte_searches_match_reference_at_all_offsets_and_lengths() {
        for offset in 0..=63 {
            for length in 0..=257 {
                let backing = (0..length + offset)
                    .map(|index| (index & 0xff) as u8)
                    .collect::<Vec<_>>();
                let input = &backing[offset..];
                for set in [&b""[..], b",", b",\"", b",\"\r", b",\"\r\n", b",\"\r\n\t"] {
                    let expected = input.iter().position(|byte| set.contains(byte));
                    assert_eq!(find_any(input, set), expected);
                }
                assert_eq!(
                    count_newlines(input),
                    input.iter().filter(|&&byte| byte == b'\n').count()
                );
            }
        }
    }

    #[test]
    fn unsupported_control_matches_reference_for_every_byte_value() {
        for offset in 0..=63 {
            for length in 0..=130 {
                for value in 0..=255u8 {
                    let mut backing = vec![b'a'; length + offset];
                    if length > 0 {
                        backing[offset + length / 2] = value;
                        backing[offset + length - 1] = value;
                    }
                    let input = &backing[offset..];
                    assert_eq!(
                        find_unsupported_control(input),
                        reference_position(input, is_unsupported_control),
                        "offset {offset} length {length} value {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn unsupported_control_allows_tab_and_bytes_above_del() {
        let allowed: Vec<u8> = (0x20..=0x7e).chain(*b"\t").chain(0x80..=0xff).collect();
        for offset in 0..=63 {
            for length in 0..=130 {
                let backing: Vec<u8> = allowed
                    .iter()
                    .cycle()
                    .take(length + offset)
                    .copied()
                    .collect();
                assert_eq!(find_unsupported_control(&backing[offset..]), None);
            }
        }
    }

    #[test]
    fn random_inputs_match_reference_kernels() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for length in 0..=130 {
            for offset in 0..=63 {
                for _ in 0..4 {
                    let backing: Vec<u8> = (0..length + offset)
                        .map(|_| interesting_byte(next_random(&mut state)))
                        .collect();
                    let input = &backing[offset..];
                    assert_eq!(
                        find_unsupported_control(input),
                        reference_position(input, is_unsupported_control)
                    );
                    assert_eq!(
                        find_any(input, b",\"\r\n"),
                        reference_position(input, |byte| matches!(
                            byte,
                            b',' | b'"' | b'\r' | b'\n'
                        ))
                    );
                    assert_eq!(
                        count_newlines(input),
                        input.iter().filter(|&&byte| byte == b'\n').count()
                    );
                }
            }
        }
    }

    #[test]
    fn kernels_handle_multibyte_utf8_across_block_boundaries() {
        let pieces = ["é", "日本語", "😀", "\n", "\r\n", "a", "Ω\n"];
        for prefix in 0..=40usize {
            for piece_count in 0..=24 {
                let text: String = pieces.iter().copied().cycle().take(piece_count).collect();
                // Shift the multi-byte text by `prefix` ASCII bytes so block
                // boundaries land inside UTF-8 sequences at every position.
                let mut backing = vec![b'x'; prefix];
                backing.extend_from_slice(text.as_bytes());
                let input = &backing[..];
                let newlines = text.chars().filter(|&character| character == '\n').count();
                assert_eq!(count_newlines(input), newlines);
                // The text holds LF and CR, which are unsupported controls here;
                // the multi-byte sequences themselves (all >= 0x80) must not hit.
                assert_eq!(
                    find_unsupported_control(input),
                    reference_position(input, is_unsupported_control)
                );
                assert!(input
                    .iter()
                    .filter(|&&byte| byte >= 0x80)
                    .all(|&byte| !is_unsupported_control(byte)));
                let tail = &input[prefix..];
                assert_eq!(
                    find_any(tail, b",\"\r\n"),
                    reference_position(tail, |byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
                );
            }
        }
    }

    #[test]
    #[ignore = "scalar byte kernel microbenchmark"]
    fn scalar_case_kernel_reports_mb_per_second() {
        fn measure(label: &str, input: &[u8], mapping: impl Fn(&u8) -> u8) {
            let mut output = Vec::with_capacity(input.len());
            let started = std::time::Instant::now();
            for _ in 0..256 {
                output.clear();
                output.extend(std::hint::black_box(input).iter().map(&mapping));
                std::hint::black_box(&output);
            }
            let mb_per_second =
                (input.len() as f64 * 256.0 / 1_000_000.0) / started.elapsed().as_secs_f64();
            eprintln!("{label}: {mb_per_second:.1} MB/s");
        }
        let input = vec![b'a'; 4 * 1024 * 1024];
        measure("scalar upper", &input, u8::to_ascii_uppercase);
        measure("scalar lower", &input, u8::to_ascii_lowercase);
    }
}
