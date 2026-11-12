//! Three-way merge decisions over line hunks (issue #385).
//!
//! Each side is aligned to the base with an LCS diff, and differences that overlap or touch
//! form one hunk. A hunk changed on one side takes that side. A hunk changed identically on
//! both sides keeps the user's lines. Any other hunk changed on both sides is `Human`, and is
//! written between the conflict markers below. Composed lines are copies of base, user or
//! default lines, or one of those three markers.

use std::ops::Range;

/// Opening marker of a `Human` hunk.
pub const MARKER_USER: &str = "<<<<<<< user";
/// Separator between the user lines and the default lines of a `Human` hunk.
pub const MARKER_SPLIT: &str = "=======";
/// Closing marker of a `Human` hunk.
pub const MARKER_DEFAULT: &str = ">>>>>>> default";

/// What happens to one hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Keep the user's lines.
    KeepUser,
    /// Take the new default's lines.
    TakeDefault,
    /// Leave both sides between conflict markers for a person.
    Human,
}

/// One hunk, with the line ranges it covers in the base, the user file and the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub base: Range<usize>,
    pub user: Range<usize>,
    pub default: Range<usize>,
    pub decision: Decision,
}

/// The merge result: one decision per hunk (its index is the hunk id) and the composed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    pub hunks: Vec<Hunk>,
    pub lines: Vec<String>,
}

impl Merge {
    /// Number of hunks left for a person.
    pub fn human_count(&self) -> usize {
        self.hunks
            .iter()
            .filter(|hunk| hunk.decision == Decision::Human)
            .count()
    }
}

/// A difference between the base and one side: `base` lines were replaced by `side` lines.
struct Change {
    base: Range<usize>,
    side: Range<usize>,
}

/// Merges `user` and `default` against their common `base`, one line per element.
pub fn merge(base: &[&str], user: &[&str], default: &[&str]) -> Merge {
    let user_changes = changes(base, user);
    let default_changes = changes(base, default);

    let mut tagged: Vec<(bool, &Change)> = user_changes
        .iter()
        .map(|change| (true, change))
        .chain(default_changes.iter().map(|change| (false, change)))
        .collect();
    tagged.sort_by_key(|(_, change)| (change.base.start, change.base.end));

    // Changes that overlap or touch on the base form one hunk.
    let mut groups: Vec<(Range<usize>, bool, bool)> = Vec::new();
    for (from_user, change) in tagged {
        match groups.last_mut() {
            Some((range, user_changed, default_changed)) if change.base.start <= range.end => {
                range.end = range.end.max(change.base.end);
                if from_user {
                    *user_changed = true;
                } else {
                    *default_changed = true;
                }
            }
            _ => groups.push((change.base.clone(), from_user, !from_user)),
        }
    }

    let mut hunks = Vec::with_capacity(groups.len());
    let mut lines = Vec::new();
    let mut cursor = 0;
    for (range, user_changed, default_changed) in groups {
        let user_span =
            side_at(&user_changes, range.start, false)..side_at(&user_changes, range.end, true);
        let default_span = side_at(&default_changes, range.start, false)
            ..side_at(&default_changes, range.end, true);
        let user_lines = span(user, user_span.clone());
        let default_lines = span(default, default_span.clone());

        let decision = if user_changed && default_changed {
            if user_lines == default_lines {
                Decision::KeepUser
            } else {
                Decision::Human
            }
        } else if default_changed {
            Decision::TakeDefault
        } else {
            Decision::KeepUser
        };

        push_all(&mut lines, span(base, cursor..range.start));
        match decision {
            Decision::KeepUser => push_all(&mut lines, user_lines),
            Decision::TakeDefault => push_all(&mut lines, default_lines),
            Decision::Human => {
                lines.push(MARKER_USER.to_string());
                push_all(&mut lines, user_lines);
                lines.push(MARKER_SPLIT.to_string());
                push_all(&mut lines, default_lines);
                lines.push(MARKER_DEFAULT.to_string());
            }
        }
        cursor = range.end;
        hunks.push(Hunk {
            base: range,
            user: user_span,
            default: default_span,
            decision,
        });
    }
    push_all(&mut lines, span(base, cursor..base.len()));

    Merge { hunks, lines }
}

/// True when every composed line is a copy of a user or default line, or a conflict marker.
pub fn is_composed_from(lines: &[String], user: &[&str], default: &[&str]) -> bool {
    lines.iter().all(|line| {
        let line = line.as_str();
        [MARKER_USER, MARKER_SPLIT, MARKER_DEFAULT].contains(&line)
            || user.contains(&line)
            || default.contains(&line)
    })
}

/// Differences between `base` and `side`, in base order.
fn changes(base: &[&str], side: &[&str]) -> Vec<Change> {
    let mut matches = lcs_pairs(base, side);
    matches.push((base.len(), side.len()));

    let mut out = Vec::new();
    let (mut base_next, mut side_next) = (0, 0);
    for (base_at, side_at) in matches {
        if base_next < base_at || side_next < side_at {
            out.push(Change {
                base: base_next..base_at,
                side: side_next..side_at,
            });
        }
        base_next = base_at + 1;
        side_next = side_at + 1;
    }
    out
}

/// Maps a base line boundary to the matching boundary in one side.
///
/// A start boundary sits before any insertion at that point, and an end boundary after all
/// changes that end there.
fn side_at(changes: &[Change], point: usize, end: bool) -> usize {
    let shift: isize = changes
        .iter()
        .filter(|change| {
            change.base.end < point
                || (change.base.end == point && (end || change.base.start < point))
        })
        .map(|change| change.side.len() as isize - change.base.len() as isize)
        .sum();
    usize::try_from(point as isize + shift).unwrap_or(0)
}

/// Longest common subsequence of two line lists, as matched index pairs.
fn lcs_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    // table[i][j] is the LCS length of a[i..] and b[j..].
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// The lines of `lines` in `range`, or nothing when the range is out of bounds.
fn span<'b, 'a>(lines: &'b [&'a str], range: Range<usize>) -> &'b [&'a str] {
    lines.get(range).unwrap_or(&[])
}

fn push_all(out: &mut Vec<String>, lines: &[&str]) {
    out.extend(lines.iter().map(|line| (*line).to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touching_changes_form_one_hunk() {
        // The user replaces line b, and the default inserts after it: one hunk, a conflict.
        let merged = merge(&["a", "b", "c"], &["a", "X", "c"], &["a", "b", "Y", "c"]);
        assert_eq!(merged.hunks.len(), 1);
        assert_eq!(merged.hunks[0].base, 1..2);
        assert_eq!(merged.hunks[0].decision, Decision::Human);
        assert_eq!(
            merged.lines,
            [
                "a",
                MARKER_USER,
                "X",
                MARKER_SPLIT,
                "b",
                "Y",
                MARKER_DEFAULT,
                "c"
            ]
        );
    }
}
