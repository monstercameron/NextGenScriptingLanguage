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

#[cfg(test)]
mod tests {
    use super::*;

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
