//! Near-miss suggestions for unknown names: applet and parameter names, and the
//! statement keywords the parser dispatches on.
//!
//! Matching uses the optimal-string-alignment distance, so one transposed pair
//! such as `rnu` for `run` costs one edit. A suggestion is offered only when it is
//! close relative to the length of the misspelled word, so unrelated names do not
//! produce a fix.

/// Words that begin a statement or clause the parser recognises. Used only to
/// suggest corrections for unknown first words, never to accept a new program.
pub const STATEMENT_KEYWORDS: &[&str] = &[
    "accept",
    "append",
    "await",
    "break",
    "cancel",
    "catch",
    "continue",
    "define",
    "else",
    "every",
    "exit",
    "fail",
    "finally",
    "for",
    "if",
    "listen",
    "loop",
    "on",
    "otherwise",
    "parallel",
    "pipe",
    "race",
    "read",
    "repeat",
    "return",
    "run",
    "spawn",
    "take",
    "then",
    "try",
    "use",
    "whenever",
    "while",
];

/// Optimal-string-alignment distance: insertions, deletions, substitutions and
/// transpositions of adjacent characters each cost one.
pub fn distance(left: &str, right: &str) -> usize {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in table.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in table[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (table[i - 1][j] + 1)
                .min(table[i][j - 1] + 1)
                .min(table[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(table[i - 2][j - 2] + 1);
            }
            table[i][j] = best;
        }
    }
    table[a.len()][b.len()]
}

/// The candidate closest to `name`, when it is within a third of the name's length
/// (at least one edit). Ties resolve to the lexically first candidate.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .filter(|candidate| *candidate != name)
        .map(|candidate| (distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(right.1)))
        .map(|(_, candidate)| candidate)
}

/// A statement keyword that a misspelled first word probably meant. Words shorter
/// than three characters are never corrected, since they match too many keywords.
pub fn keyword_for(word: &str) -> Option<&'static str> {
    if word.chars().count() < 3 {
        return None;
    }
    closest(word, STATEMENT_KEYWORDS.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_transpositions_as_one_edit() {
        assert_eq!(distance("rnu", "run"), 1);
        assert_eq!(distance("argumens", "arguments"), 1);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("kitten", "sitting"), 3);
    }

    #[test]
    fn closest_respects_the_length_relative_limit() {
        assert_eq!(
            closest("argumens", ["arguments", "text"]),
            Some("arguments")
        );
        assert_eq!(closest("echo", ["text", "path"]), None);
        assert_eq!(
            closest("run", ["run"]),
            None,
            "exact matches are not suggestions"
        );
    }

    #[test]
    fn keyword_suggestions_skip_short_and_unrelated_words() {
        assert_eq!(keyword_for("rnu"), Some("run"));
        assert_eq!(keyword_for("thne"), Some("then"));
        assert_eq!(keyword_for("echo"), None);
        assert_eq!(keyword_for("on"), None, "short words are never corrected");
    }
}
