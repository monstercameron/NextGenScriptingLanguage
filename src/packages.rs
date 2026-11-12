//! Debian package names, argument vectors and output parsers for the apt and dpkg applets (#334).
//! The Arch pacman layer (#328) lives in the [`arch`] submodule at the end of this file.
//!
//! Nothing here starts a process. Builders return an [`Invocation`]: a program, an argument
//! vector (never a shell string) and the environment to run it with. [`PackageSpec`] is the
//! only way to make a package name, so every name that reaches an argument has passed the
//! Debian policy checks and cannot be read as an option.

use std::fmt;

/// Environment for every apt-get and dpkg-query call.
pub const NON_INTERACTIVE_ENV: &[(&str, &str)] =
    &[("DEBIAN_FRONTEND", "noninteractive"), ("LC_ALL", "C")];

/// dpkg-query output format: package, architecture, status and version, tab separated.
pub const DPKG_QUERY_FORMAT: &str = r"${Package}\t${Architecture}\t${Status}\t${Version}\n";

const PLAN_VERBS: &[&str] = &["Inst", "Remv", "Purg", "Conf"];
const INFORMATIONAL: &[&str] = &[
    "Reading ",
    "Building ",
    "Note, selecting ",
    "The following ",
    "Suggested packages:",
    "Recommended packages:",
    "Use '",
    "W: ",
];

/// Why a package name, query, plan or ensure decision was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackagesError {
    InvalidSpec {
        input: String,
        reason: &'static str,
    },
    NoNames,
    VersionNotAllowed {
        input: String,
    },
    AmbiguousPackage {
        name: String,
    },
    MalformedQueryLine {
        line: String,
    },
    UnexpectedQueryLine {
        line: String,
    },
    MalformedPlanLine {
        line: String,
    },
    UnrecognisedPlanLine {
        line: String,
    },
    PlanInconsistent {
        detail: String,
    },
    AptError {
        failure: AptFailure,
        message: String,
    },
    Held {
        name: String,
    },
    NeedsConfigure {
        name: String,
        status: String,
    },
    RemovalRefused {
        names: Vec<String>,
    },
    InvalidPath {
        input: String,
        reason: &'static str,
    },
    MalformedInstalledLine {
        line: String,
    },
    MalformedOrphanLine {
        line: String,
    },
    MalformedOwnerLine {
        line: String,
    },
    MalformedUpgradeLine {
        line: String,
    },
}

/// Typed meaning of an `E:` line from apt-get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AptFailure {
    Locked,
    UnknownPackage,
    Interrupted,
    Other,
}

impl fmt::Display for PackagesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec { input, reason } => {
                write!(f, "invalid package {reason} in {input:?}")
            }
            Self::NoNames => write!(f, "no package names given"),
            Self::VersionNotAllowed { input } => {
                write!(f, "removal takes names without versions, got {input:?}")
            }
            Self::AmbiguousPackage { name } => write!(
                f,
                "{name} is installed for more than one architecture; qualify it as name:arch"
            ),
            Self::MalformedQueryLine { line } => write!(f, "unreadable dpkg-query line {line:?}"),
            Self::UnexpectedQueryLine { line } => {
                write!(
                    f,
                    "dpkg-query reported a package that was not asked for: {line:?}"
                )
            }
            Self::MalformedPlanLine { line } => write!(f, "unreadable plan line {line:?}"),
            Self::UnrecognisedPlanLine { line } => {
                write!(
                    f,
                    "unrecognised line in the simulated plan, plan not trusted: {line:?}"
                )
            }
            Self::PlanInconsistent { detail } => {
                write!(f, "simulated plan is incomplete or inconsistent: {detail}")
            }
            Self::AptError { failure, message } => {
                write!(f, "apt-get failed ({failure:?}): {message}")
            }
            Self::Held { name } => {
                write!(
                    f,
                    "{name} is held at another version; refusing to change it"
                )
            }
            Self::NeedsConfigure { name, status } => write!(
                f,
                "{name} is in state {status:?}; run dpkg --configure -a before ensuring it"
            ),
            Self::RemovalRefused { names } => write!(
                f,
                "the plan removes {}; allow removals explicitly to proceed",
                names.join(", ")
            ),
            Self::InvalidPath { input, reason } => {
                write!(f, "invalid path {reason} in {input:?}")
            }
            Self::MalformedInstalledLine { line } => {
                write!(f, "unreadable pacman -Q line {line:?}")
            }
            Self::MalformedOrphanLine { line } => {
                write!(f, "unreadable orphan package name {line:?}")
            }
            Self::MalformedOwnerLine { line } => {
                write!(f, "unreadable pacman -Qo line {line:?}")
            }
            Self::MalformedUpgradeLine { line } => {
                write!(f, "unreadable pacman -Qu line {line:?}")
            }
        }
    }
}

impl std::error::Error for PackagesError {}

/// A validated package reference: `name`, optional `:arch` and optional `=version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSpec {
    name: String,
    arch: Option<String>,
    version: Option<String>,
}

impl PackageSpec {
    /// Parses `name[:arch][=version]`. Anything outside the Debian character sets is refused.
    pub fn parse(input: &str) -> Result<Self, PackagesError> {
        let invalid = |reason: &'static str| PackagesError::InvalidSpec {
            input: input.to_string(),
            reason,
        };
        let (left, version) = match input.split_once('=') {
            Some((left, version)) => (left, Some(version)),
            None => (input, None),
        };
        let (name, arch) = match left.split_once(':') {
            Some((name, arch)) => (name, Some(arch)),
            None => (left, None),
        };
        if !is_package_name(name) {
            return Err(invalid("name"));
        }
        if arch.is_some_and(|arch| !is_arch(arch)) {
            return Err(invalid("architecture"));
        }
        if version.is_some_and(|version| !is_version(version)) {
            return Err(invalid("version"));
        }
        Ok(Self {
            name: name.to_string(),
            arch: arch.map(str::to_string),
            version: version.map(str::to_string),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arch(&self) -> Option<&str> {
        self.arch.as_deref()
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// The argument dpkg-query takes: `name` or `name:arch`.
    fn query_arg(&self) -> String {
        match &self.arch {
            Some(arch) => format!("{}:{arch}", self.name),
            None => self.name.clone(),
        }
    }

    /// The argument apt-get takes: `name[:arch][=version]`.
    fn install_arg(&self) -> String {
        let mut arg = self.query_arg();
        if let Some(version) = &self.version {
            arg.push('=');
            arg.push_str(version);
        }
        arg
    }
}

/// Debian policy: lowercase letters, digits, `+`, `-` and `.`; at least two characters;
/// starts with a letter or digit.
fn is_package_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(*c, b'+' | b'-' | b'.')
        })
}

/// Architecture names: lowercase letters, digits and `-`, starting with a letter or digit.
fn is_arch(s: &str) -> bool {
    let b = s.as_bytes();
    b.first()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Debian version: letters, digits and `. + ~ : -`, an optional numeric epoch before one
/// colon, and an upstream part that starts with a digit.
fn is_version(s: &str) -> bool {
    let rest = match s.split_once(':') {
        Some((epoch, rest)) if !epoch.is_empty() && epoch.bytes().all(|c| c.is_ascii_digit()) => {
            rest
        }
        Some(_) => return false,
        None => s,
    };
    rest.as_bytes().first().is_some_and(|c| c.is_ascii_digit())
        && !rest.contains(':')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'~' | b'-' | b':'))
}

/// A plan name may carry an architecture qualifier, as apt prints it for multiarch packages.
fn valid_plan_name(name: &str) -> bool {
    match name.split_once(':') {
        Some((name, arch)) => is_package_name(name) && is_arch(arch),
        None => is_package_name(name),
    }
}

/// A command ready to run: the program, its argument vector and its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: &'static [(&'static str, &'static str)],
}

/// What a conffile prompt resolves to. There is no default: the caller must choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conffile {
    KeepOld,
    KeepNew,
}

impl Conffile {
    fn apt_option(self) -> &'static str {
        match self {
            Self::KeepOld => "Dpkg::Options::=--force-confold",
            Self::KeepNew => "Dpkg::Options::=--force-confnew",
        }
    }
}

fn require_names(names: &[PackageSpec]) -> Result<(), PackagesError> {
    if names.is_empty() {
        Err(PackagesError::NoNames)
    } else {
        Ok(())
    }
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

fn invocation(program: &'static str, args: Vec<String>) -> Invocation {
    Invocation {
        program,
        args,
        env: NON_INTERACTIVE_ENV,
    }
}

fn lock_args(lock_timeout_secs: u32) -> Vec<String> {
    vec![
        "-o".to_string(),
        format!("DPkg::Lock::Timeout={lock_timeout_secs}"),
    ]
}

/// `dpkg-query -W -f=<format> -- names...`: the installed state of each name.
pub fn query_installed(names: &[PackageSpec]) -> Result<Invocation, PackagesError> {
    require_names(names)?;
    let mut args = vec![
        "-W".to_string(),
        format!("-f={DPKG_QUERY_FORMAT}"),
        "--".to_string(),
    ];
    args.extend(names.iter().map(PackageSpec::query_arg));
    Ok(invocation("dpkg-query", args))
}

/// `apt-get install --simulate ... -- names...`. Simulation takes no locks, so no lock timeout.
pub fn simulate_install(names: &[PackageSpec]) -> Result<Invocation, PackagesError> {
    require_names(names)?;
    let mut args = owned(&[
        "install",
        "--simulate",
        "--no-install-recommends",
        "-y",
        "--",
    ]);
    args.extend(names.iter().map(PackageSpec::install_arg));
    Ok(invocation("apt-get", args))
}

/// `apt-get -o DPkg::Lock::Timeout=N -o <conffile> install ... -- names...`.
pub fn install(
    names: &[PackageSpec],
    conffile: Conffile,
    lock_timeout_secs: u32,
) -> Result<Invocation, PackagesError> {
    require_names(names)?;
    let mut args = lock_args(lock_timeout_secs);
    args.push("-o".to_string());
    args.push(conffile.apt_option().to_string());
    args.extend(owned(&["install", "--no-install-recommends", "-y", "--"]));
    args.extend(names.iter().map(PackageSpec::install_arg));
    Ok(invocation("apt-get", args))
}

/// `apt-get -o DPkg::Lock::Timeout=N remove -y -- names...`. Removal takes no versions.
pub fn remove(names: &[PackageSpec], lock_timeout_secs: u32) -> Result<Invocation, PackagesError> {
    require_names(names)?;
    if let Some(spec) = names.iter().find(|spec| spec.version.is_some()) {
        return Err(PackagesError::VersionNotAllowed {
            input: spec.install_arg(),
        });
    }
    let mut args = lock_args(lock_timeout_secs);
    args.extend(owned(&["remove", "-y", "--"]));
    args.extend(names.iter().map(PackageSpec::query_arg));
    Ok(invocation("apt-get", args))
}

/// Installed state of one package as dpkg reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageState {
    /// `install ok installed`; `held` is true for `hold ok installed`.
    Installed { version: String, held: bool },
    /// `deinstall ok config-files`: removed, configuration files remain.
    ConfigFiles { version: String },
    /// Known to dpkg as `not-installed`, or not printed at all.
    NotInstalled,
    /// Any other status (half-installed, unpacked, reinstreq, ...): dpkg must be repaired.
    Partial { status: String },
}

/// One requested name and the state dpkg reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageStatus {
    pub spec: PackageSpec,
    pub state: PackageState,
}

#[derive(Debug, Clone)]
struct QueryRow {
    name: String,
    arch: String,
    status: String,
    version: String,
}

fn parse_query_row(line: &str) -> Result<QueryRow, PackagesError> {
    let fields: Vec<&str> = line.split('\t').collect();
    match fields.as_slice() {
        [name, arch, status, version]
            if is_package_name(name) && is_arch(arch) && status.split_whitespace().count() == 3 =>
        {
            Ok(QueryRow {
                name: name.to_string(),
                arch: arch.to_string(),
                status: status.to_string(),
                version: version.to_string(),
            })
        }
        _ => Err(PackagesError::MalformedQueryLine {
            line: line.to_string(),
        }),
    }
}

fn state_of(row: &QueryRow) -> PackageState {
    let words: Vec<&str> = row.status.split_whitespace().collect();
    match words.as_slice() {
        [want, "ok", "installed"] if matches!(*want, "install" | "hold") => {
            PackageState::Installed {
                version: row.version.clone(),
                held: *want == "hold",
            }
        }
        [_, _, "config-files"] => PackageState::ConfigFiles {
            version: row.version.clone(),
        },
        [_, _, "not-installed"] => PackageState::NotInstalled,
        _ => PackageState::Partial {
            status: row.status.clone(),
        },
    }
}

/// Parses `dpkg-query -W -f=DPKG_QUERY_FORMAT` output into one status per requested name,
/// in request order. A name with no matching line is not installed. A line for a package
/// that was not asked for is an error, and so is a name that matches several architectures
/// when it was not qualified. A nonzero exit from dpkg-query is normal when a name is
/// unknown; the caller should parse stdout regardless.
pub fn parse_query_output(
    requested: &[PackageSpec],
    output: &str,
) -> Result<Vec<PackageStatus>, PackagesError> {
    let mut rows: Vec<Vec<QueryRow>> = requested.iter().map(|_| Vec::new()).collect();
    for line in output.lines().filter(|line| !line.is_empty()) {
        let row = parse_query_row(line)?;
        let mut matched = false;
        for (spec, found) in requested.iter().zip(rows.iter_mut()) {
            if spec.name == row.name && spec.arch().is_none_or(|arch| arch == row.arch.as_str()) {
                found.push(row.clone());
                matched = true;
            }
        }
        if !matched {
            return Err(PackagesError::UnexpectedQueryLine {
                line: line.to_string(),
            });
        }
    }
    requested
        .iter()
        .zip(rows)
        .map(|(spec, found)| -> Result<PackageStatus, PackagesError> {
            let state = match found.as_slice() {
                [] => PackageState::NotInstalled,
                [row] => state_of(row),
                _ => {
                    return Err(PackagesError::AmbiguousPackage {
                        name: spec.name.clone(),
                    })
                }
            };
            Ok(PackageStatus {
                spec: spec.clone(),
                state,
            })
        })
        .collect()
}

/// What `ensure` must do, decided from the installed state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureDecision {
    pub to_install: Vec<PackageSpec>,
    pub changed: bool,
}

/// Decides which requested names need `apt-get install`. A name already installed (at the
/// requested version, when one is given) needs nothing, so a second run changes nothing.
/// A held package installed at another version is refused rather than changed, and a
/// half-configured package is refused until `dpkg --configure -a` has run.
pub fn decide_ensure(statuses: &[PackageStatus]) -> Result<EnsureDecision, PackagesError> {
    let mut to_install: Vec<PackageSpec> = Vec::new();
    for status in statuses {
        let spec = &status.spec;
        let needs_install = match &status.state {
            PackageState::Partial { status } => {
                return Err(PackagesError::NeedsConfigure {
                    name: spec.name.clone(),
                    status: status.clone(),
                })
            }
            PackageState::NotInstalled | PackageState::ConfigFiles { .. } => true,
            PackageState::Installed { version, held } => match spec.version() {
                None => false,
                Some(wanted) if wanted == version.as_str() => false,
                Some(_) if *held => {
                    return Err(PackagesError::Held {
                        name: spec.name.clone(),
                    })
                }
                Some(_) => true,
            },
        };
        if needs_install && !to_install.contains(spec) {
            to_install.push(spec.clone());
        }
    }
    let changed = !to_install.is_empty();
    Ok(EnsureDecision {
        to_install,
        changed,
    })
}

/// The action apt-get plans for one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    /// `Inst` with no old version.
    Install,
    /// `Inst` with an old version. The direction (upgrade or downgrade) is not inferred.
    Upgrade,
    /// `Remv` or `Purg`.
    Remove,
    /// `Conf`.
    Configure,
}

/// One line of `apt-get --simulate` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLine {
    pub name: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub action: PlanAction,
}

/// A complete simulated plan. An empty plan means apt-get would change nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub lines: Vec<PlanLine>,
}

/// Parses `apt-get --simulate` output into a plan.
///
/// Lines are either plan lines (`Inst`, `Remv`, `Purg`, `Conf`), the summary line, a known
/// informational line, or an error. An `E:` line fails the parse with a typed reason. The
/// summary line is required, and its counts must match the plan lines, so a partial plan is
/// never reported as complete.
pub fn parse_simulate(output: &str) -> Result<Plan, PackagesError> {
    let mut lines = Vec::new();
    let mut summary: Option<(usize, usize, usize)> = None;
    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(message) = trimmed.strip_prefix("E: ") {
            return Err(PackagesError::AptError {
                failure: classify_apt_error(message),
                message: message.to_string(),
            });
        }
        let verb = trimmed.split(' ').next().unwrap_or("");
        if PLAN_VERBS.contains(&verb) {
            let plan_line =
                parse_plan_fields(trimmed).ok_or_else(|| PackagesError::MalformedPlanLine {
                    line: line.to_string(),
                })?;
            lines.push(plan_line);
        } else if is_summary(trimmed) {
            if summary.is_some() {
                return Err(PackagesError::PlanInconsistent {
                    detail: "more than one summary line".to_string(),
                });
            }
            summary =
                Some(
                    parse_summary(trimmed).ok_or_else(|| PackagesError::MalformedPlanLine {
                        line: line.to_string(),
                    })?,
                );
        } else if !(trimmed.is_empty()
            // Known informational text, or an indented name list under a header.
            || line.starts_with(' ')
            || INFORMATIONAL.iter().any(|prefix| trimmed.starts_with(*prefix)))
        {
            return Err(PackagesError::UnrecognisedPlanLine {
                line: line.to_string(),
            });
        }
    }
    let (upgraded, newly, removed) = summary.ok_or_else(|| PackagesError::PlanInconsistent {
        detail: "no summary line, so the plan cannot be checked for completeness".to_string(),
    })?;
    let count = |action: PlanAction| lines.iter().filter(|line| line.action == action).count();
    let (u, n, r) = (
        count(PlanAction::Upgrade),
        count(PlanAction::Install),
        count(PlanAction::Remove),
    );
    if (u, n, r) != (upgraded, newly, removed) {
        return Err(PackagesError::PlanInconsistent {
            detail: format!(
                "summary says {upgraded} upgraded, {newly} new and {removed} removed; \
                 the plan lists {u}, {n} and {r}"
            ),
        });
    }
    Ok(Plan { lines })
}

fn parse_plan_fields(line: &str) -> Option<PlanLine> {
    let (verb, body) = line.split_once(' ')?;
    let body = body.trim_start();
    let (name, rest) = body.split_once(' ').unwrap_or((body, ""));
    if !valid_plan_name(name) {
        return None;
    }
    let (from, rest) = match take_bracketed(rest) {
        Some((version, rest)) => (Some(version), rest),
        None => (None, rest),
    };
    let action = match verb {
        "Inst" if from.is_some() => PlanAction::Upgrade,
        "Inst" => PlanAction::Install,
        "Conf" if from.is_none() => PlanAction::Configure,
        "Remv" | "Purg" => PlanAction::Remove,
        _ => return None,
    };
    let to = if action == PlanAction::Remove {
        None
    } else {
        Some(take_paren_version(rest)?)
    };
    Some(PlanLine {
        name: name.to_string(),
        from,
        to,
        action,
    })
}

/// `[version]` at the start of `s`, returning the version and the rest of the line.
fn take_bracketed(s: &str) -> Option<(String, &str)> {
    let inner = s.trim_start().strip_prefix('[')?;
    let (version, rest) = inner.split_once(']')?;
    if is_version(version) {
        Some((version.to_string(), rest))
    } else {
        None
    }
}

/// `(version repository ...)` at the start of `s`, returning the version.
fn take_paren_version(s: &str) -> Option<String> {
    let inner = s.trim_start().strip_prefix('(')?;
    let end = inner.find(')')?;
    let first = inner[..end].split_whitespace().next()?;
    if is_version(first) {
        Some(first.to_string())
    } else {
        None
    }
}

fn is_summary(line: &str) -> bool {
    line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(" upgraded,")
}

/// `N upgraded, N newly installed, N to remove and N not upgraded.`
fn parse_summary(line: &str) -> Option<(usize, usize, usize)> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        [upgraded, "upgraded,", newly, "newly", "installed,", removed, "to", "remove", "and", _, "not", "upgraded."] => {
            Some((
                upgraded.parse::<usize>().ok()?,
                newly.parse::<usize>().ok()?,
                removed.parse::<usize>().ok()?,
            ))
        }
        _ => None,
    }
}

fn classify_apt_error(message: &str) -> AptFailure {
    if message.contains("Unable to locate package") {
        AptFailure::UnknownPackage
    } else if message.contains("Could not get lock")
        || message.contains("frontend lock")
        || message.contains("/lock")
    {
        AptFailure::Locked
    } else if message.contains("dpkg was interrupted") || message.contains("--configure -a") {
        AptFailure::Interrupted
    } else {
        AptFailure::Other
    }
}

/// Refuses a plan that removes packages unless removals were explicitly allowed. Returns the
/// names it refuses over, so the caller can show them.
pub fn approve_plan(plan: &Plan, allow_removals: bool) -> Result<(), PackagesError> {
    let names: Vec<String> = plan
        .lines
        .iter()
        .filter(|line| line.action == PlanAction::Remove)
        .map(|line| line.name.clone())
        .collect();
    if names.is_empty() || allow_removals {
        Ok(())
    } else {
        Err(PackagesError::RemovalRefused { names })
    }
}

/// Arch Linux pacman layer (#328): names, argument vectors and output parsers.
///
/// It mirrors the Debian layer above. [`PackageSpec`] is the only way to make a name, builders
/// return an [`Invocation`] (program, argument vector and environment), and parsers return typed
/// values or an error. Nothing here starts a process. Install and AUR install already ship as
/// manifests (`package.install`, `package.install_aur`), and `package.query` wraps `pacman -Qi`.
pub mod arch {
    use super::{owned, Invocation, PackagesError};

    /// Environment for every pacman call here. The parsers expect C-locale, English output.
    pub const ARCH_ENV: &[(&str, &str)] = &[("LC_ALL", "C")];

    /// A validated Arch package name: lowercase letters and digits, and `@ . _ + -`. A name
    /// cannot start with `-` or `.`, so it can never be read as an option.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PackageSpec {
        name: String,
    }

    impl PackageSpec {
        /// Parses a bare package name. Versions, `:arch` qualifiers and `=` are refused.
        pub fn parse(input: &str) -> Result<Self, PackagesError> {
            if is_package_name(input) {
                Ok(Self {
                    name: input.to_string(),
                })
            } else {
                Err(PackagesError::InvalidSpec {
                    input: input.to_string(),
                    reason: "name",
                })
            }
        }

        pub fn name(&self) -> &str {
            &self.name
        }
    }

    /// Arch packaging rule: `[a-z0-9@._+-]+`, not starting with `-` or `.`.
    fn is_package_name(s: &str) -> bool {
        let b = s.as_bytes();
        b.first().is_some_and(|c| !matches!(*c, b'-' | b'.'))
            && b.iter().all(|c| {
                c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(*c, b'@' | b'.' | b'_' | b'+' | b'-')
            })
    }

    /// pacman versions are non-empty runs of printable ASCII with no space.
    fn is_version(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|c| c.is_ascii_graphic())
    }

    fn pacman(args: Vec<String>) -> Invocation {
        Invocation {
            program: "pacman",
            args,
            env: ARCH_ENV,
        }
    }

    fn require_names(names: &[PackageSpec]) -> Result<(), PackagesError> {
        if names.is_empty() {
            Err(PackagesError::NoNames)
        } else {
            Ok(())
        }
    }

    /// How `pacman -R` handles dependencies and configuration files. Plain is the default and
    /// removes only the named packages. The other modes are opt-in and must be chosen by name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum RemoveMode {
        /// `-R`: only the named packages. pacman refuses if another package still needs them.
        #[default]
        Plain,
        /// `-Rs`: also removes dependencies that nothing else needs (recursive).
        Cascade,
        /// `-Rns`: as `Cascade`, and does not back up modified configuration files.
        CascadeNoSave,
        /// `-Rdd`: skips every dependency check. This can leave the system with broken packages.
        NoDependencyChecks,
    }

    impl RemoveMode {
        fn flags(self) -> &'static str {
            match self {
                Self::Plain => "-R",
                Self::Cascade => "-Rs",
                Self::CascadeNoSave => "-Rns",
                Self::NoDependencyChecks => "-Rdd",
            }
        }
    }

    /// `pacman -R<mode> --noconfirm -- names...`. Names only: a version is not a removal target.
    pub fn remove(names: &[PackageSpec], mode: RemoveMode) -> Result<Invocation, PackagesError> {
        require_names(names)?;
        let mut args = owned(&[mode.flags(), "--noconfirm", "--"]);
        args.extend(names.iter().map(|spec| spec.name.clone()));
        Ok(pacman(args))
    }

    /// Which installed packages `pacman -Q` reports.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ListScope {
        /// `-Q`: every installed package.
        All,
        /// `-Qe`: packages installed explicitly.
        Explicit,
        /// `-Qd`: packages installed only as dependencies.
        Dependency,
    }

    /// `pacman -Q`, `-Qe` or `-Qd`: `name version` for each installed package, one per line.
    pub fn list_installed(scope: ListScope) -> Invocation {
        let flag = match scope {
            ListScope::All => "-Q",
            ListScope::Explicit => "-Qe",
            ListScope::Dependency => "-Qd",
        };
        pacman(owned(&[flag]))
    }

    /// `pacman -Qo -- path`: the package that owns a file. The path must be absolute, and must
    /// contain no NUL and no line break, because the answer is a single line.
    pub fn owner(path: &str) -> Result<Invocation, PackagesError> {
        let refuse = |reason: &'static str| PackagesError::InvalidPath {
            input: path.to_string(),
            reason,
        };
        if !path.starts_with('/') {
            return Err(refuse("must be absolute"));
        }
        if path.contains('\0') {
            return Err(refuse("contains NUL"));
        }
        if path.contains(['\n', '\r']) {
            return Err(refuse("contains a line break"));
        }
        Ok(pacman(owned(&["-Qo", "--", path])))
    }

    /// `pacman -Qtdq`: installed dependencies that nothing requires, one name per line.
    pub fn orphans() -> Invocation {
        pacman(owned(&["-Qtdq"]))
    }

    /// `pacman -Qu`: pending upgrades, read from the last synced package database. It reads the
    /// local database only and does not sync (no `-Sy`), so its answer is as current as the
    /// last sync and no newer.
    pub fn upgrade_plan() -> Invocation {
        pacman(owned(&["-Qu"]))
    }

    /// One installed package as `pacman -Q` reports it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InstalledPackage {
        pub name: String,
        pub version: String,
    }

    /// Parses `pacman -Q`, `-Qe` or `-Qd` output. Empty output means no packages.
    pub fn parse_installed(output: &str) -> Result<Vec<InstalledPackage>, PackagesError> {
        output
            .lines()
            .filter(|line| !line.is_empty())
            .map(
                |line| match line.split(' ').collect::<Vec<&str>>().as_slice() {
                    [name, version] if is_package_name(name) && is_version(version) => {
                        Ok(InstalledPackage {
                            name: name.to_string(),
                            version: version.to_string(),
                        })
                    }
                    _ => Err(PackagesError::MalformedInstalledLine {
                        line: line.to_string(),
                    }),
                },
            )
            .collect()
    }

    /// Parses `pacman -Qtdq` output: one name per line. Empty output means no orphans.
    pub fn parse_orphans(output: &str) -> Result<Vec<PackageSpec>, PackagesError> {
        output
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                PackageSpec::parse(line).map_err(|_| PackagesError::MalformedOrphanLine {
                    line: line.to_string(),
                })
            })
            .collect()
    }

    /// The package that owns a file, and the version installed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Owner {
        pub path: String,
        pub name: String,
        pub version: String,
    }

    /// Whether `pacman -Qo` found an owner. An unowned path is a normal answer, not an error.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Ownership {
        Owned(Owner),
        NotOwned,
    }

    /// Parses `pacman -Qo` output: `<path> is owned by <name> <version>`. Empty output means
    /// no package owns the path (pacman writes that to stderr and exits nonzero, so the caller
    /// passes stdout regardless). More than one line is an error.
    pub fn parse_owner(output: &str) -> Result<Ownership, PackagesError> {
        let lines: Vec<&str> = output.lines().filter(|line| !line.is_empty()).collect();
        match lines.as_slice() {
            [] => Ok(Ownership::NotOwned),
            [line] => parse_owner_line(line).map(Ownership::Owned),
            [_, extra, ..] => Err(PackagesError::MalformedOwnerLine {
                line: extra.to_string(),
            }),
        }
    }

    fn parse_owner_line(line: &str) -> Result<Owner, PackagesError> {
        let malformed = || PackagesError::MalformedOwnerLine {
            line: line.to_string(),
        };
        // The name and version are the last two words, so they are peeled off the end. The
        // path comes first and may itself contain " is owned by ".
        let (head, version) = line.rsplit_once(' ').ok_or_else(malformed)?;
        let (head, name) = head.rsplit_once(' ').ok_or_else(malformed)?;
        let path = head.strip_suffix(" is owned by").ok_or_else(malformed)?;
        if !path.starts_with('/') || !is_package_name(name) || !is_version(version) {
            return Err(malformed());
        }
        Ok(Owner {
            path: path.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    /// One pending upgrade from `pacman -Qu`. `ignored` is set when IgnorePkg holds it back.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Upgrade {
        pub name: String,
        pub from: String,
        pub to: String,
        pub ignored: bool,
    }

    /// Parses `pacman -Qu` output: `name old -> new`, with ` [ignored]` appended when the
    /// package is held back. Empty output means nothing is pending.
    pub fn parse_upgrade_plan(output: &str) -> Result<Vec<Upgrade>, PackagesError> {
        output
            .lines()
            .filter(|line| !line.is_empty())
            .map(parse_upgrade_line)
            .collect()
    }

    fn parse_upgrade_line(line: &str) -> Result<Upgrade, PackagesError> {
        let malformed = || PackagesError::MalformedUpgradeLine {
            line: line.to_string(),
        };
        let (name, from, to, ignored) = match line.split(' ').collect::<Vec<&str>>().as_slice() {
            [name, from, "->", to] => (*name, *from, *to, false),
            [name, from, "->", to, "[ignored]"] => (*name, *from, *to, true),
            _ => return Err(malformed()),
        };
        if !is_package_name(name) || !is_version(from) || !is_version(to) {
            return Err(malformed());
        }
        Ok(Upgrade {
            name: name.to_string(),
            from: from.to_string(),
            to: to.to_string(),
            ignored,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn spec(input: &str) -> PackageSpec {
            PackageSpec::parse(input).expect("fixture package name is valid")
        }

        fn argv(inv: &Invocation) -> Vec<&str> {
            inv.args.iter().map(String::as_str).collect()
        }

        #[test]
        fn name_validator_accepts_arch_names() {
            for ok in [
                "curl",
                "g++",
                "python-3.12",
                "lib32-glibc",
                "linux-zen",
                "x.y+z_@",
                "a",
            ] {
                assert!(PackageSpec::parse(ok).is_ok(), "{ok} should be accepted");
            }
            assert_eq!(spec("gtk4").name(), "gtk4");
        }

        #[test]
        fn name_validator_refuses_hostile_names() {
            let hostile = [
                "",
                "-",
                "-y",
                "--noconfirm",
                "--",
                ".hidden",
                "..",
                "Curl",
                "curl x",
                "curl\tx",
                "curl\nx",
                "curl;id",
                "curl$(id)",
                "curl`id`",
                "curl|id",
                "curl&id",
                "curl>x",
                "../curl",
                "cu/rl",
                "curl\0",
                "curl\u{e9}",
                "curl:amd64",
                "curl=1.0",
            ];
            for bad in hostile {
                assert!(
                    PackageSpec::parse(bad).is_err(),
                    "{bad:?} should be refused"
                );
            }
        }

        #[test]
        fn environment_is_c_locale() {
            assert_eq!(ARCH_ENV, &[("LC_ALL", "C")]);
            assert_eq!(orphans().env, ARCH_ENV);
        }

        #[test]
        fn remove_default_is_plain_and_argv_is_exact() {
            assert_eq!(RemoveMode::default(), RemoveMode::Plain);
            let inv = remove(&[spec("curl"), spec("foo")], RemoveMode::Plain).expect("names");
            assert_eq!(inv.program, "pacman");
            assert_eq!(argv(&inv), vec!["-R", "--noconfirm", "--", "curl", "foo"]);
        }

        #[test]
        fn remove_recursive_modes_are_only_the_named_flags() {
            let names = [spec("curl")];
            let flags = |mode| {
                let inv = remove(&names, mode).expect("names");
                argv(&inv)[0].to_string()
            };
            assert_eq!(flags(RemoveMode::Cascade), "-Rs");
            assert_eq!(flags(RemoveMode::CascadeNoSave), "-Rns");
            assert_eq!(flags(RemoveMode::NoDependencyChecks), "-Rdd");
        }

        #[test]
        fn remove_refuses_empty_input() {
            assert_eq!(remove(&[], RemoveMode::Plain), Err(PackagesError::NoNames));
        }

        #[test]
        fn list_argv_is_exact_for_each_scope() {
            assert_eq!(argv(&list_installed(ListScope::All)), vec!["-Q"]);
            assert_eq!(argv(&list_installed(ListScope::Explicit)), vec!["-Qe"]);
            assert_eq!(argv(&list_installed(ListScope::Dependency)), vec!["-Qd"]);
        }

        #[test]
        fn owner_argv_is_exact_and_path_is_checked() {
            let inv = owner("/usr/bin/ls").expect("absolute path");
            assert_eq!(inv.program, "pacman");
            assert_eq!(argv(&inv), vec!["-Qo", "--", "/usr/bin/ls"]);
            for (bad, reason) in [
                ("usr/bin/ls", "must be absolute"),
                ("", "must be absolute"),
                ("/a\0b", "contains NUL"),
                ("/a\nb", "contains a line break"),
                ("/a\rb", "contains a line break"),
            ] {
                assert_eq!(
                    owner(bad).err(),
                    Some(PackagesError::InvalidPath {
                        input: bad.to_string(),
                        reason,
                    }),
                    "{bad:?}"
                );
            }
        }

        #[test]
        fn orphans_and_upgrade_argv_are_exact() {
            assert_eq!(argv(&orphans()), vec!["-Qtdq"]);
            assert_eq!(argv(&upgrade_plan()), vec!["-Qu"]);
        }

        #[test]
        fn parse_installed_normal_empty_and_malformed() {
            let parsed = parse_installed("curl 8.5.0-1\nlinux-zen 6.8.1-1\n").expect("fixture");
            assert_eq!(
                parsed,
                vec![
                    InstalledPackage {
                        name: "curl".to_string(),
                        version: "8.5.0-1".to_string()
                    },
                    InstalledPackage {
                        name: "linux-zen".to_string(),
                        version: "6.8.1-1".to_string()
                    },
                ]
            );
            assert_eq!(parse_installed(""), Ok(Vec::new()));
            for bad in [
                "curl\n",
                "curl 1.0 extra\n",
                "Curl 1.0\n",
                "-y 1.0\n",
                "curl  1.0\n",
            ] {
                assert!(
                    matches!(
                        parse_installed(bad),
                        Err(PackagesError::MalformedInstalledLine { .. })
                    ),
                    "{bad:?} should be refused"
                );
            }
        }

        #[test]
        fn parse_orphans_normal_empty_and_malformed() {
            assert_eq!(
                parse_orphans("libpng\nzlib\n"),
                Ok(vec![spec("libpng"), spec("zlib")])
            );
            assert_eq!(parse_orphans(""), Ok(Vec::new()));
            for bad in ["Libpng\n", "-y\n", "two words\n", "a/b\n"] {
                assert!(
                    matches!(
                        parse_orphans(bad),
                        Err(PackagesError::MalformedOrphanLine { .. })
                    ),
                    "{bad:?} should be refused"
                );
            }
        }

        #[test]
        fn parse_owner_normal_and_not_owned() {
            assert_eq!(
                parse_owner("/usr/bin/ls is owned by coreutils 9.4-1\n"),
                Ok(Ownership::Owned(Owner {
                    path: "/usr/bin/ls".to_string(),
                    name: "coreutils".to_string(),
                    version: "9.4-1".to_string(),
                }))
            );
            assert_eq!(parse_owner(""), Ok(Ownership::NotOwned));
        }

        #[test]
        fn parse_owner_takes_name_and_version_from_the_end() {
            // A path that contains " is owned by " must not confuse the split.
            let line = "/opt/a is owned by b/c is owned by foo 1.2-3\n";
            assert_eq!(
                parse_owner(line),
                Ok(Ownership::Owned(Owner {
                    path: "/opt/a is owned by b/c".to_string(),
                    name: "foo".to_string(),
                    version: "1.2-3".to_string(),
                }))
            );
        }

        #[test]
        fn parse_owner_refuses_malformed_and_extra_lines() {
            for bad in [
                "/usr/bin/ls is owned by coreutils\n",
                "usr/bin/ls is owned by coreutils 9.4-1\n",
                "garbage\n",
                "/a is owned by  9.4-1\n",
                "/a is owned by Bad 1.0\n",
                "/a is owned by foo 1.0\n/b is owned by bar 2.0\n",
            ] {
                assert!(
                    matches!(
                        parse_owner(bad),
                        Err(PackagesError::MalformedOwnerLine { .. })
                    ),
                    "{bad:?} should be refused"
                );
            }
        }

        #[test]
        fn parse_upgrade_plan_normal_ignored_empty_and_malformed() {
            let parsed = parse_upgrade_plan(
                "linux-zen 6.8.1-1 -> 6.8.2-1\nopenssh 9.6p1-1 -> 9.7p1-1 [ignored]\n",
            )
            .expect("fixture");
            assert_eq!(
                parsed,
                vec![
                    Upgrade {
                        name: "linux-zen".to_string(),
                        from: "6.8.1-1".to_string(),
                        to: "6.8.2-1".to_string(),
                        ignored: false,
                    },
                    Upgrade {
                        name: "openssh".to_string(),
                        from: "9.6p1-1".to_string(),
                        to: "9.7p1-1".to_string(),
                        ignored: true,
                    },
                ]
            );
            assert_eq!(parse_upgrade_plan(""), Ok(Vec::new()));
            for bad in [
                "curl 1.0 2.0\n",
                "curl 1.0 => 2.0\n",
                "curl 1.0 -> 2.0 extra\n",
                "curl 1.0 -> 2.0 [held]\n",
                "Curl 1.0 -> 2.0\n",
            ] {
                assert!(
                    matches!(
                        parse_upgrade_plan(bad),
                        Err(PackagesError::MalformedUpgradeLine { .. })
                    ),
                    "{bad:?} should be refused"
                );
            }
        }
    }
}

/// Parses an `*.packages` set file (#328): one Arch package name per line, with `#` comment
/// lines and blank lines dropped and file order kept. Every name goes through the Arch validator,
/// so a malformed line refuses the whole file with [`PackagesError::InvalidSpec`].
pub fn parse_package_set(input: &str) -> Result<Vec<arch::PackageSpec>, PackagesError> {
    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(arch::PackageSpec::parse)
        .collect()
}

/// The difference between a desired package set and the installed names (#328).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetDelta {
    /// Desired names that are not installed, in set-file order, each listed once.
    pub install: Vec<String>,
    /// Installed names the set does not list, sorted. Act on these only when the caller has
    /// asked for extra packages to be removed.
    pub remove: Vec<String>,
}

/// Computes what applying `desired` would change, given the `installed` names. Nothing is
/// spawned and no version is compared: a name counts as present when it is installed at all.
pub fn set_delta(desired: &[arch::PackageSpec], installed: &[&str]) -> SetDelta {
    let installed: std::collections::BTreeSet<&str> = installed.iter().copied().collect();
    let mut wanted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut install = Vec::new();
    for spec in desired {
        let name = spec.name();
        if wanted.insert(name) && !installed.contains(name) {
            install.push(name.to_string());
        }
    }
    let remove = installed
        .iter()
        .filter(|name| !wanted.contains(*name))
        .map(|name| name.to_string())
        .collect();
    SetDelta { install, remove }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(input: &str) -> PackageSpec {
        PackageSpec::parse(input).expect("fixture package name is valid")
    }

    fn argv(inv: &Invocation) -> Vec<&str> {
        inv.args.iter().map(String::as_str).collect()
    }

    fn status(input: &str, state: PackageState) -> PackageStatus {
        PackageStatus {
            spec: spec(input),
            state,
        }
    }

    fn installed(version: &str) -> PackageState {
        PackageState::Installed {
            version: version.to_string(),
            held: false,
        }
    }

    // concat! rather than a backslash-continued string: a continuation strips the leading
    // spaces that mark the indented name lists.
    const CURL_PLAN_NEW: &str = concat!(
        "Reading package lists...\n",
        "Building dependency tree...\n",
        "Reading state information...\n",
        "The following additional packages will be installed:\n",
        "  libcurl4\n",
        "The following NEW packages will be installed:\n",
        "  curl\n",
        "The following packages have been kept back:\n",
        "  openssh-server\n",
        "1 upgraded, 1 newly installed, 0 to remove and 1 not upgraded.\n",
        "Inst libcurl4 [7.81.0-1ubuntu1.13] (7.81.0-1ubuntu1.14 Ubuntu:22.04/jammy-updates [amd64])\n",
        "Inst curl (7.81.0-1ubuntu1.14 Ubuntu:22.04/jammy-updates [amd64])\n",
        "Conf libcurl4 (7.81.0-1ubuntu1.14 Ubuntu:22.04/jammy-updates [amd64])\n",
        "Conf curl (7.81.0-1ubuntu1.14 Ubuntu:22.04/jammy-updates [amd64])\n",
    );

    #[test]
    fn validator_accepts_policy_names() {
        for ok in ["curl", "g++", "python3.12", "2to3", "lib-x+y.z", "x1"] {
            assert!(PackageSpec::parse(ok).is_ok(), "{ok} should be accepted");
        }
        let qualified = spec("libc6:i386");
        assert_eq!(qualified.name(), "libc6");
        assert_eq!(qualified.arch(), Some("i386"));
        let versioned = spec("libc6:amd64=2.35-0ubuntu3.8");
        assert_eq!(versioned.version(), Some("2.35-0ubuntu3.8"));
        assert!(PackageSpec::parse("pkg=1:2.0-1~rc1+b2").is_ok());
    }

    #[test]
    fn validator_refuses_hostile_names() {
        let hostile = [
            "",
            "a",
            "-y",
            "--allow-unauthenticated",
            "-",
            "+x",
            ".x",
            "Curl",
            "curl x",
            "curl\tx",
            "curl\nx",
            "curl;id",
            "curl$(id)",
            "curl`id`",
            "curl|id",
            "curl&id",
            "curl>x",
            "../curl",
            "cu/rl",
            "curl\0",
            "curl\u{e9}",
            "curl:",
            "curl:AMD64",
            "curl:amd64:x",
            "curl:-x",
            ":amd64",
            "=1.0",
            "curl=",
            "curl=1 2",
            "curl=-1",
            "curl=a",
            "curl=1;id",
            "curl=1:2:3",
            "curl=1:",
            "curl==1",
            "curl=1=2",
        ];
        for bad in hostile {
            assert!(
                PackageSpec::parse(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn query_argv_is_exact_and_option_free() {
        let names = [spec("curl"), spec("libc6:i386=2.35")];
        let inv = query_installed(&names).expect("names given");
        assert_eq!(inv.program, "dpkg-query");
        assert_eq!(inv.env, NON_INTERACTIVE_ENV);
        assert_eq!(
            argv(&inv),
            vec![
                "-W",
                "-f=${Package}\\t${Architecture}\\t${Status}\\t${Version}\\n",
                "--",
                "curl",
                "libc6:i386",
            ]
        );
        assert_eq!(
            format!("-f={DPKG_QUERY_FORMAT}"),
            "-f=${Package}\\t${Architecture}\\t${Status}\\t${Version}\\n"
        );
    }

    #[test]
    fn simulate_argv_is_exact() {
        let inv = simulate_install(&[spec("curl"), spec("foo=1.0")]).expect("names given");
        assert_eq!(inv.program, "apt-get");
        assert_eq!(inv.env, NON_INTERACTIVE_ENV);
        assert_eq!(
            argv(&inv),
            vec![
                "install",
                "--simulate",
                "--no-install-recommends",
                "-y",
                "--",
                "curl",
                "foo=1.0",
            ]
        );
    }

    #[test]
    fn install_argv_is_exact_for_both_conffile_choices() {
        let inv = install(&[spec("curl")], Conffile::KeepOld, 120).expect("names given");
        assert_eq!(inv.program, "apt-get");
        assert_eq!(inv.env, NON_INTERACTIVE_ENV);
        assert_eq!(
            argv(&inv),
            vec![
                "-o",
                "DPkg::Lock::Timeout=120",
                "-o",
                "Dpkg::Options::=--force-confold",
                "install",
                "--no-install-recommends",
                "-y",
                "--",
                "curl",
            ]
        );
        let new = install(&[spec("curl")], Conffile::KeepNew, 0).expect("names given");
        assert_eq!(argv(&new)[3], "Dpkg::Options::=--force-confnew");
        assert_eq!(argv(&new)[1], "DPkg::Lock::Timeout=0");
    }

    #[test]
    fn remove_argv_is_exact_and_refuses_versions() {
        let inv = remove(&[spec("curl:i386")], 60).expect("names given");
        assert_eq!(inv.program, "apt-get");
        assert_eq!(
            argv(&inv),
            vec![
                "-o",
                "DPkg::Lock::Timeout=60",
                "remove",
                "-y",
                "--",
                "curl:i386",
            ]
        );
        assert_eq!(
            remove(&[spec("curl=1.0")], 60),
            Err(PackagesError::VersionNotAllowed {
                input: "curl=1.0".to_string()
            })
        );
    }

    #[test]
    fn empty_input_builds_nothing() {
        assert_eq!(query_installed(&[]), Err(PackagesError::NoNames));
        assert_eq!(simulate_install(&[]), Err(PackagesError::NoNames));
        assert_eq!(
            install(&[], Conffile::KeepOld, 1),
            Err(PackagesError::NoNames)
        );
        assert_eq!(remove(&[], 1), Err(PackagesError::NoNames));
    }

    #[test]
    fn query_output_distinguishes_every_state() {
        let requested = [
            spec("curl"),
            spec("heldpkg"),
            spec("oldconf"),
            spec("gone"),
            spec("halfdone"),
        ];
        let output = "curl\tamd64\tinstall ok installed\t7.81.0-1ubuntu1.14\n\
heldpkg\tamd64\thold ok installed\t1.0-1\n\
oldconf\tamd64\tdeinstall ok config-files\t0.9-2\n\
halfdone\tamd64\tinstall ok half-configured\t3.0\n";
        let parsed = parse_query_output(&requested, output).expect("fixture parses");
        assert_eq!(parsed[0].state, installed("7.81.0-1ubuntu1.14"));
        assert_eq!(
            parsed[1].state,
            PackageState::Installed {
                version: "1.0-1".to_string(),
                held: true
            }
        );
        assert_eq!(
            parsed[2].state,
            PackageState::ConfigFiles {
                version: "0.9-2".to_string()
            }
        );
        assert_eq!(parsed[3].state, PackageState::NotInstalled);
        assert_eq!(
            parsed[4].state,
            PackageState::Partial {
                status: "install ok half-configured".to_string()
            }
        );
    }

    #[test]
    fn query_output_empty_means_not_installed() {
        let parsed = parse_query_output(&[spec("curl")], "").expect("empty is valid");
        assert_eq!(parsed, vec![status("curl", PackageState::NotInstalled)]);
    }

    #[test]
    fn query_output_refuses_unrequested_and_malformed_lines() {
        let unrequested =
            parse_query_output(&[spec("curl")], "other\tamd64\tinstall ok installed\t1.0\n");
        assert!(matches!(
            unrequested,
            Err(PackagesError::UnexpectedQueryLine { .. })
        ));
        let short = parse_query_output(&[spec("curl")], "curl\tamd64\tinstall ok installed\n");
        assert!(matches!(
            short,
            Err(PackagesError::MalformedQueryLine { .. })
        ));
        let bad_status = parse_query_output(&[spec("curl")], "curl\tamd64\tinstall ok\t1.0\n");
        assert!(matches!(
            bad_status,
            Err(PackagesError::MalformedQueryLine { .. })
        ));
    }

    #[test]
    fn query_output_ambiguous_unless_qualified() {
        let output = "libc6\tamd64\tinstall ok installed\t2.35\n\
libc6\ti386\tinstall ok installed\t2.35\n";
        assert_eq!(
            parse_query_output(&[spec("libc6")], output),
            Err(PackagesError::AmbiguousPackage {
                name: "libc6".to_string()
            })
        );
        // dpkg-query prints only the instance asked for, so a qualified name sees one line.
        let only_i386 = "libc6\ti386\tinstall ok installed\t2.35\n";
        let qualified = parse_query_output(&[spec("libc6:i386")], only_i386).expect("qualified");
        assert_eq!(qualified[0].state, installed("2.35"));
    }

    #[test]
    fn ensure_decides_what_to_install() {
        let decision = decide_ensure(&[
            status("curl", installed("1.0")),
            status("gone", PackageState::NotInstalled),
            status(
                "oldconf",
                PackageState::ConfigFiles {
                    version: "0.9".into(),
                },
            ),
        ])
        .expect("no refusal");
        assert_eq!(decision.to_install, vec![spec("gone"), spec("oldconf")]);
        assert!(decision.changed);
    }

    #[test]
    fn ensure_second_run_changes_nothing() {
        let decision = decide_ensure(&[
            status("curl", installed("1.0")),
            status("curl", installed("1.0")),
        ])
        .expect("no refusal");
        assert!(decision.to_install.is_empty());
        assert!(!decision.changed);
    }

    #[test]
    fn ensure_empty_input_changes_nothing() {
        let decision = decide_ensure(&[]).expect("no refusal");
        assert_eq!(
            decision,
            EnsureDecision {
                to_install: Vec::new(),
                changed: false
            }
        );
    }

    #[test]
    fn ensure_versioned_spec_compares_and_refuses_held() {
        let at_version = decide_ensure(&[status("curl=1.0", installed("1.0"))]).expect("same");
        assert!(!at_version.changed);
        let other = decide_ensure(&[status("curl=2.0", installed("1.0"))]).expect("differs");
        assert_eq!(other.to_install, vec![spec("curl=2.0")]);
        let held = decide_ensure(&[status(
            "curl=2.0",
            PackageState::Installed {
                version: "1.0".into(),
                held: true,
            },
        )]);
        assert_eq!(
            held,
            Err(PackagesError::Held {
                name: "curl".to_string()
            })
        );
    }

    #[test]
    fn ensure_refuses_partial_state_and_deduplicates() {
        assert_eq!(
            decide_ensure(&[status(
                "broken",
                PackageState::Partial {
                    status: "install ok half-configured".into()
                }
            )]),
            Err(PackagesError::NeedsConfigure {
                name: "broken".to_string(),
                status: "install ok half-configured".to_string(),
            })
        );
        let twice = decide_ensure(&[
            status("gone", PackageState::NotInstalled),
            status("gone", PackageState::NotInstalled),
        ])
        .expect("no refusal");
        assert_eq!(twice.to_install, vec![spec("gone")]);
    }

    #[test]
    fn plan_parses_every_action_and_summary() {
        let plan = parse_simulate(CURL_PLAN_NEW).expect("fixture parses");
        assert_eq!(plan.lines.len(), 4);
        assert_eq!(
            plan.lines[0],
            PlanLine {
                name: "libcurl4".to_string(),
                from: Some("7.81.0-1ubuntu1.13".to_string()),
                to: Some("7.81.0-1ubuntu1.14".to_string()),
                action: PlanAction::Upgrade,
            }
        );
        assert_eq!(plan.lines[1].action, PlanAction::Install);
        assert_eq!(plan.lines[1].from, None);
        assert_eq!(plan.lines[2].action, PlanAction::Configure);
        assert_eq!(plan.lines[3].name, "curl");
        assert!(approve_plan(&plan, false).is_ok());
    }

    #[test]
    fn plan_surfaces_removal_and_refuses_unless_allowed() {
        let output = concat!(
            "The following packages will be REMOVED:\n",
            "  badpkg\n",
            "0 upgraded, 0 newly installed, 1 to remove and 0 not upgraded.\n",
            "Remv badpkg [1.0-1]\n",
        );
        let plan = parse_simulate(output).expect("fixture parses");
        assert_eq!(plan.lines.len(), 1);
        assert_eq!(plan.lines[0].action, PlanAction::Remove);
        assert_eq!(plan.lines[0].from, Some("1.0-1".to_string()));
        assert_eq!(plan.lines[0].to, None);
        assert_eq!(
            approve_plan(&plan, false),
            Err(PackagesError::RemovalRefused {
                names: vec!["badpkg".to_string()]
            })
        );
        assert!(approve_plan(&plan, true).is_ok());
    }

    #[test]
    fn plan_with_only_held_packages_is_empty() {
        let output = concat!(
            "The following packages have been kept back:\n",
            "  heldpkg\n",
            "0 upgraded, 0 newly installed, 0 to remove and 1 not upgraded.\n",
        );
        let plan = parse_simulate(output).expect("fixture parses");
        assert_eq!(plan, Plan::default());
        assert!(approve_plan(&plan, false).is_ok());
    }

    #[test]
    fn plan_empty_output_is_incomplete_not_empty() {
        assert!(matches!(
            parse_simulate(""),
            Err(PackagesError::PlanInconsistent { .. })
        ));
    }

    #[test]
    fn plan_apt_errors_are_typed() {
        let unknown = parse_simulate("E: Unable to locate package nosuchpkg\n");
        assert!(matches!(
            unknown,
            Err(PackagesError::AptError {
                failure: AptFailure::UnknownPackage,
                ..
            })
        ));
        let locked = parse_simulate(
            "E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 4242 (apt-get)\n\
E: Unable to acquire the dpkg frontend lock (/var/lib/dpkg/lock-frontend), is another process using it?\n",
        );
        assert!(matches!(
            locked,
            Err(PackagesError::AptError {
                failure: AptFailure::Locked,
                ..
            })
        ));
        let interrupted = parse_simulate(
            "E: dpkg was interrupted, you must manually run 'sudo dpkg --configure -a' to correct the problem.\n",
        );
        assert!(matches!(
            interrupted,
            Err(PackagesError::AptError {
                failure: AptFailure::Interrupted,
                ..
            })
        ));
    }

    #[test]
    fn plan_refuses_unknown_and_malformed_plan_lines() {
        let unknown =
            "Frobnicate curl\n0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n";
        assert!(matches!(
            parse_simulate(unknown),
            Err(PackagesError::UnrecognisedPlanLine { .. })
        ));
        let no_name =
            "Remv  [1.0]\n0 upgraded, 0 newly installed, 1 to remove and 0 not upgraded.\n";
        assert!(matches!(
            parse_simulate(no_name),
            Err(PackagesError::MalformedPlanLine { .. })
        ));
        let no_version =
            "Inst curl\n1 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n";
        assert!(matches!(
            parse_simulate(no_version),
            Err(PackagesError::MalformedPlanLine { .. })
        ));
        let hostile_name =
            "Inst -x (1.0 repo)\n0 upgraded, 1 newly installed, 0 to remove and 0 not upgraded.\n";
        assert!(matches!(
            parse_simulate(hostile_name),
            Err(PackagesError::MalformedPlanLine { .. })
        ));
    }

    #[test]
    fn plan_refuses_indented_plan_line_hidden_in_a_list() {
        // An indented removal must still be counted, not skipped as list text.
        let output = concat!(
            "The following packages will be REMOVED:\n",
            "  Remv badpkg [1.0]\n",
            "0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n",
        );
        assert!(matches!(
            parse_simulate(output),
            Err(PackagesError::PlanInconsistent { .. })
        ));
    }

    #[test]
    fn plan_refuses_summary_that_disagrees_with_plan() {
        let missing_removal = "0 upgraded, 0 newly installed, 1 to remove and 0 not upgraded.\n";
        assert!(matches!(
            parse_simulate(missing_removal),
            Err(PackagesError::PlanInconsistent { .. })
        ));
        let dropped_install = "Inst curl (1.0 repo [amd64])\n\
0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n";
        assert!(matches!(
            parse_simulate(dropped_install),
            Err(PackagesError::PlanInconsistent { .. })
        ));
    }
}
