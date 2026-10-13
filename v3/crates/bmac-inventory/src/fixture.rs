// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Fixture `/sys`, `/dev`, `/proc`, and udev trees for tests, here and in
//! the Deployer. Device nodes are sparse files of the disk's size.

use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::luks::fake_header;
use crate::root::Root;
use crate::zfs::write_label;

#[derive(Debug, Clone)]
pub struct DiskSpec {
    pub name: String,
    pub size_bytes: u64,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub wwn: Option<String>,
    pub logical: u32,
    pub physical: u32,
    pub rotational: bool,
    pub removable: bool,
    /// Write a udev database entry; without one, values come from sysfs.
    pub udev: bool,
    /// udev's `ID_BUS`, such as `ata` or `scsi`.
    pub bus: Option<String>,
    /// SCSI disks: udev puts the serial in `ID_SCSI_SERIAL` and the WWN in
    /// `ID_SERIAL_SHORT`.
    pub scsi: bool,
    /// (number, start, size) in bytes.
    pub partitions: Vec<(u32, u64, u64)>,
}

impl DiskSpec {
    pub fn nvme(name: &str, serial: &str, size_bytes: u64) -> DiskSpec {
        DiskSpec {
            name: name.into(),
            size_bytes,
            serial: Some(serial.into()),
            model: Some("Dell Ent NVMe v2 AGN RI U.2 1.92TB".into()),
            wwn: Some(format!("eui.{:016x}", fnv(serial))),
            logical: 512,
            physical: 512,
            rotational: false,
            removable: false,
            udev: true,
            bus: None,
            scsi: false,
            partitions: Vec::new(),
        }
    }

    pub fn sata(name: &str, serial: &str, size_bytes: u64) -> DiskSpec {
        DiskSpec {
            name: name.into(),
            size_bytes,
            serial: Some(serial.into()),
            model: Some("ST4000NM0035".into()),
            wwn: Some(format!("0x5000c500{:08x}", fnv(serial) as u32)),
            logical: 512,
            physical: 4096,
            rotational: true,
            removable: false,
            udev: true,
            bus: Some("ata".into()),
            scsi: false,
            partitions: Vec::new(),
        }
    }

    /// A disk behind a SCSI or SAS controller (or QEMU's `scsi-hd`).
    pub fn scsi(name: &str, serial: &str, size_bytes: u64) -> DiskSpec {
        DiskSpec {
            model: Some("QEMU HARDDISK".into()),
            bus: Some("scsi".into()),
            scsi: true,
            ..DiskSpec::sata(name, serial, size_bytes)
        }
    }

    pub fn partition(mut self, number: u32, start_bytes: u64, size_bytes: u64) -> DiskSpec {
        self.partitions.push((number, start_bytes, size_bytes));
        self
    }

    /// A disk laid out as the Proxmox installer leaves a ZFS boot disk:
    /// BIOS boot, ESP, and the ZFS partition.
    pub fn proxmox_layout(self) -> DiskSpec {
        let mib = 1024 * 1024;
        let zfs_size = self.size_bytes - 1025 * mib - mib;
        self.partition(1, 17 * 1024, mib - 17 * 1024)
            .partition(2, mib, 1024 * mib)
            .partition(3, 1025 * mib, zfs_size)
    }
}

/// A stable stand-in WWN from a serial.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub fn partition_name(disk: &str, number: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{number}")
    } else {
        format!("{disk}{number}")
    }
}

pub struct Fixture {
    pub base: PathBuf,
    minor: u32,
}

fn write(path: &Path, text: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn sparse(path: &Path, size: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    File::create(path).unwrap().set_len(size).unwrap();
}

impl Fixture {
    pub fn new(base: &Path) -> Fixture {
        for dir in ["sys/block", "sys/class/net", "dev", "proc", "run/udev/data"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        Fixture {
            base: base.to_path_buf(),
            minor: 0,
        }
    }

    pub fn root(&self) -> Root {
        Root::under(&self.base)
    }

    pub fn device(&self, name: &str) -> PathBuf {
        self.base.join("dev").join(name)
    }

    pub fn disk(&mut self, spec: &DiskSpec) {
        let dir = self.base.join("sys/block").join(&spec.name);
        self.minor += 16;
        let dev = format!("8:{}", self.minor);
        write(&dir.join("size"), format!("{}\n", spec.size_bytes / 512));
        write(&dir.join("dev"), format!("{dev}\n"));
        write(
            &dir.join("removable"),
            if spec.removable { "1\n" } else { "0\n" },
        );
        write(
            &dir.join("queue/logical_block_size"),
            format!("{}\n", spec.logical),
        );
        write(
            &dir.join("queue/physical_block_size"),
            format!("{}\n", spec.physical),
        );
        write(
            &dir.join("queue/rotational"),
            if spec.rotational { "1\n" } else { "0\n" },
        );
        if let Some(serial) = &spec.serial {
            // sysfs pads serials with spaces.
            write(&dir.join("device/serial"), format!("{serial}    \n"));
        }
        if let Some(model) = &spec.model {
            write(&dir.join("device/model"), format!("{model}   \n"));
        }
        if let Some(wwn) = &spec.wwn {
            write(&dir.join("wwid"), format!("{wwn}\n"));
        }
        if spec.udev {
            let mut entry = String::new();
            match (&spec.serial, &spec.wwn) {
                (Some(serial), Some(wwn)) if spec.scsi => {
                    entry.push_str(&format!("E:ID_SCSI_SERIAL={serial}\n"));
                    entry.push_str(&format!(
                        "E:ID_SERIAL_SHORT={}\n",
                        wwn.trim_start_matches("0x")
                    ));
                }
                (Some(serial), _) => entry.push_str(&format!("E:ID_SERIAL_SHORT={serial}\n")),
                _ => {}
            }
            if let Some(model) = &spec.model {
                entry.push_str(&format!("E:ID_MODEL_ENC={}\n", model.replace(' ', "\\x20")));
            }
            if let Some(wwn) = &spec.wwn {
                entry.push_str(&format!("E:ID_WWN={wwn}\n"));
            }
            if let (Some(model), Some(serial)) = (&spec.model, &spec.serial) {
                let id_serial = format!("{}_{serial}", model.replace(' ', "_"));
                let bus = spec.bus.as_deref().unwrap_or("nvme");
                entry.push_str(&format!("S:disk/by-id/{bus}-{id_serial}\n"));
                entry.push_str(&format!("E:ID_SERIAL={id_serial}\n"));
            }
            if let Some(wwn) = &spec.wwn {
                entry.push_str(&format!("S:disk/by-id/wwn-{wwn}\n"));
            }
            if let Some(bus) = &spec.bus {
                entry.push_str(&format!("E:ID_BUS={bus}\n"));
                if bus == "ata" {
                    entry.push_str("E:ID_ATA_SATA=1\n");
                }
            }
            write(
                &self.base.join("run/udev/data").join(format!("b{dev}")),
                entry,
            );
        }
        sparse(&self.device(&spec.name), spec.size_bytes);
        for &(number, start, size) in &spec.partitions {
            let name = partition_name(&spec.name, number);
            let part = dir.join(&name);
            write(&part.join("partition"), format!("{number}\n"));
            write(&part.join("start"), format!("{}\n", start / 512));
            write(&part.join("size"), format!("{}\n", size / 512));
            sparse(&self.device(&name), size);
        }
    }

    /// Writes a ZFS label nvlist into every label slot of a device.
    pub fn label(&self, device: &str, nvlist: &[u8]) {
        write_label(&self.device(device), nvlist).unwrap();
    }

    pub fn luks(&self, device: &str, version: u16, uuid: &str) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(self.device(device))
            .unwrap();
        file.write_all(&fake_header(version, uuid)).unwrap();
    }

    pub fn gpt(&self, disk: &str, logical: u64) {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(self.device(disk))
            .unwrap();
        file.seek(SeekFrom::Start(510)).unwrap();
        file.write_all(&[0x55, 0xaa]).unwrap();
        file.seek(SeekFrom::Start(logical)).unwrap();
        file.write_all(b"EFI PART").unwrap();
    }

    /// A physical NIC. `carrier` is `None` for a NIC that is down (reading
    /// carrier fails); `speed` is what sysfs reports, -1 when unknown.
    pub fn nic(&self, name: &str, mac: &str, carrier: Option<bool>, speed: i64, driver: &str) {
        let dir = self.base.join("sys/class/net").join(name);
        write(&dir.join("address"), format!("{mac}\n"));
        write(&dir.join("type"), "1\n");
        write(
            &dir.join("operstate"),
            if carrier == Some(true) {
                "up\n"
            } else {
                "down\n"
            },
        );
        write(&dir.join("speed"), format!("{speed}\n"));
        write(
            &dir.join("duplex"),
            if speed > 0 { "full\n" } else { "unknown\n" },
        );
        match carrier {
            Some(up) => write(&dir.join("carrier"), if up { "1\n" } else { "0\n" }),
            None => {
                let _ = fs::remove_file(dir.join("carrier"));
            }
        }
        let drivers = self.base.join("sys/bus/pci/drivers").join(driver);
        fs::create_dir_all(&drivers).unwrap();
        fs::create_dir_all(dir.join("device")).unwrap();
        let _ = std::os::unix::fs::symlink(&drivers, dir.join("device/driver"));
        let _ =
            std::os::unix::fs::symlink(self.base.join("sys/bus/pci"), dir.join("device/subsystem"));
    }

    /// Sets carrier and speed, as a link coming up does.
    pub fn set_link(&self, name: &str, carrier: bool, speed: i64) {
        let dir = self.base.join("sys/class/net").join(name);
        write(&dir.join("carrier"), if carrier { "1\n" } else { "0\n" });
        write(&dir.join("speed"), format!("{speed}\n"));
        write(
            &dir.join("duplex"),
            if speed > 0 { "full\n" } else { "unknown\n" },
        );
    }

    /// A bridge, VLAN, or loopback: no backing device.
    pub fn virtual_nic(&self, name: &str, mac: &str, kind: &str) {
        let dir = self.base.join("sys/class/net").join(name);
        write(&dir.join("address"), format!("{mac}\n"));
        write(&dir.join("type"), format!("{kind}\n"));
        write(&dir.join("carrier"), "1\n");
    }

    pub fn wireless_nic(&self, name: &str, mac: &str) {
        self.nic(name, mac, Some(true), -1, "iwlwifi");
        fs::create_dir_all(self.base.join("sys/class/net").join(name).join("wireless")).unwrap();
    }

    pub fn cpuinfo(&self, sockets: u32, cores: u32, threads_per_core: u32, model: &str) {
        let mut text = String::new();
        let mut processor = 0;
        for socket in 0..sockets {
            for core in 0..cores {
                for _ in 0..threads_per_core {
                    text.push_str(&format!(
                        "processor\t: {processor}\nmodel name\t: {model}\nphysical id\t: {socket}\ncore id\t\t: {core}\n\n"
                    ));
                    processor += 1;
                }
            }
        }
        write(&self.base.join("proc/cpuinfo"), text);
    }

    pub fn meminfo(&self, kib: u64) {
        write(
            &self.base.join("proc/meminfo"),
            format!("MemTotal:       {kib} kB\nMemFree:        1024 kB\n"),
        );
    }

    /// UEFI boot, with Secure Boot on or off; `None` leaves the variable out.
    pub fn uefi(&self, secure_boot: Option<bool>) {
        let vars = self.base.join("sys/firmware/efi/efivars");
        fs::create_dir_all(&vars).unwrap();
        if let Some(on) = secure_boot {
            write(
                &vars.join("SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"),
                [6, 0, 0, 0, u8::from(on)],
            );
        }
    }

    pub fn dmi(&self, vendor: &str, product: &str, serial: &str) {
        let dir = self.base.join("sys/class/dmi/id");
        write(&dir.join("sys_vendor"), format!("{vendor}\n"));
        write(&dir.join("product_name"), format!("{product}\n"));
        write(&dir.join("product_serial"), format!("{serial}\n"));
    }
}
