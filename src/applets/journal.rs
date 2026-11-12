//! Encoder for the journald native protocol datagram used by `log.emit`.
//!
//! This module only builds the bytes. It opens no sockets. The send to
//! `/run/systemd/journal/socket` (with the memfd path for oversized
//! datagrams) is not written yet.
//!
//! Framing (systemd Journal Native Protocol):
//! - `NAME=value\n` when the value contains neither a newline nor a NUL.
//! - `NAME\n`, then the value length as 8 bytes little-endian, then the
//!   value, then `\n` for every other value. This form is binary-safe.

use std::fmt;

/// Field names Execute sets itself. User fields may not use them.
const RESERVED_FIELDS: [&str; 3] = ["MESSAGE", "PRIORITY", "SYSLOG_IDENTIFIER"];

/// Syslog priority levels accepted by `log.emit`, from `emerg` (0) to `debug` (7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    Emerg,
    Alert,
    Crit,
    Err,
    Warning,
    Notice,
    Info,
    Debug,
}

impl Priority {
    /// Parses the lowercase level name used in `log.emit`.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "emerg" => Self::Emerg,
            "alert" => Self::Alert,
            "crit" => Self::Crit,
            "err" => Self::Err,
            "warning" => Self::Warning,
            "notice" => Self::Notice,
            "info" => Self::Info,
            "debug" => Self::Debug,
            _ => return None,
        })
    }

    /// The numeric value written to the `PRIORITY` field.
    pub(crate) fn number(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JournalError {
    /// The name is empty, or has a character outside `A-Z`, `0-9`, `_`,
    /// or starts with a digit or underscore.
    InvalidFieldName(String),
    /// The name is set by Execute and cannot come from a caller.
    ReservedFieldName(String),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFieldName(name) => write!(
                f,
                "journal field name {name:?} must start with A-Z and use only A-Z, 0-9 and _"
            ),
            Self::ReservedFieldName(name) => {
                write!(
                    f,
                    "journal field name {name:?} is set by execute and cannot be passed"
                )
            }
        }
    }
}

/// Checks a field name: non-empty, first byte `A-Z`, the rest `A-Z`, `0-9` or `_`.
pub(crate) fn validate_field_name(name: &str) -> Result<(), JournalError> {
    let valid = match name.as_bytes().split_first() {
        Some((first, rest)) => {
            first.is_ascii_uppercase()
                && rest
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
        }
        None => false,
    };
    if valid {
        Ok(())
    } else {
        Err(JournalError::InvalidFieldName(name.to_owned()))
    }
}

fn push_field(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    if value.contains(&b'\n') || value.contains(&0) {
        out.push(b'\n');
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value);
        out.push(b'\n');
    } else {
        out.push(b'=');
        out.extend_from_slice(value);
        out.push(b'\n');
    }
}

/// Builds one journald datagram with `MESSAGE`, `PRIORITY` and
/// `SYSLOG_IDENTIFIER` first, then the caller's fields in order.
///
/// Caller field names are validated. A name containing a newline or `=`
/// is refused, so it cannot inject a second field. Values are binary-safe.
pub(crate) fn encode_datagram(
    message: &str,
    priority: Priority,
    identifier: &str,
    fields: &[(&str, &str)],
) -> Result<Vec<u8>, JournalError> {
    for (name, _) in fields {
        validate_field_name(name)?;
        if RESERVED_FIELDS.contains(name) {
            return Err(JournalError::ReservedFieldName((*name).to_owned()));
        }
    }
    let mut out = Vec::new();
    push_field(&mut out, "MESSAGE", message.as_bytes());
    push_field(
        &mut out,
        "PRIORITY",
        priority.number().to_string().as_bytes(),
    );
    push_field(&mut out, "SYSLOG_IDENTIFIER", identifier.as_bytes());
    for (name, value) in fields {
        push_field(&mut out, name, value.as_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_values_use_text_form() {
        let bytes = encode_datagram(
            "hi",
            Priority::Warning,
            "execute",
            &[("EXECUTE_STEP_ID", "7")],
        )
        .expect("valid datagram");
        assert_eq!(
            bytes,
            b"MESSAGE=hi\nPRIORITY=4\nSYSLOG_IDENTIFIER=execute\nEXECUTE_STEP_ID=7\n".to_vec()
        );
    }

    #[test]
    fn newline_value_uses_length_prefixed_form() {
        let bytes = encode_datagram("a\nb", Priority::Info, "x", &[]).expect("valid datagram");
        let mut expected = b"MESSAGE\n".to_vec();
        expected.extend_from_slice(&3u64.to_le_bytes());
        expected.extend_from_slice(b"a\nb\n");
        expected.extend_from_slice(b"PRIORITY=6\nSYSLOG_IDENTIFIER=x\n");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn nul_value_is_binary_safe() {
        let mut out = Vec::new();
        push_field(&mut out, "F", b"a\0b");
        let mut expected = b"F\n".to_vec();
        expected.extend_from_slice(&3u64.to_le_bytes());
        expected.extend_from_slice(b"a\0b\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn injected_field_name_is_refused() {
        let attack = "X\nPRIORITY=0";
        let err = encode_datagram("m", Priority::Info, "x", &[(attack, "v")])
            .expect_err("newline in name must be refused");
        assert!(matches!(err, JournalError::InvalidFieldName(_)));
        assert!(encode_datagram("m", Priority::Info, "x", &[("A=B", "v")]).is_err());
    }

    #[test]
    fn reserved_names_are_refused() {
        for name in ["MESSAGE", "PRIORITY", "SYSLOG_IDENTIFIER"] {
            let err = encode_datagram("m", Priority::Info, "x", &[(name, "v")])
                .expect_err("reserved name must be refused");
            assert!(matches!(err, JournalError::ReservedFieldName(_)));
        }
    }

    #[test]
    fn field_name_rules() {
        for ok in ["A", "EXECUTE_RUN_ID", "FOO_2", "Z9"] {
            assert!(validate_field_name(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "", "lower", "Mixed", "_TRUSTED", "9LIVES", "A B", "A-B", "A=B", "A\n", "\0",
        ] {
            assert!(
                validate_field_name(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn priority_names_and_numbers() {
        let names = [
            ("emerg", 0),
            ("alert", 1),
            ("crit", 2),
            ("err", 3),
            ("warning", 4),
            ("notice", 5),
            ("info", 6),
            ("debug", 7),
        ];
        for (name, number) in names {
            assert_eq!(Priority::parse(name).map(Priority::number), Some(number));
        }
        assert_eq!(Priority::parse("warn"), None);
        assert_eq!(Priority::parse("ERR"), None);
    }
}
