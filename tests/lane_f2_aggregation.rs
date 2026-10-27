use flow::applets;
use serde_json::{json, Value};

fn run(name: &str, args: Value, input: &[u8]) -> Result<Value, applets::AppletError> {
    applets::execute(name, args.as_object().unwrap(), input)
}

#[test]
fn aggregate_lists_and_pipeline_lines_with_typed_values() {
    assert_eq!(
        run("list.count", json!({"items":[1,2,3]}), &[]).unwrap(),
        json!(3)
    );
    assert_eq!(run("list.count", json!({}), b"a\nb\n").unwrap(), json!(2));
    assert_eq!(
        run("list.sum", json!({"items":[1,2,3]}), &[]).unwrap(),
        json!(6)
    );
    assert_eq!(
        run("list.min", json!({"items":[3,1,2]}), &[]).unwrap(),
        json!(1)
    );
    assert_eq!(
        run("list.max", json!({"items":[3,1,2]}), &[]).unwrap(),
        json!(3)
    );
    assert_eq!(
        run("list.unique", json!({}), b"a\na\nb\n").unwrap(),
        json!(["a", "b"])
    );
    assert_eq!(
        run("list.sort", json!({"items":[3,1,2]}), &[]).unwrap(),
        json!([1, 2, 3])
    );
    assert_eq!(
        run("list.top", json!({"items":[3,1,2],"n":2}), &[]).unwrap(),
        json!([3, 2])
    );
    let grouped = run(
        "list.group",
        json!({"items":[{"k":"a"},{"k":"a"},{"k":"b"}],"key":"/k"}),
        &[],
    )
    .unwrap();
    assert_eq!(
        grouped,
        json!([{"key":"a","count":2},{"key":"b","count":1}])
    );
    assert_eq!(
        run(
            "text.count_lines_by",
            json!({"delimiter":" ","field":0}),
            b"GET /\nPOST /\nGET /x\n"
        )
        .unwrap(),
        json!([{"key":"GET","count":2},{"key":"POST","count":1}])
    );
}

#[test]
fn aggregate_input_and_numeric_errors_fail_closed() {
    assert!(run("list.sum", json!({"items":[1,"two"]}), &[]).is_err());
    assert!(run("list.count", json!({"items":{}}), &[]).is_err());
}

#[test]
fn aggregation_items_preserve_arbitrary_json_and_omitted_items_use_stdin() {
    let values = json!([null, true, {"nested":[1, "two"]}]);
    assert_eq!(
        run("list.unique", json!({"items":values.clone()}), &[]).unwrap(),
        values
    );
    assert_eq!(
        run("list.count", json!({}), b"one\ntwo\n").unwrap(),
        json!(2)
    );
}
