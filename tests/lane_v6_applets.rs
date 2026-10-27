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
fn recursive_equality_is_shared_by_unique_group_and_filter() {
    let items = json!([
        {"v":[1,{"zero":0}]}, {"v":[1.0,{"zero":-0.0}]},
        {"v":[9007199254740993u64]}, {"v":[9007199254740992.0]},
        {"v":[18446744073709551615u64]}, {"v":[18446744073709551616.0]},
        {"v":[true]}, {"v":["1"]}
    ]);
    let unique = run("list.unique", json!({"items":items})).unwrap();
    assert_eq!(unique.as_array().unwrap().len(), 7);
    let groups = run("list.group", json!({"items":items,"key":""})).unwrap();
    assert_eq!(groups.as_array().unwrap().len(), 7);
    assert_eq!(groups[0]["count"], 2);
    let filtered = run(
        "json.filter",
        json!({"document":items,"where":[{"equals":{"v":[1.0,{"zero":0}]}}]}),
    )
    .unwrap();
    assert_eq!(filtered.as_array().unwrap().len(), 2);
    let boundary = run(
        "json.filter",
        json!({"document":items,"where":[{"equals":{"v":[9007199254740992.0]}}]}),
    )
    .unwrap();
    assert_eq!(boundary.as_array().unwrap().len(), 1);
}

#[test]
fn integer_float_equality_is_exact_at_representable_boundaries() {
    let items = json!([
        9007199254740992u64,
        9007199254740992.0,
        9007199254740993u64,
        i64::MIN,
        -9223372036854775808.0,
        u64::MAX,
        18446744073709551616.0
    ]);
    let unique = run("list.unique", json!({"items":items})).unwrap();
    assert_eq!(unique.as_array().unwrap().len(), 5);
    let groups = run("list.group", json!({"items":items,"key":""})).unwrap();
    assert_eq!(
        groups
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["count"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2, 1, 2, 1, 1]
    );
    let filtered = run(
        "json.filter",
        json!({"document":items,"where":[{"equals":9007199254740992.0}]}),
    )
    .unwrap();
    assert_eq!(filtered.as_array().unwrap().len(), 2);
}

#[test]
fn recursive_equality_framing_keeps_values_distinct() {
    let items = json!([["a","bc"],["ab","c"],{"a":"bc"},{"ab":"c"},[1],{"0":1},"n:1",1,1.0,0,-0.0]);
    assert_eq!(
        run("list.unique", json!({"items":items}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        9
    );
    assert_eq!(
        run("list.group", json!({"items":items,"key":""}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        9
    );
}

#[test]
fn list_validation_checks_nested_work_for_cancellation() {
    let calls = std::cell::Cell::new(0);
    let callback = || {
        calls.set(calls.get() + 1);
        if calls.get() > 3 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let args = json!({"items":[(0..5000).collect::<Vec<_>>() ]});
    let error = applets::execute_with_context(
        "list.unique",
        args.as_object().unwrap(),
        &[],
        Path::new("."),
        &ExecutionContext::new(&callback),
    )
    .unwrap_err();
    assert_eq!(error.code, "cancelled");
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
fn aggregation_rejects_oversized_values_and_pipeline_line_counts() {
    let input = vec![b'\n'; 1_000_001];
    for name in ["list.count", "list.unique"] {
        assert_eq!(
            applets::execute(name, &serde_json::Map::new(), &input)
                .unwrap_err()
                .code,
            "invalid_request"
        );
    }
    let args = json!({"items":["x".repeat(16 * 1024 * 1024)]});
    for name in ["list.count", "list.unique", "list.sort"] {
        assert_eq!(
            applets::execute(name, args.as_object().unwrap(), &[])
                .unwrap_err()
                .code,
            "invalid_request"
        );
    }
}

#[test]
fn oversized_nested_json_is_rejected_and_long_string_scans_can_cancel() {
    let args = json!({"document":["x".repeat(9 * 1024 * 1024), "y".repeat(9 * 1024 * 1024)]});
    let err = run("json.length", args).unwrap_err();
    assert_eq!(err.code, "invalid_request");
    let value = "x".repeat(1024 * 1024);
    let args = json!({"document":[value]});
    let calls = std::cell::Cell::new(0);
    let callback = || {
        calls.set(calls.get() + 1);
        if calls.get() > 4 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let err = applets::execute_with_context(
        "json.length",
        args.as_object().unwrap(),
        &[],
        Path::new("."),
        &ExecutionContext::new(&callback),
    )
    .unwrap_err();
    assert_eq!(err.code, "cancelled");
    assert_eq!(calls.get(), 5);
}

#[test]
fn deeply_nested_list_values_are_rejected_before_serialization() {
    let mut value = json!(1);
    for _ in 0..129 {
        value = Value::Array(vec![value]);
    }
    let args = json!({"items":[value]});
    assert_eq!(
        run("list.unique", args).unwrap_err().code,
        "invalid_request"
    );
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
        if count > 16 {
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
fn text_pad_can_use_terminal_display_width() {
    assert_eq!(
        run(
            "text.pad",
            json!({"text":"界","width":3,"display_width":true})
        )
        .unwrap(),
        json!("界 ")
    );
    assert_eq!(
        run(
            "text.pad",
            json!({"text":"界","width":3,"display_width":true,"side":"left"})
        )
        .unwrap(),
        json!(" 界")
    );
    assert_eq!(
        run(
            "text.pad",
            json!({"text":"x","width":4,"display_width":true,"fill":"界"})
        )
        .unwrap(),
        json!("x界 ")
    );
    assert_eq!(
        run(
            "text.pad",
            json!({"text":"🙂","width":3,"display_width":true,"fill":"."})
        )
        .unwrap(),
        json!("🙂.")
    );
    assert!(run(
        "text.pad",
        json!({"text":"x","width":3,"display_width":true,"fill":"\u{301}"})
    )
    .is_err());

    assert_eq!(
        run("text.pad", json!({"text":"界","width":3})).unwrap(),
        json!("界  ")
    );
    // Padding never truncates the input when it already meets or exceeds the
    // requested display width.
    assert_eq!(
        run(
            "text.pad",
            json!({"text":"界ab","width":2,"display_width":true})
        )
        .unwrap(),
        json!("界ab")
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

#[test]
fn numeric_formatters_validate_present_precision() {
    for (operation, value, maximum, default, lower, upper) in [
        (
            "number.format",
            json!(1.5),
            15,
            "2",
            "2",
            "1.500000000000000",
        ),
        (
            "number.format_size",
            json!(1024),
            6,
            "1.0 KiB",
            "1 KiB",
            "1.000000 KiB",
        ),
    ] {
        assert_eq!(
            run(operation, json!({"value":value})).unwrap(),
            json!(default)
        );
        assert_eq!(
            run(operation, json!({"value":value,"precision":0})).unwrap(),
            json!(lower)
        );
        assert_eq!(
            run(operation, json!({"value":value,"precision":maximum})).unwrap(),
            json!(upper)
        );
        for invalid in [
            json!(-1),
            json!(maximum + 1),
            json!(u64::MAX),
            json!(1.5),
            json!(1.0),
            json!("1"),
            json!(null),
            json!(true),
            json!([]),
            json!({}),
        ] {
            let error = run(operation, json!({"value":value,"precision":invalid})).unwrap_err();
            assert_eq!(error.code, "invalid_request", "{operation}: {invalid}");
            assert!(error.message.contains("args.precision"), "{error:?}");
        }
    }
}

#[test]
fn numeric_precision_errors_reach_helper_and_runtime() {
    for operation in ["number.format", "number.format_size"] {
        let helper = std::process::Command::new(env!("CARGO_BIN_EXE_execute-applet"))
            .args([operation, "--args", r#"{"value":1024,"precision":-1}"#])
            .output()
            .unwrap();
        assert_eq!(helper.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&helper.stderr);
        assert!(
            stderr.contains("invalid_request") && stderr.contains("args.precision"),
            "{stderr}"
        );
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("case.program");
        std::fs::write(
            &program,
            format!("run {operation} with value 1024 and precision -1\n"),
        )
        .unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("run")
            .arg(&program)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("args.precision"), "{stderr}");

        std::fs::write(
            &program,
            format!("try\n  run {operation} with value 1024 and precision -1\ncatch failure\n  then encode failure as json\n"),
        ).unwrap();
        let caught = std::process::Command::new(env!("CARGO_BIN_EXE_execute"))
            .arg("run")
            .arg(&program)
            .output()
            .unwrap();
        assert!(
            caught.status.success(),
            "{}",
            String::from_utf8_lossy(&caught.stderr)
        );
        let failure: Value = serde_json::from_slice(&caught.stdout).unwrap();
        assert_eq!(failure["kind"], "failure");
        assert_eq!(failure["status"], 2);
        assert!(failure["message"]
            .as_str()
            .unwrap()
            .contains("invalid_request"));
    }
}
