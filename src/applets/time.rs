use super::{invalid, optional_string, required_string, AppletError, ExecutionContext};
use chrono::{DateTime, Datelike, FixedOffset, Local, TimeZone, Timelike, Utc};
use serde_json::{Map, Number, Value};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn run(
    operation: &str,
    args: &Map<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    context.checkpoint()?;
    match operation {
        "time.now" => {
            let (secs, nanos) = now(args)?;
            let dt = DateTime::<Utc>::from_timestamp(secs, nanos)
                .ok_or_else(|| invalid("epoch is outside supported range"))?;
            let (epoch_ms, epoch_ns) = epoch_units(secs, nanos)?;
            Ok(
                serde_json::json!({"epoch_s":secs,"epoch_ms":epoch_ms,"epoch_ns":epoch_ns,"iso":dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi,true)}),
            )
        }
        "time.format" => {
            let epoch = integer(args, "epoch")?;
            let format = required_string(args, "format")?;
            let dt = at_epoch(epoch, optional_string(args, "zone", "utc")?)?;
            // Chrono reports invalid or unsupported directives through fmt::Error.
            // Display::to_string panics on that error; write into a String fallibly.
            let mut output = String::new();
            write!(&mut output, "{}", dt.format(format))
                .map_err(|_| invalid("invalid or unsupported time format"))?;
            Ok(Value::String(output))
        }
        "time.parse" => {
            let text = required_string(args, "text")?;
            let format = required_string(args, "format")?;
            let dt = DateTime::parse_from_str(text, format)
                .map_err(|e| invalid(format!("time text does not match format: {e}")))?;
            Ok(Value::Number(dt.timestamp().into()))
        }
        "time.parts" => {
            let epoch = integer(args, "epoch")?;
            let zone = optional_string(args, "zone", "utc")?;
            let dt = at_epoch(epoch, zone)?;
            Ok(parts(dt))
        }
        "time.add" => {
            let epoch = integer(args, "epoch")?;
            let amount = integer(args, "amount")?;
            let unit = required_string(args, "unit")?;
            let multiplier = unit_multiplier(unit)?;
            let result = i128::from(epoch) + i128::from(amount) * i128::from(multiplier);
            let result = i64::try_from(result).map_err(|_| invalid("time arithmetic overflow"))?;
            Ok(Value::Number(result.into()))
        }
        "time.diff" => {
            let left = integer(args, "left")?;
            let right = integer(args, "right")?;
            let unit = required_string(args, "unit")?;
            let multiplier = unit_multiplier(unit)?;
            let difference = i128::from(left) - i128::from(right);
            let multiplier = i128::from(multiplier);
            if difference % multiplier == 0 {
                // Keep exact whole-unit values as integers; converting first to
                // binary64 loses seconds beyond 2^53 even when no division is needed.
                let whole = difference / multiplier;
                if let Ok(whole) = i64::try_from(whole) {
                    Ok(Value::from(whole))
                } else if let Ok(whole) = u64::try_from(whole) {
                    Ok(Value::from(whole))
                } else {
                    Number::from_f64(whole as f64)
                        .map(Value::Number)
                        .ok_or_else(|| invalid("time difference is not finite"))
                }
            } else {
                Ok(Value::Number(
                    Number::from_f64(difference as f64 / multiplier as f64)
                        .ok_or_else(|| invalid("time difference is not finite"))?,
                ))
            }
        }
        _ => Err(invalid("unsupported time operation")),
    }
}
// Use a wider intermediate so a negative second just below i64 nanosecond
// range can become representable after its positive subsecond fraction is added.
fn epoch_units(secs: i64, nanos: u32) -> Result<(i64, i64), AppletError> {
    let ns = i128::from(secs) * 1_000_000_000 + i128::from(nanos);
    let ms = i128::from(secs) * 1_000 + i128::from(nanos / 1_000_000);
    let epoch_ns = i64::try_from(ns)
        .map_err(|_| invalid("epoch nanoseconds are outside the signed 64-bit range"))?;
    let epoch_ms = i64::try_from(ms)
        .map_err(|_| invalid("epoch milliseconds are outside the signed 64-bit range"))?;
    Ok((epoch_ms, epoch_ns))
}
fn now(args: &Map<String, Value>) -> Result<(i64, u32), AppletError> {
    if let Some(value) = args.get("epoch_s") {
        return value
            .as_i64()
            .map(|seconds| (seconds, 0))
            .ok_or_else(|| invalid("args.epoch_s must be an integer"));
    }
    match std::env::var("EXECUTE_NOW") {
        Ok(value) => {
            let seconds = value
                .parse::<i64>()
                .map_err(|_| invalid("EXECUTE_NOW must be an integer epoch second"))?;
            return Ok((seconds, 0));
        }
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(invalid("EXECUTE_NOW is not valid Unicode"));
        }
    }
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("system clock is before Unix epoch"))?;
    Ok((
        i64::try_from(d.as_secs())
            .map_err(|_| invalid("system clock is outside supported range"))?,
        d.subsec_nanos(),
    ))
}
fn integer(args: &Map<String, Value>, name: &str) -> Result<i64, AppletError> {
    args.get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid(format!("args.{name} must be an integer")))
}
fn unit_multiplier(unit: &str) -> Result<i64, AppletError> {
    match unit {
        "seconds" => Ok(1),
        "minutes" => Ok(60),
        "hours" => Ok(3600),
        "days" => Ok(86400),
        _ => Err(invalid("unit must be seconds, minutes, hours, or days")),
    }
}
fn at_epoch(epoch: i64, zone: &str) -> Result<DateTime<FixedOffset>, AppletError> {
    match zone {
        "utc" => Utc
            .timestamp_opt(epoch, 0)
            .single()
            .map(|d| d.fixed_offset())
            .ok_or_else(|| invalid("epoch is outside supported range")),
        "local" => Local
            .timestamp_opt(epoch, 0)
            .single()
            .map(|d| d.fixed_offset())
            .ok_or_else(|| invalid("epoch is outside supported range")),
        _ => {
            let offset = zone
                .parse::<i32>()
                .map_err(|_| invalid("zone must be utc, local, or a signed offset in seconds"))?;
            let offset = FixedOffset::east_opt(offset)
                .ok_or_else(|| invalid("zone offset is outside +/-24 hours"))?;
            offset
                .timestamp_opt(epoch, 0)
                .single()
                .ok_or_else(|| invalid("epoch is outside supported range"))
        }
    }
}

/// Calendar fields for one epoch second in the requested zone. Weekday
/// names are chrono's fixed English words, not locale data: callers that
/// need locale words keep using a native `date` with `LC_TIME` set.
fn parts(dt: DateTime<FixedOffset>) -> Value {
    let iso = dt.iso_week();
    serde_json::json!({
        "year": dt.year(),
        "month": dt.month(),
        "day": dt.day(),
        "hour": dt.hour(),
        "minute": dt.minute(),
        "second": dt.second(),
        "weekday": dt.weekday().number_from_monday(),
        "weekday_name": dt.format("%A").to_string(),
        "yearday": dt.ordinal(),
        "iso_year": iso.year(),
        "iso_week": iso.week(),
        "offset": dt.offset().local_minus_utc(),
    })
}

pub(super) fn valid_zone(zone: &str) -> bool {
    matches!(zone, "utc" | "local")
        || zone
            .parse::<i32>()
            .ok()
            .and_then(FixedOffset::east_opt)
            .is_some()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_clock_and_epoch_arithmetic_are_deterministic() {
        let args = Map::from_iter([("epoch_s".into(), Value::from(0))]);
        assert_eq!(
            run("time.now", &args, &ExecutionContext::unrestricted()).unwrap()["iso"],
            "1970-01-01T00:00:00Z"
        );
        let fmt = Map::from_iter([
            ("epoch".into(), Value::from(-1)),
            ("format".into(), Value::String("%Y-%m-%d".into())),
        ]);
        assert_eq!(
            run("time.format", &fmt, &ExecutionContext::unrestricted()).unwrap(),
            "1969-12-31"
        );
        let parse = Map::from_iter([
            (
                "text".into(),
                Value::String("2024-02-29T12:00:00+0000".into()),
            ),
            ("format".into(), Value::String("%Y-%m-%dT%H:%M:%S%z".into())),
        ]);
        assert_eq!(
            run("time.parse", &parse, &ExecutionContext::unrestricted()).unwrap(),
            Value::from(1709208000)
        );
        let y2038 = Map::from_iter([
            ("epoch".into(), Value::from(2_147_483_648i64)),
            ("format".into(), Value::String("%Y-%m-%d %H:%M:%S".into())),
            ("zone".into(), Value::String("19800".into())),
        ]);
        assert_eq!(
            run("time.format", &y2038, &ExecutionContext::unrestricted()).unwrap(),
            "2038-01-19 08:44:08"
        );
    }
    fn format(epoch: i64, format: &str, zone: &str) -> Value {
        let args = Map::from_iter([
            ("epoch".into(), Value::from(epoch)),
            ("format".into(), Value::String(format.into())),
            ("zone".into(), Value::String(zone.into())),
        ]);
        run("time.format", &args, &ExecutionContext::unrestricted()).unwrap()
    }

    fn parse(text: &str, format: &str) -> Value {
        let args = Map::from_iter([
            ("text".into(), Value::String(text.into())),
            ("format".into(), Value::String(format.into())),
        ]);
        run("time.parse", &args, &ExecutionContext::unrestricted()).unwrap()
    }

    fn parts(epoch: i64, zone: &str) -> Value {
        let args = Map::from_iter([
            ("epoch".into(), Value::from(epoch)),
            ("zone".into(), Value::String(zone.into())),
        ]);
        run("time.parts", &args, &ExecutionContext::unrestricted()).unwrap()
    }

    #[test]
    fn format_parse_round_trips_cover_dst_leap_negative_and_2038() {
        // (epoch, zone, rendered text): format and parse must agree, and the
        // fixed-offset rows pin both sides of the US Eastern spring-forward
        // gap (2026-03-08 02:00 local never exists).
        let table = [
            (1_791_970_860i64, "utc", "2026-10-14T09:41:00+0000"),
            (1_772_953_140i64, "-18000", "2026-03-08T01:59:00-0500"),
            (1_772_953_200i64, "-14400", "2026-03-08T03:00:00-0400"),
            (951_782_400i64, "utc", "2000-02-29T00:00:00+0000"),
            (1_709_208_000i64, "utc", "2024-02-29T12:00:00+0000"),
            (-1i64, "utc", "1969-12-31T23:59:59+0000"),
            (-86_400i64, "utc", "1969-12-31T00:00:00+0000"),
            (2_147_483_647i64, "utc", "2038-01-19T03:14:07+0000"),
            (2_147_483_648i64, "utc", "2038-01-19T03:14:08+0000"),
        ];
        for (epoch, zone, text) in table {
            assert_eq!(
                format(epoch, "%Y-%m-%dT%H:%M:%S%z", zone),
                Value::String(text.into()),
                "format {epoch} in {zone}"
            );
            assert_eq!(
                parse(text, "%Y-%m-%dT%H:%M:%S%z"),
                Value::from(epoch),
                "parse {text}"
            );
        }
    }

    #[test]
    fn parts_reports_calendar_fields_for_known_instants() {
        let epoch_zero = parts(0, "utc");
        assert_eq!(
            epoch_zero,
            serde_json::json!({
                "year": 1970, "month": 1, "day": 1,
                "hour": 0, "minute": 0, "second": 0,
                "weekday": 4, "weekday_name": "Thursday",
                "yearday": 1, "iso_year": 1970, "iso_week": 1,
                "offset": 0,
            })
        );
        let autumn = parts(1_791_970_860, "utc");
        assert_eq!(autumn["weekday"], 3);
        assert_eq!(autumn["weekday_name"], "Wednesday");
        assert_eq!(autumn["yearday"], 287);
        assert_eq!(autumn["iso_year"], 2026);
        assert_eq!(autumn["iso_week"], 42);
        let shifted = parts(1_791_970_860, "3600");
        assert_eq!(shifted["hour"], 10);
        assert_eq!(shifted["offset"], 3600);
        // 2000 was a leap year: 29 February exists and is ordinal day 60.
        let leap = parts(951_782_400, "utc");
        assert_eq!(leap["month"], serde_json::json!(2));
        assert_eq!(leap["day"], serde_json::json!(29));
        assert_eq!(leap["yearday"], serde_json::json!(60));
        let args = Map::from_iter([("epoch".into(), Value::from(0))]);
        assert!(run("time.parts", &args, &ExecutionContext::unrestricted()).is_ok());
        let missing = Map::new();
        assert!(run("time.parts", &missing, &ExecutionContext::unrestricted()).is_err());
    }

    #[test]
    fn epoch_units_check_both_fractional_boundaries() {
        assert_eq!(epoch_units(9_223_372_036, 854_775_807).unwrap().1, i64::MAX);
        assert!(epoch_units(9_223_372_036, 854_775_808).is_err());
        assert_eq!(
            epoch_units(-9_223_372_037, 145_224_192).unwrap().1,
            i64::MIN
        );
        assert!(epoch_units(-9_223_372_037, 145_224_191).is_err());
        assert_eq!(epoch_units(-1, 999_999_999).unwrap(), (-1, -1));
    }
    #[test]
    fn invalid_formats_and_overflow_fail_closed() {
        assert!(run(
            "time.parse",
            &Map::from_iter([
                ("text".into(), Value::String("bad".into())),
                ("format".into(), Value::String("%s".into()))
            ]),
            &ExecutionContext::unrestricted()
        )
        .is_err());
        assert_eq!(
            run(
                "time.diff",
                &Map::from_iter([
                    ("left".into(), Value::from(i64::MAX)),
                    ("right".into(), Value::from(i64::MIN)),
                    ("unit".into(), Value::String("seconds".into())),
                ]),
                &ExecutionContext::unrestricted()
            )
            .unwrap()
            .as_u64(),
            Some(u64::MAX)
        );
    }
}
