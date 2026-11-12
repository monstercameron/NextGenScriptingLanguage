//! Fixture-root tests for `flow::sysfacts` (#342). Each test builds a fake
//! `/sys` and `/proc` tree in a temporary directory and reads it back.
//!
//! PCI slot names use `-` rather than `:` because `:` is not a valid file name
//! character on Windows. The reader does not depend on the slot format.

use std::fs;
use std::path::Path;

use flow::sysfacts::{
    facts, BatteryStatus, ChassisKind, DrmStatus, Facts, Os, SupplyKind, Virtualization,
};

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("fixture path has a parent"))
        .expect("create fixture dir");
    fs::write(path, text).expect("write fixture file");
}

fn dmi(root: &Path, name: &str, text: &str) {
    write(root, &format!("sys/class/dmi/id/{name}"), text);
}

fn pci(root: &Path, slot: &str, class: &str, vendor: &str, device: &str) {
    let base = format!("sys/bus/pci/devices/{slot}");
    write(root, &format!("{base}/class"), &format!("{class}\n"));
    write(root, &format!("{base}/vendor"), &format!("{vendor}\n"));
    write(root, &format!("{base}/device"), &format!("{device}\n"));
}

/// A battery under `sys/class/power_supply/<name>` with the given attributes.
fn battery(root: &Path, name: &str, status: &str, capacity: &str, present: &str) {
    let base = format!("sys/class/power_supply/{name}");
    write(root, &format!("{base}/type"), "Battery\n");
    write(root, &format!("{base}/status"), &format!("{status}\n"));
    write(root, &format!("{base}/capacity"), &format!("{capacity}\n"));
    write(root, &format!("{base}/present"), &format!("{present}\n"));
}

/// A DRM connector directory such as `card0-eDP-1`.
fn connector(root: &Path, name: &str, status: &str, enabled: Option<&str>) {
    let base = format!("sys/class/drm/{name}");
    write(root, &format!("{base}/status"), &format!("{status}\n"));
    if let Some(enabled) = enabled {
        write(root, &format!("{base}/enabled"), &format!("{enabled}\n"));
    }
}

/// Hybrid laptop: Intel iGPU and NVIDIA dGPU, an audio device that must be
/// ignored, one battery and an AC adapter, and an ACPI lid.
fn laptop(root: &Path) {
    dmi(root, "sys_vendor", "LENOVO\n");
    dmi(root, "product_name", "21HMCTO1WW\n");
    dmi(root, "product_version", "ThinkPad X1 Carbon Gen 11\n");
    dmi(root, "board_name", "21HMCTO1WW\n");
    dmi(root, "chassis_type", "10\n");

    write(
        root,
        "proc/cpuinfo",
        "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Core(TM) Ultra 7\nflags\t\t: fpu vme sse2 avx2\n\nprocessor\t: 1\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Core(TM) Ultra 7\nflags\t\t: fpu vme sse2 avx2\n",
    );

    write(root, "sys/class/power_supply/AC/type", "Mains\n");
    battery(root, "BAT0", "Discharging", "87", "1");

    write(
        root,
        "proc/acpi/button/lid/LID0/state",
        "state:      open\n",
    );

    pci(root, "0000-00-02.0", "0x030000", "0x8086", "0x9a49");
    pci(root, "0000-01-00.0", "0x030200", "0x10de", "0x25a2");
    pci(root, "0000-00-1f.3", "0x040300", "0x8086", "0x51ca");

    // The panel is connected and enabled; the HDMI port is idle and disabled.
    connector(root, "card0-eDP-1", "connected", Some("enabled"));
    connector(root, "card0-HDMI-A-1", "disconnected", Some("disabled"));
    // Card and render nodes are not outputs and must not be listed.
    write(root, "sys/class/drm/card0/dev", "226:0\n");
    write(root, "sys/class/drm/renderD128/dev", "226:128\n");
    write(root, "sys/class/drm/version", "i915\n");
}

/// Desktop with an AMD GPU and a mains supply but no battery and no lid.
fn desktop(root: &Path) {
    dmi(root, "sys_vendor", "ASUSTeK COMPUTER INC.\n");
    dmi(root, "product_name", "System Product Name\n");
    dmi(root, "chassis_type", "3\n");

    write(root, "sys/class/power_supply/AC/type", "Mains\n");

    pci(root, "0000-03-00.0", "0x030000", "0x1002", "0x73df");

    connector(root, "card1-DP-1", "connected", Some("enabled"));
}

/// QEMU guest: a virtual VGA device the reader must report by id only.
fn vm(root: &Path) {
    dmi(root, "sys_vendor", "QEMU\n");
    dmi(root, "product_name", "Standard PC (Q35 + ICH9, 2009)\n");
    dmi(root, "chassis_type", "1\n");

    pci(root, "0000-00-01.0", "0x030000", "0x1234", "0x1111");
    connector(root, "card0-Virtual-1", "unknown", None);
}

/// Laptop with two batteries (one charging, one full and absent) and an
/// external DisplayPort monitor beside the built-in panel.
fn two_batteries_and_external_display(root: &Path) {
    dmi(root, "sys_vendor", "Framework\n");
    dmi(root, "product_name", "Laptop 16\n");
    dmi(root, "chassis_type", "10\n");

    write(root, "sys/class/power_supply/AC/type", "Mains\n");
    battery(root, "BAT0", "Charging", "45", "1");
    battery(root, "BAT1", "Full", "100", "0");

    connector(root, "card1-DP-2", "connected", Some("enabled"));
    connector(root, "card0-eDP-1", "connected", Some("enabled"));
    connector(root, "card1-HDMI-A-1", "disconnected", None);
}

#[test]
fn laptop_fixture_reports_identity_power_and_gpus() {
    let dir = tempfile::tempdir().expect("tempdir");
    laptop(dir.path());
    let f = facts(dir.path());

    assert_eq!(f.dmi.sys_vendor.as_deref(), Some("LENOVO"));
    assert_eq!(f.dmi.product_name.as_deref(), Some("21HMCTO1WW"));
    assert_eq!(
        f.dmi.product_version.as_deref(),
        Some("ThinkPad X1 Carbon Gen 11")
    );
    assert_eq!(f.dmi.board_name.as_deref(), Some("21HMCTO1WW"));
    assert_eq!(f.dmi.chassis_type, Some(10));
    assert_eq!(f.dmi.chassis_kind, Some(ChassisKind::Notebook));

    assert_eq!(f.power.battery, Some(true));
    assert_eq!(f.power.lid, Some(true));

    assert_eq!(f.cpu.vendor.as_deref(), Some("GenuineIntel"));
    assert_eq!(
        f.cpu.model_name.as_deref(),
        Some("Intel(R) Core(TM) Ultra 7")
    );
    assert_eq!(f.cpu.cpu_part, None);
    assert_eq!(f.cpu.logical_count, Some(2));
    let flags = f.cpu.flags.expect("flags present");
    assert_eq!(flags.len(), 4);
    assert!(flags.contains("avx2"));

    assert_eq!(
        f.pci_display.len(),
        2,
        "audio device 0x0403 must be excluded"
    );
    let intel = &f.pci_display[0];
    assert_eq!(intel.slot, "0000-00-02.0");
    assert_eq!(intel.class, "0x030000");
    assert_eq!(intel.vendor_id.as_deref(), Some("0x8086"));
    assert_eq!(intel.vendor_name.as_deref(), Some("Intel"));
    assert_eq!(intel.device_id.as_deref(), Some("0x9a49"));
    let nvidia = &f.pci_display[1];
    assert_eq!(nvidia.slot, "0000-01-00.0");
    assert_eq!(nvidia.class, "0x030200");
    assert_eq!(nvidia.vendor_id.as_deref(), Some("0x10de"));
    assert_eq!(nvidia.vendor_name.as_deref(), Some("NVIDIA"));
    assert_eq!(nvidia.device_id.as_deref(), Some("0x25a2"));
}

#[test]
fn desktop_without_battery_or_lid() {
    let dir = tempfile::tempdir().expect("tempdir");
    desktop(dir.path());
    let f = facts(dir.path());

    assert_eq!(f.dmi.chassis_kind, Some(ChassisKind::Desktop));
    assert_eq!(f.dmi.board_name, None);
    assert_eq!(f.power.battery, Some(false), "mains only, no Battery type");
    assert_eq!(f.power.lid, None, "no ACPI lid directory");

    assert_eq!(f.pci_display.len(), 1);
    let amd = &f.pci_display[0];
    assert_eq!(amd.vendor_name.as_deref(), Some("AMD"));
    assert_eq!(amd.device_id.as_deref(), Some("0x73df"));
}

#[test]
fn virtual_machine_reports_unnamed_vendor_by_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    vm(dir.path());
    let f = facts(dir.path());

    assert_eq!(f.dmi.sys_vendor.as_deref(), Some("QEMU"));
    assert_eq!(f.dmi.chassis_kind, Some(ChassisKind::Other));
    assert_eq!(f.power.battery, None, "no power_supply directory");
    assert_eq!(f.power.lid, None);

    assert_eq!(f.pci_display.len(), 1);
    let gpu = &f.pci_display[0];
    assert_eq!(gpu.vendor_id.as_deref(), Some("0x1234"));
    assert_eq!(gpu.vendor_name, None);
}

#[test]
fn arm64_cpuinfo_uses_arm_field_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "proc/cpuinfo",
        "processor\t: 0\nBogoMIPS\t: 50.00\nFeatures\t: fp asimd evtstrm aes asimdhp\nCPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x3\nCPU part\t: 0xd0c\nCPU revision\t: 1\n\nprocessor\t: 1\nFeatures\t: fp asimd evtstrm aes asimdhp\nCPU implementer\t: 0x41\nCPU part\t: 0xd0c\n",
    );
    let f = facts(dir.path());

    assert_eq!(f.cpu.vendor.as_deref(), Some("0x41"));
    assert_eq!(f.cpu.cpu_part.as_deref(), Some("0xd0c"));
    assert_eq!(f.cpu.model_name, None, "arm64 cpuinfo has no model name");
    assert_eq!(f.cpu.implementer.as_deref(), Some("Arm"));
    assert_eq!(f.cpu.logical_count, Some(2));
    let flags = f.cpu.flags.expect("Features read as flags");
    assert!(flags.contains("asimdhp"));
    assert!(flags.contains("fp"));
    assert_eq!(f.dmi, Default::default(), "no DMI files, so no DMI facts");
}

#[test]
fn empty_root_yields_all_absent_facts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let f = facts(dir.path());
    assert_eq!(f, Facts::default());
    assert_eq!(f.dmi.sys_vendor, None);
    assert_eq!(f.cpu.vendor, None);
    assert_eq!(f.cpu.logical_count, None);
    assert_eq!(f.power.battery, None);
    assert_eq!(f.power.lid, None);
    assert!(f.pci_display.is_empty());
}

#[test]
fn missing_root_yields_all_absent_facts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    assert_eq!(facts(&missing), Facts::default());
}

#[test]
fn oversized_sysfs_attribute_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    dmi(dir.path(), "product_name", &"A".repeat(8 * 1024));
    dmi(dir.path(), "sys_vendor", "ACME\n");
    let f = facts(dir.path());
    assert_eq!(f.dmi.product_name, None);
    assert_eq!(f.dmi.sys_vendor.as_deref(), Some("ACME"));
}

#[test]
fn oversized_cpuinfo_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let big = "processor\t: 0\n".repeat(600 * 1024);
    write(dir.path(), "proc/cpuinfo", &big);
    let f = facts(dir.path());
    assert_eq!(f.cpu, Default::default());
}

#[test]
fn bad_chassis_code_is_a_number_without_a_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    dmi(dir.path(), "chassis_type", "99\n");
    let f = facts(dir.path());
    assert_eq!(f.dmi.chassis_type, Some(99));
    assert_eq!(f.dmi.chassis_kind, None);
}

#[test]
fn facts_serialise_with_snake_case_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    laptop(dir.path());
    let value = serde_json::to_value(facts(dir.path())).expect("serialise");
    assert_eq!(value["dmi"]["chassis_kind"], "notebook");
    assert_eq!(value["power"]["battery"], true);
    assert_eq!(value["pci_display"][1]["vendor_name"], "NVIDIA");
}

#[cfg(unix)]
#[test]
fn symlink_leading_out_of_the_root_is_refused() {
    let outside = tempfile::tempdir().expect("outside dir");
    let secret = outside.path().join("product_name");
    fs::write(&secret, "SECRET\n").expect("write outside file");

    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("sys/class/dmi/id")).expect("create dir");
    std::os::unix::fs::symlink(&secret, dir.path().join("sys/class/dmi/id/product_name"))
        .expect("symlink");

    let f = facts(dir.path());
    assert_eq!(f.dmi.product_name, None);
}

#[cfg(unix)]
#[test]
fn symlink_that_stays_inside_the_root_is_followed() {
    let dir = tempfile::tempdir().expect("tempdir");
    pci(dir.path(), "0000-00-02.0", "0x030000", "0x8086", "0x9a49");
    fs::create_dir_all(dir.path().join("sys/devices/pci0000")).expect("create dir");
    fs::rename(
        dir.path().join("sys/bus/pci/devices/0000-00-02.0"),
        dir.path().join("sys/devices/pci0000/0000-00-02.0"),
    )
    .expect("move device");
    std::os::unix::fs::symlink(
        "../../../devices/pci0000/0000-00-02.0",
        dir.path().join("sys/bus/pci/devices/0000-00-02.0"),
    )
    .expect("symlink");

    let f = facts(dir.path());
    assert_eq!(f.pci_display.len(), 1);
    assert_eq!(f.pci_display[0].vendor_name.as_deref(), Some("Intel"));
}

#[test]
fn laptop_lists_batteries_and_drm_connectors() {
    let dir = tempfile::tempdir().expect("tempdir");
    laptop(dir.path());
    let f = facts(dir.path());

    let names: Vec<&str> = f.power_supplies.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["AC", "BAT0"]);
    let ac = &f.power_supplies[0];
    assert_eq!(ac.kind, Some(SupplyKind::Mains));
    assert_eq!(ac.status, None, "battery fields only for batteries");
    let bat = &f.power_supplies[1];
    assert_eq!(bat.supply_type.as_deref(), Some("Battery"));
    assert_eq!(bat.kind, Some(SupplyKind::Battery));
    assert_eq!(bat.status, Some(BatteryStatus::Discharging));
    assert_eq!(bat.capacity, Some(87));
    assert_eq!(bat.present, Some(true));

    let outputs: Vec<&str> = f.drm_outputs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(outputs, ["card0-HDMI-A-1", "card0-eDP-1"]);
    let hdmi = &f.drm_outputs[0];
    assert_eq!(hdmi.connector, "HDMI-A-1");
    assert_eq!(hdmi.status, Some(DrmStatus::Disconnected));
    assert_eq!(hdmi.enabled, Some(false));
    let panel = &f.drm_outputs[1];
    assert_eq!(panel.connector, "eDP-1");
    assert_eq!(panel.status, Some(DrmStatus::Connected));
    assert_eq!(panel.enabled, Some(true));

    assert_eq!(f.virtualization.likely_virtual, Some(false));
    assert_eq!(f.virtualization.hypervisor, None);
    assert!(f.virtualization.evidence.is_empty());
}

#[test]
fn desktop_lists_mains_and_one_display() {
    let dir = tempfile::tempdir().expect("tempdir");
    desktop(dir.path());
    let f = facts(dir.path());

    assert_eq!(f.power_supplies.len(), 1);
    assert_eq!(f.power_supplies[0].kind, Some(SupplyKind::Mains));
    assert_eq!(f.drm_outputs.len(), 1);
    assert_eq!(f.drm_outputs[0].name, "card1-DP-1");
    assert_eq!(f.drm_outputs[0].connector, "DP-1");
    assert_eq!(f.drm_outputs[0].status, Some(DrmStatus::Connected));
    assert_eq!(f.drm_outputs[0].enabled, Some(true));
    assert_eq!(f.virtualization.likely_virtual, Some(false));
}

#[test]
fn virtual_machine_reports_hypervisor_and_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    vm(dir.path());
    let f = facts(dir.path());

    assert_eq!(
        f.virtualization,
        Virtualization {
            hypervisor: Some("QEMU".to_owned()),
            hypervisor_type: None,
            evidence: vec!["sys_vendor contains \"QEMU\"".to_owned()],
            likely_virtual: Some(true),
        }
    );
    assert_eq!(f.drm_outputs.len(), 1);
    assert_eq!(f.drm_outputs[0].status, Some(DrmStatus::Unknown));
    assert_eq!(f.drm_outputs[0].enabled, None, "no enabled attribute");
}

#[test]
fn two_batteries_and_external_display_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    two_batteries_and_external_display(dir.path());
    let f = facts(dir.path());

    let names: Vec<&str> = f.power_supplies.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["AC", "BAT0", "BAT1"]);
    assert_eq!(f.power_supplies[1].status, Some(BatteryStatus::Charging));
    assert_eq!(f.power_supplies[1].capacity, Some(45));
    assert_eq!(f.power_supplies[1].present, Some(true));
    assert_eq!(f.power_supplies[2].status, Some(BatteryStatus::Full));
    assert_eq!(f.power_supplies[2].capacity, Some(100));
    assert_eq!(f.power_supplies[2].present, Some(false));

    let outputs: Vec<(&str, &str)> = f
        .drm_outputs
        .iter()
        .map(|o| (o.name.as_str(), o.connector.as_str()))
        .collect();
    assert_eq!(
        outputs,
        [
            ("card0-eDP-1", "eDP-1"),
            ("card1-DP-2", "DP-2"),
            ("card1-HDMI-A-1", "HDMI-A-1"),
        ]
    );
    assert_eq!(f.drm_outputs[1].status, Some(DrmStatus::Connected));
    assert_eq!(f.drm_outputs[2].status, Some(DrmStatus::Disconnected));
    assert_eq!(f.drm_outputs[2].enabled, None, "no enabled attribute");
    assert_eq!(f.virtualization.likely_virtual, Some(false));
}

#[test]
fn malformed_power_and_drm_values_are_absent_not_clamped() {
    let dir = tempfile::tempdir().expect("tempdir");
    battery(dir.path(), "BAT0", "Empty", "101", "yes");
    battery(dir.path(), "BAT1", "Full", "-5", "1");
    battery(dir.path(), "BAT2", "Charging", "50.0", "0");
    battery(dir.path(), "BAT3", "Discharging", " 50", "1");
    battery(dir.path(), "BAT4", "Full", "0", "1");
    battery(dir.path(), "BAT5", "Full", "100", "1");
    write(dir.path(), "sys/class/power_supply/WEIRD/type", "Gadget\n");
    connector(dir.path(), "card0-DP-1", "sleeping", Some("maybe"));

    let f = facts(dir.path());
    let by_name = |name: &str| {
        f.power_supplies
            .iter()
            .find(|s| s.name == name)
            .expect("supply listed")
    };
    let bad = by_name("BAT0");
    assert_eq!(bad.status, None, "Empty is not a documented state");
    assert_eq!(
        bad.capacity, None,
        "101 is out of range, not clamped to 100"
    );
    assert_eq!(bad.present, None, "yes is not a flag");
    assert_eq!(by_name("BAT1").capacity, None, "negative capacity");
    assert_eq!(by_name("BAT2").capacity, None, "fraction");
    assert_eq!(by_name("BAT3").capacity, None, "surrounding space");
    assert_eq!(by_name("BAT2").present, Some(false));
    assert_eq!(by_name("BAT4").capacity, Some(0), "zero is in range");
    assert_eq!(by_name("BAT5").capacity, Some(100), "100 is in range");

    let weird = by_name("WEIRD");
    assert_eq!(weird.supply_type.as_deref(), Some("Gadget"));
    assert_eq!(weird.kind, None, "undocumented type has no kind");
    assert_eq!(weird.capacity, None, "battery fields need a Battery type");

    assert_eq!(f.drm_outputs.len(), 1);
    assert_eq!(f.drm_outputs[0].status, None, "sleeping is not a status");
    assert_eq!(f.drm_outputs[0].enabled, None, "maybe is not enabled");
}

#[test]
fn power_supply_kinds_follow_the_kernel_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, kind) in [
        ("USB0", "USB"),
        ("USBC0", "USB_C"),
        ("PD0", "USB_PD_DRP"),
        ("UPS0", "UPS"),
    ] {
        write(
            dir.path(),
            &format!("sys/class/power_supply/{name}/type"),
            &format!("{kind}\n"),
        );
    }
    let f = facts(dir.path());
    let kind_of = |name: &str| {
        f.power_supplies
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.kind)
    };
    assert_eq!(kind_of("USB0"), Some(SupplyKind::Usb));
    assert_eq!(kind_of("USBC0"), Some(SupplyKind::UsbC));
    assert_eq!(kind_of("PD0"), Some(SupplyKind::UsbPdDrp));
    assert_eq!(kind_of("UPS0"), Some(SupplyKind::Ups));
}

#[test]
fn hypervisor_table_and_xen_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    dmi(dir.path(), "sys_vendor", "Microsoft Corporation\n");
    dmi(dir.path(), "product_name", "Surface Laptop 5\n");
    let f = facts(dir.path());
    assert_eq!(
        f.virtualization.likely_virtual,
        Some(false),
        "vendor name alone must not match Hyper-V"
    );

    let hyperv = tempfile::tempdir().expect("tempdir");
    dmi(hyperv.path(), "sys_vendor", "Microsoft Corporation\n");
    dmi(hyperv.path(), "product_name", "Virtual Machine\n");
    let f = facts(hyperv.path());
    assert_eq!(f.virtualization.hypervisor.as_deref(), Some("Hyper-V"));
    assert_eq!(f.virtualization.likely_virtual, Some(true));

    let vbox = tempfile::tempdir().expect("tempdir");
    dmi(vbox.path(), "sys_vendor", "innotek GmbH\n");
    dmi(vbox.path(), "product_name", "VirtualBox\n");
    let f = facts(vbox.path());
    assert_eq!(f.virtualization.hypervisor.as_deref(), Some("VirtualBox"));
    assert_eq!(
        f.virtualization.evidence,
        [
            "sys_vendor contains \"innotek GmbH\"",
            "product_name contains \"VirtualBox\"",
        ]
    );

    let xen = tempfile::tempdir().expect("tempdir");
    write(xen.path(), "sys/hypervisor/type", "xen\n");
    let f = facts(xen.path());
    assert_eq!(f.dmi, Default::default(), "no DMI, so only the hint");
    assert_eq!(f.virtualization.hypervisor, None);
    assert_eq!(f.virtualization.hypervisor_type.as_deref(), Some("xen"));
    assert_eq!(f.virtualization.likely_virtual, Some(true));
}

#[test]
fn no_identity_and_no_hint_is_unknown_not_false() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "proc/cpuinfo", "processor\t: 0\n");
    let f = facts(dir.path());
    assert_eq!(f.virtualization, Virtualization::default());
    assert_eq!(f.virtualization.likely_virtual, None);
}

#[test]
fn unknown_arm_implementer_stays_raw_and_x86_has_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "proc/cpuinfo",
        "processor\t: 0\nCPU implementer\t: 0x99\nCPU part\t: 0x001\n",
    );
    let f = facts(dir.path());
    assert_eq!(f.cpu.vendor.as_deref(), Some("0x99"));
    assert_eq!(f.cpu.implementer.as_deref(), Some("0x99"));

    let x86 = tempfile::tempdir().expect("tempdir");
    write(
        x86.path(),
        "proc/cpuinfo",
        "processor\t: 0\nvendor_id\t: AuthenticAMD\n",
    );
    let f = facts(x86.path());
    assert_eq!(f.cpu.vendor.as_deref(), Some("AuthenticAMD"));
    assert_eq!(f.cpu.implementer, None);
}

#[test]
fn first_slice_field_names_survive_serialisation() {
    let dir = tempfile::tempdir().expect("tempdir");
    laptop(dir.path());
    let f = facts(dir.path());
    let value = serde_json::to_value(&f).expect("serialise");

    for key in ["dmi", "cpu", "power", "pci_display"] {
        assert!(value.get(key).is_some(), "top-level field {key} kept");
    }
    for key in [
        "sys_vendor",
        "product_name",
        "product_version",
        "board_name",
        "chassis_type",
        "chassis_kind",
    ] {
        assert!(value["dmi"].get(key).is_some(), "dmi.{key} kept");
    }
    for key in ["vendor", "model_name", "cpu_part", "logical_count", "flags"] {
        assert!(value["cpu"].get(key).is_some(), "cpu.{key} kept");
    }
    assert_eq!(value["power"]["battery"], true);
    assert_eq!(value["power"]["lid"], true);
    for key in ["slot", "class", "vendor_id", "vendor_name", "device_id"] {
        assert!(value["pci_display"][0].get(key).is_some(), "pci.{key} kept");
    }
    assert_eq!(value["power_supplies"][1]["capacity"], 87);
    assert_eq!(value["drm_outputs"][1]["status"], "connected");
    assert_eq!(value["virtualization"]["likely_virtual"], false);

    let back: Facts = serde_json::from_value(value).expect("deserialise");
    assert_eq!(back, f);
}

#[test]
fn os_release_reports_id_like_and_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        "etc/os-release",
        "# Ubuntu\nNAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\nVERSION_CODENAME=noble\n",
    );
    let f = facts(dir.path());
    assert_eq!(f.os.id.as_deref(), Some("ubuntu"));
    assert_eq!(f.os.id_like, Some(vec!["debian".to_owned()]));
    assert_eq!(f.os.version_id.as_deref(), Some("24.04"));
    assert_eq!(f.os.codename.as_deref(), Some("noble"));

    let multi = tempfile::tempdir().expect("tempdir");
    write(
        multi.path(),
        "etc/os-release",
        "ID='fedora'\nID_LIKE=\"rhel fedora\"\nVERSION_ID=40\n",
    );
    let f = facts(multi.path());
    assert_eq!(f.os.id.as_deref(), Some("fedora"));
    assert_eq!(
        f.os.id_like,
        Some(vec!["rhel".to_owned(), "fedora".to_owned()])
    );
    assert_eq!(f.os.version_id.as_deref(), Some("40"));
    assert_eq!(f.os.codename, None, "no VERSION_CODENAME key");
}

#[test]
fn usr_lib_os_release_is_the_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "usr/lib/os-release", "ID=arch\n");
    let f = facts(dir.path());
    assert_eq!(f.os.id.as_deref(), Some("arch"));
    assert_eq!(f.os.id_like, None);
}

#[test]
fn missing_os_release_gives_null_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), "etc/os-release", "NAME=\"Only a name\"\n");
    let f = facts(dir.path());
    assert_eq!(f.os, Os::default());
    assert_eq!(f.os.id, None);
    assert_eq!(f.os.id_like, None);
    assert_eq!(f.os.version_id, None);

    let empty = tempfile::tempdir().expect("tempdir");
    assert_eq!(facts(empty.path()).os, Os::default());
}

#[cfg(unix)]
#[test]
fn os_release_symlink_leaving_the_root_is_refused() {
    let outside = tempfile::tempdir().expect("outside dir");
    let secret = outside.path().join("os-release");
    fs::write(&secret, "ID=evil\nVERSION_ID=1\n").expect("write outside file");

    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("etc")).expect("create dir");
    std::os::unix::fs::symlink(&secret, dir.path().join("etc/os-release")).expect("symlink");

    let f = facts(dir.path());
    assert_eq!(f.os, Os::default());
}

#[test]
fn first_slice_json_without_new_fields_still_reads() {
    let old = serde_json::json!({
        "dmi": {"sys_vendor": "LENOVO", "product_name": null, "product_version": null,
                "board_name": null, "chassis_type": 10, "chassis_kind": "notebook"},
        "cpu": {"vendor": "GenuineIntel", "model_name": null, "cpu_part": null,
                "logical_count": 2, "flags": null},
        "power": {"battery": true, "lid": null},
        "pci_display": []
    });
    let f: Facts = serde_json::from_value(old).expect("old shape still reads");
    assert_eq!(f.dmi.sys_vendor.as_deref(), Some("LENOVO"));
    assert_eq!(f.cpu.implementer, None);
    assert!(f.power_supplies.is_empty());
    assert!(f.drm_outputs.is_empty());
    assert_eq!(f.virtualization, Virtualization::default());
}
