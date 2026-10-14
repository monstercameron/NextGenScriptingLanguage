//! Deliberately bounded Rust ports for the browser host, not host binaries.
use super::*;

impl BrowserSession {
    pub(super) fn applet(
        &mut self,
        name: &str,
        args: &[String],
        input: &[u8],
        cwd: &str,
    ) -> Result<(Vec<u8>, i64)> {
        if !APPLETS.contains(&name) {
            bail!("{name:?} is unavailable in the browser; use native execute");
        }
        let text = || std::str::from_utf8(input).context("browser text applet requires UTF-8");
        let mut status = 0;
        let output = match name {
            "true" | "false" => {
                no_args(args)?;
                status = i64::from(name == "false");
                Vec::new()
            }
            "pwd" => {
                no_args(args)?;
                format!("{cwd}\n").into_bytes()
            }
            "cd" => {
                if args.len() != 1 {
                    bail!("cd requires one virtual directory");
                }
                let next = absolute(&args[0], cwd)?;
                if !self.directories.contains(&next) {
                    bail!("directory does not exist: {next}");
                }
                self.cwd = next;
                Vec::new()
            }
            "mkdir" => {
                let mut parents = false;
                let mut paths = Vec::new();
                let mut options = true;
                for arg in args {
                    if options && arg == "--" {
                        options = false;
                    } else if options && arg == "-p" {
                        parents = true;
                    } else if options && arg.starts_with('-') {
                        bail!("mkdir supports only -p and -- in the browser");
                    } else {
                        paths.push(absolute(arg, cwd)?);
                    }
                }
                if paths.is_empty() {
                    bail!("mkdir needs a path");
                }
                for path in paths {
                    if self.files.contains_key(&path) {
                        bail!("file exists: {path}");
                    }
                    if self.directories.contains(&path) && !parents {
                        bail!("directory exists: {path}");
                    }
                    let mut partial = String::new();
                    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
                    for (at, part) in parts.iter().enumerate() {
                        partial.push('/');
                        partial.push_str(part);
                        if self.files.contains_key(&partial) {
                            bail!("not a directory: {partial}");
                        }
                        if at + 1 < parts.len() && !parents && !self.directories.contains(&partial)
                        {
                            bail!("parent does not exist: {partial}");
                        }
                        if self.directories.len() >= 128 && !self.directories.contains(&partial) {
                            bail!("virtual directory limit is 128");
                        }
                        self.directories.insert(partial.clone());
                    }
                }
                Vec::new()
            }
            "echo" => {
                let (newline, args) = if args.first().is_some_and(|arg| arg == "-n") {
                    (false, &args[1..])
                } else {
                    (true, args)
                };
                let mut out = args.join(" ").into_bytes();
                if newline {
                    out.push(b'\n');
                }
                out
            }
            "printf" => printf(args)?,
            "cat" => {
                let args = args.strip_prefix(&["--".into()]).unwrap_or(args);
                if args.is_empty() {
                    input.to_vec()
                } else {
                    let mut out = Vec::new();
                    for path in args {
                        let bytes = if path == "-" {
                            input.to_vec()
                        } else {
                            self.read(path, cwd)?
                        };
                        append(&mut out, &bytes)?;
                    }
                    out
                }
            }
            "ls" => {
                let mut path = ".";
                let mut show_hidden = false;
                let mut options = true;
                let mut paths = 0;
                for arg in args {
                    match arg.as_str() {
                        "--" if options => options = false,
                        "-a" | "-A" if options => show_hidden = true,
                        "-1" if options => {}
                        flag if options && flag.starts_with('-') => {
                            bail!("unsupported browser ls flag {flag:?}; use -1 or -a")
                        }
                        value => {
                            path = value;
                            paths += 1;
                        }
                    }
                }
                if paths > 1 {
                    bail!("browser ls accepts one directory");
                }
                let full = absolute(path, cwd)?;
                if self.files.contains_key(&full) {
                    format!("{path}\n").into_bytes()
                } else {
                    if !self.directories.contains(&full) {
                        bail!("directory does not exist: {full}");
                    }
                    let prefix = if full == "/" {
                        "/".into()
                    } else {
                        format!("{full}/")
                    };
                    let names: BTreeSet<_> = self
                        .files
                        .keys()
                        .chain(self.directories.iter())
                        .filter_map(|path| {
                            let relative = path.strip_prefix(&prefix)?;
                            (!relative.is_empty()
                                && !relative.contains('/')
                                && (show_hidden || !relative.starts_with('.')))
                            .then_some(relative)
                        })
                        .collect();
                    lines(names.into_iter())
                }
            }
            "sort" => {
                let mut reverse = false;
                let mut unique = false;
                for arg in args {
                    match arg.as_str() {
                        "-r" => reverse = true,
                        "-u" => unique = true,
                        "--" => {}
                        _ => bail!(
                            "browser sort supports -r and -u; bytewise UTF-8 order, no locale"
                        ),
                    }
                }
                let mut values: Vec<_> = text()?.split_terminator('\n').collect();
                values.sort_unstable();
                if unique {
                    values.dedup();
                }
                if reverse {
                    values.reverse();
                }
                lines(values)
            }
            "uniq" => {
                let count = args == ["-c"];
                if !args.is_empty() && !count {
                    bail!("browser uniq supports only -c");
                }
                let mut out = String::new();
                let mut values = text()?.split_terminator('\n').peekable();
                while let Some(line) = values.next() {
                    let mut n = 1;
                    while values.peek() == Some(&line) {
                        values.next();
                        n += 1;
                    }
                    if count {
                        out.push_str(&format!("{n:7} {line}\n"));
                    } else {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                out.into_bytes()
            }
            "wc" => {
                let counts = [
                    input.iter().filter(|byte| **byte == b'\n').count(),
                    text()?.split_whitespace().count(),
                    input.len(),
                ];
                match args {
                    [] => format!("{} {} {}\n", counts[0], counts[1], counts[2]).into_bytes(),
                    [flag] => {
                        let count = match flag.as_str() {
                            "-l" => counts[0],
                            "-w" => counts[1],
                            "-c" => counts[2],
                            _ => bail!("browser wc supports one of -l, -w, -c"),
                        };
                        format!("{count}\n").into_bytes()
                    }
                    _ => bail!("browser wc supports one of -l, -w, -c"),
                }
            }
            "head" => {
                let n = match args {
                    [] => 10,
                    [flag, n] if flag == "-n" => n
                        .parse::<usize>()
                        .context("browser head needs a nonnegative line count")?,
                    _ => bail!("browser head supports -n COUNT on pipeline input"),
                };
                let end = input
                    .iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'\n')
                    .nth(n.saturating_sub(1))
                    .map(|(at, _)| at + 1)
                    .unwrap_or(input.len());
                input[..if n == 0 { 0 } else { end }].to_vec()
            }
            "grep" => {
                let mut insensitive = false;
                let mut invert = false;
                let mut number = false;
                let mut options = true;
                let mut pattern = None;
                for arg in args {
                    match arg.as_str() {
                        "--" if options => options = false,
                        "-F" if options => {}
                        "-i" if options => insensitive = true,
                        "-v" if options => invert = true,
                        "-n" if options => number = true,
                        flag if options && flag.starts_with('-') => {
                            bail!("browser grep is literal-only (-F); supported flags: -F -i -v -n")
                        }
                        value => {
                            if pattern.replace(value).is_some() {
                                bail!("browser grep accepts one pattern and pipeline input");
                            }
                        }
                    }
                }
                let pattern = pattern.context("grep requires a literal pattern")?;
                // Never silently interpret a regex as a literal: require -F explicitly.
                if !args
                    .iter()
                    .take_while(|arg| arg.as_str() != "--")
                    .any(|arg| arg == "-F")
                {
                    bail!("browser grep requires -F (literal patterns); use native execute for regular expressions");
                }
                let pattern = if insensitive {
                    pattern.to_lowercase()
                } else {
                    pattern.into()
                };
                let mut out = String::new();
                let mut matched = false;
                for (at, line) in text()?.split_terminator('\n').enumerate() {
                    let contains = if insensitive {
                        line.to_lowercase().contains(&pattern)
                    } else {
                        line.contains(&pattern)
                    };
                    if contains != invert {
                        matched = true;
                        if number {
                            out.push_str(&format!("{}:", at + 1));
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                status = i64::from(!matched);
                out.into_bytes()
            }
            _ => unreachable!(),
        };
        if output.len() > LIMIT {
            bail!("output exceeds 1 MiB browser limit");
        }
        Ok((output, status))
    }
}

fn no_args(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("this browser applet does not accept arguments");
    }
    Ok(())
}
fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    if bytes.len() > LIMIT.saturating_sub(out.len()) {
        bail!("output exceeds 1 MiB browser limit");
    }
    out.extend_from_slice(bytes);
    Ok(())
}
fn lines<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out
}

fn printf(args: &[String]) -> Result<Vec<u8>> {
    let format = args.first().context("printf needs a format")?;
    let mut values = args[1..].iter();
    let mut out = Vec::new();
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '%' => match chars.next() {
                Some('%') => out.push(b'%'),
                Some('s') => append(
                    &mut out,
                    values.next().map_or("", String::as_str).as_bytes(),
                )?,
                Some('d') => {
                    let value = values
                        .next()
                        .map_or("0", String::as_str)
                        .parse::<i64>()
                        .context("printf %d requires an integer")?;
                    append(&mut out, value.to_string().as_bytes())?;
                }
                _ => bail!("browser printf supports %s, %d and %%"),
            },
            '\\' => match chars.next() {
                Some('n') => out.push(b'\n'),
                Some('t') => out.push(b'\t'),
                Some('r') => out.push(b'\r'),
                Some('\\') => out.push(b'\\'),
                _ => bail!("unsupported browser printf escape"),
            },
            ch => {
                let mut buffer = [0; 4];
                append(&mut out, ch.encode_utf8(&mut buffer).as_bytes())?;
            }
        }
    }
    if values.next().is_some() {
        bail!("browser printf requires one format pass; unused arguments");
    }
    Ok(out)
}
