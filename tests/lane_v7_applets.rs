use flow::applets::{execute_in_cwd, AppletErrorKind};
use serde_json::{json, Value};
use std::path::Path;

fn call(name: &str, args: Value) -> Result<Value, flow::applets::AppletError> {
    execute_in_cwd(
        name,
        &args.as_object().cloned().unwrap_or_default(),
        b"",
        Path::new("."),
    )
}

#[test]
fn json_get_uses_default_for_absence_and_null_without_coercion() {
    for (document, pointer) in [
        (json!({"a":{}}), "/a/missing"),
        (json!([1]), "/4"),
        (json!({"a":null}), "/a"),
    ] {
        let got = call(
            "json.get",
            json!({"document":document,"pointer":pointer,"default":17}),
        )
        .unwrap();
        assert_eq!(got, json!(17));
    }
    assert_eq!(
        call(
            "json.get",
            json!({"document":{"a":{}},"pointer":"/a/x","default":"none"})
        )
        .unwrap(),
        json!("none")
    );
    assert_eq!(
        call(
            "json.get",
            json!({
                "document":{"bar":{"position":null},"position":"top"},
                "pointers":["/bar/position","/position"],
                "default":"fallback"
            })
        )
        .unwrap(),
        json!("top")
    );
    assert_eq!(
        call("json.get", json!({"document":{"n":null},"pointer":"/n"})).unwrap(),
        Value::Null
    );
    assert!(call(
        "json.get",
        json!({"document":[1],"pointer":"/01","default":0})
    )
    .is_err());
    assert_eq!(
        call("json.get", json!({"document":{"a":{}},"pointer":"/a/x"}))
            .unwrap_err()
            .kind,
        AppletErrorKind::Absence
    );
    assert_eq!(
        call(
            "json.map",
            json!({
                "document":[{"x":null},{}, {"x":1}],
                "pointer":"",
                "select":"/x",
                "default":"fallback"
            })
        )
        .unwrap(),
        json!(["fallback", "fallback", 1])
    );
}

#[test]
fn json_object_operations_are_stable_and_round_trip() {
    let document = json!({"z":1,"a":2,"n":{"x":3}});
    assert_eq!(
        call("json.keys", json!({"document":document})).unwrap(),
        json!(["a", "n", "z"])
    );
    assert_eq!(
        call("json.keys", json!({"document":document})).unwrap(),
        json!(["a", "n", "z"])
    );
    assert_eq!(
        call("json.has", json!({"document":document,"path":"/n/x"})).unwrap(),
        json!(true)
    );
    assert_eq!(
        call("json.entries", json!({"document":{"b":2,"a":1}})).unwrap(),
        json!([{"key":"a","value":1},{"key":"b","value":2}])
    );
    assert_eq!(
        call("json.values", json!({"document":{"b":2,"a":1}})).unwrap(),
        json!([1, 2])
    );
    assert_eq!(
        call("json.omit", json!({"document":{"a":1,"b":2},"keys":["a"]})).unwrap(),
        json!({"b":2})
    );
    assert_eq!(
        call(
            "json.rename_keys",
            json!({"document":{"a":1},"renames":{"a":"b"}})
        )
        .unwrap(),
        json!({"b":1})
    );
    assert!(call(
        "json.rename_keys",
        json!({"document":{"a":1,"b":2},"renames":{"a":"b"}})
    )
    .is_err());
    assert_eq!(
        call("json.concat", json!({"left":[1],"right":[2]})).unwrap(),
        json!([1, 2])
    );
    assert_eq!(
        call("json.first", json!({"document":[1,2]})).unwrap(),
        json!(1)
    );
    assert_eq!(
        call("json.last", json!({"document":[1,2]})).unwrap(),
        json!(2)
    );
    assert_eq!(
        call("json.sum", json!({"document":[1,2,3]})).unwrap(),
        json!(6)
    );
    assert_eq!(
        call(
            "json.unique_by",
            json!({"document":[{"k":1},{"k":1},{"k":2}],"pointer":"/k"})
        )
        .unwrap(),
        json!([{"k":1},{"k":2}])
    );
    assert_eq!(
        call(
            "json.group_by",
            json!({"document":[{"k":2},{"k":1},{"k":2}],"pointer":"/k"})
        )
        .unwrap(),
        json!([[{"k":1}],[{"k":2},{"k":2}]])
    );
    let deleted = call("json.delete", json!({"document":document,"path":"/n/x"})).unwrap();
    assert_eq!(deleted, json!({"a":2,"n":{},"z":1}));
    let encoded = call("json.stringify", json!({"value":deleted})).unwrap();
    assert_eq!(
        call("json.parse", json!({"text":encoded})).unwrap(),
        deleted
    );
    assert_eq!(
        call(
            "json.merge",
            json!({"left":{"x":1,"keep":2},"right":{"x":3}})
        )
        .unwrap(),
        json!({"x":3,"keep":2})
    );
    assert_eq!(
        call("json.length", json!({"document":[1,2,3]})).unwrap(),
        json!(3)
    );
    assert_eq!(
        call(
            "json.sort_by",
            json!({"document":[{"n":4},{"n":1}],"pointer":"/n","numeric":true})
        )
        .unwrap(),
        json!([{"n":1},{"n":4}])
    );
    assert!(call(
        "json.sort_by",
        json!({"document":[{"n":"4"}],"pointer":"/n","numeric":true})
    )
    .is_err());
}

#[test]
fn invalid_encodings_and_invalid_time_are_errors() {
    assert!(call("decode.base64", json!({"text":"%%%="})).is_err());
    assert!(call("decode.hex", json!({"text":"0"})).is_err());
    assert!(call("time.format", json!({"epoch":"now","format":"%Y"})).is_err());
    assert!(call("time.now", json!({"epoch_s":"bad"})).is_err());
    assert_eq!(
        call("decode.url", json!({"text":"a+b%2Fc"})).unwrap(),
        json!("a+b/c")
    );
    assert_eq!(
        call(
            "decode.base64",
            json!({"text":"Zm9v","url_safe":true,"padding":false})
        )
        .unwrap(),
        json!("foo")
    );
}

#[test]
fn injectable_clock_and_wait_deadline_work() {
    assert_eq!(
        call("time.now", json!({"epoch_s":0})).unwrap()["epoch_s"],
        json!(0)
    );
    let result = call(
        "wait.until",
        json!({"probe":"file_exists","path":"__lane_v7_missing__","timeout_ms":0}),
    )
    .unwrap();
    assert_eq!(result["ready"], false);
}

#[test]
fn json_collection_equality_and_numeric_sort_match_shared_rules() {
    let document = json!([
        {"k":{"n":[1,0]},"id":"first"},
        {"k":{"n":[1.0,-0.0]},"id":"second"},
        {"k":{"n":[9007199254740993u64]},"id":"large"},
        {"k":{"n":[9007199254740992.0]},"id":"float"},
        {"k":1,"id":"integer"}, {"k":1.0,"id":"decimal"}
    ]);
    let unique = call(
        "json.unique_by",
        json!({"document":document,"pointer":"/k"}),
    )
    .unwrap();
    assert_eq!(unique.as_array().unwrap().len(), 4);
    assert_eq!(unique[0]["id"], "first");
    let groups = call("json.group_by", json!({"document":document,"pointer":"/k"})).unwrap();
    let groups = groups.as_array().unwrap();
    assert_eq!(groups.len(), 4);
    assert!(groups.iter().any(|group| group
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>()
        == vec!["first", "second"]));
    assert!(groups.iter().any(|group| group
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>()
        == vec!["integer", "decimal"]));
    let sorted = call("json.sort_by", json!({"document":[{"n":9007199254740993u64},{"n":9007199254740992.0},{"n":9007199254740992u64},{"n":u64::MAX},{"n":18446744073709551616.0}],"pointer":"/n","numeric":true})).unwrap();
    assert_eq!(sorted[0]["n"], json!(9007199254740992.0));
    assert_eq!(sorted[1]["n"], json!(9007199254740992u64));
    assert_eq!(sorted[2]["n"], json!(9007199254740993u64));
    assert_eq!(sorted[3]["n"], json!(u64::MAX));
    assert_eq!(sorted[4]["n"], json!(18446744073709551616.0));
}

#[test]
fn json_sort_cancellation_during_comparisons_returns_budget_error() {
    use flow::applets::{execute_with_context, ExecutionBudgetExceeded, ExecutionContext};
    let document: Vec<_> = (0..8192).map(|i| json!({"n":(i * 7919) % 8192})).collect();
    let args = json!({"document":document,"pointer":"/n","numeric":true});
    let calls = std::cell::Cell::new(0usize);
    let measure = || {
        calls.set(calls.get() + 1);
        Ok(())
    };
    execute_with_context(
        "json.sort_by",
        args.as_object().unwrap(),
        &[],
        Path::new("."),
        &ExecutionContext::new(&measure),
    )
    .unwrap();
    let total = calls.get();
    assert!(total > 4);
    calls.set(0);
    let cancel = || {
        calls.set(calls.get() + 1);
        if calls.get() >= total - 1 {
            Err(ExecutionBudgetExceeded::Cancelled)
        } else {
            Ok(())
        }
    };
    let error = execute_with_context(
        "json.sort_by",
        args.as_object().unwrap(),
        &[],
        Path::new("."),
        &ExecutionContext::new(&cancel),
    )
    .unwrap_err();
    assert_eq!(error.code, "cancelled");
    assert_eq!(calls.get(), total - 1);
}
