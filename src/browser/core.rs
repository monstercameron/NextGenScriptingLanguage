//! Bounded browser implementations of the data applets used by the playground.
use super::*;

pub(super) fn supports(call: &Invocation) -> bool {
    !call.direct
        && matches!(
            call.operation.as_str(),
            "json.parse" | "json.stringify" | "json.set" | "text.format"
        )
}

pub(super) fn execute(
    call: &Invocation,
    bindings: &BTreeMap<String, Value>,
    input: &[u8],
) -> Result<Value> {
    let args = call
        .parameters
        .iter()
        .map(|(name, value)| Ok((name.as_str(), resolve(value, bindings)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let required = |name: &str| args.get(name).with_context(|| format!("missing {name}"));
    match call.operation.as_str() {
        "json.stringify" => Ok(Value::String(String::from_utf8(
            crate::structured::encode(required("value")?, LIMIT)?,
        )?)),
        "json.parse" => {
            let text = match args.get("text") {
                Some(Value::String(text)) => text.as_bytes(),
                Some(_) => bail!("json.parse text must be text"),
                None => input,
            };
            if text.len() > LIMIT {
                bail!("JSON text exceeds 1 MiB browser limit");
            }
            crate::structured::parse(text)
        }
        "json.set" => {
            let mut document = match args.get("document") {
                Some(document) => crate::structured::to_json(document)?,
                None => crate::structured::parse_json_value(input)?,
            };
            let pointer = match args.get("pointer") {
                Some(Value::String(pointer)) => pointer.as_str(),
                Some(_) => bail!("JSON pointer must be text"),
                None => "",
            };
            crate::structured::validate_pointer(pointer)?;
            if pointer.len() > 4096 || pointer.bytes().filter(|byte| *byte == b'/').count() > 256 {
                bail!("JSON pointer exceeds browser limit");
            }
            let value = crate::structured::to_json(required("value")?)?;
            if pointer.is_empty() {
                document = value;
            } else {
                let (parent, key) = pointer.rsplit_once('/').context("invalid JSON pointer")?;
                let key = key.replace("~1", "/").replace("~0", "~");
                let target = document
                    .pointer_mut(parent)
                    .context("JSON pointer parent is absent")?;
                match target {
                    serde_json::Value::Object(fields) => {
                        fields.insert(key, value);
                    }
                    serde_json::Value::Array(items) => {
                        let index = if key == "-" {
                            items.len()
                        } else {
                            if key.is_empty()
                                || key.len() > 1 && key.starts_with('0')
                                || !key.bytes().all(|byte| byte.is_ascii_digit())
                            {
                                bail!("invalid JSON array index");
                            }
                            key.parse::<usize>().context("invalid JSON array index")?
                        };
                        if index == items.len() {
                            items.push(value);
                        } else {
                            *items.get_mut(index).context("JSON array index is absent")? = value;
                        }
                    }
                    _ => bail!("JSON pointer parent must be an object or array"),
                }
            }
            let value = crate::structured::from_json(document);
            crate::structured::encode(&value, LIMIT)?;
            Ok(value)
        }
        "text.format" => {
            let Value::String(template) = required("template")? else {
                bail!("template must be text");
            };
            let Value::Record(values) = required("values")? else {
                bail!("values must be a record");
            };
            if !matches!(required("escape")?, Value::String(mode) if mode == "plain") {
                bail!("browser text.format supports only escape plain");
            }
            let mut output = String::new();
            let mut rest = template.as_str();
            while let Some(start) = rest.find("{{") {
                let (before, brace) = rest.split_at(start);
                output.push_str(before);
                if let Some(tail) = brace.strip_prefix("{{{{") {
                    output.push_str("{{");
                    rest = tail;
                } else {
                    let body = &brace[2..];
                    let end = body.find("}}").context("unterminated placeholder")?;
                    let name = body[..end].trim_matches([' ', '\t']);
                    if name.is_empty()
                        || name.len() > 128
                        || !name.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                        })
                    {
                        bail!("invalid placeholder name");
                    }
                    match values
                        .get(name)
                        .with_context(|| format!("no value for placeholder {name:?}"))?
                    {
                        Value::String(text) => output.push_str(text),
                        Value::Integer(number) => output.push_str(&number.to_string()),
                        Value::Number(number) => output.push_str(&number.to_string()),
                        Value::Bool(flag) => output.push_str(if *flag { "true" } else { "false" }),
                        _ => bail!("placeholder must be text, a number or a boolean"),
                    }
                    rest = &body[end + 2..];
                }
                if output.len().saturating_add(rest.len()) > LIMIT {
                    bail!("formatted output exceeds 1 MiB browser limit");
                }
            }
            output.push_str(rest);
            if output.len() > LIMIT {
                bail!("formatted output exceeds 1 MiB browser limit");
            }
            Ok(Value::String(output))
        }
        _ => bail!("unsupported browser data applet"),
    }
}
