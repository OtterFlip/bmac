// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup pool's encryption (`BACKUP.md`, "Appliance platform";
//! `STORAGE.md`, "Backup appliances"): LUKS2 on every member disk, ZFS on
//! the opened mappings, and `unlock` at the console after a reboot. Every
//! member's LUKS2 label names the pool, so `unlock` finds them with no state
//! from the OS disk, as after a reinstall. The pool's cache file is `none`
//! and nothing is in crypttab: nothing opens or imports it unattended.

use std::time::Duration;

use bmac_exec::{Command, CommandRunner, ExecError};
use serde::Deserialize;

use crate::pool::{ZPOOL, pool_guid};
use crate::terminal::Prompter;

pub const CRYPTSETUP: &str = "/usr/sbin/cryptsetup";
pub const BLKID: &str = "/usr/sbin/blkid";
pub const LSBLK: &str = "/usr/bin/lsblk";
pub const WIPEFS: &str = "/usr/sbin/wipefs";
const LABEL_PREFIX: &str = "bmac-backup:";
const BY_ID: &str = "/dev/disk/by-id/";
const SLOW: Duration = Duration::from_secs(600);
pub const MIN_PASSPHRASE: usize = 12;
const ATTEMPTS: usize = 3;

pub fn label(pool: &str) -> String {
    format!("{LABEL_PREFIX}{pool}")
}

pub fn mapper(luks_uuid: &str) -> String {
    format!("bmac-bk-{luks_uuid}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Stripe,
    Mirror,
    Raidz1,
    Raidz2,
    Raidz3,
}

impl Layout {
    pub fn parse(text: &str) -> Option<Layout> {
        Some(match text {
            "stripe" => Layout::Stripe,
            "mirror" => Layout::Mirror,
            "raidz1" => Layout::Raidz1,
            "raidz2" => Layout::Raidz2,
            "raidz3" => Layout::Raidz3,
            _ => return None,
        })
    }

    fn min_disks(self) -> usize {
        match self {
            Layout::Stripe => 1,
            Layout::Mirror => 2,
            Layout::Raidz1 => 3,
            Layout::Raidz2 => 4,
            Layout::Raidz3 => 5,
        }
    }

    fn vdev(self) -> Option<&'static str> {
        match self {
            Layout::Stripe => None,
            Layout::Mirror => Some("mirror"),
            Layout::Raidz1 => Some("raidz1"),
            Layout::Raidz2 => Some("raidz2"),
            Layout::Raidz3 => Some("raidz3"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Lsblk {
    blockdevices: Vec<Node>,
}

#[derive(Debug, Clone, Deserialize)]
struct Node {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    #[serde(default)]
    children: Vec<Node>,
}

impl Node {
    fn all(&self) -> Vec<&Node> {
        let mut out = vec![self];
        for child in &self.children {
            out.extend(child.all());
        }
        out
    }
}

/// One disk offered for the pool, as `lsblk` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub by_id: String,
    pub device: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    /// Why it can't be used: mounted, an open mapping, or in a pool.
    pub in_use: Option<String>,
    /// Signatures and partitions that formatting erases.
    pub erases: Vec<String>,
}

async fn run(runner: &dyn CommandRunner, command: Command) -> Result<String, String> {
    runner
        .run(command)
        .await
        .map(|o| o.stdout_text())
        .map_err(|e| e.to_string())
}

/// Checks a disk named by `/dev/disk/by-id/`, the only names that survive
/// reordering.
pub async fn inspect(runner: &dyn CommandRunner, by_id: &str) -> Result<Disk, String> {
    if !by_id.starts_with(BY_ID) || by_id.contains("/../") {
        return Err(format!("{by_id}: name disks by {BY_ID}…"));
    }
    if by_id
        .rsplit_once("-part")
        .is_some_and(|(_, n)| n.parse::<u32>().is_ok())
    {
        return Err(format!("{by_id} is a partition; give the whole disk"));
    }
    let text = run(
        runner,
        Command::new(LSBLK).args([
            "-J",
            "-b",
            "-p",
            "-o",
            "NAME,TYPE,SIZE,MODEL,SERIAL,FSTYPE,MOUNTPOINTS",
            by_id,
        ]),
    )
    .await?;
    let parsed: Lsblk = serde_json::from_str(&text).map_err(|e| format!("lsblk {by_id}: {e}"))?;
    let node = parsed
        .blockdevices
        .into_iter()
        .next()
        .ok_or_else(|| format!("lsblk shows nothing for {by_id}"))?;
    if node.kind != "disk" {
        return Err(format!("{by_id} is a {}, not a disk", node.kind));
    }
    let imported = run(runner, Command::new(ZPOOL).args(["status", "-LP"]))
        .await
        .unwrap_or_default();
    let in_pool = |name: &str| imported.split_whitespace().any(|word| word == name);
    let mut in_use = None;
    let mut erases = Vec::new();
    for part in node.all() {
        if let Some(mount) = part.mountpoints.iter().flatten().next() {
            in_use = Some(format!("{} is mounted at {mount}", part.name));
        } else if matches!(part.kind.as_str(), "crypt" | "lvm") || part.kind.starts_with("raid") {
            in_use = Some(format!("{} is open as {}", part.name, part.kind));
        } else if in_pool(&part.name) {
            in_use = Some(format!("{} is in an imported ZFS pool", part.name));
        }
        if in_use.is_some() {
            break;
        }
        if let Some(fstype) = &part.fstype {
            erases.push(format!("{} ({fstype})", part.name));
        } else if part.kind == "part" {
            erases.push(format!("{} (partition)", part.name));
        }
    }
    Ok(Disk {
        by_id: by_id.to_string(),
        device: node.name.clone(),
        size_bytes: node.size.unwrap_or(0),
        model: node.model.clone().unwrap_or_default().trim().to_string(),
        serial: node.serial.clone().unwrap_or_default().trim().to_string(),
        in_use,
        erases,
    })
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// Asks for a new passphrase twice.
fn new_passphrase(prompter: &mut dyn Prompter) -> Result<String, String> {
    let read = |p: &mut dyn Prompter, prompt: &str| {
        p.secret(prompt)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no passphrase was typed".to_string())
    };
    let first = read(prompter, "New LUKS passphrase: ")?;
    if first.chars().count() < MIN_PASSPHRASE {
        return Err(format!(
            "the passphrase must be at least {MIN_PASSPHRASE} characters"
        ));
    }
    if first.contains(['\n', '\r', '\0']) {
        return Err("the passphrase must be one line".into());
    }
    if read(prompter, "Again: ")? != first {
        return Err("the passphrases differ".into());
    }
    Ok(first)
}

/// `bmac-backupd create-pool`: LUKS2 on each disk, then the pool on the
/// mappings. Every disk is erased, after the operator types the pool's name.
pub async fn create(
    runner: &dyn CommandRunner,
    prompter: &mut dyn Prompter,
    pool: &str,
    layout: Layout,
    disks: &[String],
) -> Result<String, String> {
    if !crate::state::is_name(pool) {
        return Err("the pool name must be a lowercase DNS label".into());
    }
    if disks.len() < layout.min_disks() {
        return Err(format!(
            "this layout needs at least {} disks",
            layout.min_disks()
        ));
    }
    if pool_guid(runner, pool).await?.is_some() {
        return Err(format!("a pool named {pool} is already imported"));
    }
    let mut found = Vec::new();
    for by_id in disks {
        let disk = inspect(runner, by_id).await?;
        if found.iter().any(|d: &Disk| d.device == disk.device) {
            return Err(format!("{by_id} names a disk given twice"));
        }
        found.push(disk);
    }
    for disk in &found {
        prompter.say(&format!(
            "{}  {}  {}  {}  serial {}",
            disk.by_id,
            disk.device,
            gib(disk.size_bytes),
            disk.model,
            disk.serial
        ));
        if !disk.erases.is_empty() {
            prompter.say(&format!("    erases {}", disk.erases.join(", ")));
        }
    }
    if let Some(busy) = found.iter().find_map(|d| d.in_use.clone()) {
        return Err(format!("{busy}; it can't be used"));
    }
    prompter.say(&format!(
        "Every disk above is erased and encrypted for pool {pool}."
    ));
    let typed = prompter
        .line(&format!("Type the pool name ({pool}) to continue: "))
        .map_err(|e| e.to_string())?;
    if typed.as_deref().map(str::trim) != Some(pool) {
        return Err("not confirmed; nothing was changed".into());
    }
    let passphrase = new_passphrase(prompter)?;
    let mut mappings = Vec::new();
    for disk in &found {
        run(
            runner,
            Command::new(WIPEFS).args(["--all", "--force", &disk.device]),
        )
        .await?;
        run(
            runner,
            Command::new(CRYPTSETUP)
                .args([
                    "luksFormat",
                    "--batch-mode",
                    "--type",
                    "luks2",
                    "--label",
                    &label(pool),
                    "--key-file=-",
                    &disk.device,
                ])
                .secret_stdin(passphrase.clone())
                .timeout(SLOW),
        )
        .await?;
        let uuid = run(
            runner,
            Command::new(CRYPTSETUP).args(["luksUUID", &disk.device]),
        )
        .await?
        .trim()
        .to_string();
        let name = mapper(&uuid);
        run(
            runner,
            Command::new(CRYPTSETUP)
                .args(["open", "--key-file=-", &disk.device, &name])
                .secret_stdin(passphrase.clone())
                .timeout(SLOW),
        )
        .await?;
        mappings.push(format!("/dev/mapper/{name}"));
    }
    let mut args: Vec<String> = [
        "create",
        "-o",
        "ashift=12",
        "-o",
        "cachefile=none",
        "-O",
        "compression=on",
        "-O",
        "atime=off",
        "-O",
        "mountpoint=none",
        "-O",
        "canmount=off",
        pool,
    ]
    .map(String::from)
    .to_vec();
    args.extend(layout.vdev().map(String::from));
    args.extend(mappings);
    run(runner, Command::new(ZPOOL).args(args).timeout(SLOW)).await?;
    pool_guid(runner, pool)
        .await?
        .ok_or_else(|| format!("zpool create left no pool {pool}"))
}

/// One LUKS member of a backup pool, found by its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub device: String,
    pub luks_uuid: String,
    pub pool: String,
}

/// Every LUKS2 device whose label names a backup pool, read from the
/// devices themselves rather than a cache.
pub async fn members(runner: &dyn CommandRunner) -> Result<Vec<Member>, String> {
    let text = match runner
        .run(
            Command::new(BLKID)
                .args(["-c", "/dev/null", "-o", "export", "-t", "TYPE=crypto_LUKS"])
                .expect_codes(&[0, 2]),
        )
        .await
    {
        Ok(o) => o.stdout_text(),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let field = |key: &str| {
            block
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .map(str::to_string)
        };
        if let (Some(device), Some(uuid), Some(label)) =
            (field("DEVNAME"), field("UUID"), field("LABEL"))
            && let Some(pool) = label.strip_prefix(LABEL_PREFIX)
        {
            out.push(Member {
                device,
                luks_uuid: uuid,
                pool: pool.to_string(),
            });
        }
    }
    Ok(out)
}

async fn is_open(runner: &dyn CommandRunner, name: &str) -> bool {
    runner
        .run(Command::new(CRYPTSETUP).args(["status", name]))
        .await
        .is_ok()
}

/// What `unlock` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unlocked {
    pub pool: String,
    pub pool_guid: String,
    pub opened: usize,
    pub imported: bool,
}

/// `bmac-backupd unlock`: one passphrase opens every member of the pool,
/// then the pool is imported by GUID (or by name when this OS has never
/// seen it, before `adopt`), with nothing mounted.
pub async fn unlock(
    runner: &dyn CommandRunner,
    prompter: &mut dyn Prompter,
    bound: Option<(&str, &str)>,
) -> Result<Unlocked, String> {
    let all = members(runner).await?;
    let pool = match bound {
        Some((pool, _)) => pool.to_string(),
        None => {
            let mut pools: Vec<&str> = all.iter().map(|m| m.pool.as_str()).collect();
            pools.sort_unstable();
            pools.dedup();
            match pools.as_slice() {
                [one] => one.to_string(),
                [] => return Err("no disk here holds a backup pool".into()),
                many => {
                    return Err(format!(
                        "disks here hold several backup pools ({}); this appliance isn't bound to one",
                        many.join(", ")
                    ));
                }
            }
        }
    };
    let mine: Vec<&Member> = all.iter().filter(|m| m.pool == pool).collect();
    if mine.is_empty() {
        return Err(format!(
            "no disk labelled {} was found; are the pool's disks attached?",
            label(&pool)
        ));
    }
    let mut closed = Vec::new();
    for member in &mine {
        if !is_open(runner, &mapper(&member.luks_uuid)).await {
            closed.push(*member);
        }
    }
    if let Some(first) = closed.first() {
        let mut passphrase = None;
        for attempt in 1..=ATTEMPTS {
            let typed = prompter
                .secret(&format!("Passphrase for pool {pool}: "))
                .map_err(|e| e.to_string())?
                .ok_or("no passphrase was typed")?;
            let tested = runner
                .run(
                    Command::new(CRYPTSETUP)
                        .args(["open", "--test-passphrase", "--key-file=-", &first.device])
                        .secret_stdin(typed.clone())
                        .timeout(SLOW),
                )
                .await;
            match tested {
                Ok(_) => {
                    passphrase = Some(typed);
                    break;
                }
                Err(ExecError::UnexpectedExit { output, .. }) if output.code() == Some(2) => {
                    prompter.say(if attempt < ATTEMPTS {
                        "That passphrase doesn't open the pool; try again."
                    } else {
                        "That passphrase doesn't open the pool."
                    });
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let passphrase = passphrase.ok_or("the pool stays locked")?;
        for member in &closed {
            run(
                runner,
                Command::new(CRYPTSETUP)
                    .args([
                        "open",
                        "--key-file=-",
                        &member.device,
                        &mapper(&member.luks_uuid),
                    ])
                    .secret_stdin(passphrase.clone())
                    .timeout(SLOW),
            )
            .await
            .map_err(|e| format!("opening {}: {e}", member.device))?;
        }
    }
    let mut imported = false;
    let guid = match pool_guid(runner, &pool).await? {
        Some(guid) => guid,
        None => {
            let target = bound.map(|(_, guid)| guid).unwrap_or(&pool);
            run(
                runner,
                Command::new(ZPOOL)
                    .args([
                        "import",
                        "-d",
                        "/dev/mapper",
                        "-N",
                        "-o",
                        "cachefile=none",
                        target,
                    ])
                    .timeout(SLOW),
            )
            .await
            .map_err(|e| format!("importing pool {pool}: {e}"))?;
            imported = true;
            pool_guid(runner, &pool)
                .await?
                .ok_or_else(|| format!("pool {pool} isn't imported after zpool import"))?
        }
    };
    if let Some((_, want)) = bound
        && guid != want
    {
        return Err(format!(
            "the pool imported as {pool} has GUID {guid}, not this appliance's {want}"
        ));
    }
    Ok(Unlocked {
        pool,
        pool_guid: guid,
        opened: closed.len(),
        imported,
    })
}

#[cfg(test)]
#[path = "luks_tests.rs"]
mod tests;
