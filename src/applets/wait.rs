use super::{invalid, optional_u64, required_string, AppletError, ExecutionContext};
use serde_json::{Map, Value};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const MAX_WAIT_MS: u64 = 86_400_000;
pub(super) fn run(
    operation: &str,
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    match operation {
        "time.sleep" => sleep(args, context),
        "wait.until" => wait(args, cwd, context),
        _ => Err(invalid("unsupported wait operation")),
    }
}
fn sleep(args: &Map<String, Value>, context: &ExecutionContext<'_>) -> Result<Value, AppletError> {
    // Validate supplied JSON types without changing the existing precedence or
    // the fallback from a negative milliseconds value to valid seconds.
    if args
        .get("milliseconds")
        .is_some_and(|value| !value.is_i64() && !value.is_u64())
    {
        return Err(invalid("args.milliseconds must be an integer"));
    }
    if args.get("seconds").is_some_and(|value| !value.is_number()) {
        return Err(invalid("args.seconds must be a number"));
    }
    let ms = args
        .get("milliseconds")
        .and_then(Value::as_u64)
        .or_else(|| {
            args.get("seconds").and_then(Value::as_f64).and_then(|s| {
                if s.is_finite() && s >= 0.0 && (s * 1000.0) <= MAX_WAIT_MS as f64 {
                    Some((s * 1000.0).ceil() as u64)
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| invalid("provide a bounded non-negative milliseconds or seconds value"))?;
    if ms > MAX_WAIT_MS {
        return Err(invalid("sleep exceeds the 24-hour bound"));
    }
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        context.checkpoint()?;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(10)),
        );
    }
    Ok(Value::Bool(true))
}
fn wait(
    args: &Map<String, Value>,
    cwd: &Path,
    context: &ExecutionContext<'_>,
) -> Result<Value, AppletError> {
    let timeout = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("args.timeout_ms must be an integer"))?;
    let interval = optional_u64(args, "interval_ms", 50)?;
    if timeout > MAX_WAIT_MS || interval == 0 || interval > 10_000 {
        return Err(invalid(
            "timeout_ms must be at most 24 hours and interval_ms must be 1..10000",
        ));
    }
    let kind = required_string(args, "probe")?;
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout);
    let mut next_probe = started;
    loop {
        context.checkpoint()?;
        let now = Instant::now();
        if now >= next_probe {
            let remaining = deadline.saturating_duration_since(now);
            if probe(kind, args, cwd, remaining.min(Duration::from_millis(40)))? {
                return Ok(
                    serde_json::json!({"ready":true,"waited_ms":started.elapsed().as_millis()}),
                );
            }
            next_probe = now + Duration::from_millis(interval);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(
                serde_json::json!({"ready":false,"waited_ms":started.elapsed().as_millis()}),
            );
        }
        thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(10)),
        );
    }
}
fn probe(
    kind: &str,
    args: &Map<String, Value>,
    cwd: &Path,
    max_duration: Duration,
) -> Result<bool, AppletError> {
    match kind {
        "file_exists" => {
            let path = required_string(args, "path")?;
            Ok(cwd.join(path).exists())
        }
        "file_readable" => {
            let path = cwd.join(required_string(args, "path")?);
            Ok(std::fs::File::open(path).is_ok())
        }
        "tcp_port_open" => {
            let host = required_string(args, "host")?;
            let port = args
                .get("port")
                .and_then(Value::as_u64)
                .filter(|p| *p <= 65535)
                .ok_or_else(|| invalid("args.port must be an integer from 0 through 65535"))?;
            let ip = host
                .parse::<IpAddr>()
                .map_err(|_| invalid("tcp_port_open host must be an IP address"))?
                .to_canonical();
            // The manifest grants network.loopback only, so other hosts are refused.
            if !ip.is_loopback() {
                return Err(invalid(
                    "tcp_port_open host must be a loopback address (127.0.0.0/8 or ::1)",
                ));
            }
            let addr = SocketAddr::new(ip, port as u16);
            if max_duration.is_zero() {
                return Ok(false);
            }
            Ok(TcpStream::connect_timeout(&addr, max_duration).is_ok())
        }
        "command_succeeds" | "process_exited" | "socket_exists" | "file_changed" => Err(invalid(
            format!("probe {kind:?} is not supported by this in-process wait applet"),
        )),
        _ => Err(invalid(
            "probe must be file_exists, file_readable, or tcp_port_open",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::AppletErrorKind;
    use super::*;
    #[test]
    fn sleep_is_bounded_and_cooperatively_cancelled() {
        let args = Map::from_iter([("milliseconds".into(), Value::from(20))]);
        assert_eq!(
            sleep(&args, &ExecutionContext::unrestricted()).unwrap(),
            Value::Bool(true)
        );
        let cancel = || Err(super::super::ExecutionBudgetExceeded::Cancelled);
        assert_eq!(
            sleep(&args, &ExecutionContext::new(&cancel))
                .unwrap_err()
                .kind,
            AppletErrorKind::Cancelled
        );
    }
    #[test]
    fn wait_returns_false_at_its_deadline() {
        let args = Map::from_iter([
            ("probe".into(), Value::String("file_exists".into())),
            (
                "path".into(),
                Value::String("lane-v7-definitely-absent".into()),
            ),
            ("timeout_ms".into(), Value::from(0)),
        ]);
        let got = wait(&args, Path::new("."), &ExecutionContext::unrestricted()).unwrap();
        assert_eq!(got["ready"], false);
    }
    #[test]
    fn unsupported_probe_fails_closed() {
        let args = Map::from_iter([
            ("probe".into(), Value::String("command_succeeds".into())),
            ("timeout_ms".into(), Value::from(10)),
        ]);
        assert!(wait(&args, Path::new("."), &ExecutionContext::unrestricted()).is_err());
    }
    #[test]
    fn tcp_probe_refuses_non_loopback_hosts() {
        for host in [
            "10.0.0.1",
            "169.254.169.254",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            let args = Map::from_iter([
                ("probe".into(), Value::String("tcp_port_open".into())),
                ("host".into(), Value::String(host.into())),
                ("port".into(), Value::from(80)),
                ("timeout_ms".into(), Value::from(10)),
            ]);
            let error = wait(&args, Path::new("."), &ExecutionContext::unrestricted()).unwrap_err();
            assert!(
                error.message.contains("loopback"),
                "{host}: {}",
                error.message
            );
        }
        let mapped = Map::from_iter([
            ("probe".into(), Value::String("tcp_port_open".into())),
            ("host".into(), Value::String("::ffff:127.0.0.1".into())),
            ("port".into(), Value::from(1)),
            ("timeout_ms".into(), Value::from(0)),
        ]);
        assert!(wait(&mapped, Path::new("."), &ExecutionContext::unrestricted()).is_ok());
    }
}
