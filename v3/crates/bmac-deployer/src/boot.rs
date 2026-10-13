// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Booting the target from an ISO (`DEPLOYMENT.md`, "Booting the target").
//! The operator always mounts and boots the ISO; the Deployer gives the
//! steps for the chosen method, the destructive warning that names the
//! disks by identity, and then waits for the machine on the tailnet, after
//! v2's `installation_gate`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootMethod {
    /// A Dell iDRAC's Virtual Console, with its Virtual Media.
    Idrac,
    /// Any other remote console: another vendor's BMC, a KVM over IP, or a
    /// provider's console.
    RemoteConsole,
    /// The operator writes the ISO to a USB stick.
    Usb,
}

/// A disk the installer erases, as the warning shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasedDisk {
    pub name: String,
    pub model: Option<String>,
    pub identity: String,
    pub size_bytes: u64,
}

/// What the instructions name.
#[derive(Debug, Clone)]
pub struct InstallTarget {
    pub host: String,
    pub fqdn: String,
    pub public_ip: String,
    pub iso_path: String,
    pub erased: Vec<ErasedDisk>,
    pub public_mac: String,
    pub tailscale_name: String,
    pub tag: String,
    /// The iDRAC's address, when the operator gave one.
    pub idrac: Option<String>,
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The destructive warning shown before booting the installer.
pub fn destructive_warning(target: &InstallTarget) -> Vec<String> {
    let mut lines = vec![format!(
        "Installing erases these disks on {} and installs Proxmox VE on them:",
        target.host
    )];
    for disk in &target.erased {
        lines.push(format!(
            "{} ({}, {}, {})",
            disk.name,
            disk.model.as_deref().unwrap_or("unknown model"),
            disk.identity,
            size(disk.size_bytes)
        ));
    }
    lines.push(
        "The installer selects them by serial number or WWN, never by device name. Every other \
         disk is left as it is."
            .into(),
    );
    lines.push(format!(
        "The installed system uses the network port with MAC address {}.",
        target.public_mac.to_ascii_uppercase()
    ));
    lines
}

/// The iDRAC's own web interface, for the manual steps.
pub fn idrac_console_url(address: &str) -> String {
    format!("https://{address}/restgui/start.html")
}

/// Mounting `iso` and booting the machine from it once.
fn boot_steps(method: BootMethod, iso: &str, idrac: Option<&str>) -> Vec<String> {
    match method {
        BootMethod::Idrac => vec![
            match idrac {
                Some(address) => format!(
                    "Open the iDRAC at {}, log in, and launch the Virtual Console.",
                    idrac_console_url(address)
                ),
                None => "Open the machine's iDRAC web interface, log in, and launch the Virtual \
                         Console."
                    .to_string(),
            },
            "In the Virtual Console, click Virtual Media, then Connect Virtual Media.".to_string(),
            format!("Under Map CD/DVD, choose {iso}, then click Map Device."),
            "Close the Virtual Media dialog, click Boot, choose Virtual CD/DVD/ISO, and confirm. \
             This boots from it once."
                .to_string(),
            format!(
                "Click Power and choose Power On System. If the machine is already on, choose \
                 Power Off System first and wait until it is off, not Reset or Power Cycle: \
                 {WARM_REBOOT}"
            ),
            "Keep the Virtual Console open until the end: the iDRAC reads the ISO through your \
             browser, and closing the console unmaps it."
                .to_string(),
        ],
        BootMethod::RemoteConsole => vec![
            "Open the machine's remote console.".to_string(),
            format!("Mount {iso} as a virtual CD or DVD."),
            format!(
                "Boot the machine once from that virtual media. If it is already on, power it \
                 off fully and wait until it is off before powering it on: {WARM_REBOOT}"
            ),
        ],
        BootMethod::Usb => vec![
            format!(
                "Write {iso} to a USB stick, for example with: sudo dd if='{iso}' of=/dev/sdX \
                 bs=4M conv=fsync status=progress (replace /dev/sdX with the stick; everything \
                 on it is erased)."
            ),
            format!(
                "Plug the stick into the machine and boot it once from the stick. If it is \
                 already on, power it off fully and wait until it is off before powering it on: \
                 {WARM_REBOOT}"
            ),
        ],
    }
}

/// What to do when a DHCP-only discovery ISO never reaches the tailnet.
const DHCP_FALLBACK: &str = "This ISO has no network settings, so it asks DHCP for an \
     address on each port in turn, three times over. Many data centers have no DHCP. If the \
     console says no port reached the internet, or the machine never joins the tailnet, cancel \
     this task and start it again with the public port's static address, prefix length, \
     gateway, DNS servers, and MAC address from your provider; that builds a new ISO to boot.";

const WARM_REBOOT: &str = "a reboot that doesn't power the machine off can hang early in the \
                           ISO's boot.";

/// Taking the media out again, once `when`.
fn removal_step(method: BootMethod, when: &str) -> String {
    match method {
        BootMethod::Idrac => format!(
            "{when}, click Virtual Media, then Disconnect Virtual Media, and confirm with Yes."
        ),
        BootMethod::RemoteConsole => format!("{when}, unmount the virtual media."),
        BootMethod::Usb => format!(
            "{when}, remove the stick. Wipe it afterwards: it holds the host's private SSH key \
             and root password hash."
        ),
    }
}

/// The operator's steps for installing a host.
pub fn steps(method: BootMethod, target: &InstallTarget) -> Vec<String> {
    let mut steps = boot_steps(method, &target.iso_path, target.idrac.as_deref());
    steps.push(
        "At the boot menu, keep the default \"Install Proxmox VE (Automated)\". The installation \
         runs unattended and reboots into the installed system."
            .into(),
    );
    steps.push(removal_step(
        method,
        "When the installed system's login prompt appears",
    ));
    steps.push(format!(
        "The Deployer then waits for {} to join the tailnet with {}. A machine of that name left \
         from an earlier install is ignored; only one that joined after this ISO was built counts.",
        target.tailscale_name, target.tag
    ));
    steps
}

/// The operator's steps for a discovery session; `dhcp` when the ISO has
/// no static network settings.
pub fn discovery_steps(
    method: BootMethod,
    iso: &str,
    idrac: Option<&str>,
    dhcp: bool,
) -> Vec<String> {
    let mut steps = boot_steps(method, iso, idrac);
    steps.push(
        "The discovery OS starts by itself and writes nothing to the disks. A login prompt \
         appears on the console part way through: don't log in, just wait. About 30 seconds \
         later a \"ready\" banner between lines of = signs says the machine is online on your \
         tailnet, and the Deployer finds it and reads the hardware."
            .into(),
    );
    if dhcp {
        steps.push(DHCP_FALLBACK.into());
    }
    let removal = removal_step(method, "Once the Deployer has powered it off");
    steps.push(match method {
        BootMethod::Usb => removal.replace(
            "it holds the host's private SSH key and root password hash",
            "it holds the session's SSH keys and Tailscale key",
        ),
        _ => removal,
    });
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> InstallTarget {
        InstallTarget {
            host: "mox1".into(),
            fqdn: "mox1.pve.internal".into(),
            public_ip: "203.0.113.33".into(),
            iso_path: "/home/op/isos/proxmox-ve_9.2-1-mox1-auto.iso".into(),
            erased: vec![
                ErasedDisk {
                    name: "nvme0n1".into(),
                    model: Some("Dell Ent NVMe".into()),
                    identity: "serial S1BOOT".into(),
                    size_bytes: 1_600_321_314_816,
                },
                ErasedDisk {
                    name: "nvme1n1".into(),
                    model: None,
                    identity: "wwn eui.0025".into(),
                    size_bytes: 1_600_321_314_816,
                },
            ],
            public_mac: "aa:bb:cc:dd:ee:ff".into(),
            tailscale_name: "mox1".into(),
            tag: "tag:bmac-proxmox-host".into(),
            idrac: Some("10.0.0.5".into()),
        }
    }

    #[test]
    fn the_warning_names_every_erased_disk_by_identity() {
        assert_eq!(
            destructive_warning(&target()),
            [
                "Installing erases these disks on mox1 and installs Proxmox VE on them:",
                "nvme0n1 (Dell Ent NVMe, serial S1BOOT, 1.6 TB)",
                "nvme1n1 (unknown model, wwn eui.0025, 1.6 TB)",
                "The installer selects them by serial number or WWN, never by device name. Every other disk is left as it is.",
                "The installed system uses the network port with MAC address AA:BB:CC:DD:EE:FF.",
            ]
        );
    }

    #[test]
    fn idrac_steps_connect_map_boot_once_and_disconnect() {
        assert_eq!(
            steps(BootMethod::Idrac, &target()),
            [
                "Open the iDRAC at https://10.0.0.5/restgui/start.html, log in, and launch the Virtual Console.",
                "In the Virtual Console, click Virtual Media, then Connect Virtual Media.",
                "Under Map CD/DVD, choose /home/op/isos/proxmox-ve_9.2-1-mox1-auto.iso, then click Map Device.",
                "Close the Virtual Media dialog, click Boot, choose Virtual CD/DVD/ISO, and confirm. This boots from it once.",
                "Click Power and choose Power On System. If the machine is already on, choose Power Off System first and wait until it is off, not Reset or Power Cycle: a reboot that doesn't power the machine off can hang early in the ISO's boot.",
                "Keep the Virtual Console open until the end: the iDRAC reads the ISO through your browser, and closing the console unmaps it.",
                "At the boot menu, keep the default \"Install Proxmox VE (Automated)\". The installation runs unattended and reboots into the installed system.",
                "When the installed system's login prompt appears, click Virtual Media, then Disconnect Virtual Media, and confirm with Yes.",
                "The Deployer then waits for mox1 to join the tailnet with tag:bmac-proxmox-host. A machine of that name left from an earlier install is ignored; only one that joined after this ISO was built counts.",
            ]
        );
    }

    #[test]
    fn idrac_steps_without_an_address_still_say_where_to_go() {
        let mut target = target();
        target.idrac = None;
        assert_eq!(
            steps(BootMethod::Idrac, &target)[0],
            "Open the machine's iDRAC web interface, log in, and launch the Virtual Console."
        );
    }

    #[test]
    fn remote_console_steps() {
        let steps = steps(BootMethod::RemoteConsole, &target());
        assert_eq!(steps[0], "Open the machine's remote console.");
        assert_eq!(
            steps[1],
            "Mount /home/op/isos/proxmox-ve_9.2-1-mox1-auto.iso as a virtual CD or DVD."
        );
        assert_eq!(
            steps[4],
            "When the installed system's login prompt appears, unmount the virtual media."
        );
        assert_eq!(steps.len(), 6);
    }

    #[test]
    fn usb_steps_say_to_wipe_the_stick() {
        let steps = steps(BootMethod::Usb, &target());
        assert!(
            steps[0]
                .starts_with("Write /home/op/isos/proxmox-ve_9.2-1-mox1-auto.iso to a USB stick")
        );
        assert!(steps[3].contains("Wipe it afterwards"));
        assert!(steps[3].contains("private SSH key and root password hash"));
    }

    #[test]
    fn discovery_steps_end_after_the_power_off() {
        let iso = "/home/op/isos/bmac-discovery-ab12.iso";
        let steps = discovery_steps(BootMethod::Idrac, iso, Some("10.0.0.5"), false);
        assert!(steps[2].contains(iso));
        assert!(steps[6].contains("writes nothing to the disks"));
        assert!(steps[6].contains("don't log in, just wait"));
        assert!(steps[6].contains("\"ready\" banner"));
        assert_eq!(
            steps[7],
            "Once the Deployer has powered it off, click Virtual Media, then Disconnect Virtual Media, and confirm with Yes."
        );
        assert_eq!(steps.len(), 8);
        let usb = discovery_steps(BootMethod::Usb, iso, None, false);
        assert!(
            usb.last()
                .unwrap()
                .contains("session's SSH keys and Tailscale key")
        );
        assert!(!usb.last().unwrap().contains("root password"));
    }

    #[test]
    fn a_dhcp_iso_says_to_rebuild_with_static_settings_if_it_never_joins() {
        let steps = discovery_steps(BootMethod::Idrac, "/d.iso", None, true);
        assert_eq!(steps.len(), 9);
        assert!(steps[7].contains("never joins the tailnet"));
        assert!(steps[7].contains("static address, prefix length, gateway, DNS servers, and MAC"));
        assert!(steps[8].contains("Disconnect Virtual Media"));
    }

    #[test]
    fn every_method_says_to_power_off_before_booting() {
        for method in [
            BootMethod::Idrac,
            BootMethod::RemoteConsole,
            BootMethod::Usb,
        ] {
            let steps = discovery_steps(method, "/d.iso", None, false);
            assert!(
                steps.iter().any(|s| s.contains("wait until it is off")),
                "{method:?}: {steps:?}"
            );
        }
    }

    #[test]
    fn the_console_link_is_idracs() {
        assert_eq!(
            idrac_console_url("10.0.0.5"),
            "https://10.0.0.5/restgui/start.html"
        );
    }
}
