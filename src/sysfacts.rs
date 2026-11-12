//! System facts read under a fixture root (#342).
//!
//! Every fact is read from a file below `root`: `/` in production, a temporary
//! directory in tests. A missing or unreadable file is an absent fact (`None`),
//! never an error. Reads are bounded, no path may leave `root` through a
//! symlink, and nothing is executed.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Largest sysfs attribute read. Attributes hold a few bytes; this is a ceiling.
const SYSFS_ATTR_LIMIT: u64 = 4 * 1024;
/// Largest `/proc/cpuinfo` read. Each logical CPU adds a few KiB, so this is generous.
const CPUINFO_LIMIT: u64 = 8 * 1024 * 1024;

/// The first slice of machine facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub dmi: Dmi,
    pub cpu: Cpu,
    pub power: Power,
    pub pci_display: Vec<PciDevice>,
}

/// Identity from `sys/class/dmi/id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dmi {
    pub sys_vendor: Option<String>,
    pub product_name: Option<String>,
    pub product_version: Option<String>,
    pub board_name: Option<String>,
    /// The SMBIOS chassis type code, as read.
    pub chassis_type: Option<u32>,
    /// The name of `chassis_type`, when the code is known.
    pub chassis_kind: Option<ChassisKind>,
}

/// SMBIOS chassis type names (DMTF DSP0134).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChassisKind {
    Other,
    Unknown,
    Desktop,
    LowProfileDesktop,
    PizzaBox,
    MiniTower,
    Tower,
    Portable,
    Laptop,
    Notebook,
    HandHeld,
    DockingStation,
    AllInOne,
    SubNotebook,
    SpaceSaving,
    LunchBox,
    MainServer,
    Expansion,
    SubChassis,
    BusExpansion,
    PeripheralChassis,
    RaidChassis,
    RackMount,
    SealedCase,
    MultiSystem,
    CompactPci,
    AdvancedTca,
    Blade,
    BladeEnclosure,
    Tablet,
    Convertible,
    Detachable,
    IotGateway,
    EmbeddedPc,
    MiniPc,
    StickPc,
}

impl ChassisKind {
    /// Maps an SMBIOS chassis type code to its name. Unlisted codes give `None`.
    pub fn from_code(code: u32) -> Option<Self> {
        use ChassisKind::*;
        Some(match code {
            1 => Other,
            2 => Unknown,
            3 => Desktop,
            4 => LowProfileDesktop,
            5 => PizzaBox,
            6 => MiniTower,
            7 => Tower,
            8 => Portable,
            9 => Laptop,
            10 => Notebook,
            11 => HandHeld,
            12 => DockingStation,
            13 => AllInOne,
            14 => SubNotebook,
            15 => SpaceSaving,
            16 => LunchBox,
            17 => MainServer,
            18 => Expansion,
            19 => SubChassis,
            20 => BusExpansion,
            21 => PeripheralChassis,
            22 => RaidChassis,
            23 => RackMount,
            24 => SealedCase,
            25 => MultiSystem,
            26 => CompactPci,
            27 => AdvancedTca,
            28 => Blade,
            29 => BladeEnclosure,
            30 => Tablet,
            31 => Convertible,
            32 => Detachable,
            33 => IotGateway,
            34 => EmbeddedPc,
            35 => MiniPc,
            36 => StickPc,
            _ => return None,
        })
    }
}

/// CPU identity from `proc/cpuinfo`. The field names differ on arm64: the
/// vendor is `CPU implementer`, the model is `CPU part`, and the flags are
/// `Features`. Each field takes the first value found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cpu {
    /// x86 `vendor_id`, or arm64 `CPU implementer` (raw hex, e.g. `0x41`).
    pub vendor: Option<String>,
    /// x86 `model name`. arm64 has none.
    pub model_name: Option<String>,
    /// arm64 `CPU part` (raw hex).
    pub cpu_part: Option<String>,
    /// Number of `processor` entries.
    pub logical_count: Option<u32>,
    /// x86 `flags`, or arm64 `Features`, split on whitespace.
    pub flags: Option<BTreeSet<String>>,
}

/// Battery and lid, from the power supply and ACPI button trees.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Power {
    /// `None` when `sys/class/power_supply` is absent; otherwise whether any
    /// supply has type `Battery`.
    pub battery: Option<bool>,
    /// `None` when `proc/acpi/button/lid` is absent; otherwise whether a lid
    /// entry with a `state` file exists.
    pub lid: Option<bool>,
}

/// A PCI device whose class is a display controller (base class 0x03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PciDevice {
    /// Directory name under `sys/bus/pci/devices`, e.g. `0000:01:00.0`.
    pub slot: String,
    /// Raw class code, e.g. `0x030000`.
    pub class: String,
    /// Raw vendor id, e.g. `0x10de`.
    pub vendor_id: Option<String>,
    /// Name for NVIDIA, AMD and Intel. `None` for other vendors, which are
    /// reported by `vendor_id` only.
    pub vendor_name: Option<String>,
    /// Raw device id, e.g. `0x25a2`.
    pub device_id: Option<String>,
}

/// Reads every fact below `root`. Facts that cannot be read are `None` (or an
/// empty list). If `root` does not exist, every fact is absent.
pub fn facts(root: &Path) -> Facts {
    let Ok(root) = fs::canonicalize(root) else {
        return Facts::default();
    };
    Facts {
        dmi: dmi(&root),
        cpu: cpu(&root),
        power: Power {
            battery: battery(&root),
            lid: lid(&root),
        },
        pci_display: pci_display(&root),
    }
}

fn dmi(root: &Path) -> Dmi {
    let chassis_type = attr(root, &["sys", "class", "dmi", "id", "chassis_type"])
        .and_then(|text| text.parse::<u32>().ok());
    Dmi {
        sys_vendor: attr(root, &["sys", "class", "dmi", "id", "sys_vendor"]),
        product_name: attr(root, &["sys", "class", "dmi", "id", "product_name"]),
        product_version: attr(root, &["sys", "class", "dmi", "id", "product_version"]),
        board_name: attr(root, &["sys", "class", "dmi", "id", "board_name"]),
        chassis_type,
        chassis_kind: chassis_type.and_then(ChassisKind::from_code),
    }
}

fn cpu(root: &Path) -> Cpu {
    let Some(path) = join_checked(root, &["proc", "cpuinfo"]) else {
        return Cpu::default();
    };
    let Some(text) = read_bounded(&path, CPUINFO_LIMIT) else {
        return Cpu::default();
    };
    parse_cpuinfo(&text)
}

fn parse_cpuinfo(text: &str) -> Cpu {
    let mut cpu = Cpu::default();
    let mut processors: u32 = 0;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim_start();
        match key.trim() {
            "processor" => processors = processors.saturating_add(1),
            "vendor_id" | "CPU implementer" => keep_first(&mut cpu.vendor, value),
            "model name" => keep_first(&mut cpu.model_name, value),
            "CPU part" => keep_first(&mut cpu.cpu_part, value),
            "flags" | "Features" if cpu.flags.is_none() => {
                cpu.flags = Some(value.split_whitespace().map(str::to_owned).collect());
            }
            _ => {}
        }
    }
    cpu.logical_count = Some(processors);
    cpu
}

fn keep_first(slot: &mut Option<String>, value: &str) {
    if slot.is_none() && !value.is_empty() {
        *slot = Some(value.to_owned());
    }
}

fn battery(root: &Path) -> Option<bool> {
    let dir = join_checked(root, &["sys", "class", "power_supply"])?;
    let mut battery = false;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        if attr_at(root, &entry.path().join("type")).as_deref() == Some("Battery") {
            battery = true;
        }
    }
    Some(battery)
}

fn lid(root: &Path) -> Option<bool> {
    let dir = join_checked(root, &["proc", "acpi", "button", "lid"])?;
    let mut lid = false;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        if attr_at(root, &entry.path().join("state")).is_some() {
            lid = true;
        }
    }
    Some(lid)
}

fn pci_display(root: &Path) -> Vec<PciDevice> {
    let Some(dir) = join_checked(root, &["sys", "bus", "pci", "devices"]) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut devices: Vec<PciDevice> = entries
        .flatten()
        .filter_map(|entry| {
            let slot = entry.file_name().into_string().ok()?;
            let base = entry.path();
            let class = attr_at(root, &base.join("class"))?;
            if (parse_hex(&class)? >> 16) != 0x03 {
                return None;
            }
            let vendor_id = attr_at(root, &base.join("vendor"));
            let vendor_name = vendor_id
                .as_deref()
                .and_then(parse_hex)
                .and_then(gpu_vendor_name)
                .map(str::to_owned);
            Some(PciDevice {
                slot,
                class,
                vendor_id,
                vendor_name,
                device_id: attr_at(root, &base.join("device")),
            })
        })
        .collect();
    devices.sort_by(|a, b| a.slot.cmp(&b.slot));
    devices
}

/// The name of the three common GPU vendors. Other ids have no name.
fn gpu_vendor_name(id: u32) -> Option<&'static str> {
    match id {
        0x10de => Some("NVIDIA"),
        0x1002 => Some("AMD"),
        0x8086 => Some("Intel"),
        _ => None,
    }
}

/// Parses a `0x`-prefixed hex id such as a PCI class or vendor code.
fn parse_hex(text: &str) -> Option<u32> {
    u32::from_str_radix(text.strip_prefix("0x")?, 16).ok()
}

/// Joins single-name parts onto `root` and returns the resolved path if it
/// stays inside `root`. A part that is not one plain name (`..`, `/`, a
/// drive prefix) is refused.
fn join_checked(root: &Path, parts: &[&str]) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in parts {
        let mut comps = Path::new(part).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(name)), None) => path.push(name),
            _ => return None,
        }
    }
    contained(root, &path)
}

/// Resolves `path` through any symlinks and returns it only if the result is
/// still under `root`. Symlinks that lead out of the root are refused.
fn contained(root: &Path, path: &Path) -> Option<PathBuf> {
    let resolved = fs::canonicalize(path).ok()?;
    resolved.starts_with(root).then_some(resolved)
}

/// Reads at most `limit` bytes. A file longer than `limit` is absent.
fn read_bounded(path: &Path, limit: u64) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(limit.checked_add(1)?)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > limit {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Reads a sysfs attribute: one trailing newline removed, empty is absent.
fn attr(root: &Path, parts: &[&str]) -> Option<String> {
    let path = join_checked(root, parts)?;
    sysfs_value(&path)
}

/// Like `attr`, for a path the caller built from a directory listing.
fn attr_at(root: &Path, path: &Path) -> Option<String> {
    let path = contained(root, path)?;
    sysfs_value(&path)
}

fn sysfs_value(path: &Path) -> Option<String> {
    let text = read_bounded(path, SYSFS_ATTR_LIMIT)?;
    let value = text.strip_suffix('\n').unwrap_or(&text);
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}
