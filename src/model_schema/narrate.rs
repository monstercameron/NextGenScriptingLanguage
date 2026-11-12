//! Deterministic verification of model-written narrations (issue #392).
//!
//! A narration is display text a model wrote about typed facts such as diff
//! rows, plan steps or run events. `verify` decides whether that text may be
//! shown. It enforces that every citation names a real fact, that every fact is
//! cited or omitted and never both, that irreversible facts are cited, that
//! every number, quoted token, path-like token and capitalised word traces to a
//! field of a fact the sentence cites, and that sentence count, sentence length
//! and control characters stay within bounds.
//!
//! Nothing here calls a model. A verified narration is display text only; it is
//! never an input to an approval, and `render_deterministic` is the fallback
//! that is shown whenever verification fails.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A scalar field value of a fact.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub(crate) enum Scalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl Scalar {
    /// The verbatim text of the value, used for quote and name checks.
    fn display(&self) -> String {
        match self {
            Scalar::Bool(value) => value.to_string(),
            Scalar::Int(value) => value.to_string(),
            Scalar::Float(value) => value.to_string(),
            Scalar::Text(value) => value.clone(),
        }
    }
}

/// A typed row that a narration may cite. `id` is the citation key.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Fact {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) fields: BTreeMap<String, Scalar>,
}

/// The strict shape the model is asked to produce through `llm.extract`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Narration {
    pub(crate) sentences: Vec<Sentence>,
    pub(crate) omitted: Vec<String>,
}

/// One sentence of narration and the fact ids it claims to rest on.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct Sentence {
    pub(crate) text: String,
    pub(crate) cites: Vec<String>,
}

/// Bounds and the caller-supplied irreversibility marker.
///
/// A fact is irreversible when its kind is in `irreversible_kinds`, or when its
/// field `name` holds exactly `value` in `irreversible_field`. The marker comes
/// from the caller because the contract field of #327 is not wired yet.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Limits {
    pub(crate) max_sentences: usize,
    pub(crate) max_sentence_chars: usize,
    pub(crate) max_violations: usize,
    pub(crate) irreversible_kinds: BTreeSet<String>,
    pub(crate) irreversible_field: Option<(String, String)>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_sentences: 200,
            max_sentence_chars: 2_000,
            max_violations: 1_000,
            irreversible_kinds: BTreeSet::new(),
            irreversible_field: None,
        }
    }
}

/// A narration that passed every rule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Verified {
    pub(crate) narration: Narration,
}

/// One broken rule, naming the sentence index and the offending token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Violation {
    EmptySentence {
        sentence: usize,
    },
    SentenceTooLong {
        sentence: usize,
        chars: usize,
        max: usize,
    },
    TooManySentences {
        count: usize,
        max: usize,
    },
    ControlCharacter {
        sentence: usize,
        code: u32,
    },
    UnknownCitation {
        sentence: usize,
        id: String,
    },
    UnknownOmitted {
        id: String,
    },
    DuplicateFactId {
        id: String,
    },
    UncitedFact {
        id: String,
    },
    CitedAndOmitted {
        id: String,
    },
    IrreversibleNotCited {
        id: String,
    },
    InventedNumber {
        sentence: usize,
        token: String,
    },
    InventedQuote {
        sentence: usize,
        token: String,
    },
    InventedPath {
        sentence: usize,
        token: String,
    },
    InventedName {
        sentence: usize,
        token: String,
    },
    Truncated {
        limit: usize,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::EmptySentence { sentence } => {
                write!(formatter, "sentence {sentence} is empty")
            }
            Violation::SentenceTooLong {
                sentence,
                chars,
                max,
            } => write!(
                formatter,
                "sentence {sentence} has {chars} characters, the limit is {max}"
            ),
            Violation::TooManySentences { count, max } => {
                write!(
                    formatter,
                    "narration has {count} sentences, the limit is {max}"
                )
            }
            Violation::ControlCharacter { sentence, code } => write!(
                formatter,
                "sentence {sentence} contains control character U+{code:04X}"
            ),
            Violation::UnknownCitation { sentence, id } => {
                write!(formatter, "sentence {sentence} cites unknown fact {id:?}")
            }
            Violation::UnknownOmitted { id } => {
                write!(formatter, "omitted list names unknown fact {id:?}")
            }
            Violation::DuplicateFactId { id } => write!(formatter, "fact id {id:?} is repeated"),
            Violation::UncitedFact { id } => {
                write!(formatter, "fact {id:?} is neither cited nor omitted")
            }
            Violation::CitedAndOmitted { id } => {
                write!(formatter, "fact {id:?} is both cited and omitted")
            }
            Violation::IrreversibleNotCited { id } => {
                write!(
                    formatter,
                    "irreversible fact {id:?} must be cited, not omitted"
                )
            }
            Violation::InventedNumber { sentence, token } => write!(
                formatter,
                "sentence {sentence}: number {token:?} does not occur in a cited fact"
            ),
            Violation::InventedQuote { sentence, token } => write!(
                formatter,
                "sentence {sentence}: quoted {token:?} does not occur in a cited fact"
            ),
            Violation::InventedPath { sentence, token } => write!(
                formatter,
                "sentence {sentence}: path {token:?} does not occur in a cited fact"
            ),
            Violation::InventedName { sentence, token } => write!(
                formatter,
                "sentence {sentence}: capitalised word {token:?} does not occur in a cited fact"
            ),
            Violation::Truncated { limit } => {
                write!(
                    formatter,
                    "more than {limit} violations; the rest are not listed"
                )
            }
        }
    }
}

/// Collects violations up to a bound, then records that the list was cut.
struct Collector {
    items: Vec<Violation>,
    limit: usize,
    truncated: bool,
}

impl Collector {
    fn push(&mut self, violation: Violation) {
        if self.items.len() < self.limit {
            self.items.push(violation);
        } else if !self.truncated {
            self.truncated = true;
            self.items.push(Violation::Truncated { limit: self.limit });
        }
    }
}

/// Checks a model narration against the facts it may cite.
///
/// Returns every violation found (bounded by `limits.max_violations`), or the
/// narration unchanged when every rule holds. Never panics on any input.
pub(crate) fn verify(
    facts: &[Fact],
    narration: &Narration,
    limits: &Limits,
) -> Result<Verified, Vec<Violation>> {
    let mut out = Collector {
        items: Vec::new(),
        limit: limits.max_violations,
        truncated: false,
    };
    if narration.sentences.len() > limits.max_sentences {
        out.push(Violation::TooManySentences {
            count: narration.sentences.len(),
            max: limits.max_sentences,
        });
    }

    let mut known: BTreeMap<&str, &Fact> = BTreeMap::new();
    for fact in facts {
        if known.contains_key(fact.id.as_str()) {
            out.push(Violation::DuplicateFactId {
                id: fact.id.clone(),
            });
        } else {
            known.insert(fact.id.as_str(), fact);
        }
    }

    let mut cited: BTreeSet<&str> = BTreeSet::new();
    for (index, sentence) in narration.sentences.iter().enumerate() {
        let mut cited_texts: Vec<String> = Vec::new();
        let mut cited_numbers: BTreeSet<String> = BTreeSet::new();
        for id in &sentence.cites {
            match known.get(id.as_str()) {
                Some(fact) => {
                    cited.insert(id.as_str());
                    for scalar in fact.fields.values() {
                        cited_texts.push(scalar.display());
                        cited_numbers.extend(field_numbers(scalar));
                    }
                }
                None => out.push(Violation::UnknownCitation {
                    sentence: index,
                    id: id.clone(),
                }),
            }
        }
        check_sentence(
            index,
            &sentence.text,
            &cited_texts,
            &cited_numbers,
            limits,
            &mut out,
        );
    }

    let mut omitted: BTreeSet<&str> = BTreeSet::new();
    for id in &narration.omitted {
        if !known.contains_key(id.as_str()) {
            out.push(Violation::UnknownOmitted { id: id.clone() });
        }
        omitted.insert(id.as_str());
    }

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for fact in facts {
        if !seen.insert(fact.id.as_str()) {
            continue;
        }
        let is_cited = cited.contains(fact.id.as_str());
        let is_omitted = omitted.contains(fact.id.as_str());
        if is_cited && is_omitted {
            out.push(Violation::CitedAndOmitted {
                id: fact.id.clone(),
            });
        } else if is_irreversible(fact, limits) {
            if !is_cited {
                out.push(Violation::IrreversibleNotCited {
                    id: fact.id.clone(),
                });
            }
        } else if !is_cited && !is_omitted {
            out.push(Violation::UncitedFact {
                id: fact.id.clone(),
            });
        }
    }

    if out.items.is_empty() {
        Ok(Verified {
            narration: narration.clone(),
        })
    } else {
        Err(out.items)
    }
}

fn is_irreversible(fact: &Fact, limits: &Limits) -> bool {
    if limits.irreversible_kinds.contains(&fact.kind) {
        return true;
    }
    match &limits.irreversible_field {
        Some((name, value)) => fact
            .fields
            .get(name)
            .is_some_and(|scalar| scalar.display() == *value),
        None => false,
    }
}

/// Runs the text rules for one sentence. `texts` and `numbers` come only from
/// the facts that sentence cites.
fn check_sentence(
    index: usize,
    text: &str,
    texts: &[String],
    numbers: &BTreeSet<String>,
    limits: &Limits,
    out: &mut Collector,
) {
    let chars: Vec<char> = text.chars().collect();
    if text.trim().is_empty() {
        out.push(Violation::EmptySentence { sentence: index });
    }
    if chars.len() > limits.max_sentence_chars {
        out.push(Violation::SentenceTooLong {
            sentence: index,
            chars: chars.len(),
            max: limits.max_sentence_chars,
        });
    }
    let controls: BTreeSet<u32> = chars
        .iter()
        .filter(|c| c.is_control() && **c != ' ')
        .map(|c| *c as u32)
        .collect();
    for code in controls {
        out.push(Violation::ControlCharacter {
            sentence: index,
            code,
        });
    }

    for number in scan_numbers(&chars) {
        if !numbers.contains(&number.value) {
            out.push(Violation::InventedNumber {
                sentence: index,
                token: number.spelling,
            });
        }
    }

    let spans = quoted_spans(&chars);
    for &(start, end) in &spans {
        let token: String = chars.get(start + 1..end).unwrap_or(&[]).iter().collect();
        if !token.is_empty() && !texts.iter().any(|field| field.contains(token.as_str())) {
            out.push(Violation::InventedQuote {
                sentence: index,
                token,
            });
        }
    }

    // Words outside quotes. A quoted span becomes one lowercase placeholder so
    // that word positions still mean "first word of the sentence".
    let masked = mask_quotes(&chars, &spans);
    let masked_text: String = masked.iter().collect();
    let mut seen_words = 0usize;
    for raw in masked_text.split_whitespace() {
        let token = trim_token(raw);
        if token.is_empty() {
            continue;
        }
        let first_word = seen_words == 0;
        seen_words += 1;
        let occurs = texts.iter().any(|field| field.contains(token));
        if token.contains('/') || token.contains('\\') || token.starts_with('~') {
            if !occurs {
                out.push(Violation::InventedPath {
                    sentence: index,
                    token: token.to_owned(),
                });
            }
        } else if !first_word && token.chars().next().is_some_and(char::is_uppercase) && !occurs {
            out.push(Violation::InventedName {
                sentence: index,
                token: token.to_owned(),
            });
        }
    }
}

/// Strips punctuation from both ends of a word but keeps path characters.
fn trim_token(word: &str) -> &str {
    word.trim_matches(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '\\' | '~' | '_')))
}

/// Replaces each quoted span with a lowercase placeholder word.
fn mask_quotes(chars: &[char], spans: &[(usize, usize)]) -> Vec<char> {
    let mut masked = Vec::with_capacity(chars.len());
    let mut next = 0usize;
    for &(start, end) in spans {
        masked.extend(chars.get(next..start).unwrap_or(&[]));
        masked.extend([' ', 'q', ' ']);
        next = end + 1;
    }
    masked.extend(chars.get(next..).unwrap_or(&[]));
    masked
}

/// The closing mark for an opening quote, if `open` can open a quote.
fn closer_for(open: char) -> Option<char> {
    match open {
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        '“' => Some('”'),
        '‘' => Some('’'),
        _ => None,
    }
}

/// Finds quoted spans as inclusive (open, close) char indices.
///
/// A straight quote opens only at the start of a word, so an apostrophe inside
/// a word does not start a quote. A closing mark must be followed by a
/// non-alphanumeric char or the end of the text. Unclosed openers are ignored.
fn quoted_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    // For each closing mark: an opener at or after this index has no closer.
    let mut dead_from: BTreeMap<char, usize> = BTreeMap::new();
    let mut i = 0usize;
    while i < chars.len() {
        let Some(close) = closer_for(chars[i]) else {
            i += 1;
            continue;
        };
        let typographic_open = chars[i] == '“';
        let at_word_start = i == 0 || chars[i - 1].is_whitespace() || "([{<".contains(chars[i - 1]);
        let no_closer = dead_from.get(&close).is_some_and(|&from| i >= from);
        if (!typographic_open && !at_word_start) || no_closer {
            i += 1;
            continue;
        }
        let end = (i + 1..chars.len()).find(|&j| {
            chars[j] == close && (j + 1 == chars.len() || !chars[j + 1].is_alphanumeric())
        });
        match end {
            Some(j) => {
                spans.push((i, j));
                i = j + 1;
            }
            None => {
                dead_from.insert(close, i);
                i += 1;
            }
        }
    }
    spans
}

/// A number as written in text, with its canonical value for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Number {
    spelling: String,
    value: String,
}

/// Canonical decimal text: no leading zeros, no trailing fraction zeros, no
/// sign on zero. Two numbers are equal exactly when their canonical texts are.
fn canonical(negative: bool, integer: &str, fraction: &str) -> String {
    let integer = integer.trim_start_matches('0');
    let integer = if integer.is_empty() { "0" } else { integer };
    let fraction = fraction.trim_end_matches('0');
    let negative = negative && (integer != "0" || !fraction.is_empty());
    let mut value = String::new();
    if negative {
        value.push('-');
    }
    value.push_str(integer);
    if !fraction.is_empty() {
        value.push('.');
        value.push_str(fraction);
    }
    value
}

/// Canonical form of plain decimal text such as "-3", "0.12" or "1200".
fn canonical_plain(text: &str) -> String {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (integer, fraction) = body.split_once('.').unwrap_or((body, ""));
    canonical(negative, integer, fraction)
}

/// The numbers a field contributes. Text fields contribute the numbers they
/// contain; a percent sign does not scale, so "12%" needs a field value of 12.
fn field_numbers(scalar: &Scalar) -> Vec<String> {
    match scalar {
        Scalar::Int(value) => vec![canonical_plain(&value.to_string())],
        Scalar::Float(value) if value.is_finite() => vec![canonical_plain(&value.to_string())],
        Scalar::Float(_) | Scalar::Bool(_) => Vec::new(),
        Scalar::Text(text) => {
            let chars: Vec<char> = text.chars().collect();
            scan_numbers(&chars)
                .into_iter()
                .map(|number| number.value)
                .collect()
        }
    }
}

/// Finds numbers in text: an optional sign (not glued to a preceding word), a
/// digit run with optional comma groups of three (the first group at most three
/// digits), and an optional fraction. Anything else splits into separate numbers.
fn scan_numbers(chars: &[char]) -> Vec<Number> {
    let n = chars.len();
    let mut found = Vec::new();
    let mut i = 0usize;
    while i < n {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let sign = if i >= 1
            && (chars[i - 1] == '-' || chars[i - 1] == '+')
            && (i < 2 || !chars[i - 2].is_alphanumeric())
        {
            Some(chars[i - 1])
        } else {
            None
        };
        let start = if sign.is_some() { i - 1 } else { i };
        let negative = sign == Some('-');

        let mut j = i;
        let mut integer = String::new();
        while j < n && chars[j].is_ascii_digit() {
            integer.push(chars[j]);
            j += 1;
        }
        let leading = integer.len();
        while leading <= 3
            && j + 4 <= n
            && chars[j] == ','
            && chars[j + 1..j + 4].iter().all(char::is_ascii_digit)
            && (j + 4 == n || !chars[j + 4].is_ascii_digit())
        {
            integer.extend(&chars[j + 1..j + 4]);
            j += 4;
        }
        let mut fraction = String::new();
        if j + 1 < n && chars[j] == '.' && chars[j + 1].is_ascii_digit() {
            j += 1;
            while j < n && chars[j].is_ascii_digit() {
                fraction.push(chars[j]);
                j += 1;
            }
        }
        found.push(Number {
            spelling: chars[start..j].iter().collect(),
            value: canonical(negative, &integer, &fraction),
        });
        i = j;
    }
    found
}

/// Deterministic fallback text built from the typed rows alone.
///
/// One line per fact, in input order, after a header that labels the output as
/// deterministic. The output never includes narration text, and control
/// characters in the rows are flattened so each fact stays on one line.
pub(crate) fn render_deterministic(facts: &[Fact]) -> String {
    let mut out = String::from("deterministic rendering of the typed rows (no model text)\n");
    for fact in facts {
        out.push_str(&one_line(&fact.id));
        out.push_str(" [");
        out.push_str(&one_line(&fact.kind));
        out.push(']');
        let fields: Vec<String> = fact
            .fields
            .iter()
            .map(|(name, value)| format!("{}={}", one_line(name), one_line(&value.display())))
            .collect();
        if !fields.is_empty() {
            out.push(' ');
            out.push_str(&fields.join(", "));
        }
        out.push('\n');
    }
    out
}

fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Scalar {
        Scalar::Text(value.to_owned())
    }

    fn fact(id: &str, kind: &str, fields: &[(&str, Scalar)]) -> Fact {
        Fact {
            id: id.to_owned(),
            kind: kind.to_owned(),
            fields: fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn sentence(value: &str, cites: &[&str]) -> Sentence {
        Sentence {
            text: value.to_owned(),
            cites: cites.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn narration(sentences: Vec<Sentence>, omitted: &[&str]) -> Narration {
        Narration {
            sentences,
            omitted: omitted.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    /// The single diff row most tests cite: path, a count of 3, a label.
    fn diff_row() -> Vec<Fact> {
        vec![fact(
            "d1",
            "diff.change",
            &[
                ("path", text("src/app.rs")),
                ("count", Scalar::Int(3)),
                ("label", text("Deploy")),
            ],
        )]
    }

    fn violations(facts: &[Fact], narration: &Narration) -> Vec<Violation> {
        violations_with(facts, narration, &Limits::default())
    }

    fn violations_with(facts: &[Fact], narration: &Narration, limits: &Limits) -> Vec<Violation> {
        match verify(facts, narration, limits) {
            Ok(_) => Vec::new(),
            Err(found) => found,
        }
    }

    #[test]
    fn faithful_narration_passes() {
        let facts = vec![
            diff_row()[0].clone(),
            fact(
                "d2",
                "diff.change",
                &[("path", text("src/lib.rs")), ("count", Scalar::Int(30))],
            ),
            fact(
                "r1",
                "refund",
                &[("amount", Scalar::Int(13)), ("label", text("Refund"))],
            ),
        ];
        let narr = narration(
            vec![
                sentence("Changed `Deploy` in src/app.rs, 3 hunks.", &["d1"]),
                sentence("Left src/lib.rs with 30 changes.", &["d2"]),
                sentence("Refund of 13 is pending.", &["r1"]),
            ],
            &[],
        );
        let limits = Limits {
            irreversible_kinds: BTreeSet::from(["refund".to_owned()]),
            ..Limits::default()
        };
        let verified = verify(&facts, &narr, &limits);
        assert_eq!(
            verified,
            Ok(Verified {
                narration: narr.clone()
            })
        );
    }

    #[test]
    fn unknown_citation_is_reported_with_sentence_and_id() {
        let narr = narration(vec![sentence("Changed src/app.rs.", &["d1", "d9"])], &[]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::UnknownCitation {
                sentence: 0,
                id: "d9".to_owned()
            }]
        );
    }

    #[test]
    fn uncited_fact_fails_unless_omitted() {
        let empty = narration(Vec::new(), &[]);
        assert_eq!(
            violations(&diff_row(), &empty),
            vec![Violation::UncitedFact {
                id: "d1".to_owned()
            }]
        );
        assert!(violations(&diff_row(), &narration(Vec::new(), &["d1"])).is_empty());
    }

    #[test]
    fn fact_both_cited_and_omitted_fails() {
        let narr = narration(vec![sentence("Changed src/app.rs.", &["d1"])], &["d1"]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::CitedAndOmitted {
                id: "d1".to_owned()
            }]
        );
    }

    #[test]
    fn omitted_unknown_id_fails() {
        let narr = narration(vec![sentence("Changed src/app.rs.", &["d1"])], &["d7"]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::UnknownOmitted {
                id: "d7".to_owned()
            }]
        );
    }

    #[test]
    fn omitted_irreversible_fact_fails() {
        let facts = vec![fact("r1", "refund", &[("amount", Scalar::Int(13))])];
        let limits = Limits {
            irreversible_kinds: BTreeSet::from(["refund".to_owned()]),
            ..Limits::default()
        };
        assert_eq!(
            violations_with(&facts, &narration(Vec::new(), &["r1"]), &limits),
            vec![Violation::IrreversibleNotCited {
                id: "r1".to_owned()
            }]
        );
    }

    #[test]
    fn irreversible_marker_field_is_honoured() {
        let facts = vec![fact(
            "s1",
            "step",
            &[("irreversible", text("yes")), ("name", text("Send"))],
        )];
        let limits = Limits {
            irreversible_field: Some(("irreversible".to_owned(), "yes".to_owned())),
            ..Limits::default()
        };
        assert_eq!(
            violations_with(&facts, &narration(Vec::new(), &["s1"]), &limits),
            vec![Violation::IrreversibleNotCited {
                id: "s1".to_owned()
            }]
        );
    }

    #[test]
    fn invented_number_fails() {
        let narr = narration(
            vec![sentence("Changed src/app.rs, 30 hunks.", &["d1"])],
            &[],
        );
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::InventedNumber {
                sentence: 0,
                token: "30".to_owned()
            }]
        );
    }

    #[test]
    fn number_that_is_a_substring_of_a_real_number_fails() {
        let facts = vec![fact("d1", "diff.change", &[("count", Scalar::Int(13))])];
        let narr = narration(vec![sentence("Changed 3 files.", &["d1"])], &[]);
        assert_eq!(
            violations(&facts, &narr),
            vec![Violation::InventedNumber {
                sentence: 0,
                token: "3".to_owned()
            }]
        );
    }

    #[test]
    fn numbers_match_by_value_across_spellings() {
        let facts = vec![
            fact("d1", "diff.change", &[("count", Scalar::Int(3))]),
            fact("d2", "diff.change", &[("total", Scalar::Int(1200))]),
        ];
        let narr = narration(
            vec![sentence(
                "Changed 3.0 files, 03 times, 1,200 lines.",
                &["d1", "d2"],
            )],
            &[],
        );
        assert!(violations(&facts, &narr).is_empty());
    }

    #[test]
    fn percent_does_not_scale() {
        let facts = vec![fact("d1", "diff.change", &[("share", Scalar::Float(0.12))])];
        let narr = narration(vec![sentence("Share is 12%.", &["d1"])], &[]);
        assert_eq!(
            violations(&facts, &narr),
            vec![Violation::InventedNumber {
                sentence: 0,
                token: "12".to_owned()
            }]
        );
    }

    #[test]
    fn invented_quoted_token_fails() {
        let narr = narration(
            vec![sentence("Changed \"Release\" in src/app.rs.", &["d1"])],
            &[],
        );
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::InventedQuote {
                sentence: 0,
                token: "Release".to_owned()
            }]
        );
    }

    #[test]
    fn typographic_and_backtick_quotes_of_real_text_pass() {
        let narr = narration(
            vec![sentence(
                "Changed \u{201C}Deploy\u{201D} and `Deploy` in src/app.rs.",
                &["d1"],
            )],
            &[],
        );
        assert!(violations(&diff_row(), &narr).is_empty());
    }

    #[test]
    fn invented_path_fails_for_slash_backslash_and_tilde() {
        let narr = narration(
            vec![
                sentence("Changed lib/other.rs.", &["d1"]),
                sentence("Changed lib\\other.txt.", &["d1"]),
                sentence("Read ~/notes.", &["d1"]),
            ],
            &[],
        );
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![
                Violation::InventedPath {
                    sentence: 0,
                    token: "lib/other.rs".to_owned()
                },
                Violation::InventedPath {
                    sentence: 1,
                    token: "lib\\other.txt".to_owned()
                },
                Violation::InventedPath {
                    sentence: 2,
                    token: "~/notes".to_owned()
                },
            ]
        );
    }

    #[test]
    fn invented_capitalised_name_fails_but_sentence_start_is_exempt() {
        let narr = narration(vec![sentence("Changed Release now.", &["d1"])], &[]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::InventedName {
                sentence: 0,
                token: "Release".to_owned()
            }]
        );
    }

    #[test]
    fn too_many_sentences_fails() {
        let narr = narration(
            vec![
                sentence("Changed src/app.rs.", &["d1"]),
                sentence("Changed src/app.rs.", &["d1"]),
            ],
            &[],
        );
        let limits = Limits {
            max_sentences: 1,
            ..Limits::default()
        };
        assert_eq!(
            violations_with(&diff_row(), &narr, &limits),
            vec![Violation::TooManySentences { count: 2, max: 1 }]
        );
    }

    #[test]
    fn overlong_sentence_fails() {
        let narr = narration(vec![sentence("Changed src/app.rs.", &["d1"])], &[]);
        let limits = Limits {
            max_sentence_chars: 10,
            ..Limits::default()
        };
        assert_eq!(
            violations_with(&diff_row(), &narr, &limits),
            vec![Violation::SentenceTooLong {
                sentence: 0,
                chars: 19,
                max: 10
            }]
        );
    }

    #[test]
    fn control_character_fails() {
        let narr = narration(vec![sentence("Changed\tsrc/app.rs.", &["d1"])], &[]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::ControlCharacter {
                sentence: 0,
                code: 9
            }]
        );
    }

    #[test]
    fn empty_sentence_fails() {
        let narr = narration(vec![sentence("   ", &["d1"])], &[]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::EmptySentence { sentence: 0 }]
        );
    }

    #[test]
    fn duplicate_fact_id_fails() {
        let mut facts = diff_row();
        facts.push(diff_row()[0].clone());
        let narr = narration(vec![sentence("Changed src/app.rs.", &["d1"])], &[]);
        assert_eq!(
            violations(&facts, &narr),
            vec![Violation::DuplicateFactId {
                id: "d1".to_owned()
            }]
        );
    }

    #[test]
    fn violations_are_bounded_with_a_truncation_marker() {
        let narr = narration(vec![sentence("Changed 1 2 3 4 5.", &["d1"])], &[]);
        let limits = Limits {
            max_violations: 2,
            ..Limits::default()
        };
        let found = violations_with(&diff_row(), &narr, &limits);
        assert_eq!(found.len(), 3);
        assert_eq!(found.last(), Some(&Violation::Truncated { limit: 2 }));
    }

    #[test]
    fn empty_inputs_do_not_panic() {
        assert!(verify(&[], &narration(Vec::new(), &[]), &Limits::default()).is_ok());
        let narr = narration(vec![sentence("", &[])], &[]);
        assert_eq!(
            violations(&[], &narr),
            vec![Violation::EmptySentence { sentence: 0 }]
        );
    }

    #[test]
    fn very_long_token_and_multibyte_text_do_not_panic() {
        let long = "X".repeat(50_000);
        let narr = narration(
            vec![sentence(
                &format!("Changed {long} and \u{00FC}ber \u{2014} ok."),
                &["d1"],
            )],
            &[],
        );
        let limits = Limits {
            max_sentence_chars: 100_000,
            ..Limits::default()
        };
        let found = violations_with(&diff_row(), &narr, &limits);
        assert_eq!(
            found,
            vec![Violation::InventedName {
                sentence: 0,
                token: long
            }]
        );
    }

    #[test]
    fn unicode_names_are_reported_whole() {
        let narr = narration(vec![sentence("Changed \u{00C9}cole here.", &["d1"])], &[]);
        assert_eq!(
            violations(&diff_row(), &narr),
            vec![Violation::InventedName {
                sentence: 0,
                token: "\u{00C9}cole".to_owned()
            }]
        );
    }

    #[test]
    fn number_scanner_compares_canonical_values() {
        let spellings = |text: &str| -> Vec<(String, String)> {
            let chars: Vec<char> = text.chars().collect();
            scan_numbers(&chars)
                .into_iter()
                .map(|n| (n.spelling, n.value))
                .collect()
        };
        assert_eq!(
            spellings("1,200"),
            vec![("1,200".to_owned(), "1200".to_owned())]
        );
        assert_eq!(
            spellings("1,20"),
            vec![
                ("1".to_owned(), "1".to_owned()),
                ("20".to_owned(), "20".to_owned())
            ]
        );
        assert_eq!(spellings("-3"), vec![("-3".to_owned(), "-3".to_owned())]);
        assert_eq!(
            spellings("2024-03"),
            vec![
                ("2024".to_owned(), "2024".to_owned()),
                ("03".to_owned(), "3".to_owned())
            ]
        );
        assert_eq!(
            spellings("3.50"),
            vec![("3.50".to_owned(), "3.5".to_owned())]
        );
        assert_eq!(spellings("-0.0"), vec![("-0.0".to_owned(), "0".to_owned())]);
    }

    #[test]
    fn deterministic_render_is_stable_and_names_every_fact() {
        let facts = vec![
            diff_row()[0].clone(),
            fact(
                "r1",
                "refund",
                &[("amount", Scalar::Int(13)), ("note", text("two\nlines"))],
            ),
        ];
        let first = render_deterministic(&facts);
        assert_eq!(first, render_deterministic(&facts));
        assert!(first.starts_with("deterministic"));
        assert!(first.contains("d1 [diff.change]"));
        assert!(first.contains("r1 [refund]"));
        assert_eq!(first.lines().count(), facts.len() + 1);
        assert!(first.lines().all(|line| !line.contains('\n')));
    }

    #[test]
    fn deterministic_render_of_no_facts_is_just_the_label() {
        let rendered = render_deterministic(&[]);
        assert_eq!(rendered.lines().count(), 1);
        assert!(rendered.starts_with("deterministic"));
    }
}
