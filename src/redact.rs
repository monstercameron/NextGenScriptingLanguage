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
//! Covered: PEM private-key blocks, caller literals, URL userinfo credentials,
//! token shapes, email addresses, IPv6 addresses, IPv4 addresses and MAC
//! addresses. Not covered yet: hostnames, usernames, home paths, serials, UUIDs,
//! Wi-Fi SSID and BSSID names, token query parameters, JWTs, entropy, OpenPGP
//! blocks, non-ASCII domains, and allowlists.

use std::collections::BTreeMap;
use std::fmt;

/// Most caller-supplied literal values one call accepts.
pub const MAX_LITERALS: usize = 256;
/// Longest accepted literal value, in bytes.
pub const MAX_LITERAL_BYTES: usize = 64 * 1024;

/// Shortest body accepted after a token prefix or a `Bearer` keyword.
const MIN_TOKEN_BODY: usize = 16;

/// Bytes in one colon- or hyphen-separated MAC address: six pairs and five separators.
const MAC_LEN: usize = 17;

const PEM_BEGIN: &[u8] = b"-----BEGIN ";
const PEM_DASHES: &[u8] = b"-----";

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
    /// A PEM private-key block whose matching END line never appears: the span
    /// runs to the end of the text (fail closed).
    PemUnterminated,
    /// A complete PEM private-key block, BEGIN line to matching END line.
    PemKey,
    /// A caller-supplied exact value.
    Literal,
    /// The password of a URL userinfo, or the whole userinfo when it has no colon.
    UrlCredential,
    /// A credential-shaped token, or the token after `Bearer`.
    Token,
    /// An email address.
    Email,
    /// An IPv6 address, full or `::`-compressed, optionally ending in an IPv4 tail.
    Ipv6,
    /// A dotted-quad IPv4 address.
    Ipv4,
    /// A MAC address written as six pairs separated by colons or hyphens.
    Mac,
}

impl Category {
    /// The placeholder label and the report key for this category.
    pub const fn label(self) -> &'static str {
        match self {
            Self::PemUnterminated => "pem-unterminated",
            Self::PemKey => "pem-key",
            Self::Literal => "literal",
            Self::UrlCredential => "url-credential",
            Self::Token => "token",
            Self::Email => "email",
            Self::Ipv6 => "ipv6",
            Self::Ipv4 => "ipv4",
            Self::Mac => "mac",
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
    pub url_credential: usize,
    pub pem_key: usize,
    pub pem_unterminated: usize,
    pub ipv6: usize,
    pub mac: usize,
}

impl Counts {
    /// Redacted spans across all categories. Zero means nothing was found.
    pub fn total(&self) -> usize {
        [
            self.literal,
            self.token,
            self.email,
            self.ipv4,
            self.url_credential,
            self.pem_key,
            self.pem_unterminated,
            self.ipv6,
            self.mac,
        ]
        .iter()
        .fold(0usize, |sum, &count| sum.saturating_add(count))
    }

    fn bump(&mut self, category: Category) {
        let slot = match category {
            Category::PemUnterminated => &mut self.pem_unterminated,
            Category::PemKey => &mut self.pem_key,
            Category::Literal => &mut self.literal,
            Category::UrlCredential => &mut self.url_credential,
            Category::Token => &mut self.token,
            Category::Email => &mut self.email,
            Category::Ipv6 => &mut self.ipv6,
            Category::Ipv4 => &mut self.ipv4,
            Category::Mac => &mut self.mac,
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
    let spans = detect(input.as_bytes(), literals);

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

/// Whether `input` contains anything [`redact_text`] would redact. Shares the
/// detectors with the redaction pass. An input over the literal limits is not
/// clean: the caller must not send it.
pub fn require_clean(input: &str, literals: &[&str]) -> bool {
    check_literals(literals).is_ok() && detect(input.as_bytes(), literals).is_empty()
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

/// Run every detector and apply the precedence rules. Returns unmerged spans.
fn detect(bytes: &[u8], literals: &[&str]) -> Vec<Span> {
    let mut spans = Vec::new();
    scan_pem_blocks(bytes, &mut spans);
    scan_literals(bytes, literals, &mut spans);
    let mut urls = Vec::new();
    scan_url_credentials(bytes, &mut urls);
    scan_bearer(bytes, &mut spans);
    scan_prefixed_tokens(bytes, &mut spans);
    scan_emails(bytes, &mut spans);
    scan_ipv4(bytes, &mut spans);
    let mut ipv6 = Vec::new();
    scan_ipv6(bytes, &mut ipv6);
    let mut macs = Vec::new();
    scan_macs(bytes, &mut macs);

    // Precedence 1: a MAC-shaped window inside a valid IPv6 address is part of
    // that address, so the IPv6 detector wins. A six-group run is never IPv6
    // (IPv6 needs eight groups or a `::`), so the two only meet inside longer runs.
    macs.retain(|mac| !overlaps_sorted(&ipv6, mac.start, mac.end));
    spans.extend(ipv6);
    spans.extend(macs);

    // Precedence 2: URL userinfo is authoritative. Other matches that overlap a
    // credential are dropped, so an email or address that starts inside the
    // userinfo cannot take the host with it. Literals and PEM blocks still merge.
    spans.retain(|span| {
        survives_url_userinfo(span.category) || !overlaps_sorted(&urls, span.start, span.end)
    });
    spans.extend(urls);
    spans
}

fn survives_url_userinfo(category: Category) -> bool {
    matches!(
        category,
        Category::PemUnterminated | Category::PemKey | Category::Literal | Category::UrlCredential
    )
}

/// `sorted` must be sorted by start with disjoint spans, as every detector
/// that produces it emits them. Only the last span that starts before `end` can
/// reach `start`, so one lookup decides the overlap.
fn overlaps_sorted(sorted: &[Span], start: usize, end: usize) -> bool {
    let after = sorted.partition_point(|span| span.start < end);
    after > 0 && sorted[after - 1].end > start
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

/// Bytes that can appear in an IPv6 address, including the dots of an IPv4 tail.
fn is_ipv6_byte(byte: u8) -> bool {
    byte.is_ascii_hexdigit() || byte == b':' || byte == b'.'
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

/// Anything that can sit in a URL authority. Whitespace, `/`, `?`, `#` and the
/// quote and bracket characters end it. All of those are ASCII, so a span that
/// starts after a `:` or ends before an `@` stays on a character boundary.
fn is_authority_byte(byte: u8) -> bool {
    !(byte.is_ascii_whitespace()
        || matches!(byte, b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' | b'`'))
}

fn is_pem_label_byte(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b' '
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

/// A PEM private-key block runs from its `-----BEGIN <label>-----` marker to
/// the `-----END <label>-----` line with the same label. The label must be
/// uppercase letters, digits and spaces, and must be `PRIVATE KEY` or end in
/// ` PRIVATE KEY`, so certificates, public keys and OpenPGP blocks are not matched.
/// When no matching END line follows, the block runs to the end of the text and
/// is reported as unterminated. Each scan resumes after the span it consumed, so
/// the scan is linear.
fn scan_pem_blocks(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut from = 0;
    while let Some(offset) = memchr::memmem::find(&bytes[from..], PEM_BEGIN) {
        let begin = from + offset;
        let label_start = begin + PEM_BEGIN.len();
        let label_end = run_end(bytes, label_start, is_pem_label_byte);
        let label = &bytes[label_start..label_end];
        let private_key = label == &b"PRIVATE KEY"[..] || label.ends_with(&b" PRIVATE KEY"[..]);
        if !private_key || !bytes[label_end..].starts_with(PEM_DASHES) {
            from = begin + 1;
            continue;
        }
        let close = label_end + PEM_DASHES.len();
        let mut end_line = Vec::with_capacity(label.len() + 16);
        end_line.extend_from_slice(b"-----END ");
        end_line.extend_from_slice(label);
        end_line.extend_from_slice(PEM_DASHES);
        let (end, category) = match memchr::memmem::find(&bytes[close..], &end_line) {
            Some(found) => (close + found + end_line.len(), Category::PemKey),
            None => (bytes.len(), Category::PemUnterminated),
        };
        spans.push(Span {
            start: begin,
            end,
            category,
        });
        from = end;
    }
}

/// `scheme://userinfo@host`: redacts the password after the first colon, or the
/// whole userinfo when there is no colon. The scheme and host stay readable.
/// Authorities end at the first `/`, so the authorities of distinct `://`
/// markers are disjoint and the scan is linear. Spans come out sorted and disjoint.
fn scan_url_credentials(bytes: &[u8], spans: &mut Vec<Span>) {
    for marker in memchr::memmem::find_iter(bytes, b"://") {
        let mut scheme = marker;
        while scheme > 0 && is_scheme_byte(bytes[scheme - 1]) {
            scheme -= 1;
        }
        if scheme == marker || !bytes[scheme].is_ascii_alphabetic() {
            continue;
        }
        let authority_start = marker + 3;
        let authority_end = run_end(bytes, authority_start, is_authority_byte);
        let authority = &bytes[authority_start..authority_end];
        let Some(at) = authority.iter().rposition(|&byte| byte == b'@') else {
            continue;
        };
        let userinfo_end = authority_start + at;
        let start = match authority[..at].iter().position(|&byte| byte == b':') {
            Some(colon) => authority_start + colon + 1,
            None => authority_start,
        };
        if start < userinfo_end {
            spans.push(Span {
                start,
                end: userinfo_end,
                category: Category::UrlCredential,
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

/// Each maximal run of IPv6 bytes is one candidate. A run with a leading single
/// colon (as in `addr:fe80::1`) starts after that colon. A candidate must not
/// touch a word byte, and it may drop one trailing `.` or `:` when the full run
/// is not an address (as in `fe80::1: up`). Runs are disjoint, so the scan is linear.
fn scan_ipv6(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut i = 0;
    while i < bytes.len() {
        if !is_ipv6_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let stop = run_end(bytes, i, is_ipv6_byte);
        if let Some(span) = ipv6_in_run(bytes, i, stop) {
            spans.push(span);
        }
        i = stop;
    }
}

fn ipv6_in_run(bytes: &[u8], run_start: usize, run_stop: usize) -> Option<Span> {
    let mut start = run_start;
    if bytes[start] == b':' && bytes.get(start + 1) != Some(&b':') {
        start += 1;
    }
    if start >= run_stop {
        return None;
    }
    if start > 0 && is_word_byte(bytes[start - 1]) {
        return None;
    }
    let trimmed = if matches!(bytes[run_stop - 1], b'.' | b':') {
        run_stop - 1
    } else {
        run_stop
    };
    for end in [run_stop, trimmed] {
        let candidate = &bytes[start..end];
        let bounded_after = end == bytes.len() || !is_word_byte(bytes[end]);
        if bounded_after && candidate.contains(&b':') && is_ipv6(candidate) {
            return Some(Span {
                start,
                end,
                category: Category::Ipv6,
            });
        }
    }
    None
}

/// Strict IPv6 check. A full address has exactly eight groups. A `::`-compressed
/// address has at most seven explicit groups, at least one, and only one `::`.
/// Each group is one to four hex digits. A dotted-quad IPv4 tail counts as two
/// groups and is allowed only at the end.
fn is_ipv6(text: &[u8]) -> bool {
    match memchr::memmem::find(text, b"::") {
        None => count_ipv6_groups(text, true) == Some(8),
        Some(at) => {
            let tail = &text[at + 2..];
            if memchr::memmem::find(tail, b"::").is_some() {
                return false;
            }
            match (
                count_ipv6_groups(&text[..at], false),
                count_ipv6_groups(tail, true),
            ) {
                (Some(head), Some(rest)) => (1..=7).contains(&(head + rest)),
                _ => false,
            }
        }
    }
}

/// Counts the groups in one side of an IPv6 address, or `None` when a group is
/// malformed. Stops early past eight groups, so long input stays cheap.
fn count_ipv6_groups(part: &[u8], last_may_be_quad: bool) -> Option<usize> {
    if part.is_empty() {
        return Some(0);
    }
    let mut groups = 0usize;
    let mut pieces = part.split(|&byte| byte == b':').peekable();
    while let Some(piece) = pieces.next() {
        let last = pieces.peek().is_none();
        groups += if last && last_may_be_quad && piece.contains(&b'.') {
            if !is_dotted_quad(piece) {
                return None;
            }
            2
        } else if (1..=4).contains(&piece.len()) && piece.iter().all(u8::is_ascii_hexdigit) {
            1
        } else {
            return None;
        };
        if groups > 8 {
            return None;
        }
    }
    Some(groups)
}

/// Six pairs of hex digits joined by one repeated separator (`:` or `-`). Each
/// window is a fixed 17 bytes, so the scan is linear. Adjacent alphanumerics or
/// underscores disqualify a window. A window that falls inside a valid IPv6
/// address is removed in [`detect`], so IPv6 takes precedence. A longer colon
/// chain that is not IPv6 still has its first six groups redacted (fail closed).
fn scan_macs(bytes: &[u8], spans: &mut Vec<Span>) {
    let mut at = 0;
    while at + MAC_LEN <= bytes.len() {
        let end = at + MAC_LEN;
        let bounded = (at == 0 || !is_word_byte(bytes[at - 1]))
            && (end == bytes.len() || !is_word_byte(bytes[end]));
        if bounded && is_mac_window(&bytes[at..end]) {
            spans.push(Span {
                start: at,
                end,
                category: Category::Mac,
            });
            at = end;
        } else {
            at += 1;
        }
    }
}

fn is_mac_window(window: &[u8]) -> bool {
    let Some(&separator) = window.get(2) else {
        return false;
    };
    (separator == b':' || separator == b'-')
        && window.len() == MAC_LEN
        && window.iter().enumerate().all(|(index, &byte)| {
            if index % 3 == 2 {
                byte == separator
            } else {
                byte.is_ascii_hexdigit()
            }
        })
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
        let gh = concat!("gh", "p_0123456789abcdefABCDEF0123456789abcd");
        assert_eq!(redact(&format!("token={gh} ok")).text, "token=<token-1> ok");
        assert_eq!(
            redact(concat!(
                "key sk",
                "-proj-abcdefghijklmnopqrstuvwxyz012345 end"
            ))
            .text,
            "key <token-1> end"
        );
        assert_eq!(
            redact(concat!("AK", "IAIOSFODNN7EXAMPLE")).text,
            "<token-1>"
        );
        // Bearer keeps the keyword and redacts only the token.
        assert_eq!(
            redact("Authorization: Bearer abcDEF123456_-.~+/=").text,
            "Authorization: Bearer <token-1>"
        );
    }

    #[test]
    fn repeated_token_shares_one_placeholder() {
        let r = redact(concat!(
            "gh",
            "p_abcdefghijklmnopqrst gh",
            "p_abcdefghijklmnopqrst"
        ));
        assert_eq!(r.text, "<token-1> <token-1>");
        assert_eq!(r.counts.token, 2);
    }

    #[test]
    fn token_near_misses_are_left_alone() {
        for text in [
            "ghp_short",
            "ghp_",
            concat!("task", "-manager-abcdefghijklmnop"),
            concat!("xsk", "-abcdefghijklmnopqrstuvwx"),
            "3f786850e387550fdab836ed7e6dc881de23001b",
            "Bearer short",
            "Bearer [REDACTED]",
            "bearers-are-words-here-x",
            concat!("AK", "IA0123456789"),
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
        let r = redact_with(concat!("gh", "p_0123456789abcdef0123 end"), &["0123"]);
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
                email: 3,
                ipv4: 3,
                ..Counts::default()
            }
        );
    }

    #[test]
    fn old_categories_output_is_pinned() {
        // Text with only the original categories must keep its exact output and
        // its exact counts, so existing callers see no change.
        let input = concat!(
            "mail alice@example.com from 10.0.0.1 token gh",
            "p_0123456789abcdef0123 pw hunter2"
        );
        let r = redact_with(input, &["hunter2"]);
        assert_eq!(
            r.text,
            "mail <email-1> from <ipv4-1> token <token-1> pw <literal-1>"
        );
        assert_eq!(
            r.counts,
            Counts {
                literal: 1,
                token: 1,
                email: 1,
                ipv4: 1,
                ..Counts::default()
            }
        );
    }

    #[test]
    fn redaction_is_deterministic_and_idempotent() {
        let input = concat!("a@b.com 10.1.1.1 gh", "p_abcdefghijklmnopqrst pw hunter2");
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
        assert_eq!(
            redact(concat!("ñgh", "p_abcdefghijklmnopqrst")).text,
            "ñ<token-1>"
        );
        assert_eq!(redact_with("aéaé", &["é"]).text, "a<literal-1>a<literal-1>");
        assert_eq!(redact_with("éé", &["é"]).counts.literal, 2);
    }

    #[test]
    fn url_password_is_redacted_and_scheme_and_host_stay() {
        let r = redact("fetch https://alice:s3cret@example.com/a?b=1 now");
        assert_eq!(
            r.text,
            "fetch https://alice:<url-credential-1>@example.com/a?b=1 now"
        );
        assert_eq!(r.counts.url_credential, 1);
    }

    #[test]
    fn url_without_colon_redacts_the_whole_userinfo() {
        // The userinfo is the credential here, so the email-shaped match on it
        // is dropped and the host stays readable.
        let r = redact("ssh://deploy@build.example.org");
        assert_eq!(r.text, "ssh://<url-credential-1>@build.example.org");
        assert_eq!(r.counts.url_credential, 1);
        assert_eq!(r.counts.email, 0);
    }

    #[test]
    fn url_userinfo_wins_over_email_ipv4_and_token_overlaps() {
        let r = redact("https://alice:pw@example.com");
        assert_eq!(r.text, "https://alice:<url-credential-1>@example.com");
        assert_eq!(r.counts.email, 0);

        let r = redact("https://10.0.0.1@example.com");
        assert_eq!(r.text, "https://<url-credential-1>@example.com");
        assert_eq!(r.counts.ipv4, 0);

        let r = redact(concat!(
            "https://u:gh",
            "p_0123456789abcdefghij@github.com/x"
        ));
        assert_eq!(r.text, "https://u:<url-credential-1>@github.com/x");
        assert_eq!(r.counts.token, 0);
    }

    #[test]
    fn url_passwords_get_numbered_placeholders_per_value() {
        let r = redact("https://a:one@x.io https://b:two@y.io https://c:one@z.io");
        assert_eq!(
            r.text,
            "https://a:<url-credential-1>@x.io https://b:<url-credential-2>@y.io https://c:<url-credential-1>@z.io"
        );
        assert_eq!(r.counts.url_credential, 3);
    }

    #[test]
    fn url_near_misses_are_left_alone() {
        for text in [
            "https://example.com:8080/a@b",
            "https://example.com/@user",
            "https://example.com?x=a@b",
            "alice:pw@host",
            "://alice:pw@host",
            "https://",
        ] {
            let r = redact(text);
            assert_eq!(r.text, text, "{text} must not match");
        }
    }

    #[test]
    fn pem_private_key_block_becomes_one_placeholder() {
        let text = concat!(
            "before\n-----BEGIN RSA PR",
            "IVATE KEY-----\nMIIBOgIBAAJBAK\nAAAAB\n-----END RSA PR",
            "IVATE KEY-----\nafter"
        );
        let r = redact(text);
        assert_eq!(r.text, "before\n<pem-key-1>\nafter");
        assert_eq!(r.counts.pem_key, 1);
        assert_eq!(r.counts.pem_unterminated, 0);
    }

    #[test]
    fn pem_labels_cover_private_keys_only() {
        for label in [
            concat!("PR", "IVATE KEY"),
            concat!("RSA PR", "IVATE KEY"),
            concat!("EC PR", "IVATE KEY"),
            concat!("ENCRYPTED PR", "IVATE KEY"),
            concat!("OPENSSH PR", "IVATE KEY"),
        ] {
            let text = format!("-----BEGIN {label}-----\nAB\n-----END {label}-----");
            assert_eq!(redact(&text).text, "<pem-key-1>", "{label} must match");
        }
        for label in [
            "CERTIFICATE",
            "PUBLIC KEY",
            "RSA PUBLIC KEY",
            "PGP PUBLIC KEY BLOCK",
        ] {
            let text = format!("-----BEGIN {label}-----\nAB\n-----END {label}-----");
            let r = redact(&text);
            assert_eq!(r.text, text, "{label} must not match");
            assert_eq!(r.counts.total(), 0);
        }
    }

    #[test]
    fn pem_near_misses_are_left_alone() {
        for text in [
            concat!("-----BEGIN PR", "IVATE KEY"),
            concat!(
                "-----BEGIN rsa priv",
                "ate key-----\nA\n-----END rsa priv",
                "ate key-----"
            ),
            concat!("BEGIN PR", "IVATE KEY-----"),
        ] {
            let r = redact(text);
            assert_eq!(r.text, text, "{text} must not match");
        }
    }

    #[test]
    fn unterminated_pem_block_fails_closed_to_the_end() {
        let r = redact(concat!(
            "keep\n-----BEGIN OPENSSH PR",
            "IVATE KEY-----\nAAAA\nBBBB"
        ));
        assert_eq!(r.text, "keep\n<pem-unterminated-1>");
        assert_eq!(r.counts.pem_unterminated, 1);
        assert_eq!(r.counts.pem_key, 0);
        // An END line for a different label does not close the block.
        let r = redact(concat!(
            "-----BEGIN PR",
            "IVATE KEY-----\nAAAA\n-----END RSA PR",
            "IVATE KEY-----\ntail"
        ));
        assert_eq!(r.text, "<pem-unterminated-1>");
    }

    #[test]
    fn consecutive_pem_blocks_get_separate_placeholders() {
        let text = concat!(
            "-----BEGIN PR",
            "IVATE KEY-----\nA\n-----END PR",
            "IVATE KEY-----\n-----BEGIN PR",
            "IVATE KEY-----\nB\n-----END PR",
            "IVATE KEY-----"
        );
        let r = redact(text);
        assert_eq!(r.text, "<pem-key-1>\n<pem-key-2>");
        assert_eq!(r.counts.pem_key, 2);
    }

    #[test]
    fn ipv6_true_positives_are_redacted() {
        assert_eq!(redact("route 2001:db8::1 up").text, "route <ipv6-1> up");
        assert_eq!(redact("lo ::1").text, "lo <ipv6-1>");
        assert_eq!(redact("fe80::1%eth0").text, "<ipv6-1>%eth0");
        assert_eq!(redact("addr fe80::1: up").text, "addr <ipv6-1>: up");
        assert_eq!(redact("addr:fe80::1 up").text, "addr:<ipv6-1> up");
        assert_eq!(redact("[::1]:8080").text, "[<ipv6-1>]:8080");
        assert_eq!(
            redact("2001:0db8:0000:0000:0000:ff00:0042:8329").text,
            "<ipv6-1>"
        );
        // An IPv4 tail is part of the IPv6 match, not a separate IPv4 match.
        let r = redact("::ffff:192.0.2.128");
        assert_eq!(r.text, "<ipv6-1>");
        assert_eq!(r.counts.ipv4, 0);
    }

    #[test]
    fn ipv6_near_misses_are_left_alone() {
        for text in [
            "12:34:56",
            "10:30 and 01:52:49",
            "std::string",
            "Foo::Bar::baz",
            "::",
            "a:b",
            "1:2:3:4:5:6:7:8:9",
            "12345::1",
            "1::2::3",
            "1:2:3:4:5:6:7::8",
            "gggg::1",
            "fe80::1::",
            "2001:db8:1:2:3:4:5:6:7",
            "1.2.3.4:5",
        ] {
            let r = redact(text);
            assert_eq!(r.counts.ipv6, 0, "{text} must not match as IPv6");
        }
    }

    #[test]
    fn mac_true_positives_are_redacted() {
        assert_eq!(redact("mac 00:1a:2b:3c:4d:5e.").text, "mac <mac-1>.");
        assert_eq!(redact("BSSID=AA-BB-CC-DD-EE-FF").text, "BSSID=<mac-1>");
        assert_eq!(redact("mac:00:11:22:33:44:55").text, "mac:<mac-1>");
    }

    #[test]
    fn mac_near_misses_are_left_alone() {
        for text in [
            "00:1a:2b:3c:4d",
            "x00:1a:2b:3c:4d:5e",
            "00:1a:2b:3c:4d:5g",
            "00:1a:2b:3c:4d:5e7",
            "00:1a-2b:3c:4d:5e",
            "00-1a:2b-3c-4d-5e",
        ] {
            let r = redact(text);
            assert_eq!(r.counts.mac, 0, "{text} must not match as a MAC");
        }
    }

    #[test]
    fn ipv6_takes_precedence_over_mac_shaped_fragments() {
        let r = redact("::aa:bb:cc:dd:ee:ff");
        assert_eq!(r.text, "<ipv6-1>");
        assert_eq!(r.counts.mac, 0);
        let r = redact("fe80::aa:bb:cc:dd:ee:ff");
        assert_eq!(r.text, "<ipv6-1>");
        assert_eq!(r.counts.mac, 0);
        let r = redact("aa:bb:cc:dd:ee:ff:00:11");
        assert_eq!(r.text, "<ipv6-1>");
        assert_eq!(r.counts.mac, 0);
        // Seven groups are not an IPv6 address, so the six-group window still matches.
        let r = redact("1:aa:bb:cc:dd:ee:ff");
        assert_eq!(r.text, "1:<mac-1>");
        assert_eq!(r.counts.ipv6, 0);
    }

    #[test]
    fn new_categories_coexist_with_old_ones() {
        let r = redact(
            "login https://bob:pw1@h.io ip 2001:db8::7 mac aa-bb-cc-dd-ee-01 mail bob@corp.io",
        );
        assert_eq!(
            r.text,
            "login https://bob:<url-credential-1>@h.io ip <ipv6-1> mac <mac-1> mail <email-1>"
        );
        assert_eq!(
            r.counts,
            Counts {
                email: 1,
                url_credential: 1,
                ipv6: 1,
                mac: 1,
                ..Counts::default()
            }
        );
    }

    #[test]
    fn pem_block_absorbs_matches_inside_it() {
        let text = concat!(
            "-----BEGIN PR",
            "IVATE KEY-----\nabc 10.0.0.1 hunter2 AK",
            "IAIOSFODNN7EXAMPLE\n-----END PR",
            "IVATE KEY-----"
        );
        let r = redact_with(text, &["hunter2"]);
        assert_eq!(r.text, "<pem-key-1>");
        assert_eq!(r.counts.total(), 1);
    }

    #[test]
    fn require_clean_reports_any_detector_hit() {
        assert!(require_clean("plain text, 12:34:56 and std::string", &[]));
        assert!(require_clean("", &[]));
        assert!(!require_clean("mail a@b.co", &[]));
        assert!(!require_clean("ip 2001:db8::1", &[]));
        assert!(!require_clean("mac 00:1a:2b:3c:4d:5e", &[]));
        assert!(!require_clean("https://u:p@h.io", &[]));
        assert!(!require_clean(
            concat!("-----BEGIN PR", "IVATE KEY-----"),
            &[]
        ));
        assert!(!require_clean(
            concat!("token gh", "p_0123", "456789", "abcdef", "0123"),
            &[]
        ));
        assert!(!require_clean("hunter2 here", &["hunter2"]));
        assert!(require_clean("hunter3 here", &["hunter2"]));
        // Over the literal limit the input is refused, so it is not clean.
        assert!(!require_clean("plain", &vec!["a"; MAX_LITERALS + 1]));
    }

    #[test]
    fn require_clean_agrees_with_redact_text() {
        for text in [
            "clean words only",
            "mail a@b.co",
            "ssh 10.0.0.1",
            "https://u:p@h.io",
            "fe80::1",
        ] {
            let clean = require_clean(text, &[]);
            assert_eq!(clean, redact(text).counts.total() == 0, "{text}");
        }
    }

    #[test]
    fn one_megabyte_inputs_finish_quickly() {
        const MB: usize = 1 << 20;
        let line = concat!(
            "mail alice@example.com from 10.0.0.1 token gh",
            "p_0123456789abcdef0123\n"
        );
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
    fn new_detectors_stay_linear_on_one_megabyte() {
        const MB: usize = 1 << 20;
        let inputs = [
            "::".repeat(MB / 2),
            "1:".repeat(MB / 2),
            "aa:bb:".repeat(MB / 6),
            "00-".repeat(MB / 3),
            "https://u:p@".repeat(MB / 12),
            "https://".repeat(MB / 8),
            format!(
                concat!("-----BEGIN PR", "IVATE KEY-----\n{}"),
                "A".repeat(MB)
            ),
        ];
        let started = Instant::now();
        for input in &inputs {
            let first = redact(input);
            let second = redact(input);
            assert_eq!(first, second, "output must be deterministic");
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "1 MB inputs for the new detectors took {:?}",
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
