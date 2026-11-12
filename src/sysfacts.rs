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
/// Most entries read from one listing (`power_supply`, `drm`). Real machines have a handful.
const MAX_LISTED: usize = 1024;
/// Largest `os-release` read. The file is a few hundred bytes; this is a ceiling.
const OS_RELEASE_LIMIT: u64 = 64 * 1024;

/// Machine facts. The first four fields are the first slice; the rest were
/// added later and default to empty when absent from older serialised data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub dmi: Dmi,
    pub cpu: Cpu,
    pub power: Power,
    pub pci_display: Vec<PciDevice>,
    /// Every entry under `sys/class/power_supply`, sorted by name.
    #[serde(default)]
    pub power_supplies: Vec<PowerSupply>,
    /// Every `cardN-CONNECTOR` entry under `sys/class/drm`, sorted by name.
    #[serde(default)]
    pub drm_outputs: Vec<DrmOutput>,
    /// Hypervisor evidence from DMI and `sys/hypervisor/type`.
    #[serde(default)]
    pub virtualization: Virtualization,
    /// Distribution identity from `os-release`.
    #[serde(default)]
    pub os: Os,
}

/// Distribution identity from `etc/os-release`, or `usr/lib/os-release` when
/// the first is absent. Each field is absent when its key is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Os {
    /// `ID`, e.g. `ubuntu`.
    pub id: Option<String>,
    /// `ID_LIKE` split on whitespace, e.g. `["debian"]`.
    pub id_like: Option<Vec<String>>,
    /// `VERSION_ID`, e.g. `24.04`.
    pub version_id: Option<String>,
    /// `VERSION_CODENAME`, e.g. `noble`.
    pub codename: Option<String>,
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
    /// The vendor name for an arm64 implementer code published by Arm (`0x41`
    /// is `Arm`). A hex code not in the table stays the raw code. `None` when
    /// `vendor` is not a hex code (x86).
    #[serde(default)]
    pub implementer: Option<String>,
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

/// One entry under `sys/class/power_supply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerSupply {
    /// Directory name, e.g. `BAT0` or `AC`.
    pub name: String,
    /// The `type` attribute as read, e.g. `Battery`.
    pub supply_type: Option<String>,
    /// `supply_type` when it is a type the kernel documents. Other text is `None`.
    pub kind: Option<SupplyKind>,
    /// Battery only: the `status` attribute when it is a documented state.
    pub status: Option<BatteryStatus>,
    /// Battery only: the `capacity` attribute, 0 to 100. Anything else (a sign,
    /// a fraction, a value over 100) is absent, not clamped.
    pub capacity: Option<u8>,
    /// Battery only: the `present` attribute, `1` true and `0` false.
    pub present: Option<bool>,
}

/// Power supply types from the kernel's `power_supply` sysfs ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupplyKind {
    Unknown,
    Battery,
    Ups,
    Mains,
    Usb,
    UsbDcp,
    UsbCdp,
    UsbAca,
    UsbC,
    UsbPd,
    UsbPdDrp,
    BrickId,
    Wireless,
}

impl SupplyKind {
    /// Maps the kernel's type name (case-sensitive, as the kernel writes it).
    pub fn from_name(name: &str) -> Option<Self> {
        use SupplyKind::*;
        Some(match name {
            "Unknown" => Unknown,
            "Battery" => Battery,
            "UPS" => Ups,
            "Mains" => Mains,
            "USB" => Usb,
            "USB_DCP" => UsbDcp,
            "USB_CDP" => UsbCdp,
            "USB_ACA" => UsbAca,
            "USB_C" => UsbC,
            "USB_PD" => UsbPd,
            "USB_PD_DRP" => UsbPdDrp,
            "BrickID" => BrickId,
            "Wireless" => Wireless,
            _ => return None,
        })
    }
}

/// Battery `status` values from the kernel's `power_supply` sysfs ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryStatus {
    Unknown,
    Charging,
    Discharging,
    NotCharging,
    Full,
}

impl BatteryStatus {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Unknown" => Self::Unknown,
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Not charging" => Self::NotCharging,
            "Full" => Self::Full,
            _ => return None,
        })
    }
}

/// One `cardN-CONNECTOR` entry under `sys/class/drm`, such as `card0-eDP-1`.
/// The card and render nodes are not outputs and are not listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrmOutput {
    /// Directory name, e.g. `card0-eDP-1`.
    pub name: String,
    /// The connector part, e.g. `eDP-1` or `HDMI-A-1`.
    pub connector: String,
    /// The `status` attribute.
    pub status: Option<DrmStatus>,
    /// The `enabled` attribute: `enabled` true, `disabled` false, else absent.
    pub enabled: Option<bool>,
}

/// Connector `status` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrmStatus {
    Connected,
    Disconnected,
    Unknown,
}

impl DrmStatus {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "connected" => Self::Connected,
            "disconnected" => Self::Disconnected,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// Virtual machine hint from files only. Nothing is executed, so this never
/// asks `systemd-detect-virt`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Virtualization {
    /// The hypervisor name from the identifier table, for the first match.
    pub hypervisor: Option<String>,
    /// `sys/hypervisor/type` as read (Xen sets it; it also appears on a Xen host).
    pub hypervisor_type: Option<String>,
    /// Each piece of evidence, e.g. `sys_vendor contains "QEMU"`.
    pub evidence: Vec<String>,
    /// `Some(true)` with any evidence. `Some(false)` when DMI identity was
    /// read, matched nothing and `sys/hypervisor/type` is absent. `None` when
    /// there is no DMI identity and no hypervisor file. The table is not
    /// exhaustive, so `Some(false)` does not prove bare metal.
    pub likely_virtual: Option<bool>,
}

/// Well-known hypervisor identifiers: (DMI field, identifier, hypervisor name).
/// An identifier matches when the field contains it, ignoring ASCII case. The
/// list is short and not exhaustive; a hypervisor not listed gives no DMI evidence.
const HYPERVISOR_IDENTIFIERS: &[(&str, &str, &str)] = &[
    ("sys_vendor", "QEMU", "QEMU"),
    ("sys_vendor", "VMware", "VMware"),
    ("product_name", "VMware Virtual Platform", "VMware"),
    ("sys_vendor", "innotek GmbH", "VirtualBox"),
    ("product_name", "VirtualBox", "VirtualBox"),
    ("product_name", "Virtual Machine", "Hyper-V"),
    ("sys_vendor", "Xen", "Xen"),
    ("sys_vendor", "Parallels", "Parallels"),
    ("sys_vendor", "Bochs", "Bochs"),
    ("sys_vendor", "Amazon EC2", "Amazon EC2"),
    (
        "product_name",
        "Google Compute Engine",
        "Google Compute Engine",
    ),
];

/// Reads every fact below `root`. Facts that cannot be read are `None` (or an
/// empty list). If `root` does not exist, every fact is absent.
pub fn facts(root: &Path) -> Facts {
    let Ok(root) = fs::canonicalize(root) else {
        return Facts::default();
    };
    let dmi = dmi(&root);
    let virtualization = virtualization(&root, &dmi);
    Facts {
        dmi,
        cpu: cpu(&root),
        power: Power {
            battery: battery(&root),
            lid: lid(&root),
        },
        pci_display: pci_display(&root),
        power_supplies: power_supplies(&root),
        drm_outputs: drm_outputs(&root),
        virtualization,
        os: os_release(&root),
    }
}

/// Reads `etc/os-release`, falling back to `usr/lib/os-release`. A file that
/// is missing, unreadable, too large or a symlink leaving `root` is skipped.
fn os_release(root: &Path) -> Os {
    let candidates: [&[&str]; 2] = [&["etc", "os-release"], &["usr", "lib", "os-release"]];
    let text = candidates.iter().find_map(|parts| {
        let path = join_checked(root, parts)?;
        read_bounded(&path, OS_RELEASE_LIMIT)
    });
    text.map(|text| parse_os_release(&text)).unwrap_or_default()
}

/// Parses `KEY=value` lines. Blank lines and `#` comments are skipped, values
/// may be wrapped in one pair of matching quotes, and the first value of each
/// key wins.
fn parse_os_release(text: &str) -> Os {
    let mut os = Os::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "ID" => keep_first(&mut os.id, value),
            "ID_LIKE" if os.id_like.is_none() && !value.is_empty() => {
                os.id_like = Some(value.split_whitespace().map(str::to_owned).collect());
            }
            "VERSION_ID" => keep_first(&mut os.version_id, value),
            "VERSION_CODENAME" => keep_first(&mut os.codename, value),
            _ => {}
        }
    }
    os
}

/// Removes one pair of matching surrounding double or single quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

fn power_supplies(root: &Path) -> Vec<PowerSupply> {
    let Some(dir) = join_checked(root, &["sys", "class", "power_supply"]) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut supplies: Vec<PowerSupply> = entries
        .take(MAX_LISTED)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let base = entry.path();
            let supply_type = attr_at(root, &base.join("type"));
            let kind = supply_type.as_deref().and_then(SupplyKind::from_name);
            let is_battery = kind == Some(SupplyKind::Battery);
            Some(PowerSupply {
                name,
                supply_type,
                kind,
                status: is_battery
                    .then(|| attr_at(root, &base.join("status")))
                    .flatten()
                    .and_then(|text| BatteryStatus::from_name(&text)),
                capacity: is_battery
                    .then(|| attr_at(root, &base.join("capacity")))
                    .flatten()
                    .and_then(|text| parse_capacity(&text)),
                present: is_battery
                    .then(|| attr_at(root, &base.join("present")))
                    .flatten()
                    .and_then(|text| parse_flag(&text)),
            })
        })
        .collect();
    supplies.sort_by(|a, b| a.name.cmp(&b.name));
    supplies
}

/// Parses a battery capacity: one to three ASCII digits, at most 100.
fn parse_capacity(text: &str) -> Option<u8> {
    if text.is_empty() || text.len() > 3 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u8>().ok().filter(|value| *value <= 100)
}

/// Parses a sysfs boolean flag: `1` is true, `0` is false, anything else absent.
fn parse_flag(text: &str) -> Option<bool> {
    match text {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

fn drm_outputs(root: &Path) -> Vec<DrmOutput> {
    let Some(dir) = join_checked(root, &["sys", "class", "drm"]) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut outputs: Vec<DrmOutput> = entries
        .take(MAX_LISTED)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let connector = drm_connector(&name)?.to_owned();
            let base = entry.path();
            Some(DrmOutput {
                status: attr_at(root, &base.join("status"))
                    .and_then(|text| DrmStatus::from_name(&text)),
                enabled: attr_at(root, &base.join("enabled")).and_then(|text| {
                    match text.as_str() {
                        "enabled" => Some(true),
                        "disabled" => Some(false),
                        _ => None,
                    }
                }),
                connector,
                name,
            })
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    outputs
}

/// The connector part of a `cardN-CONNECTOR` name. Any other name (`card0`,
/// `renderD128`, `version`) gives `None`.
fn drm_connector(name: &str) -> Option<&str> {
    let (digits, connector) = name.strip_prefix("card")?.split_once('-')?;
    let numbered = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    (numbered && !connector.is_empty()).then_some(connector)
}

fn virtualization(root: &Path, dmi: &Dmi) -> Virtualization {
    let mut evidence = Vec::new();
    let mut hypervisor = None;
    let identity = [
        ("sys_vendor", dmi.sys_vendor.as_deref()),
        ("product_name", dmi.product_name.as_deref()),
    ];
    for (field, value) in identity {
        let Some(value) = value else {
            continue;
        };
        for (_, identifier, name) in HYPERVISOR_IDENTIFIERS
            .iter()
            .filter(|(entry_field, _, _)| *entry_field == field)
        {
            if value
                .to_ascii_lowercase()
                .contains(&identifier.to_ascii_lowercase())
            {
                evidence.push(format!("{field} contains \"{identifier}\""));
                if hypervisor.is_none() {
                    hypervisor = Some((*name).to_owned());
                }
            }
        }
    }
    let hypervisor_type = attr(root, &["sys", "hypervisor", "type"]);
    if let Some(text) = &hypervisor_type {
        evidence.push(format!("sys/hypervisor/type is \"{text}\""));
    }
    let likely_virtual = if !evidence.is_empty() {
        Some(true)
    } else if dmi.sys_vendor.is_some() || dmi.product_name.is_some() {
        Some(false)
    } else {
        None
    };
    Virtualization {
        hypervisor,
        hypervisor_type,
        evidence,
        likely_virtual,
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
    cpu.implementer = implementer(cpu.vendor.as_deref());
    cpu
}

/// Names an arm64 implementer code. Only codes Arm publishes in the MIDR
/// implementer table are listed; others are left to the caller as raw codes.
fn implementer(vendor: Option<&str>) -> Option<String> {
    let raw = vendor?;
    let code = parse_hex(raw)?;
    let name = match code {
        0x41 => "Arm",
        0x42 => "Broadcom",
        0x43 => "Cavium",
        0x44 => "Digital Equipment Corporation",
        0x46 => "Fujitsu",
        0x48 => "HiSilicon",
        0x49 => "Infineon",
        0x4e => "NVIDIA",
        0x50 => "Applied Micro",
        0x51 => "Qualcomm",
        0x53 => "Samsung",
        0x56 => "Marvell",
        0x61 => "Apple",
        0x66 => "Faraday",
        0x69 => "Intel",
        0xc0 => "Ampere",
        _ => return Some(raw.to_owned()),
    };
    Some(name.to_owned())
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
