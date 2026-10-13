// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Boot drills (`STORAGE.md`, "Boot drills"; v2 `raw_boot_test`,
//! `encrypted_boot_test`, `final_normal_boot_test`): for each boot member,
//! boot with that member offline and its ESP disabled, then bring it back
//! and resilver; then boot the whole pool. Each boot is proved by a changed
//! boot ID. The job asks for the restart, `bmacd` resumes it once the host
//! is back, and its checkpoints say which drill it was in.
//!
//! Unlike v2, an encrypted member stays in crypttab during its drill: the
//! passphrase file is gone by now, so nothing could reopen it afterwards.
//! The initramfs unlocks it with the rest and ZFS imports the pool without
//! it, which is the degraded boot the drill is for.

use std::time::Duration;

use bmac_adapters::programs;
use bmac_core::job::{Instruction, Severity};
use bmac_exec::Command;
use bmac_storage::{ById, Encryption};
use serde_json::{Value, json};

use super::{BOOT_PARTITION, PROXMOX_BOOT_TOOL, SLOW, Storage, mapper_path, part};
use crate::native::{Checkpoints, Stop};

const ESP_PARTITION: u32 = 2;
const MOUNT: &str = "/usr/bin/mount";
const UMOUNT: &str = "/usr/bin/umount";
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const MOUNTPOINT: &str = "/run/bmac/esp-drill";
const EFI: &str = "EFI";
const EFI_DISABLED: &str = "EFI.BMAC-DRILL";
const UEFI: &str = "/sys/firmware/efi";
/// How long to wait for the restart the job asked for before calling it
/// failed.
const RESTART_WITHIN: Duration = Duration::from_secs(10 * 60);

/// The checkpoint names inside the drills phase.
fn booted(drill: &str) -> String {
    format!("drills/{drill}/booted")
}
fn passed(drill: &str) -> String {
    format!("drills/{drill}/passed")
}

impl Storage<'_> {
    fn uefi(&self) -> bool {
        self.at(UEFI).exists()
    }

    /// Renames the member's `EFI` directory so the firmware can't boot from
    /// it (`disabled`), or back.
    async fn esp(&self, member: &ById, disabled: bool) -> Result<(), Stop> {
        if !self.uefi() {
            return Ok(());
        }
        let mountpoint = self.at(MOUNTPOINT);
        std::fs::create_dir_all(&mountpoint)
            .map_err(|e| Stop::failed(format!("cannot create {MOUNTPOINT}: {e}")))?;
        self.run(Command::new(MOUNT).args([part(member, ESP_PARTITION), MOUNTPOINT.to_string()]))
            .await?;
        let (from, to) = if disabled {
            (EFI, EFI_DISABLED)
        } else {
            (EFI_DISABLED, EFI)
        };
        let moved = (|| -> Result<(), String> {
            let (from, to) = (mountpoint.join(from), mountpoint.join(to));
            if from.is_dir() && !to.exists() {
                std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
            }
            if to.is_dir() && !from.exists() {
                Ok(())
            } else {
                Err(format!("expected only {} on the ESP", to.display()))
            }
        })();
        let unmounted = self.run(Command::new(UMOUNT).arg(MOUNTPOINT)).await;
        moved.map_err(|why| {
            Stop::failed(format!(
                "cannot {} the ESP of {}: {why}",
                if disabled { "disable" } else { "restore" },
                member.as_str()
            ))
        })?;
        unmounted.map(|_| ())
    }

    /// The name the pool lists the member under, and whether it's online.
    async fn leaf(&self, member: &ById) -> Result<(String, bool), Stop> {
        let disks = self.host.disks();
        let disk = self.disk(&disks, member)?;
        let device = if self.layout.encryption == Encryption::Luks2 {
            mapper_path(&self.mapper(disk)?)
        } else {
            part(member, BOOT_PARTITION)
        };
        let status = self.status().await?;
        let (_, leaf) = self.find(&status, &device).ok_or_else(|| {
            Stop::failed(format!("{} ({device}) isn't in the pool", member.as_str()))
        })?;
        Ok((leaf.name.clone(), leaf.state.as_deref() == Some("ONLINE")))
    }

    async fn healthy(&self) -> Result<(), Stop> {
        let mut waited = 0u32;
        loop {
            let status = self.status().await?;
            let resilvering = status
                .scan()
                .is_some_and(|s| s.contains("resilver in progress"));
            if !resilvering {
                if status.state() == Some("ONLINE") {
                    return Ok(());
                }
                return Err(Stop::failed(format!(
                    "{} is {}, not ONLINE",
                    self.layout.pool,
                    status.state().unwrap_or("in an unknown state")
                )));
            }
            if waited.is_multiple_of(20) {
                self.flow.info(format!(
                    "Resilvering: {}",
                    status.scan().unwrap_or_default()
                ));
            }
            waited += 1;
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Warns the operator, checkpoints, and restarts the host. Doesn't
    /// return unless the restart never comes.
    async fn restart(&self, boot_id: &str, why: &str) -> Result<(), Stop> {
        let host = crate::engine::listen::node_name();
        if self.layout.encryption == Encryption::Luks2 {
            self.flow
                .manual(
                    &format!("Open {host}'s console before it restarts"),
                    vec![
                        Instruction::Text(format!("{why} {host} restarts when you continue.")),
                        Instruction::Text(format!(
                            "Open {host}'s console (iDRAC's virtual console, another remote \
                             console, or a keyboard and screen) and type the LUKS passphrase \
                             when it asks."
                        )),
                        Instruction::Text(
                            "This page loses its connection while the host is down and \
                             reconnects when it's back; the job carries on by itself."
                                .into(),
                        ),
                    ],
                    Some("Restart now"),
                )
                .await?;
        } else {
            self.flow.info(format!(
                "{why} {host} restarts now; this page reconnects when it's back and the job \
                 carries on by itself."
            ));
        }
        self.flow.expect_restart(boot_id)?;
        let unit = format!("bmac-drill-restart-{}", std::process::id());
        self.run(Command::new(SYSTEMD_RUN).args([
            format!("--unit={unit}"),
            "--on-active=1s".into(),
            "--timer-property=AccuracySec=100ms".into(),
            "--collect".into(),
            programs::SYSTEMCTL.into(),
            "reboot".into(),
        ]))
        .await?;
        tokio::time::sleep(RESTART_WITHIN).await;
        Err(Stop::failed(format!(
            "{host} didn't restart within {} minutes of being asked",
            RESTART_WITHIN.as_secs() / 60
        )))
    }

    /// A boot with `member` offline and its ESP disabled; `None` for the
    /// boot of the whole pool.
    async fn drill(
        &self,
        done: &mut Checkpoints,
        name: &str,
        member: Option<&ById>,
        boot_id: &str,
    ) -> Result<(), Stop> {
        if done.is_done(&passed(name)) {
            return Ok(());
        }
        if !done.is_done(&booted(name)) {
            match self.flow.take_restart(boot_id)? {
                Some(true) => done.record(&booted(name), json!({ "boot_id": boot_id }))?,
                armed => {
                    if armed.is_none() {
                        self.healthy().await?;
                    }
                    let why = match member {
                        Some(member) => {
                            self.esp(member, true).await?;
                            let (leaf, online) = self.leaf(member).await?;
                            if online {
                                let pool = self.layout.pool.to_string();
                                self.run(Command::new(programs::ZPOOL).args([
                                    "offline",
                                    pool.as_str(),
                                    &leaf,
                                ]))
                                .await?;
                            }
                            format!(
                                "Drill: boot without {}, which is offline and can't be booted \
                                 from.",
                                member.as_str()
                            )
                        }
                        None => "Drill: boot the whole pool.".to_string(),
                    };
                    return self.restart(boot_id, &why).await;
                }
            }
        }
        match member {
            Some(member) => {
                let (leaf, online) = self.leaf(member).await?;
                if !online {
                    self.flow.info(format!(
                        "{} booted without {}.",
                        crate::engine::listen::node_name(),
                        member.as_str()
                    ));
                }
                self.esp(member, false).await?;
                if !online {
                    let pool = self.layout.pool.to_string();
                    self.run(Command::new(programs::ZPOOL).args(["online", pool.as_str(), &leaf]))
                        .await?;
                }
                self.healthy().await?;
                self.run(Command::new(PROXMOX_BOOT_TOOL).arg("refresh").timeout(SLOW))
                    .await?;
            }
            None => self.healthy().await?,
        }
        done.record(&passed(name), json!({}))?;
        self.flow.info(format!("The {name} drill passed."));
        Ok(())
    }

    /// The drills phase: asks whether to run them, then runs whichever
    /// haven't passed.
    pub async fn drills(&self, boot_id: &str) -> Result<Value, Stop> {
        let mut done = self.flow.checkpoints();
        let members = self.boot_vdev()?.members.clone();
        if !done.is_done("drills/asked") {
            let mut details: Vec<String> = members
                .iter()
                .map(|m| format!("Boot with {} offline, then resilver it.", m.as_str()))
                .collect();
            details.push("Boot the whole pool.".into());
            if self.layout.encryption == Encryption::Luks2 {
                details.push("Each boot waits for the passphrase at the console.".into());
            }
            let run = self
                .flow
                .confirm(
                    "Run the boot drills?",
                    "Each drill restarts this host to prove it boots with one boot disk \
                     missing. They take a few minutes each and can be skipped.",
                    Severity::Warning,
                    None,
                    details,
                )
                .await?;
            if !run {
                self.flow.info("The boot drills were skipped.");
                return Ok(json!({ "skipped": true }));
            }
            done.record("drills/asked", json!({}))?;
        }
        if members.len() < 2 {
            self.flow.warning(
                "A single boot disk has no other disk to boot from, so only the whole-pool \
                 drill runs.",
            );
        } else {
            for (i, member) in members.iter().enumerate() {
                self.drill(
                    &mut done,
                    &format!("member-{}", i + 1),
                    Some(member),
                    boot_id,
                )
                .await?;
            }
        }
        self.drill(&mut done, "whole-pool", None, boot_id).await?;
        Ok(json!({ "skipped": false }))
    }
}
