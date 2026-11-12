//! Issue #379: tree routing over a large closed label set. The classifier is a
//! deterministic in-process fake, so these tests make no model call.
#![cfg(feature = "model")]

use flow::models::{route_label_tree, LevelAnswer, RouteStop, LABEL_TREE_MAX_DEPTH};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const LEVEL_PREFIX: [&str; 3] = ["a", "b", "c"];

/// 5 groups x 8 subgroups x 10 leaves = 400 leaves.
fn synthetic_tree() -> Value {
    let groups: Vec<Value> = (0..5)
        .map(|a| {
            let subs: Vec<Value> = (0..8)
                .map(|b| {
                    let leaves: Vec<Value> = (0..10)
                        .map(|c| json!({"id": format!("c{c}"), "description": format!("leaf {a}.{b}.{c}")}))
                        .collect();
                    json!({"id": format!("b{b}"), "description": format!("sub {a}.{b}"), "children": leaves})
                })
                .collect();
            json!({"id": format!("a{a}"), "description": format!("group {a}"), "children": subs})
        })
        .collect();
    json!({ "children": groups })
}

fn count_leaves(node: &Value) -> usize {
    match node.get("children").and_then(Value::as_array) {
        Some(children) if !children.is_empty() => children.iter().map(count_leaves).sum(),
        _ => 1,
    }
}

/// A chain of `levels` nested labels, the deepest one a leaf.
fn chain(levels: usize) -> Value {
    let mut node = json!({"id": format!("n{levels}"), "description": "deepest"});
    for level in (1..levels).rev() {
        node = json!({"id": format!("n{level}"), "description": "level", "children": [node]});
    }
    json!({ "children": [node] })
}

/// Deterministic fake classifier. It answers the target leaf's label at each
/// level, or `none` when `target` is None. Every call is recorded by level.
fn fake(
    seen: &mut Vec<usize>,
    target: Option<[usize; 3]>,
    confidence: u8,
) -> impl FnMut(usize, &[String], &BTreeMap<String, String>) -> anyhow::Result<LevelAnswer> + '_ {
    move |level, labels, criteria| {
        seen.push(level);
        assert_eq!(
            labels.last().map(String::as_str),
            Some("none"),
            "every level offers none"
        );
        assert_eq!(
            criteria.len(),
            labels.len(),
            "every label has a description"
        );
        match target {
            Some(path) => Ok(LevelAnswer {
                label: format!("{}{}", LEVEL_PREFIX[level - 1], path[level - 1]),
                confidence,
            }),
            None => Ok(LevelAnswer {
                label: "none".to_owned(),
                confidence: 100,
            }),
        }
    }
}

#[test]
fn synthetic_tree_has_400_leaves_and_routes_twenty_phrases() {
    let tree = synthetic_tree();
    assert_eq!(count_leaves(&tree), 400);
    for i in 0..20 {
        let target = [(i * 7) % 5, (i * 3) % 8, (i * 11) % 10];
        let mut seen = Vec::new();
        let outcome = route_label_tree(&tree, &[60], fake(&mut seen, Some(target), 90))
            .unwrap_or_else(|error| panic!("phrase {i} failed: {error}"));
        assert!(outcome.accepted, "phrase {i} should be accepted");
        assert_eq!(
            outcome.path,
            vec![
                format!("a{}", target[0]),
                format!("b{}", target[1]),
                format!("c{}", target[2]),
            ],
            "phrase {i} routed to the wrong leaf"
        );
        assert_eq!(
            outcome.leaf.as_deref(),
            Some(format!("c{}", target[2]).as_str())
        );
        assert_eq!(outcome.failed_level, None);
        assert_eq!(outcome.stop, None);
        assert_eq!(outcome.confidences, vec![90, 90, 90]);
        assert_eq!(seen, vec![1, 2, 3], "one classify call per level");
        let leaf_confidence = outcome
            .leaf_confidence
            .expect("accepted routes report a confidence");
        assert!((leaf_confidence - 0.9f64.powi(3)).abs() < 1e-9);
    }
}

#[test]
fn out_of_tree_phrase_returns_none_at_level_one() {
    let tree = synthetic_tree();
    let mut seen = Vec::new();
    let outcome = route_label_tree(&tree, &[60], fake(&mut seen, None, 100)).expect("routing");
    assert!(!outcome.accepted);
    assert_eq!(outcome.failed_level, Some(1));
    assert_eq!(outcome.stop, Some(RouteStop::None));
    assert!(outcome.path.is_empty());
    assert_eq!(outcome.leaf, None);
    assert_eq!(outcome.leaf_confidence, None);
    assert_eq!(seen, vec![1], "routing stops at the first none");
}

#[test]
fn low_confidence_level_two_returns_partial_path() {
    let tree = synthetic_tree();
    let mut seen = Vec::new();
    // Level 1 clears its minimum of 30; level 2 answers 40 against a minimum of 60.
    let outcome = route_label_tree(&tree, &[30, 60, 60], fake(&mut seen, Some([1, 2, 3]), 40))
        .expect("routing");
    assert!(!outcome.accepted);
    assert_eq!(outcome.failed_level, Some(2));
    assert_eq!(outcome.stop, Some(RouteStop::BelowThreshold));
    assert_eq!(outcome.path, vec!["a1".to_owned()]);
    assert_eq!(outcome.confidences, vec![40, 40]);
    assert_eq!(seen, vec![1, 2]);
}

#[test]
fn none_answer_at_level_two_returns_partial_path() {
    let tree = synthetic_tree();
    let mut seen = Vec::new();
    let outcome = route_label_tree(&tree, &[50], |level, _labels, _criteria| {
        seen.push(level);
        Ok(LevelAnswer {
            label: if level == 1 { "a2" } else { "none" }.to_owned(),
            confidence: 95,
        })
    })
    .expect("routing");
    assert_eq!(outcome.failed_level, Some(2));
    assert_eq!(outcome.stop, Some(RouteStop::None));
    assert_eq!(outcome.path, vec!["a2".to_owned()]);
}

#[test]
fn per_level_minimums_must_cover_the_walk() {
    let tree = synthetic_tree();
    let mut seen = Vec::new();
    let error = route_label_tree(&tree, &[10, 10], fake(&mut seen, Some([0, 0, 0]), 90))
        .expect_err("two minimums cannot cover three levels");
    assert!(error.to_string().contains("3 levels"), "{error}");
    assert!(
        seen.is_empty(),
        "no classify call before the minimums are checked"
    );
}

#[test]
fn classifier_answer_outside_the_closed_set_is_an_error() {
    let tree = synthetic_tree();
    let error = route_label_tree(&tree, &[50], |_, _, _| {
        Ok(LevelAnswer {
            label: "not-a-label".to_owned(),
            confidence: 90,
        })
    })
    .expect_err("an answer outside the set must fail");
    assert!(error.to_string().contains("closed label set"), "{error}");
}

#[test]
fn depth_limit_is_six_levels() {
    assert_eq!(LABEL_TREE_MAX_DEPTH, 6);
    let mut walked = 0usize;
    let outcome = route_label_tree(&chain(6), &[50], |level, labels, _| {
        walked = level;
        Ok(LevelAnswer {
            label: labels[0].clone(),
            confidence: 90,
        })
    })
    .expect("a six-level tree is within the limit");
    assert!(outcome.accepted);
    assert_eq!(outcome.path.len(), 6);
    assert_eq!(walked, 6);

    let mut calls = 0usize;
    let error = route_label_tree(&chain(7), &[50], |_, _, _| {
        calls += 1;
        Ok(LevelAnswer {
            label: "none".to_owned(),
            confidence: 100,
        })
    })
    .expect_err("a seven-level tree must be refused");
    assert!(
        error.to_string().contains("deeper than 6 levels"),
        "{error}"
    );
    assert_eq!(calls, 0, "the depth is checked before any classify call");
}

fn wide_tree(width: usize, level: usize) -> Value {
    let labels: Vec<Value> = (0..width)
        .map(|i| json!({"id": format!("w{i}"), "description": format!("item {i}")}))
        .collect();
    if level == 1 {
        json!({ "children": labels })
    } else {
        // Put the wide level under a single parent at level 1.
        json!({ "children": [{"id": "p", "description": "parent", "children": labels}] })
    }
}

#[test]
fn width_limit_is_128_children_per_level() {
    let mut seen = Vec::new();
    let at_limit = route_label_tree(&wide_tree(128, 1), &[50], fake(&mut seen, None, 100));
    assert!(at_limit.is_ok(), "128 children are allowed");

    let mut calls = 0usize;
    let over = route_label_tree(&wide_tree(129, 1), &[50], |_, _, _| {
        calls += 1;
        Ok(LevelAnswer {
            label: "none".to_owned(),
            confidence: 100,
        })
    })
    .expect_err("129 children must be refused");
    assert!(
        over.to_string().contains("level 1 has 129 children"),
        "{over}"
    );
    assert_eq!(calls, 0);

    let deeper = route_label_tree(&wide_tree(129, 2), &[50, 50], |_, _, _| {
        Ok(LevelAnswer {
            label: "none".to_owned(),
            confidence: 100,
        })
    })
    .expect_err("a wide second level must be refused");
    assert!(
        deeper.to_string().contains("level 2 has 129 children"),
        "{deeper}"
    );
}

#[test]
fn reserved_none_id_and_bad_confidence_are_refused() {
    let tree = json!({"children": [{"id": "none", "description": "reserved"}, {"id": "x", "description": "y"}]});
    let error = route_label_tree(&tree, &[50], |_, _, _| unreachable!("no call"))
        .expect_err("none is reserved");
    assert!(error.to_string().contains("reserved"), "{error}");

    let tree = synthetic_tree();
    let error = route_label_tree(&tree, &[101], |_, _, _| unreachable!("no call"))
        .expect_err("minimum above 100 is refused");
    assert!(error.to_string().contains("between 0 and 100"), "{error}");

    let error = route_label_tree(&tree, &[50], |_, _, _| {
        Ok(LevelAnswer {
            label: "a0".to_owned(),
            confidence: 101,
        })
    })
    .expect_err("confidence above 100 is refused");
    assert!(error.to_string().contains("confidence"), "{error}");
}
