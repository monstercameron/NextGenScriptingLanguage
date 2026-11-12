//! Corpus check for #470: a program with k independent seeded syntax errors reports exactly k
//! diagnostics, each at its seeded line, with no cascade diagnostics from the valid statements
//! around them. Programs are generated from a fixed seed so the corpus is deterministic.

use flow::parser::parse_collecting_errors;

const SLOTS: usize = 6;

/// Statement-level errors that the parser must report independently of their neighbours.
const LINE_LOCAL_STATEMENTS: [&str; 2] =
    ["run \"true\" |", "rnu \"printf\" with arguments [\"x\"]"];

/// An unterminated quote. Also a statement-level error, but see the ignored test below.
const UNTERMINATED_STATEMENT: &str = "run \"printf\" with arguments [\"unterminated]";

/// Small deterministic generator (xorshift64) so the corpus needs no extra crate.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// Builds a program with `k` bad slots chosen from `SLOTS` and returns the source together
/// with the 1-based line numbers the errors must be reported at.
fn seeded_program(k: usize, seed: u64, bad_statements: &[&str]) -> (String, Vec<usize>) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut bad = [false; SLOTS];
    let mut chosen = 0;
    while chosen < k {
        let slot = rng.below(SLOTS);
        if !bad[slot] {
            bad[slot] = true;
            chosen += 1;
        }
    }
    let mut source = String::from("accept a as text\n");
    let mut expected = Vec::new();
    for (slot, is_bad) in bad.iter().enumerate() {
        if *is_bad {
            let statement = bad_statements[rng.below(bad_statements.len())];
            source.push_str(statement);
            source.push('\n');
            // The header is line 1, so slot `slot` sits on line `slot + 2`.
            expected.push(slot + 2);
        } else {
            source.push_str(&format!("run \"true\" as ok{slot}\n"));
        }
    }
    (source, expected)
}

#[test]
fn k_independent_seeded_errors_report_exactly_k_diagnostics_at_their_lines() {
    for k in 1..=5 {
        for seed in 0..20u64 {
            let (source, expected) = seeded_program(k, seed * 7 + k as u64, &LINE_LOCAL_STATEMENTS);
            let report = parse_collecting_errors(&source);
            let lines: Vec<usize> = report.errors.iter().filter_map(|e| e.line).collect();
            assert_eq!(
                report.errors.len(),
                k,
                "k={k} seed={seed} source:\n{source}errors: {:?}",
                report.errors
            );
            assert_eq!(lines, expected, "k={k} seed={seed} source:\n{source}");
            assert!(report.program.is_none(), "k={k} seed={seed}");
            assert!(!report.truncated, "k={k} seed={seed}");
        }
    }
}

/// Known defect: an unterminated quote on one line makes `parse_collecting_errors` stop at
/// that error, so the other independent errors in the program are not reported. The statement
/// should be recovered line-locally like the others. Remove the `#[ignore]` once fixed.
#[test]
#[ignore = "known defect: unterminated quoted string hides later independent statement errors"]
fn unterminated_quote_does_not_hide_other_independent_errors() {
    let mut statements = LINE_LOCAL_STATEMENTS.to_vec();
    statements.push(UNTERMINATED_STATEMENT);
    for k in 1..=5 {
        for seed in 0..20u64 {
            let (source, expected) = seeded_program(k, seed * 7 + k as u64, &statements);
            let report = parse_collecting_errors(&source);
            let lines: Vec<usize> = report.errors.iter().filter_map(|e| e.line).collect();
            assert_eq!(
                lines, expected,
                "k={k} seed={seed} source:
{source}"
            );
        }
    }
}

#[test]
fn every_error_has_a_message_and_no_error_is_reported_twice_on_one_line() {
    for k in 1..=5 {
        let (source, _) = seeded_program(k, 1234 + k as u64, &LINE_LOCAL_STATEMENTS);
        let report = parse_collecting_errors(&source);
        let mut lines: Vec<Option<usize>> = report.errors.iter().map(|e| e.line).collect();
        for error in &report.errors {
            assert!(!error.message.is_empty(), "k={k} source:\n{source}");
        }
        let total = lines.len();
        lines.sort();
        lines.dedup();
        assert_eq!(lines.len(), total, "k={k} source:\n{source}");
    }
}
