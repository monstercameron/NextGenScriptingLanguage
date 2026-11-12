//! Issue #385: three-way hunk selection and composition over config-like line lists.

use flow::merge3::{is_composed_from, merge, Decision, MARKER_DEFAULT, MARKER_SPLIT, MARKER_USER};
use proptest::collection::vec;
use proptest::prelude::*;

const BASE: [&str; 3] = [
    "bind = SUPER, Q, killactive",
    "bind = SUPER, T, exec, terminal",
    "monitor = , preferred, auto, 1",
];

type Lines = Vec<&'static str>;
type Case = (Lines, Lines, Lines, Vec<Decision>, Lines);

/// Each case: base, user, default, expected decisions per hunk, expected composed lines.
fn fixtures() -> Vec<Case> {
    use Decision::{Human, KeepUser, TakeDefault};
    let b = BASE.to_vec();
    vec![
        // 1. The user adds a binding: keep it.
        (
            b.clone(),
            vec![BASE[0], BASE[1], "bind = SUPER, G, exec, git", BASE[2]],
            b.clone(),
            vec![KeepUser],
            vec![BASE[0], BASE[1], "bind = SUPER, G, exec, git", BASE[2]],
        ),
        // 2. The new default changes the monitor line and the user did not: take it.
        (
            b.clone(),
            b.clone(),
            vec![BASE[0], BASE[1], "monitor = , preferred, auto, 2"],
            vec![TakeDefault],
            vec![BASE[0], BASE[1], "monitor = , preferred, auto, 2"],
        ),
        // 3. Both sides make the same change: keep the user's copy.
        (
            b.clone(),
            vec![BASE[0], "bind = SUPER, T, exec, kitty", BASE[2]],
            vec![BASE[0], "bind = SUPER, T, exec, kitty", BASE[2]],
            vec![KeepUser],
            vec![BASE[0], "bind = SUPER, T, exec, kitty", BASE[2]],
        ),
        // 4. Both sides change the same line differently: a human decides.
        (
            b.clone(),
            vec![BASE[0], "bind = SUPER, T, exec, foot", BASE[2]],
            vec![BASE[0], "bind = SUPER, T, exec, alacritty", BASE[2]],
            vec![Human],
            vec![
                BASE[0],
                MARKER_USER,
                "bind = SUPER, T, exec, foot",
                MARKER_SPLIT,
                "bind = SUPER, T, exec, alacritty",
                MARKER_DEFAULT,
                BASE[2],
            ],
        ),
        // 5. Separate hunks: the user edits line one, the default edits line three.
        (
            b.clone(),
            vec!["bind = SUPER, Q, close", BASE[1], BASE[2]],
            vec![BASE[0], BASE[1], "monitor = , preferred, auto, 2"],
            vec![KeepUser, TakeDefault],
            vec![
                "bind = SUPER, Q, close",
                BASE[1],
                "monitor = , preferred, auto, 2",
            ],
        ),
        // 6. The default rewrites the whole file and the user did not touch it.
        (
            b.clone(),
            b.clone(),
            vec!["theme = dark", "gaps = 4"],
            vec![TakeDefault],
            vec!["theme = dark", "gaps = 4"],
        ),
        // 7. The user deletes a line: keep the deletion.
        (
            b.clone(),
            vec![BASE[0], BASE[2]],
            b.clone(),
            vec![KeepUser],
            vec![BASE[0], BASE[2]],
        ),
        // 8. The default appends a line: take the addition.
        (
            b.clone(),
            b.clone(),
            vec![BASE[0], BASE[1], BASE[2], "bind = SUPER, E, exec, editor"],
            vec![TakeDefault],
            vec![BASE[0], BASE[1], BASE[2], "bind = SUPER, E, exec, editor"],
        ),
        // 9. Nothing changed on either side: no hunks, output is the base.
        (b.clone(), b.clone(), b.clone(), vec![], b.clone()),
        // 10. Both sides insert different lines at the same place: a human decides.
        (
            b.clone(),
            vec![BASE[0], "bind = SUPER, G, exec, git", BASE[1], BASE[2]],
            vec![BASE[0], "bind = SUPER, H, exec, htop", BASE[1], BASE[2]],
            vec![Human],
            vec![
                BASE[0],
                MARKER_USER,
                "bind = SUPER, G, exec, git",
                MARKER_SPLIT,
                "bind = SUPER, H, exec, htop",
                MARKER_DEFAULT,
                BASE[1],
                BASE[2],
            ],
        ),
        // 11. No base at all, and the two sides create different content: a human decides.
        (
            vec![],
            vec!["a = 1"],
            vec!["a = 2"],
            vec![Human],
            vec![MARKER_USER, "a = 1", MARKER_SPLIT, "a = 2", MARKER_DEFAULT],
        ),
        // 12. One conflicting hunk and one user-only addition in the same file.
        (
            b.clone(),
            vec![
                "bind = SUPER, Q, close",
                BASE[1],
                BASE[2],
                "bind = SUPER, G, exec, git",
            ],
            vec!["bind = SUPER, Q, exit", BASE[1], BASE[2]],
            vec![Human, KeepUser],
            vec![
                MARKER_USER,
                "bind = SUPER, Q, close",
                MARKER_SPLIT,
                "bind = SUPER, Q, exit",
                MARKER_DEFAULT,
                BASE[1],
                BASE[2],
                "bind = SUPER, G, exec, git",
            ],
        ),
    ]
}

#[test]
fn twelve_fixture_pairs_select_expected_decisions() {
    for (index, (base, user, default, decisions, lines)) in fixtures().into_iter().enumerate() {
        let merged = merge(&base, &user, &default);
        let got: Vec<Decision> = merged.hunks.iter().map(|hunk| hunk.decision).collect();
        assert_eq!(got, decisions, "case {}: decisions", index + 1);
        assert_eq!(merged.lines, lines, "case {}: composed lines", index + 1);
        assert!(
            is_composed_from(&merged.lines, &user, &default),
            "case {}: a composed line is not from an input",
            index + 1
        );
    }
}

#[test]
fn hunk_ids_match_position_and_cover_user_and_default_spans() {
    let base = ["a", "b", "c", "d"];
    let user = ["a", "B", "c", "d"];
    let default = ["a", "b", "c", "D"];
    let merged = merge(&base, &user, &default);
    assert_eq!(merged.hunks.len(), 2);
    assert_eq!(merged.hunks[0].base, 1..2);
    assert_eq!(merged.hunks[0].user, 1..2);
    assert_eq!(merged.hunks[0].decision, Decision::KeepUser);
    assert_eq!(merged.hunks[1].base, 3..4);
    assert_eq!(merged.hunks[1].default, 3..4);
    assert_eq!(merged.hunks[1].decision, Decision::TakeDefault);
    assert_eq!(merged.lines, ["a", "B", "c", "D"]);
    assert_eq!(merged.human_count(), 0);
}

#[test]
fn unchanged_side_round_trips_odd_lines() {
    // The composer never writes text that is not in an input, even for odd lines.
    let base = ["", "x", "x", "  y  "];
    let user = ["", "x", "z", "  y  "];
    let merged = merge(&base, &user, &base);
    assert_eq!(merged.lines, user);
}

fn owned(values: &[u8]) -> Vec<String> {
    values
        .iter()
        .map(|value| format!("line {}", value % 5))
        .collect()
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

proptest! {
    #[test]
    fn every_output_line_appears_in_an_input(
        base in vec(0u8..40, 0..14),
        user in vec(0u8..40, 0..14),
        default in vec(0u8..40, 0..14),
    ) {
        let (base, user, default) = (owned(&base), owned(&user), owned(&default));
        let (base, user, default) = (refs(&base), refs(&user), refs(&default));
        let merged = merge(&base, &user, &default);
        prop_assert!(is_composed_from(&merged.lines, &user, &default));
        if merged.human_count() == 0 {
            prop_assert!(!merged.lines.iter().any(|line| line == MARKER_USER));
        }
    }

    #[test]
    fn an_unchanged_user_yields_the_default(
        base in vec(0u8..40, 0..14),
        default in vec(0u8..40, 0..14),
    ) {
        let (base, default) = (owned(&base), owned(&default));
        let merged = merge(&refs(&base), &refs(&base), &refs(&default));
        prop_assert_eq!(merged.lines, default);
    }

    #[test]
    fn an_unchanged_default_keeps_the_user(
        base in vec(0u8..40, 0..14),
        user in vec(0u8..40, 0..14),
    ) {
        let (base, user) = (owned(&base), owned(&user));
        let merged = merge(&refs(&base), &refs(&user), &refs(&base));
        prop_assert_eq!(merged.lines, user);
    }

    #[test]
    fn identical_sides_are_kept_as_the_user_wrote_them(
        base in vec(0u8..40, 0..14),
        side in vec(0u8..40, 0..14),
    ) {
        let (base, side) = (owned(&base), owned(&side));
        let merged = merge(&refs(&base), &refs(&side), &refs(&side));
        prop_assert_eq!(merged.human_count(), 0);
        prop_assert_eq!(merged.lines, side);
    }
}
