//! Owned executable fixtures for native Windows/Unix process contract tests.
use std::io::{BufRead, Read, Write};
// This fixture intentionally lets the helper process outlive its short-lived parent
// so the tests can exercise inherited-stderr draining after the parent exits.
#[cfg(unix)]
#[allow(clippy::zombie_processes)]
fn spawn_detached_child(command: &mut std::process::Command) -> std::io::Result<()> {
    let _child = command.spawn()?;
    Ok(())
}

/// Report ConPTY child stdio routing from inside the pseudo-console session:
/// the inherited standard-handle values, their file types, console-mode
/// status, and which write path reaches the screen. Every line is short ASCII
/// so the captured VT stream keeps each line (and marker) contiguous for the
/// harness byte-window assertions.
#[cfg(windows)]
fn conpty_probe_report() {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{GetFileType, FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_PIPE},
        System::Console::{
            GetConsoleMode, GetStdHandle, WriteConsoleW, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
    };
    for (name, kind) in [
        ("stdin", STD_INPUT_HANDLE),
        ("stdout", STD_OUTPUT_HANDLE),
        ("stderr", STD_ERROR_HANDLE),
    ] {
        // SAFETY: kind is one of the three standard-handle constants.
        // GetStdHandle takes no pointers, and its borrowed result is never
        // closed here.
        let handle = unsafe { GetStdHandle(kind) };
        let null = handle.is_null();
        let invalid = handle == INVALID_HANDLE_VALUE;
        let file_type = if null || invalid {
            "NONE"
        } else {
            // SAFETY: the handle is live here (neither null nor invalid),
            // and GetFileType takes no out-pointers.
            let kind = unsafe { GetFileType(handle) };
            if kind == FILE_TYPE_CHAR {
                "CHAR"
            } else if kind == FILE_TYPE_DISK {
                "DISK"
            } else if kind == FILE_TYPE_PIPE {
                "PIPE"
            } else {
                "OTHER"
            }
        };
        let mut mode = 0u32;
        let console = if null || invalid {
            false
        } else {
            // SAFETY: mode is a live local u32 for the out-parameter, and
            // the handle is neither null nor invalid here.
            unsafe { GetConsoleMode(handle, &mut mode) != 0 }
        };
        println!(
            "PROBE-REPORT {name} null={null} invalid={invalid} type={file_type} console={console} mode={mode:#X}"
        );
    }
    // SAFETY: STD_OUTPUT_HANDLE is re-queried as a borrowed handle that is
    // never closed. WriteConsoleW runs only when GetConsoleMode succeeds
    // above, so the handle is a console; the marker buffer is in bounds for
    // its length, written is a live local out-parameter, and the reserved
    // argument is null.
    let (console_ok, console_written) = unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if out.is_null() || out == INVALID_HANDLE_VALUE || GetConsoleMode(out, &mut mode) == 0 {
            (false, 0)
        } else {
            let marker: Vec<u16> = "\r\nCONPTY-PROBE-CONSOLE-OK\r\n".encode_utf16().collect();
            let mut written = 0u32;
            let ok = WriteConsoleW(
                out,
                marker.as_ptr().cast(),
                marker.len() as u32,
                &mut written,
                std::ptr::null(),
            ) != 0;
            (ok, written)
        }
    };
    println!("PROBE-REPORT write_console ok={console_ok} written={console_written}");
    let file_ok = std::io::stdout()
        .write_all(b"CONPTY-PROBE-FILE-OK\n")
        .is_ok()
        && std::io::stdout().flush().is_ok();
    println!("PROBE-REPORT write_file ok={file_ok}");
}

/// Second-stage ConPTY launcher: this process is already attached to the
/// pseudo-console, but its own stdio are still the parent's pipes (without
/// STARTF_USESTDHANDLES the parent std handles propagate into the ConPTY
/// child instead of defaulting to the console buffer). Re-spawn the real
/// target with genuine console standard handles: open CONIN$/CONOUT$ on the
/// attached console, spawn suspended with STARTF_USESTDHANDLES plus a
/// HANDLE_LIST containing exactly those three console handles so no other
/// handle leaks into the target, hold it in a KILL_ON_JOB_CLOSE-only job so
/// a harness timeout can never orphan it, then wait and exit with the
/// target's code. Silent on success; every failure prints to the inherited
/// cargo stderr and exits nonzero.
#[cfg(windows)]
fn conpty_bootstrap(args: &[String]) -> ! {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{
            JobObjects::*,
            Threading::{
                CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
                GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread,
                TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED,
                CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
                LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
                STARTUPINFOW,
            },
        },
    };
    let fail = |message: &str, code: i32| -> ! {
        eprintln!("conpty-bootstrap: {message}");
        std::process::exit(code);
    };
    let Some(target) = args.first() else {
        fail("usage: conpty-bootstrap <exe> [args...]", 64);
    };
    // CONIN$/CONOUT$ resolve to this process's attached console, which is the
    // pseudo-console session. Opening them is also the attach proof: failure
    // here means the session never attached, and failing loudly beats running
    // the target on cargo pipes while the harness waits for screen bytes.
    let wide = |name: &str| {
        name.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let conout_name = wide("CONOUT$");
    let conin_name = wide("CONIN$");
    // SAFETY: the names are live NUL-terminated UTF-16 locals; no security
    // attributes, no template. The returned handles are checked before use.
    let conout = unsafe {
        CreateFileW(
            conout_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: same contract as the CONOUT$ open above.
    let conin = unsafe {
        CreateFileW(
            conin_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if conout.is_null()
        || conout == INVALID_HANDLE_VALUE
        || conin.is_null()
        || conin == INVALID_HANDLE_VALUE
    {
        fail(
            "attached console unavailable (CONIN$/CONOUT$ did not open)",
            3,
        );
    }
    // The CreateFileW handles are non-inheritable; duplicate them inheritable
    // for the target. OwnedHandle takes each duplicate exactly once below.
    let mut conout_dup: HANDLE = std::ptr::null_mut();
    let mut conin_dup: HANDLE = std::ptr::null_mut();
    // SAFETY: the current-process pseudo-handle is valid for duplication, the
    // source handles are live console handles from above, and each out-pointer
    // is a live local. DUPLICATE_SAME_ACCESS keeps the console access rights.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            conout,
            GetCurrentProcess(),
            &mut conout_dup,
            0,
            TRUE,
            DUPLICATE_SAME_ACCESS,
        ) != FALSE
            && DuplicateHandle(
                GetCurrentProcess(),
                conin,
                GetCurrentProcess(),
                &mut conin_dup,
                0,
                TRUE,
                DUPLICATE_SAME_ACCESS,
            ) != FALSE
    };
    if !duplicated {
        fail("console handle duplication failed", 4);
    }
    // SAFETY: the duplicates are valid and uniquely owned from here on.
    let (conout_dup, conin_dup) = unsafe {
        (
            OwnedHandle::from_raw_handle(conout_dup),
            OwnedHandle::from_raw_handle(conin_dup),
        )
    };
    // A KILL_ON_JOB_CLOSE-only job: no other limits, so the runtime's own
    // inner jobs (every native spawn nests the same way the handoff path
    // already does) keep working, while a harness timeout that terminates
    // this bootstrap still ends the target instead of orphaning it.
    // SAFETY: both arguments null ask for default security and an unnamed
    // job; the handle is checked before use.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        fail("bootstrap job creation failed", 4);
    }
    // SAFETY: the job handle is live and uniquely owned here.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    // SAFETY: JOBOBJECT_EXTENDED_LIMIT_INFORMATION is plain integers, so the
    // zeroed value is valid; only LimitFlags is set, which means no other
    // limit. The info class matches the struct, and address plus size_of_val
    // delimit exactly one struct for the kernel.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the job handle is live; the struct layout matches the class.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == FALSE
    {
        fail("bootstrap job configuration failed", 4);
    }
    // Exactly the three console handles: the target inherits nothing else, so
    // the host pipe ends and the cargo pipes stay out of its handle table.
    let inherit = [
        conout_dup.as_raw_handle() as HANDLE,
        conout_dup.as_raw_handle() as HANDLE,
        conin_dup.as_raw_handle() as HANDLE,
    ];
    let mut attr_size: usize = 0;
    // SAFETY: a null list with a live size out-pointer is the documented size
    // query; failure with ERROR_INSUFFICIENT_BUFFER is expected here.
    unsafe {
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
    }
    if attr_size == 0 {
        fail("bootstrap attribute list size query failed", 4);
    }
    let mut attr_storage = vec![0u8; attr_size];
    let attr_list = attr_storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: the storage is exactly the queried size and outlives process
    // creation; one attribute slot is requested.
    if unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) } == FALSE {
        fail("bootstrap attribute list init failed", 4);
    }
    // SAFETY: the list is initialized; the array holds three live inheritable
    // console handles that outlive the spawn (OwnedHandles above).
    if unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherit.as_ptr().cast(),
            (inherit.len() * std::mem::size_of::<HANDLE>()) as usize,
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    } == FALSE
    {
        // SAFETY: the list was initialized above and is deleted exactly once.
        unsafe {
            DeleteProcThreadAttributeList(attr_list);
        }
        fail("bootstrap handle-list attribute failed", 4);
    }
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: conin_dup.as_raw_handle() as HANDLE,
            hStdOutput: conout_dup.as_raw_handle() as HANDLE,
            hStdError: conout_dup.as_raw_handle() as HANDLE,
            ..Default::default()
        },
        lpAttributeList: attr_list,
    };
    let text = std::iter::once(target)
        .chain(args[1..].iter())
        .map(|arg| quote_bootstrap_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: the command buffer is a live NUL-terminated mutable UTF-16
    // buffer; startup carries explicit console stdio plus the exact handle
    // list; environment and directory are inherited. The grandchild shares
    // this process's console by ordinary inheritance, so no pseudo-console
    // attribute is needed on this hop.
    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            TRUE,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            std::ptr::null(),
            &startup.StartupInfo as *const STARTUPINFOW,
            &mut info,
        )
    };
    // SAFETY: the attribute list was initialized above and is deleted exactly
    // once here on every path past initialization.
    unsafe {
        DeleteProcThreadAttributeList(attr_list);
    }
    drop(attr_storage);
    if created == FALSE {
        fail("bootstrap target launch failed", 4);
    }
    // SAFETY: CreateProcessW succeeded, so both handles are valid and uniquely
    // owned here.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    // SAFETY: same successful launch owns the suspended primary thread; it is
    // resumed into the job below, then the handle closes at once.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    // No target instruction runs until it belongs to the bootstrap job.
    // SAFETY: the job and process handles are live for this call, and the
    // target is still suspended, so none of its code has run yet.
    if unsafe {
        AssignProcessToJobObject(
            job.as_raw_handle() as HANDLE,
            process.as_raw_handle() as HANDLE,
        )
    } == FALSE
    {
        // SAFETY: the process handle is live; terminating a suspended target
        // whose job assignment failed keeps the session bounded.
        unsafe {
            TerminateProcess(process.as_raw_handle() as HANDLE, 1);
        }
        fail("bootstrap job assignment failed", 4);
    }
    // SAFETY: the thread handle is the suspended primary thread from the
    // launch above and stays open for this call.
    if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
        // SAFETY: the process handle is live; see the assignment-failure path.
        unsafe {
            TerminateProcess(process.as_raw_handle() as HANDLE, 1);
        }
        fail("bootstrap thread resume failed", 4);
    }
    drop(thread);
    // The harness bounds this wait from the outside and terminates this
    // process on timeout, which closes the job and ends the target; INFINITE
    // here therefore cannot deadlock teardown.
    // SAFETY: the process handle is live and owned; the wait ends when the
    // target exits or this process is terminated.
    let wait = unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, INFINITE) };
    let mut status = 1u32;
    // SAFETY: the process handle and the exit-code pointer are live; both
    // results are checked before the code propagates below.
    let queried =
        unsafe { GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut status) } != FALSE;
    drop(process);
    if wait != WAIT_OBJECT_0 || !queried {
        fail("bootstrap target wait failed", 4);
    }
    std::process::exit(status as i32);
}

/// Command-line quoting with the MSVCRT rules, so the bootstrap target sees
/// the exact argv the harness intended.
#[cfg(windows)]
fn quote_bootstrap_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut slashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
        } else {
            if ch == '"' {
                quoted.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
            } else {
                quoted.extend(std::iter::repeat_n('\\', slashes));
            }
            slashes = 0;
            quoted.push(ch);
        }
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('"');
    quoted
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("") {
        "argv" => serde_json::to_writer(std::io::stdout(), &args[1..]).unwrap(),
        "json-lines" => json_lines_worker(&args[1..]),
        "write" => {
            std::io::stdout().write_all(args[1].as_bytes()).unwrap();
        }
        "invalid-utf8" => {
            // Raw bytes for the Windows console replacement contract: 0xFF is
            // not valid UTF-8, so a console must show U+FFFD while a pipe keeps
            // the exact bytes. Flush while still attached so the bytes reach
            // the ConPTY screen before teardown instead of being lost or
            // landing on the test runner.
            let mut stdout = std::io::stdout();
            stdout.write_all(&[0xFF, 0x41]).unwrap();
            stdout.flush().unwrap();
        }
        "conpty-probe" => {
            // Bounded ConPTY routing probe: report the inherited standard
            // handles, console status and write-path delivery from inside the
            // pseudo-console child, so the next native run measures routing
            // instead of guessing it.
            #[cfg(windows)]
            conpty_probe_report();
            #[cfg(not(windows))]
            println!("PROBE-REPORT windows-only");
        }
        "conpty-bootstrap" => {
            // Second-stage ConPTY launcher: re-spawns the real target with
            // genuine console stdio; see conpty_bootstrap.
            #[cfg(windows)]
            conpty_bootstrap(&args[1..]);
            #[cfg(not(windows))]
            {
                eprintln!("conpty-bootstrap is Windows-only");
                std::process::exit(64);
            }
        }
        "repeat-byte" => {
            let mut remaining: usize = args[1].parse().unwrap();
            let bytes = [b'x'; 4096];
            let mut stdout = std::io::stdout().lock();
            while remaining > 0 {
                let next = remaining.min(bytes.len());
                stdout.write_all(&bytes[..next]).unwrap();
                remaining -= next;
            }
        }
        "flood-lines" => {
            // Fill more than an anonymous pipe can hold. After a downstream
            // reader exits, choose a process-defined status as many Windows
            // runtimes do when stdout reports ERROR_NO_DATA / ERROR_BROKEN_PIPE.
            let status: i32 = args[1].parse().unwrap();
            let mut stdout = std::io::stdout().lock();
            let chunk = [b'x'; 8192];
            let result = (|| -> std::io::Result<()> {
                stdout.write_all(b"first\n")?;
                for _ in 0..128 {
                    stdout.write_all(&chunk)?;
                }
                stdout.flush()
            })();
            if let Err(error) = result {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    std::process::exit(status);
                }
                std::process::exit(90);
            }
        }
        "flood-independent-failure" => {
            // Even if the reader closes, this process exits with its own
            // unrelated failure status after attempting a large write.
            let mut stdout = std::io::stdout().lock();
            let chunk = [b'x'; 8192];
            let _ = stdout.write_all(b"first\n");
            for _ in 0..128 {
                match stdout.write_all(&chunk) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => break,
                    Err(_) => std::process::exit(90),
                }
            }
            std::process::exit(42);
        }
        "first-line" => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            std::io::stdout().write_all(line.as_bytes()).unwrap();
            std::io::stdout().flush().unwrap();
        }
        "delayed-copy" => {
            std::thread::sleep(std::time::Duration::from_millis(250));
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
        }
        "copy" => {
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
        }
        "upper" => {
            let mut data = Vec::new();
            std::io::stdin()
                .take(16 * 1024 * 1024)
                .read_to_end(&mut data)
                .unwrap();
            data.make_ascii_uppercase();
            std::io::stdout().write_all(&data).unwrap();
        }
        "exit" => {
            if let Some(data) = args.get(2) {
                print!("{data}");
                std::io::stdout().flush().unwrap();
            }
            eprintln!("fixture failed");
            std::process::exit(args[1].parse().unwrap());
        }
        "produce" => {
            for text in ["one\n", "two\n", "three\n"] {
                print!("{text}");
                std::io::stdout().flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(35));
            }
        }
        "produce-exit" => {
            println!("event");
            std::io::stdout().flush().unwrap();
            std::process::exit(args[1].parse().unwrap());
        }
        "sleep" => std::thread::sleep(std::time::Duration::from_millis(
            args[1].parse::<u64>().unwrap().min(10000),
        )),
        "late-write" => {
            std::thread::sleep(std::time::Duration::from_millis(700));
            std::fs::write(&args[1], b"escaped").unwrap();
        }
        "spawn" => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["late-write", &args[1]])
                .spawn()
                .unwrap();
            println!("started");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            child.wait().unwrap();
        }
        "increment" => {
            let p = std::path::Path::new(&args[1]);
            let n = std::fs::read_to_string(p)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(p, n.to_string()).unwrap();
            print!("{n}");
        }
        "count-wait" => {
            // args: counter directory, element name, gate directory. Counts this
            // run in `<dir>/<name>`, then waits while `<gate>/<name>.hold` exists.
            let path = std::path::Path::new(&args[1]).join(&args[2]);
            let n = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(&path, n.to_string()).unwrap();
            let hold = std::path::Path::new(&args[3]).join(format!("{}.hold", args[2]));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while hold.exists() {
                if std::time::Instant::now() >= deadline {
                    std::process::exit(3);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            print!("{n}");
        }
        "flaky" => {
            // flaky COUNTER FAILURES STATUS: count this run, and exit STATUS while
            // the count is at most FAILURES. Otherwise print the count and succeed.
            let p = std::path::Path::new(&args[1]);
            let n = std::fs::read_to_string(p)
                .unwrap_or_else(|_| "0".into())
                .parse::<u32>()
                .unwrap()
                + 1;
            std::fs::write(p, n.to_string()).unwrap();
            let failures: u32 = args[2].parse().unwrap();
            if n <= failures {
                eprintln!("fixture busy");
                std::process::exit(args[3].parse().unwrap());
            }
            print!("{n}");
        }
        "stderr-bytes" => {
            let count: usize = args[1].parse().unwrap();
            let bytes = [b'e'; 4096];
            let mut remaining = count;
            let mut sink = std::io::stderr().lock();
            while remaining > 0 {
                let next = remaining.min(bytes.len());
                sink.write_all(&bytes[..next]).unwrap();
                remaining -= next;
            }
            if let Some(status) = args.get(2) {
                std::process::exit(status.parse().unwrap());
            }
        }
        #[cfg(unix)]
        "hold-stderr-after-parent-exit" => {
            use std::os::unix::process::CommandExt;
            let marker = &args[1];
            let release = &args[2];
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["hold-stderr-child", marker, release])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit());
            // SAFETY: setsid is async-signal-safe and the closure uses no shared
            // state in the forked child before exec.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            spawn_detached_child(&mut command).unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !std::path::Path::new(marker).exists() {
                assert!(
                    std::time::Instant::now() < until,
                    "descendant did not start"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        #[cfg(unix)]
        "hold-stderr-child" => {
            std::fs::write(&args[1], b"started").unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !std::path::Path::new(&args[2]).exists() && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::write(format!("{}.done", args[1]), b"done").unwrap();
        }
        "stderr-flood" => {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let bytes = [b'e'; 8192];
            let mut sink = std::io::stderr().lock();
            while std::time::Instant::now() < until {
                if sink.write_all(&bytes).is_err() {
                    break;
                }
            }
        }
        "flood" => {
            let bytes = [b'x'; 32768];
            for _ in 0..128 {
                if std::io::stdout().write_all(&bytes).is_err() {
                    break;
                }
            }
        }
        _ => std::process::exit(64),
    }
}

fn is_handshake_request(request: &serde_json::Value) -> bool {
    request.get("type").and_then(serde_json::Value::as_str) == Some("handshake")
        && request.get("protocol").and_then(serde_json::Value::as_str)
            == Some("ngsl.json-lines-worker")
        && request
            .get("versions")
            .and_then(serde_json::Value::as_array)
            .is_some()
        && !request
            .as_object()
            .is_some_and(|object| object.contains_key("id") || object.contains_key("arguments"))
}

fn json_lines_worker(arguments: &[String]) {
    let mode = arguments.first().map(String::as_str).unwrap_or("echo");
    let marker = arguments
        .get(1)
        .expect("json-lines fixture needs a launch marker");
    let effects = arguments.get(2);
    use std::fs::OpenOptions;
    let mut marker_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)
        .expect("opening worker launch marker");
    writeln!(marker_file, "{}", std::process::id()).expect("recording worker pid");

    if mode == "ignore" {
        std::thread::sleep(std::time::Duration::from_secs(30));
        return;
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut sequence = 0u64;
    let mut v2_negotiated = false;
    for line in stdin.lock().lines() {
        let line = line.expect("reading worker request");
        let request: serde_json::Value =
            serde_json::from_str(&line).expect("decoding worker request");
        if is_handshake_request(&request) {
            if v2_negotiated {
                let response = serde_json::json!({
                    "type":"handshake",
                    "protocol":"ngsl.json-lines-worker",
                    "error":"protocol version cannot be renegotiated"
                });
                serde_json::to_writer(&mut stdout, &response)
                    .expect("encoding renegotiation error");
                stdout
                    .write_all(b"\n")
                    .expect("terminating renegotiation error");
                stdout.flush().expect("flushing renegotiation error");
                continue;
            }
            let valid = request.get("protocol").and_then(serde_json::Value::as_str)
                == Some("ngsl.json-lines-worker")
                && request
                    .get("versions")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().any(|version| version.as_u64() == Some(2))
                    });
            v2_negotiated |= valid;
            let response = if valid {
                serde_json::json!({
                    "type":"handshake",
                    "protocol":"ngsl.json-lines-worker",
                    "version":2
                })
            } else {
                serde_json::json!({"type":"handshake","version":1})
            };
            serde_json::to_writer(&mut stdout, &response).expect("encoding handshake response");
            stdout
                .write_all(b"\n")
                .expect("terminating handshake response");
            stdout.flush().expect("flushing handshake response");
            continue;
        }
        let request_version = request.get("version").and_then(serde_json::Value::as_u64);
        if !v2_negotiated
            && request_version == Some(2)
            && request.get("input_base64").is_some()
            && request.get("input").is_none()
        {
            let response = serde_json::json!({
                "id": request.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0),
                "status": 2,
                "output": [],
                "stderr": "fixture: v2 frame received before handshake"
            });
            serde_json::to_writer(&mut stdout, &response)
                .expect("encoding unnegotiated v2 frame error");
            stdout
                .write_all(b"\n")
                .expect("terminating unnegotiated v2 frame error");
            stdout
                .flush()
                .expect("flushing unnegotiated v2 frame error");
            continue;
        }
        if v2_negotiated && request_version != Some(2) {
            let mut response = serde_json::json!({
                "version": 2,
                "id": request.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0),
                "status": 2,
            });
            if request["json_output"].as_bool().unwrap_or(false) {
                response["value"] = serde_json::Value::Null;
            } else {
                response["output_base64"] = serde_json::Value::String(String::new());
            }
            response["stderr"] = serde_json::Value::String(
                "fixture: frame version is missing or unsupported after v2 handshake".into(),
            );
            serde_json::to_writer(&mut stdout, &response).expect("encoding v2 version error");
            stdout
                .write_all(b"\n")
                .expect("terminating v2 version error");
            stdout.flush().expect("flushing v2 version error");
            continue;
        }
        let protocol_v2 = v2_negotiated && request_version == Some(2);
        let request_id = request["id"].as_u64().expect("request id");
        let input = if protocol_v2 {
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                request["input_base64"]
                    .as_str()
                    .expect("v2 input Base64 string"),
            )
            .expect("decoding v2 input Base64")
        } else {
            request["input"]
                .as_array()
                .expect("request input bytes")
                .iter()
                .map(|byte| {
                    u8::try_from(byte.as_u64().expect("byte value")).expect("byte in range")
                })
                .collect::<Vec<_>>()
        };
        let mut response_id = request_id;
        let mut status = 0i32;
        let mut output = input;
        let mut typed_value = None;
        let mut stderr = None;

        match mode {
            "sequence" => {
                sequence += 1;
                output = sequence.to_string().into_bytes();
            }
            "typed" => {
                output = br#"{"n":42}"#.to_vec();
                stderr = Some("é: diagnostic");
            }
            "echo-value" => {
                typed_value = request
                    .get("value")
                    .and_then(|value| value.get("payload"))
                    .cloned();
                if typed_value.is_none() {
                    status = 2;
                }
            }
            "status" => {
                status = -17;
                output = vec![0, 255, 1];
                stderr = Some("é: diagnostic");
            }
            "wrong-id" => response_id = response_id.wrapping_add(1),
            "invalid-json" => {
                stdout.write_all(b"{invalid json}\n").unwrap();
                stdout.flush().unwrap();
                return;
            }
            "partial" => {
                stdout.write_all(b"{\"id\":").unwrap();
                stdout.flush().unwrap();
                return;
            }
            "eof" => return,
            "oversized" => output = vec![b'x'; 256],
            "slow" => {
                if let Some(effects) = effects {
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(effects)
                        .expect("opening effect marker");
                    writeln!(file, "received").expect("recording request effect");
                }
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
            _ => {}
        }

        let response = if protocol_v2 {
            let mut response = serde_json::json!({
                "version": 2,
                "id": response_id,
                "status": status,
            });
            if request["json_output"].as_bool().unwrap_or(false) {
                let value = typed_value.unwrap_or_else(|| {
                    serde_json::from_slice(&output).expect("typed JSON worker output")
                });
                response["value"] = value;
            } else {
                response["output_base64"] = serde_json::Value::String(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &output,
                ));
            }
            if let Some(stderr) = stderr {
                response["stderr"] = serde_json::Value::String(stderr.to_owned());
            }
            response
        } else {
            serde_json::json!({
                "id": response_id,
                "status": status,
                "output": output,
                "stderr": stderr,
            })
        };
        serde_json::to_writer(&mut stdout, &response).expect("encoding worker response");
        stdout.write_all(b"\n").expect("terminating worker frame");
        stdout.flush().expect("flushing worker response");
        if mode == "trailing-and-exit" {
            let effects = effects.expect("trailing-output fixture needs a release marker");
            while !std::path::Path::new(effects).exists() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            stdout
                .write_all(b"unsolicited\n")
                .expect("writing unsolicited worker bytes");
            stdout.flush().expect("flushing unsolicited worker bytes");
            return;
        }
        if mode == "exit-after-one" {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_handshake_request;

    #[test]
    fn handshake_discriminator_preserves_legacy_extension_requests() {
        let handshake = serde_json::json!({
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "versions": [2]
        });
        assert!(is_handshake_request(&handshake));

        let legacy_request = serde_json::json!({
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "versions": [2],
            "id": 8,
            "arguments": ["legacy.operation"]
        });
        assert!(!is_handshake_request(&legacy_request));
    }
}
