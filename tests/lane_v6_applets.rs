use flow::applets::{self, ExecutionBudgetExceeded, ExecutionContext};
use serde_json::{json, Value};
use std::path::Path;

fn run(name: &str, args: Value) -> Result<Value, applets::AppletError> {
    applets::execute(name, args.as_object().unwrap(), &[])
}

#[test]
fn aggregation_min_and_sort_keep_integer_precision() {
    assert_eq!(
        run(
            "list.min",
            json!({"items":[9007199254740993i64,9007199254740992i64]})
        )
        .unwrap(),
        json!(9007199254740992i64)
    );
    assert_eq!(
        run(
            "list.sort",
            json!({"items":[9007199254740993i64,9007199254740992i64]})
        )
        .unwrap(),
        json!([9007199254740992i64, 9007199254740993i64])
    );
}

#[test]
fn aggregation_sum_keeps_integer_precision() {
    assert_eq!(
        run("list.sum", json!({"items":[9007199254740993i64,2]})).unwrap(),
        json!(9007199254740995i64)
    );
}

#[test]
fn aggregation_unique_and_group_use_numeric_equality() {
    assert_eq!(
        run("list.unique", json!({"items":[1,1.0]})).unwrap(),
        json!([1])
    );
    assert_eq!(
        run("list.group", json!({"items":[1,1.0],"key":""})).unwrap(),
        json!([{"key":1,"count":2}])
    );
}

#[test]
fn mixed_integer_float_comparison_is_exact() {
    assert_eq!(
        run(
            "number.compare",
            json!({"left":9007199254740993i64,"right":9007199254740992.0})
        )
        .unwrap(),
        json!(1)
    );
}

#[test]
fn aggregation_integer_overflow_is_typed_error() {
    let error = run("list.sum", json!({"items":[u64::MAX,1]})).unwrap_err();
    assert_eq!(error.code, "overflow");
}

#[test]
fn oversized_stdin_is_refused_before_list_and_text_work() {
    let input = vec![b'x'; 16 * 1024 * 1024 + 1];
    assert!(applets::execute("list.count", &serde_json::Map::new(), &input).is_err());
    assert!(applets::execute("text.length", &serde_json::Map::new(), &input).is_err());
    let args = json!({"delimiter": ","});
    assert!(applets::execute("text.count_lines_by", args.as_object().unwrap(), &input).is_err());
}

#[test]
fn number_integer_and_rounding_applets_have_defined_edges() {
    assert_eq!(
        run("number.int_divide", json!({"left":-7,"right":3})).unwrap(),
        json!(-2)
    );
    assert_eq!(
        run("number.modulo", json!({"left":-7,"right":3})).unwrap(),
        json!(-1)
    );
    assert_eq!(
        run("number.divmod", json!({"left":7,"right":3})).unwrap(),
        json!({"quotient":2,"remainder":1})
    );
    assert_eq!(
        run("number.clamp", json!({"value":9,"min":1,"max":5})).unwrap(),
        json!(5)
    );
    assert_eq!(
        run("number.round", json!({"value":2.5})).unwrap(),
        json!(3.0)
    );
    assert_eq!(
        run("number.round", json!({"value":124,"digits":-1})).unwrap(),
        json!(120)
    );
    assert_eq!(
        run(
            "number.format",
            json!({"value":12345,"precision":2,"grouping":true})
        )
        .unwrap(),
        json!("12,345.00")
    );
    assert_eq!(
        run(
            "number.format_size",
            json!({"value":1536,"system":"iec","precision":1})
        )
        .unwrap(),
        json!("1.5 KiB")
    );
}

#[test]
fn aggregation_sort_observes_cancellation() {
    let calls = std::cell::Cell::new(0);
    let callback = || {
        let count = calls.get() + 1;
        calls.set(count);
        if count > 2 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let context = ExecutionContext::new(&callback);
    let args = json!({"items":(0..2000).rev().collect::<Vec<_>>()});
    let error = applets::execute_with_context(
        "list.sort",
        args.as_object().unwrap(),
        &[],
        Path::new("."),
        &context,
    )
    .unwrap_err();
    assert_eq!(error.code, "cancelled");
}

#[test]
fn text_and_list_shaping_are_utf8_aware() {
    assert_eq!(
        run("text.trim", json!({"text":"  e\u{301}\u{1f642}  "})).unwrap(),
        json!("e\u{301}\u{1f642}")
    );
    assert_eq!(
        run(
            "text.length",
            json!({"text":"e\u{301}\u{1f642}","unit":"chars"})
        )
        .unwrap(),
        json!(3)
    );
    assert_eq!(
        run(
            "text.substring",
            json!({"text":"a\u{1f642}bc","start":1,"length":1})
        )
        .unwrap(),
        json!("\u{1f642}")
    );
    assert_eq!(
        run(
            "text.pad",
            json!({"text":"x","width":3,"side":"left","fill":"0"})
        )
        .unwrap(),
        json!("00x")
    );
    assert_eq!(
        run("text.length", json!({"text":"🙂","unit":"bytes"})).unwrap(),
        json!(4)
    );
    assert!(run(
        "text.substring",
        json!({"text":"🙂","start":1,"length":1,"unit":"bytes"})
    )
    .is_err());
    assert!(run("text.substring", json!({"text":"abc","start":4,"length":1})).is_err());
    assert_eq!(
        run("text.repeat", json!({"text":"ab","count":3})).unwrap(),
        json!("ababab")
    );
    assert_eq!(
        run("text.starts_with", json!({"text":"hello","prefix":"he"})).unwrap(),
        json!(true)
    );
    assert_eq!(
        run("text.ends_with", json!({"text":"hello","suffix":"lo"})).unwrap(),
        json!(true)
    );
    assert_eq!(
        run("text.truncate", json!({"text":"abcdef","width":4})).unwrap(),
        json!("abc…")
    );
    assert_eq!(
        run(
            "text.contains_any",
            json!({"text":"hello","needles":["x","ell"]})
        )
        .unwrap(),
        json!(true)
    );
}

#[test]
fn list_shaping_covers_ranges_slices_and_sequences() {
    assert_eq!(
        run("list.range", json!({"start":1,"stop":5})).unwrap(),
        json!([1, 2, 3, 4])
    );
    assert_eq!(
        run("list.enumerate", json!({"items":["a","b"]})).unwrap(),
        json!([{"index":0,"item":"a"},{"index":1,"item":"b"}])
    );
    assert_eq!(
        run(
            "list.slice",
            json!({"items":[1,2,3,4],"offset":-2,"length":1})
        )
        .unwrap(),
        json!([3])
    );
    assert_eq!(
        run("list.reverse", json!({"items":[1,2,3]})).unwrap(),
        json!([3, 2, 1])
    );
    assert_eq!(
        run("list.concat", json!({"items":[[1],[2,3]]})).unwrap(),
        json!([1, 2, 3])
    );
    assert_eq!(
        run("list.flatten", json!({"items":[[1,[2]],3]})).unwrap(),
        json!([1, 2, 3])
    );
    assert_eq!(
        run("list.zip", json!({"left":[1,2],"right":["a"]})).unwrap(),
        json!([[1, "a"]])
    );
    assert_eq!(
        run("list.chunk", json!({"items":[1,2,3],"size":2})).unwrap(),
        json!([[1, 2], [3]])
    );
    assert_eq!(run("list.reverse", json!({"items":[]})).unwrap(), json!([]));
    assert!(run("list.slice", json!({"items":[1,2],"offset":2,"length":1})).is_err());
    assert!(run("list.first", json!({"items":[]})).is_err());
}

#[test]
fn round_decimal_values_at_positive_and_negative_places() {
    for (value, digits, expected) in [
        (124.5, -1, 120.0),
        (125.1, -1, 130.0),
        (-124.5, -1, -120.0),
        (-125.1, -1, -130.0),
        (125.0, -1, 130.0),
        (-125.0, -1, -130.0),
        (1.005, 2, 1.01),
        (-1.005, 2, -1.01),
        (1.004, 2, 1.0),
        (-1.004, 2, -1.0),
        (2.5, 0, 3.0),
        (-2.5, 0, -3.0),
        (9.995, 2, 10.0),
        (0.005, 2, 0.01),
        (4.999, -1, 0.0),
        (5.0, -1, 10.0),
        (0.5, 0, 1.0),
        (-0.5, 0, -1.0),
    ] {
        let result = run("number.round", json!({"value":value,"digits":digits})).unwrap();
        assert_eq!(
            result.as_f64().unwrap(),
            expected,
            "value={value} digits={digits}"
        );
    }
}

#[test]
fn round_float_extremes_without_intermediate_overflow() {
    for (value, digits, expected) in [
        (1e308, 308, 1e308),
        (-1e308, 308, -1e308),
        (5e-309, 308, 1e-308),
        (-5e-309, 308, -1e-308),
        (1e-308, -308, 0.0),
        (5e307, -308, 1e308),
        (-5e307, -308, -1e308),
        (f64::from_bits(1), 308, 0.0),
        (f64::MAX, 308, f64::MAX),
        (1.25e308, -307, 1.3e308),
    ] {
        let result = run("number.round", json!({"value":value,"digits":digits})).unwrap();
        assert_eq!(
            result.as_f64().unwrap(),
            expected,
            "value={value} digits={digits}"
        );
    }
    let negative_zero = run("number.round", json!({"value":-0.004,"digits":2}))
        .unwrap()
        .as_f64()
        .unwrap();
    assert_eq!(negative_zero, 0.0);
    assert!(negative_zero.is_sign_negative());
    for value in [1.79e308, -1.79e308] {
        let error = run("number.round", json!({"value":value,"digits":-307})).unwrap_err();
        assert_eq!(error.code, "overflow");
    }
    for digits in [-309, 309] {
        let error = run("number.round", json!({"value":1.5,"digits":digits})).unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }
}

#[test]
fn round_integer_behavior_remains_exact() {
    for (value, digits, expected) in [
        (125, -1, 130),
        (-125, -1, -130),
        (124, -1, 120),
        (123, 3, 123),
    ] {
        assert_eq!(
            run("number.round", json!({"value":value,"digits":digits})).unwrap(),
            json!(expected)
        );
    }
    assert_eq!(
        run("number.round", json!({"value":u64::MAX,"digits":-1}))
            .unwrap_err()
            .code,
        "overflow"
    );
}

#[test]
fn list_range_stops_at_integer_boundaries() {
    let cases = [
        (i64::MAX - 1, i64::MAX, 2, vec![i64::MAX - 1]),
        (i64::MIN + 1, i64::MIN, -2, vec![i64::MIN + 1]),
        (
            i64::MIN,
            i64::MAX,
            i64::MAX,
            vec![i64::MIN, -1, i64::MAX - 1],
        ),
        (i64::MAX, i64::MIN, i64::MIN, vec![i64::MAX, -1]),
        (i64::MAX, i64::MIN, 1, vec![]),
        (i64::MIN, i64::MAX, -1, vec![]),
        (i64::MAX, i64::MAX, i64::MAX, vec![]),
    ];
    for (start, stop, step, expected) in cases {
        assert_eq!(
            run(
                "list.range",
                json!({"start": start, "stop": stop, "step": step})
            )
            .unwrap(),
            json!(expected),
            "start={start}, stop={stop}, step={step}"
        );
    }
    assert_eq!(
        run(
            "list.range",
            json!({"start": i64::MAX - 1, "stop": i64::MAX})
        )
        .unwrap(),
        json!([i64::MAX - 1])
    );
    assert_eq!(
        run(
            "list.range",
            json!({"start": i64::MIN + 1, "stop": i64::MIN})
        )
        .unwrap(),
        json!([i64::MIN + 1])
    );
}

#[test]
fn list_range_retains_validation_and_item_limit() {
    for args in [
        json!({"start": 0, "stop": 1, "step": 0}),
        json!({"start": "0", "stop": 1}),
        json!({"start": 0, "stop": null}),
        json!({"start": 0, "stop": 1, "step": 1.5}),
        json!({"stop": 1}),
    ] {
        assert_eq!(run("list.range", args).unwrap_err().code, "invalid_request");
    }
    let full = run("list.range", json!({"start": 0, "stop": 100000})).unwrap();
    assert_eq!(full.as_array().unwrap().len(), 100000);
    for args in [
        json!({"start": 0, "stop": 100001}),
        json!({"start": 0, "stop": -100001}),
    ] {
        let error = run("list.range", args).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains("100000 item limit"), "{error:?}");
    }
}
