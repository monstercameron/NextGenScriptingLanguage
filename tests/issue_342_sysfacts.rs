//! Fixture-root tests for `flow::sysfacts` (#342). Each test builds a fake
//! `/sys` and `/proc` tree in a temporary directory and reads it back.
//!
//! PCI slot names use `-` rather than `:` because `:` is not a valid file name
//! character on Windows. The reader does not depend on the slot format.

use std::fs;
use std::path::Path;

use flow::sysfacts::{facts, ChassisKind, Facts};

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
    write(root, "sys/class/power_supply/BAT0/type", "Battery\n");

    write(
        root,
        "proc/acpi/button/lid/LID0/state",
        "state:      open\n",
    );

    pci(root, "0000-00-02.0", "0x030000", "0x8086", "0x9a49");
    pci(root, "0000-01-00.0", "0x030200", "0x10de", "0x25a2");
    pci(root, "0000-00-1f.3", "0x040300", "0x8086", "0x51ca");
}

/// Desktop with an AMD GPU and a mains supply but no battery and no lid.
fn desktop(root: &Path) {
    dmi(root, "sys_vendor", "ASUSTeK COMPUTER INC.\n");
    dmi(root, "product_name", "System Product Name\n");
    dmi(root, "chassis_type", "3\n");

    write(root, "sys/class/power_supply/AC/type", "Mains\n");

    pci(root, "0000-03-00.0", "0x030000", "0x1002", "0x73df");
}

/// QEMU guest: a virtual VGA device the reader must report by id only.
fn vm(root: &Path) {
    dmi(root, "sys_vendor", "QEMU\n");
    dmi(root, "product_name", "Standard PC (Q35 + ICH9, 2009)\n");
    dmi(root, "chassis_type", "1\n");

    pci(root, "0000-00-01.0", "0x030000", "0x1234", "0x1111");
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
