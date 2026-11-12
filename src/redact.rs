//! Deterministic redaction of prompts, logs and uploads (#378).
//!
//! This is the pure engine behind the `redact.text` applet. It works on text
//! only: no regex engine, no randomness, no clock, no I/O. Each detector is one
//! forward scan with bounded lookbehind and lookahead, and caller literals are
//! found with a linear substring search, so running time is linear in the input
//! for at most [`MAX_LITERALS`] literals. Untrusted text cannot drive a
//! backtracking blow-up.
//!
//! The output is the redacted text and a count per category. Matched values are
//! never returned: each distinct value becomes a numbered placeholder such as
//! `<email-1>`, numbered by first occurrence, so one secret always gets one
//! placeholder within a call and the text stays readable.
//!
//! Not covered by this slice: IPv6, MAC addresses, hostnames, usernames, home
//! paths, serials, URLs, PEM blocks, entropy, non-ASCII domains, and allowlists.

use std::collections::BTreeMap;
use std::fmt;

/// Most caller-supplied literal values one call accepts.
pub const MAX_LITERALS: usize = 256;
/// Longest accepted literal value, in bytes.
pub const MAX_LITERAL_BYTES: usize = 64 * 1024;

/// Shortest body accepted after a token prefix or a `Bearer` keyword.
const MIN_TOKEN_BODY: usize = 16;

/// Credential prefixes. `src/` has no shared shape list (the exact-value
/// registry in `security.rs` holds registered values only), so the list lives here.
const TOKEN_PREFIXES: &[&[u8]] = &[
    b"github_pat_",
    b"ghp_",
    b"gho_",
    b"ghu_",
    b"ghs_",
    b"ghr_",
    b"glpat-",
    b"sk_live_",
    b"sk_test_",
    b"sk-",
    b"xoxb-",
    b"xoxp-",
    b"xoxa-",
    b"xoxr-",
    b"xoxs-",
    b"AKIA",
    b"ASIA",
];

/// What a redacted span was detected as. Declaration order is priority: when
/// spans overlap, the merged span takes the earliest category listed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// A caller-supplied exact value.
    Literal,
    /// A credential-shaped token, or the token after `Bearer`.
    Token,
    /// An email address.
    Email,
    /// A dotted-quad IPv4 address.
    Ipv4,
}

impl Category {
    /// The placeholder label and the report key for this category.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Token => "token",
            Self::Email => "email",
            Self::Ipv4 => "ipv4",
        }
    }
}

/// Number of redacted spans per category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub literal: usize,
    pub token: usize,
    pub email: usize,
    pub ipv4: usize,
}

impl Counts {
    /// Redacted spans across all categories. Zero means nothing was found.
    pub fn total(&self) -> usize {
        self.literal
            .saturating_add(self.token)
            .saturating_add(self.email)
            .saturating_add(self.ipv4)
    }

    fn bump(&mut self, category: Category) {
        let slot = match category {
            Category::Literal => &mut self.literal,
            Category::Token => &mut self.token,
            Category::Email => &mut self.email,
            Category::Ipv4 => &mut self.ipv4,
        };
        *slot = slot.saturating_add(1);
    }
}

/// The result of one redaction pass. It holds no matched values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    pub counts: Counts,
}

/// Why a redaction pass refused its input. Each error fails closed: the caller
/// must not send the text onward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedactError {
    TooManyLiterals,
    LiteralTooLong,
    /// Internal invariant violation: a span was not on a character boundary.
    SpanOutOfRange,
}

impl fmt::Display for RedactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyLiterals => "too many redaction literals",
            Self::LiteralTooLong => "a redaction literal is too long",
            Self::SpanOutOfRange => "redaction span was not on a character boundary",
        })
    }
}

impl std::error::Error for RedactError {}

#[derive(Clone, Copy, Debug)]
struct Span {
    start: usize,
    end: usize,
    category: Category,
}

/// Redact `input`. Literal values are matched exactly; empty literals are
/// ignored. Detectors run in one pass each and overlapping matches merge.
pub fn redact_text(input: &str, literals: &[&str]) -> Result<Redaction, RedactError> {
    check_literals(literals)?;
    let bytes = input.as_bytes();
    let mut spans = Vec::new();
    scan_literals(bytes, literals, &mut spans);
    scan_bearer(bytes, &mut spans);
    scan_prefixed_tokens(bytes, &mut spans);
    scan_emails(bytes, &mut spans);
    scan_ipv4(bytes, &mut spans);

    let mut text = String::with_capacity(input.len());
    let mut counts = Counts::default();
    let mut numbers: BTreeMap<(Category, &str), usize> = BTreeMap::new();
    let mut distinct: BTreeMap<Category, usize> = BTreeMap::new();
    let mut cursor = 0;
    for group in merge_spans(spans) {
        let value = input
            .get(group.start..group.end)
            .ok_or(RedactError::SpanOutOfRange)?;
        text.push_str(
            input
                .get(cursor..group.start)
                .ok_or(RedactError::SpanOutOfRange)?,
        );
        let key = (group.category, value);
        let number = if let Some(&known) = numbers.get(&key) {
            known
        } else {
            let seen = distinct.entry(group.category).or_insert(0);
            *seen += 1;
            numbers.insert(key, *seen);
            *seen
        };
        text.push('<');
        text.push_str(group.category.label());
        text.push('-');
        text.push_str(&number.to_string());
        text.push('>');
        counts.bump(group.category);
        cursor = group.end;
    }
    text.push_str(input.get(cursor..).ok_or(RedactError::SpanOutOfRange)?);
    Ok(Redaction { text, counts })
}

fn check_literals(literals: &[&str]) -> Result<(), RedactError> {
    if literals.len() > MAX_LITERALS {
        return Err(RedactError::TooManyLiterals);
    }
    if literals
        .iter()
        .any(|literal| literal.len() > MAX_LITERAL_BYTES)
    {
        return Err(RedactError::LiteralTooLong);
    }
    Ok(())
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_token_body(byte: u8) -> bool {
    is_word_byte(byte) || byte == b'-'
}

fn is_bearer_body(byte: u8) -> bool {
    is_word_byte(byte) || matches!(byte, b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

/// Local-part bytes. Bytes of 0x80 and above count as local, so a non-ASCII
/// local part is covered whole. A run stops only at an ASCII byte, which keeps
/// the span on a character boundary.
fn is_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-') || byte >= 0x80
}

fn is_domain_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-'
}

fn is_ip_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || byte == b'.'
}

/// Advance from `at` while `class` holds and return the first index past the run.
fn run_end(bytes: &[u8], mut at: usize, class: fn(u8) -> bool) -> usize {
    while at < bytes.len() && class(bytes[at]) {
        at += 1;
    }
    at
}

fn scan_literals(bytes: &[u8], literals: &[&str], spans: &mut Vec<Span>) {
    for literal in literals.iter().filter(|literal| !literal.is_empty()) {
        for start in memchr::memmem::find_iter(bytes, literal.as_bytes()) {
            spans.push(Span {
                start,
                end: start + literal.len(),
                category: Category::Literal,
            });
        }
    }
}

/// `Bearer <token>`: redacts the token only, keeping the keyword. The keyword
/// must start at a word boundary and be followed by blanks, so each keyword
/// owns its blank run and its body run, and the scan stays linear.
fn scan_bearer(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut i = 0;
    while i < bytes.len() {
        let keyword_end = i + 6;
        let bounded = i == 0 || !is_word_byte(bytes[i - 1]);
        if bounded
            && keyword_end <= bytes.len()
            && bytes[i..keyword_end].eq_ignore_ascii_case(b"bearer")
        {
            let blanks_end = run_end(bytes, keyword_end, |b| b == b' ' || b == b'\t');
            if blanks_end > keyword_end {
                let body_end = run_end(bytes, blanks_end, is_bearer_body);
                if body_end - blanks_end >= MIN_TOKEN_BODY {
                    spans.push(Span {
                        start: blanks_end,
                        end: body_end,
                        category: Category::Token,
                    });
                    i = body_end;
                    continue;
                }
            }
        }
        i += 1;
    }
}

/// Prefixed credentials such as `ghp_...` or `sk-...`. A prefix can start only
/// where the previous byte is not a token-body byte, so each body run is
/// scanned once and a long `sk-sk-sk-...` line stays linear.
fn scan_prefixed_tokens(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut i = 0;
    while i < bytes.len() {
        if i > 0 && is_token_body(bytes[i - 1]) {
            i += 1;
            continue;
        }
        if let Some(end) = prefixed_token_end(bytes, i) {
            spans.push(Span {
                start: i,
                end,
                category: Category::Token,
            });
            i = end;
        } else {
            i += 1;
        }
    }
}

fn prefixed_token_end(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at..];
    let prefix = TOKEN_PREFIXES
        .iter()
        .find(|candidate| rest.starts_with(candidate))?;
    let body_start = at + prefix.len();
    let body_end = run_end(bytes, body_start, is_token_body);
    (body_end - body_start >= MIN_TOKEN_BODY).then_some(body_end)
}

/// Each `@` owns the local-part run before it and the domain run after it. The
/// runs for distinct `@` signs cannot grow past another `@`, so the scan is linear.
fn scan_emails(bytes: &[u8], spans: &mut Vec<Span>) {
    for at in memchr::memchr_iter(b'@', bytes) {
        let mut start = at;
        while start > 0 && is_local_byte(bytes[start - 1]) {
            start -= 1;
        }
        while start < at && bytes[start] == b'.' {
            start += 1;
        }
        if start == at || bytes[at - 1] == b'.' {
            continue;
        }
        let domain_start = at + 1;
        let mut domain_end = run_end(bytes, domain_start, is_domain_byte);
        while domain_end > domain_start && bytes[domain_end - 1] == b'.' {
            domain_end -= 1;
        }
        if is_email_domain(&bytes[domain_start..domain_end]) {
            spans.push(Span {
                start,
                end: domain_end,
                category: Category::Email,
            });
        }
    }
}

/// At least two labels, no empty label, no leading or trailing hyphen, and an
/// alphabetic top-level label of two or more letters.
fn is_email_domain(domain: &[u8]) -> bool {
    let mut labels = 0usize;
    let mut last: &[u8] = &[];
    for label in domain.split(|&b| b == b'.') {
        labels += 1;
        if label.is_empty() || label.first() == Some(&b'-') || label.last() == Some(&b'-') {
            return false;
        }
        last = label;
    }
    labels >= 2 && last.len() >= 2 && last.iter().all(u8::is_ascii_alphabetic)
}

/// A maximal run of digits and dots, trimmed of outer dots, must be exactly four
/// octets 0 to 255 and must not touch a word byte on either side. Runs are disjoint,
/// so the scan is linear.
fn scan_ipv4(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut i = 0;
    while i < bytes.len() {
        if !is_ip_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let stop = run_end(bytes, i, is_ip_byte);
        let mut start = i;
        let mut end = stop;
        while start < end && bytes[start] == b'.' {
            start += 1;
        }
        while end > start && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let bounded = (start == 0 || !is_word_byte(bytes[start - 1]))
            && (end == bytes.len() || !is_word_byte(bytes[end]));
        if bounded && is_dotted_quad(&bytes[start..end]) {
            spans.push(Span {
                start,
                end,
                category: Category::Ipv4,
            });
        }
        i = stop;
    }
}

fn is_dotted_quad(text: &[u8]) -> bool {
    let mut octets = 0usize;
    for part in text.split(|&b| b == b'.') {
        octets += 1;
        if octets > 4 || part.is_empty() || part.len() > 3 || !part.iter().all(u8::is_ascii_digit) {
            return false;
        }
        let value = part
            .iter()
            .fold(0u32, |acc, &digit| acc * 10 + u32::from(digit - b'0'));
        if value > 255 {
            return false;
        }
    }
    octets == 4
}

/// Sort by start and merge overlapping spans. A merged span keeps the widest
/// reach and the highest-priority category. Ordering is total, so the result is
/// deterministic and no matched byte escapes its group.
fn merge_spans(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|span| span.start < span.end);
    spans.sort_by_key(|span| span.start);
    let mut groups: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match groups.last_mut() {
            Some(group) if span.start < group.end => {
                group.end = group.end.max(span.end);
                group.category = group.category.min(span.category);
            }
            _ => groups.push(span),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn redact(input: &str) -> Redaction {
        redact_text(input, &[]).expect("no literals to reject")
    }

    fn redact_with(input: &str, literals: &[&str]) -> Redaction {
        redact_text(input, literals).expect("literals within limits")
    }

    #[test]
    fn email_true_positives_are_redacted() {
        assert_eq!(
            redact("mail alice@example.com now").text,
            "mail <email-1> now"
        );
        assert_eq!(redact("a.b+c@sub.example.co.uk").text, "<email-1>");
        // The sentence dot after an address is not part of it.
        let r = redact("ping x@y.io.");
        assert_eq!(r.text, "ping <email-1>.");
        assert_eq!(r.counts.email, 1);
    }

    #[test]
    fn email_near_misses_are_left_alone() {
        for text in [
            "user@localhost",
            "@example.com",
            "a@b.c",
            "a@b.123",
            "a@-bad.com",
            "a@bad-.com",
            "a@.com",
            "a@@example.com",
            "trailing@dot..com",
        ] {
            let r = redact(text);
            assert_eq!(r.text, text, "{text} must not match");
            assert_eq!(r.counts.total(), 0, "{text} must not count");
        }
    }

    #[test]
    fn ipv4_true_positives_are_redacted() {
        assert_eq!(redact("eth0 10.0.0.1 up").text, "eth0 <ipv4-1> up");
        assert_eq!(redact("host 192.168.1.20.").text, "host <ipv4-1>.");
        assert_eq!(redact("10.0.0.1/24").text, "<ipv4-1>/24");
        assert_eq!(redact("ssh root@203.0.113.7").text, "ssh root@<ipv4-1>");
    }

    #[test]
    fn ipv4_near_misses_are_left_alone() {
        for text in [
            "256.1.1.1",
            "1.2.3",
            "1.2.3.4.5",
            "v1.2.3.4",
            "10.0.0.1a",
            "1234.1.1.1",
        ] {
            let r = redact(text);
            assert_eq!(r.counts.ipv4, 0, "{text} must not match as IPv4");
        }
    }

    #[test]
    fn token_true_positives_are_redacted() {
        let gh = "ghp_0123456789abcdefABCDEF0123456789abcd";
        assert_eq!(redact(&format!("token={gh} ok")).text, "token=<token-1> ok");
        assert_eq!(
            redact("key sk-proj-abcdefghijklmnopqrstuvwxyz012345 end").text,
            "key <token-1> end"
        );
        assert_eq!(redact("AKIAIOSFODNN7EXAMPLE").text, "<token-1>");
        // Bearer keeps the keyword and redacts only the token.
        assert_eq!(
            redact("Authorization: Bearer abcDEF123456_-.~+/=").text,
            "Authorization: Bearer <token-1>"
        );
    }

    #[test]
    fn repeated_token_shares_one_placeholder() {
        let r = redact("ghp_abcdefghijklmnopqrst ghp_abcdefghijklmnopqrst");
        assert_eq!(r.text, "<token-1> <token-1>");
        assert_eq!(r.counts.token, 2);
    }

    #[test]
    fn token_near_misses_are_left_alone() {
        for text in [
            "ghp_short",
            "ghp_",
            "task-manager-abcdefghijklmnop",
            "xsk-abcdefghijklmnopqrstuvwx",
            "3f786850e387550fdab836ed7e6dc881de23001b",
            "Bearer short",
            "Bearer [REDACTED]",
            "bearers-are-words-here-x",
            "AKIA0123456789",
        ] {
            let r = redact(text);
            assert_eq!(r.counts.token, 0, "{text} must not match as a token");
        }
    }

    #[test]
    fn literal_values_are_redacted_everywhere_they_occur() {
        let r = redact_with("pw=hunter2 again hunter2", &["hunter2"]);
        assert_eq!(r.text, "pw=<literal-1> again <literal-1>");
        assert_eq!(r.counts.literal, 2);
    }

    #[test]
    fn absent_and_empty_literals_change_nothing() {
        let r = redact_with("nothing here", &["zzz", ""]);
        assert_eq!(r.text, "nothing here");
        assert_eq!(r.counts.total(), 0);
    }

    #[test]
    fn literal_limits_fail_closed() {
        let many = vec!["abc"; MAX_LITERALS + 1];
        assert_eq!(redact_text("abc", &many), Err(RedactError::TooManyLiterals));
        let long = "a".repeat(MAX_LITERAL_BYTES + 1);
        assert_eq!(
            redact_text("abc", &[long.as_str()]),
            Err(RedactError::LiteralTooLong)
        );
    }

    #[test]
    fn overlapping_literal_and_email_merge_into_one_placeholder() {
        // The literal covers part of the address, so the whole address goes.
        let r = redact_with("mail alice@example.com", &["example.com"]);
        assert_eq!(r.text, "mail <literal-1>");
        assert_eq!(r.counts.literal, 1);
        assert_eq!(r.counts.email, 0);
    }

    #[test]
    fn overlapping_literals_merge() {
        assert_eq!(redact_with("abcd", &["abc", "bcd"]).text, "<literal-1>");
    }

    #[test]
    fn literal_inside_token_removes_the_whole_token() {
        let r = redact_with("ghp_0123456789abcdef0123 end", &["0123"]);
        assert!(
            !r.text.contains("ghp_"),
            "token remainder must not survive: {}",
            r.text
        );
        assert_eq!(r.text, "<literal-1> end");
    }

    #[test]
    fn placeholders_are_numbered_by_first_occurrence_per_category() {
        let r = redact("b@x.com a@y.com b@x.com 10.0.0.1 10.0.0.2 10.0.0.1");
        assert_eq!(
            r.text,
            "<email-1> <email-2> <email-1> <ipv4-1> <ipv4-2> <ipv4-1>"
        );
        assert_eq!(
            r.counts,
            Counts {
                literal: 0,
                token: 0,
                email: 3,
                ipv4: 3
            }
        );
    }

    #[test]
    fn redaction_is_deterministic_and_idempotent() {
        let input = "a@b.com 10.1.1.1 ghp_abcdefghijklmnopqrst pw hunter2";
        let first = redact_with(input, &["hunter2"]);
        let second = redact_with(input, &["hunter2"]);
        assert_eq!(first, second);
        // A second pass finds nothing left: the require-clean check.
        assert_eq!(redact(&first.text).counts.total(), 0);
    }

    #[test]
    fn multibyte_text_around_matches_is_preserved_whole() {
        let r = redact("café — naïve@example.com — ünï 10.0.0.1 ✓ é");
        assert_eq!(r.text, "café — <email-1> — ünï <ipv4-1> ✓ é");
    }

    #[test]
    fn matches_never_split_a_multibyte_character() {
        assert_eq!(redact("10.0.0.1é").text, "<ipv4-1>é");
        assert_eq!(redact("ñghp_abcdefghijklmnopqrst").text, "ñ<token-1>");
        assert_eq!(redact_with("aéaé", &["é"]).text, "a<literal-1>a<literal-1>");
        assert_eq!(redact_with("éé", &["é"]).counts.literal, 2);
    }

    #[test]
    fn one_megabyte_inputs_finish_quickly() {
        const MB: usize = 1 << 20;
        let line = "mail alice@example.com from 10.0.0.1 token ghp_0123456789abcdef0123\n";
        let repetitions = MB / line.len();
        let mixed = line.repeat(repetitions);
        let pathological = [
            "a".repeat(MB),
            "@".repeat(MB),
            "a@".repeat(MB / 2),
            "1.".repeat(MB / 2),
            "sk-".repeat(MB / 3),
            "bearer".to_string() + &" ".repeat(MB),
            "Bearer ".repeat(MB / 7),
        ];
        let literal = ["hunter2"];

        let started = Instant::now();
        let r = redact_with(&mixed, &literal);
        assert_eq!(r.counts.email, repetitions);
        assert_eq!(r.counts.ipv4, repetitions);
        assert_eq!(r.counts.token, repetitions);
        for input in &pathological {
            let _ = redact_with(input, &literal);
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "1 MB redaction took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn literal_cap_bounds_work_on_a_large_input() {
        // Literal search is linear per literal, so the cost is at most
        // MAX_LITERALS passes over the input. Keep this input small: the
        // test binary runs unoptimised.
        const KB: usize = 1 << 10;
        let input = "x".repeat(64 * KB);
        let literals: Vec<String> = (0..MAX_LITERALS).map(|i| format!("lit{i:03}")).collect();
        let refs: Vec<&str> = literals.iter().map(String::as_str).collect();
        let started = Instant::now();
        let r = redact_with(&input, &refs);
        assert_eq!(r.text, input);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "64 KB with {MAX_LITERALS} literals took {:?}",
            started.elapsed()
        );
    }
}
