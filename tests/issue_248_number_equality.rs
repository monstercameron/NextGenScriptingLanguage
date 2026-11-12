use flow::applets::execute;
use serde_json::{json, Map, Value};

fn filter(document: &str, equals: Value) -> Value {
    let document: Value = serde_json::from_str(document).expect("test document is JSON");
    let mut args = Map::new();
    args.insert("document".to_owned(), document);
    args.insert(
        "where".to_owned(),
        json!([{"pointer": "/v", "equals": equals}]),
    );
    execute("json.filter", &args, &[]).expect("json.filter accepts numeric predicates")
}

#[test]
fn json_filter_equals_compares_numbers_by_value() {
    let document = r#"[{"v":1},{"v":1.0},{"v":1e0},{"v":2}]"#;
    let matched = filter(document, json!(1));
    assert_eq!(
        matched,
        json!([{"v": 1}, {"v": 1.0}, {"v": 1e0}]),
        "1, 1.0 and 1e0 are equal by value; 2 is not"
    );
}

#[test]
fn json_filter_equals_keeps_integers_beyond_2_pow_53_exact() {
    let document = r#"[{"v":9007199254740992},{"v":9007199254740993}]"#;
    assert_eq!(
        filter(document, json!(9007199254740993_u64)),
        json!([{"v": 9007199254740993_u64}])
    );
    assert_eq!(
        filter(document, json!(9007199254740992.0)),
        json!([{"v": 9007199254740992_u64}]),
        "a float equal to 2^53 must not match the integer 2^53 + 1"
    );
}
