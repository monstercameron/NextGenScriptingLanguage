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
/// Reads an optional absolute `deadline_ms` argument (epoch milliseconds) and
/// returns the milliseconds left until it on the applet clock. A deadline that
/// has already passed yields zero, so a repeated wait after a resume ends at the
/// original time instead of starting its full duration again.
fn remaining_to_deadline(args: &Map<String, Value>) -> Result<Option<u64>, AppletError> {
    let Some(value) = args.get("deadline_ms") else {
        return Ok(None);
    };
    let deadline_ms = value.as_i64().ok_or_else(|| {
        invalid("args.deadline_ms must be an integer epoch millisecond timestamp")
    })?;
    Ok(Some(remaining_until(deadline_ms, wall_clock_ms()?)))
}

/// Milliseconds from `now_ms` until `deadline_ms`, or zero when it has passed.
fn remaining_until(deadline_ms: i64, now_ms: i64) -> u64 {
    u64::try_from(deadline_ms.saturating_sub(now_ms)).unwrap_or(0)
}

/// Current epoch milliseconds from the same clock that `time.now` reads.
fn wall_clock_ms() -> Result<i64, AppletError> {
    let (secs, nanos) = super::time::now(&Map::new())?;
    secs.checked_mul(1_000)
        .and_then(|ms| ms.checked_add(i64::from(nanos / 1_000_000)))
        .ok_or_else(|| invalid("clock is outside supported range"))
}

/// The relative delay from `milliseconds` or `seconds`, in milliseconds.
fn duration_ms(args: &Map<String, Value>) -> Result<u64, AppletError> {
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
    args.get("milliseconds")
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
        .ok_or_else(|| invalid("provide a bounded non-negative milliseconds or seconds value"))
}

fn sleep(args: &Map<String, Value>, context: &ExecutionContext<'_>) -> Result<Value, AppletError> {
    let ms = match remaining_to_deadline(args)? {
        Some(remaining) => {
            if args.contains_key("milliseconds") || args.contains_key("seconds") {
                return Err(invalid(
                    "provide either deadline_ms or a milliseconds/seconds delay, not both",
                ));
            }
            remaining
        }
        None => duration_ms(args)?,
    };
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
    let timeout = match remaining_to_deadline(args)? {
        Some(remaining) => {
            if args.contains_key("timeout_ms") {
                return Err(invalid(
                    "provide either deadline_ms or timeout_ms, not both",
                ));
            }
            remaining
        }
        None => args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("args.timeout_ms must be an integer"))?,
    };
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
        "socket_exists" => {
            let path = cwd.join(required_string(args, "path")?);
            socket_file_exists(&path)
        }
        "command_succeeds" | "process_exited" | "file_changed" => Err(invalid(format!(
            "probe {kind:?} is not supported by this in-process wait applet"
        ))),
        _ => Err(invalid(
            "probe must be file_exists, file_readable, socket_exists, or tcp_port_open",
        )),
    }
}
#[cfg(unix)]
fn socket_file_exists(path: &Path) -> Result<bool, AppletError> {
    use std::os::unix::fs::FileTypeExt;
    // Follows symlinks, like `test -S`.
    Ok(std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket()))
}
#[cfg(not(unix))]
fn socket_file_exists(_path: &Path) -> Result<bool, AppletError> {
    Err(invalid(
        "probe \"socket_exists\" is only supported on Unix platforms",
    ))
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
    fn deadline_wait_resumes_with_the_remaining_time() {
        // A 1 h wait begun at t=0 is killed at 10 min. On resume the same
        // absolute deadline is used, so the remaining wait is what is left.
        let deadline_ms = 3_600_000;
        assert_eq!(remaining_until(deadline_ms, 0), 3_600_000);
        // Resumed at 30 min: waits the remaining 30 min.
        assert_eq!(remaining_until(deadline_ms, 1_800_000), 1_800_000);
        // Resumed at 70 min: the deadline has passed, so it proceeds at once.
        assert_eq!(remaining_until(deadline_ms, 4_200_000), 0);
        // Extreme values neither overflow nor panic.
        assert_eq!(remaining_until(i64::MIN, i64::MAX), 0);
        assert_eq!(remaining_until(i64::MAX, i64::MIN), i64::MAX as u64);
    }
    #[test]
    fn sleep_to_a_past_deadline_returns_at_once() {
        let args = Map::from_iter([("deadline_ms".into(), Value::from(0))]);
        let started = Instant::now();
        assert_eq!(
            sleep(&args, &ExecutionContext::unrestricted()).unwrap(),
            Value::Bool(true)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn sleep_accepts_a_future_absolute_deadline() {
        let now = wall_clock_ms().unwrap();
        let args = Map::from_iter([("deadline_ms".into(), Value::from(now + 20))]);
        assert_eq!(
            sleep(&args, &ExecutionContext::unrestricted()).unwrap(),
            Value::Bool(true)
        );
    }
    #[test]
    fn deadline_and_duration_arguments_are_exclusive() {
        let sleep_args = Map::from_iter([
            ("deadline_ms".into(), Value::from(0)),
            ("milliseconds".into(), Value::from(1)),
        ]);
        assert!(sleep(&sleep_args, &ExecutionContext::unrestricted()).is_err());
        let wait_args = Map::from_iter([
            ("probe".into(), Value::String("file_exists".into())),
            (
                "path".into(),
                Value::String("lane-v7-definitely-absent".into()),
            ),
            ("deadline_ms".into(), Value::from(0)),
            ("timeout_ms".into(), Value::from(10)),
        ]);
        assert!(wait(
            &wait_args,
            Path::new("."),
            &ExecutionContext::unrestricted()
        )
        .is_err());
    }
    #[test]
    fn wait_until_past_deadline_returns_not_ready_at_once() {
        let args = Map::from_iter([
            ("probe".into(), Value::String("file_exists".into())),
            (
                "path".into(),
                Value::String("lane-v7-definitely-absent".into()),
            ),
            ("deadline_ms".into(), Value::from(0)),
        ]);
        let got = wait(&args, Path::new("."), &ExecutionContext::unrestricted()).unwrap();
        assert_eq!(got["ready"], false);
    }
    #[test]
    fn deadline_must_be_an_integer() {
        let args = Map::from_iter([("deadline_ms".into(), Value::String("soon".into()))]);
        assert!(sleep(&args, &ExecutionContext::unrestricted()).is_err());
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
    #[cfg(unix)]
    #[test]
    fn socket_exists_matches_only_unix_sockets() {
        use std::os::unix::net::UnixListener;
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("ready.sock");
        let file_path = temp.path().join("plain.txt");
        std::fs::write(&file_path, b"not a socket").unwrap();
        let probe_args = |path: &Path| {
            Map::from_iter([
                ("probe".into(), Value::String("socket_exists".into())),
                ("path".into(), Value::String(path.to_string_lossy().into())),
                ("timeout_ms".into(), Value::from(0)),
            ])
        };
        let context = ExecutionContext::unrestricted();
        // Absent path: not ready.
        let got = wait(&probe_args(&socket_path), temp.path(), &context).unwrap();
        assert_eq!(got["ready"], false);
        // Regular file at the path: not a socket, not ready.
        let got = wait(&probe_args(&file_path), temp.path(), &context).unwrap();
        assert_eq!(got["ready"], false);
        // Bound Unix socket: ready.
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let got = wait(&probe_args(&socket_path), temp.path(), &context).unwrap();
        assert_eq!(got["ready"], true);
    }
    #[cfg(not(unix))]
    #[test]
    fn socket_exists_fails_closed_off_unix() {
        let args = Map::from_iter([
            ("probe".into(), Value::String("socket_exists".into())),
            ("path".into(), Value::String("any.sock".into())),
            ("timeout_ms".into(), Value::from(0)),
        ]);
        let error = wait(&args, Path::new("."), &ExecutionContext::unrestricted()).unwrap_err();
        assert!(error.message.contains("Unix"), "{}", error.message);
    }
}
