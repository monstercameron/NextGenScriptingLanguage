use flow::applets;
use serde_json::{json, Value};
use std::{fs, process::Command};

fn diff(left: i64, right: i64, unit: &str) -> Result<Value, applets::AppletError> {
    let args = json!({"left": left, "right": right, "unit": unit});
    applets::execute("time.diff", args.as_object().unwrap(), &[])
}

fn add(epoch: i64, amount: i64, unit: &str) -> Result<Value, applets::AppletError> {
    let args = json!({"epoch": epoch, "amount": amount, "unit": unit});
    applets::execute("time.add", args.as_object().unwrap(), &[])
}

#[test]
fn time_add_checks_only_the_final_signed_64_bit_epoch() {
    let amount = 106_751_991_167_301;
    assert_eq!(
        add(i64::MIN, amount, "days").unwrap().as_i64(),
        Some(30_592)
    );
    assert_eq!(
        add(i64::MAX, -amount, "days").unwrap().as_i64(),
        Some(-30_593)
    );

    assert_eq!(
        add(i64::MAX, 1, "seconds").unwrap_err().code,
        "invalid_request"
    );
    assert_eq!(
        add(i64::MIN, -1, "seconds").unwrap_err().code,
        "invalid_request"
    );
    assert_eq!(
        add(0, i64::MAX, "days").unwrap_err().code,
        "invalid_request"
    );
}

#[test]
fn time_diff_uses_wide_subtraction_and_only_rounds_when_json_requires_float() {
    let positive = diff(i64::MAX, i64::MIN, "days").unwrap();
    let negative = diff(i64::MIN, i64::MAX, "days").unwrap();
    let expected = u64::MAX as f64 / 86_400.0;
    assert!((positive.as_f64().unwrap() - expected).abs() < 0.1);
    assert!((negative.as_f64().unwrap() + expected).abs() < 0.1);

    assert_eq!(
        diff(i64::MAX, i64::MIN, "seconds").unwrap().as_u64(),
        Some(u64::MAX)
    );
    let negative_seconds = diff(i64::MIN, i64::MAX, "seconds").unwrap();
    assert!((negative_seconds.as_f64().unwrap() + u64::MAX as f64).abs() < 4096.0);
    // Results that fit the established integer representation remain exact.
    assert_eq!(diff(8_640_000, 0, "days").unwrap().as_i64(), Some(100));
}

#[test]
fn whole_units_remain_exact_across_the_binary64_boundary() {
    for seconds in [
        9_007_199_254_740_991,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        -9_007_199_254_740_993,
        i64::MIN,
        i64::MAX,
    ] {
        let value = diff(seconds, 0, "seconds").unwrap();
        assert_eq!(value.as_i64(), Some(seconds), "{value}");
    }
    assert_eq!(
        diff(0, -9_007_199_254_740_993, "seconds").unwrap().as_i64(),
        Some(9_007_199_254_740_993)
    );
    assert_eq!(
        diff(0, 9_007_199_254_740_993, "seconds").unwrap().as_i64(),
        Some(-9_007_199_254_740_993)
    );
    for (unit, multiplier) in [("minutes", 60_i64), ("hours", 3600), ("days", 86400)] {
        let difference = i64::MAX / multiplier * multiplier;
        for value in [difference, -difference] {
            assert_eq!(
                diff(value, 0, unit).unwrap().as_i64(),
                Some(value / multiplier),
                "{unit}"
            );
        }
    }
}

#[test]
fn fractional_units_and_overflow_keep_their_contract() {
    for (unit, expected) in [("minutes", 0.5), ("hours", 1.5), ("days", 1.5)] {
        let seconds = match unit {
            "minutes" => 30,
            "hours" => 5400,
            _ => 129600,
        };
        assert_eq!(diff(seconds, 0, unit).unwrap().as_f64(), Some(expected));
        assert_eq!(diff(-seconds, 0, unit).unwrap().as_f64(), Some(-expected));
        assert!(diff(seconds, 0, unit).unwrap().as_i64().is_none());
    }
    assert_eq!(
        diff(i64::MAX, i64::MIN, "seconds").unwrap().as_u64(),
        Some(u64::MAX)
    );
}

#[test]
fn workflow_preserves_large_integer_through_typed_binding_and_json() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("test.program"), "run time.diff with left 9007199254740993 and right 0 and unit \"seconds\" as result\nthen encode result as json\n").unwrap();
    for command in ["check", "run"] {
        let output = Command::new(env!("CARGO_BIN_EXE_execute"))
            .current_dir(dir.path())
            .args([command, "test.program"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if command == "run" {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value.as_i64(), Some(9_007_199_254_740_993));
        }
    }
}

#[test]
fn independent_applet_serializes_large_whole_seconds_exactly() {
    let output = Command::new(env!("CARGO_BIN_EXE_execute-applet"))
        .args([
            "time.diff",
            "--args",
            r#"{"left":9007199254740993,"right":0,"unit":"seconds"}"#,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value.as_i64(), Some(9_007_199_254_740_993));
}
