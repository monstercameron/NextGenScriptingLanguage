use super::{checkpoint_work, invalid, optional_bool, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha512};

/// Chunk between budget checks so hashing or recoding a large input stops
/// promptly on cancellation or deadline. Each chunk is plain CPU work; no
/// helper thread is spawned.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn run(
    operation: &str,
    args: &Map<String, Value>,
    input: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    let bytes = match args.get("text") {
        Some(Value::String(text)) => text.as_bytes(),
        Some(_) => return Err(invalid("args.text must be a string")),
        None => input,
    };
    match operation {
        "hash.sha256" => Ok(Value::String(hex(&digest_sha256(bytes, context)?))),
        "hash.sha512" => Ok(Value::String(hex(&digest_sha512(bytes, context)?))),
        "encode.hex" => Ok(Value::String(hex_chunked(bytes, context)?)),
        "decode.hex" => Ok(Value::String(
            String::from_utf8(decode_hex_with_context(required_string(args, "text")?, context)?)
                .map_err(|_| invalid("hex input is not UTF-8"))?,
        )),
        "encode.base64" => {
            let mut encoded = base64(bytes, context)?;
            if optional_bool(args, "url_safe", false)? {
                encoded = encoded.replace('+', "-").replace('/', "_");
            }
            if !optional_bool(args, "padding", true)? {
                encoded.truncate(encoded.trim_end_matches('=').len());
            }
            Ok(Value::String(encoded))
        }
        "decode.base64" => {
            let text = required_string(args, "text")?;
            let url_safe = optional_bool(args, "url_safe", false)?;
            let mut normalized = if url_safe {
                text.replace('-', "+").replace('_', "/")
            } else {
                text.to_owned()
            };
            if !optional_bool(args, "padding", true)? {
                if normalized.len() % 4 == 1 {
                    return Err(invalid("base64 input has an impossible unpadded length"));
                }
                while !normalized.len().is_multiple_of(4) {
                    normalized.push('=');
                }
            }
            Ok(Value::String(
                String::from_utf8(decode_base64(&normalized, context)?)
                    .map_err(|_| invalid("base64 input is not UTF-8"))?,
            ))
        }
        "encode.url" | "encode.form" => Ok(Value::String(url_encode(
            operation, args, bytes, context,
        )?)),
        "decode.url" => Ok(Value::String(url_decode(
            required_string(args, "text")?,
            optional_bool(args, "form", false)?,
            context,
        )?)),
        _ => Err(invalid("unsupported hash or encoding operation")),
    }
}

fn digest_sha256(bytes: &[u8], context: &ExecutionContext<'_>) -> Result<[u8; 32], AppletError> {
    let mut hasher = Sha256::new();
    for chunk in bytes.chunks(HASH_CHUNK_BYTES) {
        hasher.update(chunk);
        context.checkpoint()?;
    }
    Ok(hasher.finalize().into())
}

fn digest_sha512(bytes: &[u8], context: &ExecutionContext<'_>) -> Result<[u8; 64], AppletError> {
    let mut hasher = Sha512::new();
    for chunk in bytes.chunks(HASH_CHUNK_BYTES) {
        hasher.update(chunk);
        context.checkpoint()?;
    }
    Ok(hasher.finalize().into())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Hex-encode a digest or byte string, checking the budget for every
/// 64 KiB chunk so a large encoding stops promptly on cancellation or
/// deadline. Each chunk is plain CPU work; no helper thread is spawned.
fn hex_chunked(bytes: &[u8], context: &ExecutionContext<'_>) -> Result<String, AppletError> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for chunk in bytes.chunks(HASH_CHUNK_BYTES) {
        context.checkpoint()?;
        for b in chunk {
            out.push(DIGITS[(b >> 4) as usize] as char);
            out.push(DIGITS[(b & 15) as usize] as char);
        }
    }
    context.checkpoint()?;
    Ok(out)
}
fn decode_hex_with_context(
    text: &str,
    context: &ExecutionContext<'_>,
) -> Result<Vec<u8>, AppletError> {
    if !text.len().is_multiple_of(2) {
        return Err(invalid("hex input must have an even number of digits"));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = (pair[0] as char).to_digit(16);
        let lo = (pair[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(a), Some(b)) => out.push(((a << 4) | b) as u8),
            _ => return Err(invalid("hex input contains a non-hex digit")),
        }
        checkpoint_work(context, index)?;
    }
    context.checkpoint()?;
    Ok(out)
}
fn base64(bytes: &[u8], context: &ExecutionContext<'_>) -> Result<String, AppletError> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for (index, chunk) in bytes.chunks(3).enumerate() {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
        checkpoint_work(context, index)?;
    }
    context.checkpoint()?;
    Ok(out)
}
fn decode_base64(text: &str, context: &ExecutionContext<'_>) -> Result<Vec<u8>, AppletError> {
    let clean = text.as_bytes();
    if !clean.len().is_multiple_of(4) {
        return Err(invalid("base64 input length must be a multiple of four"));
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for (idx, ch) in clean.as_chunks::<4>().0.iter().enumerate() {
        checkpoint_work(context, idx)?;
        let last = idx + 1 == clean.len() / 4;
        let val = |b: u8| -> Option<u8> {
            match b {
                b'A'..=b'Z' => Some(b - b'A'),
                b'a'..=b'z' => Some(b - b'a' + 26),
                b'0'..=b'9' => Some(b - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        };
        let a = val(ch[0]).ok_or_else(|| invalid("invalid base64 character"))?;
        let b = val(ch[1]).ok_or_else(|| invalid("invalid base64 character"))?;
        let c = if ch[2] == b'=' {
            None
        } else {
            Some(val(ch[2]).ok_or_else(|| invalid("invalid base64 character"))?)
        };
        let d = if ch[3] == b'=' {
            None
        } else {
            Some(val(ch[3]).ok_or_else(|| invalid("invalid base64 character"))?)
        };
        if (!last && (c.is_none() || d.is_none()))
            || (c.is_none() && d.is_some())
            || (c.is_none() && b & 15 != 0)
            || (d.is_none() && c.is_some_and(|v| v & 3 != 0))
        {
            return Err(invalid("invalid base64 padding"));
        }
        out.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            out.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                out.push((c << 6) | d);
            }
        }
    }
    context.checkpoint()?;
    Ok(out)
}
fn url_encode(
    op: &str,
    args: &Map<String, Value>,
    bytes: &[u8],
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    if op == "encode.form" {
        let object = args
            .get("value")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("args.value must be an object"))?;
        let mut pairs = Vec::with_capacity(object.len());
        for (index, (k, v)) in object.iter().enumerate() {
            let value = match v {
                Value::String(s) => s.clone(),
                _ => v.to_string(),
            };
            pairs.push(format!(
                "{}={}",
                form_encode(k.as_bytes(), context)?,
                form_encode(value.as_bytes(), context)?
            ));
            checkpoint_work(context, index)?;
        }
        context.checkpoint()?;
        return Ok(pairs.join("&"));
    }
    let form = optional_bool(args, "form", false)?;
    percent_encode(bytes, form, context)
}
fn form_encode(bytes: &[u8], context: &ExecutionContext<'_>) -> Result<String, AppletError> {
    percent_encode(bytes, true, context)
}
fn percent_encode(
    bytes: &[u8],
    form: bool,
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    let mut out = String::new();
    for (index, &b) in bytes.iter().enumerate() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char)
        } else if form && b == b' ' {
            out.push('+')
        } else {
            out.push('%');
            out.push(
                char::from_digit((b >> 4) as u32, 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((b & 15) as u32, 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
        }
        checkpoint_work(context, index)?;
    }
    context.checkpoint()?;
    Ok(out)
}
fn url_decode(
    text: &str,
    form: bool,
    context: &ExecutionContext<'_>,
) -> Result<String, AppletError> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut work = 0usize;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err(invalid("incomplete percent escape"));
            }
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            match (h, l) {
                (Some(h), Some(l)) => out.push(((h << 4) | l) as u8),
                _ => return Err(invalid("invalid percent escape")),
            }
            i += 3;
        } else if form && b[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
        work += 1;
        checkpoint_work(context, work)?;
    }
    context.checkpoint()?;
    String::from_utf8(out).map_err(|_| invalid("decoded URL value is not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn unrestricted() -> ExecutionContext<'static> {
        ExecutionContext::unrestricted()
    }
    #[test]
    fn nist_hash_and_rfc_encodings() {
        let context = unrestricted();
        assert_eq!(
            run("hash.sha256", &Map::new(), b"abc", &context).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            run("hash.sha512", &Map::new(), b"abc", &context).unwrap(),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            run("encode.hex", &Map::new(), b"foo", &context).unwrap(),
            "666f6f"
        );
        assert_eq!(
            run("encode.base64", &Map::new(), b"foo", &context).unwrap(),
            "Zm9v"
        );
        assert_eq!(
            run(
                "decode.base64",
                &Map::from_iter([("text".into(), Value::String("Zm9v".into()))]),
                b"",
                &context,
            )
            .unwrap(),
            "foo"
        );
        assert_eq!(
            run("encode.url", &Map::new(), b"a b/~", &context).unwrap(),
            "a%20b%2F~"
        );
        assert_eq!(
            run(
                "decode.url",
                &Map::from_iter([("text".into(), Value::String("a+b%2Fc".into()))]),
                b"",
                &context,
            )
            .unwrap(),
            "a+b/c"
        );
        let url_args = Map::from_iter([
            ("text".into(), Value::String("Zm9v".into())),
            ("url_safe".into(), Value::Bool(true)),
            ("padding".into(), Value::Bool(false)),
        ]);
        assert_eq!(run("decode.base64", &url_args, b"", &context).unwrap(), "foo");
    }
    #[test]
    fn malformed_encodings_return_errors() {
        let context = unrestricted();
        assert!(run(
            "decode.base64",
            &Map::from_iter([("text".into(), Value::String("A===".into()))]),
            b"",
            &context,
        )
        .is_err());
        assert!(run(
            "decode.hex",
            &Map::from_iter([("text".into(), Value::String("0g".into()))]),
            b"",
            &context,
        )
        .is_err());
    }
}
