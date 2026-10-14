// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `storage.add_vdev` (v2 `add_new_disk_vdev.sh`) and `storage.attach_disk`.
//!
//! The chosen disks are erased before the journal records them, so a disk
//! the journal names was blanked by BMAC, and a later run carries on with
//! whatever partitions and LUKS it finds there rather than erasing again.

use std::collections::BTreeMap;

use bmac_core::DiskIdentity;
use bmac_core::job::{FieldType, Severity};
use bmac_storage::checks::Findings;
use bmac_storage::journal::Addition;
use bmac_storage::types::{data_type, helper};
use bmac_storage::{Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use serde_json::{Map, Value, json};

use super::{
    Candidate, Context, LUKS2_HEADER_BYTES, POOL_PHASE, Snapshot, Top, choice, field, list, named,
    online, refuse, text,
};
use crate::native::Stop;

const CHOOSE: (&str, &str) = ("choose", "Choose the disks");
const PREPARE: (&str, &str) = ("prepare", "Prepare the disks");
const FINISH: (&str, &str) = ("finish", "Finish");
const RESUME: &str = "resume";

fn class_help(class: VdevClass) -> String {
    match helper(class) {
        Some(h) => format!("{}. {} {}", h.does, h.pros, h.cons),
        None => "Holds the pool's data.".into(),
    }
}

fn kind_help(kind: VdevType) -> String {
    let info = data_type(kind, 2);
    format!(
        "Survives {}; {} usable. {}. {}.",
        info.survives.to_lowercase(),
        info.usable,
        info.pros,
        info.cons
    )
}

fn parse_class(text: &str) -> Option<VdevClass> {
    VdevClass::ALL.into_iter().find(|c| c.as_str() == text)
}

fn parse_kind(text: &str) -> Option<VdevType> {
    [
        VdevType::Single,
        VdevType::Mirror,
        VdevType::Raidz1,
        VdevType::Raidz2,
        VdevType::Raidz3,
    ]
    .into_iter()
    .find(|k| k.as_str() == text)
}

/// The new vdev the form describes, and its findings; field errors when
/// it can't be built.
pub fn proposal(
    snapshot: &Snapshot,
    pool: &bmac_core::PoolName,
    values: &Map<String, Value>,
) -> Result<(Vdev, Vec<DiskIdentity>, Findings), BTreeMap<String, String>> {
    let class =
        parse_class(&text(values, "class")).ok_or_else(|| refuse("class", "Choose a class."))?;
    let kind = parse_kind(&text(values, "type")).ok_or_else(|| refuse("type", "Choose a type."))?;
    if !class.allowed_types().contains(&kind) {
        let allowed: Vec<&str> = class.allowed_types().iter().map(|k| k.as_str()).collect();
        return Err(refuse(
            "type",
            format!("A {class} vdev can be {}.", allowed.join(", ")),
        ));
    }
    let chosen = list(values, "disks");
    let candidates = snapshot.candidates();
    let mut members = Vec::new();
    let mut identities = Vec::new();
    for name in &chosen {
        let c: &Candidate = candidates
            .iter()
            .find(|c| c.by_id.as_str() == name)
            .ok_or_else(|| refuse("disks", format!("{name} can't be used now.")))?;
        members.push(c.by_id.clone());
        identities.push(c.identity.clone());
    }
    if members.len() < kind.min_members() {
        return Err(refuse(
            "disks",
            format!("A {kind} vdev needs at least {} disks.", kind.min_members()),
        ));
    }
    if kind.max_members().is_some_and(|max| members.len() > max) {
        return Err(refuse(
            "disks",
            "A single-disk vdev takes exactly one disk.",
        ));
    }
    let vdev = Vdev::new(class, kind, members);
    let (_, findings) = snapshot.findings(pool, vdev.clone());
    if !findings.refused.is_empty() {
        let why: Vec<&str> = findings
            .refused
            .iter()
            .map(|f| f.message.as_str())
            .collect();
        return Err(refuse("disks", why.join(" ")));
    }
    Ok((vdev, identities, findings))
}

/// What adding `vdev` changes about what the pool allows later.
pub fn consequences(snapshot: &Snapshot, pool: &bmac_core::PoolName, vdev: &Vdev) -> Vec<String> {
    let mut out = Vec::new();
    let before = snapshot.pool_layout(pool, None);
    if vdev.kind.is_raidz() && before.allows_vdev_removal() {
        out.push(
            "The pool has no raidz vdev yet. After this one, no data or special vdev can ever \
             be removed from it."
                .into(),
        );
    }
    if matches!(vdev.class, VdevClass::Data | VdevClass::Special) && vdev.failures_tolerated() == 0
    {
        out.push(format!(
            "A {} survives no disk failure, and losing it loses the whole pool.",
            vdev.describe()
        ));
    }
    out
}

fn form_description(snapshot: &Snapshot, pool: &bmac_core::PoolName) -> String {
    let layout = snapshot.pool_layout(pool, None);
    let removal = if layout.allows_vdev_removal() {
        "The pool has no raidz vdev, so data and special vdevs can still be removed later; \
         adding raidz rules that out for good."
    } else {
        "The pool has raidz, so no data or special vdev can be removed from it."
    };
    format!(
        "{removal} Log, cache, and spare vdevs can always be removed. Every chosen disk is \
         erased. {}",
        if snapshot.encryption() == Encryption::Luks2 {
            "The pool is encrypted, so each disk gets LUKS2 with the pool's passphrase."
        } else {
            "The pool isn't encrypted."
        }
    )
}

fn disk_choices(snapshot: &Snapshot) -> Vec<bmac_core::job::FieldOption> {
    snapshot
        .candidates()
        .iter()
        .map(|c| {
            let state = if c.blank() {
                "blank"
            } else {
                "has old partitions or signatures, which are erased"
            };
            choice(c.by_id.as_str(), &c.describe(), Some(state.into()))
        })
        .collect()
}

/// The new vdev's form.
fn add_form(snapshot: &Snapshot, pool: &bmac_core::PoolName) -> Vec<bmac_core::job::Field> {
    let mut class = field("class", FieldType::Select, "Class");
    class.default = Some(json!("data"));
    class.options = VdevClass::ALL
        .iter()
        .map(|c| choice(c.as_str(), c.as_str(), Some(class_help(*c))))
        .collect();
    let mut kind = field("type", FieldType::Select, "Type");
    kind.default = Some(json!("mirror"));
    kind.options = [
        VdevType::Single,
        VdevType::Mirror,
        VdevType::Raidz1,
        VdevType::Raidz2,
        VdevType::Raidz3,
    ]
    .iter()
    .map(|k| choice(k.as_str(), data_type(*k, 2).label, Some(kind_help(*k))))
    .collect();
    kind.help = Some(
        "Special and log vdevs can be single disks or mirrors; cache and spare vdevs are \
         single disks."
            .into(),
    );
    let mut disks = field("disks", FieldType::Multiselect, "Disks");
    disks.options = disk_choices(snapshot);
    disks.min_selected = Some(1);
    disks.help = Some(format!(
        "Disks in no pool, recognized by WWN or serial, that nothing is using. {}",
        form_description(snapshot, pool)
    ));
    vec![class, kind, disks]
}

/// Finishes or offers the additions earlier runs recorded; the one to
/// carry on with.
async fn pending(cx: &Context<'_>, snapshot: &Snapshot) -> Result<Option<Addition>, Stop> {
    let in_pool: Vec<DiskIdentity> = snapshot
        .tops()
        .iter()
        .flat_map(|t| t.members.iter())
        .filter_map(|m| m.identity.clone())
        .collect();
    let mut open = Vec::new();
    for addition in &snapshot.journal.additions {
        if addition.members.iter().all(|m| in_pool.contains(m)) {
            cx.update(|s| {
                s.finish_addition(addition.number);
                Ok(())
            })?;
            cx.flow.info(format!(
                "New vdev {} ({} {}) is in the pool; an earlier run added it.",
                addition.number, addition.class, addition.kind
            ));
        } else {
            open.push(addition.clone());
        }
    }
    let Some(addition) = open.into_iter().next() else {
        return Ok(None);
    };
    let checkpoints = cx.flow.checkpoints();
    if checkpoints.get(RESUME).and_then(Value::as_u64) == Some(addition.number) {
        return Ok(Some(addition));
    }
    let disks: Vec<String> = addition.members.iter().map(named).collect();
    let mut how = field("choice", FieldType::Select, "What to do");
    how.default = Some(json!("resume"));
    how.options = vec![
        choice(
            "resume",
            "Carry on adding it",
            Some("Prepares whatever is left on the disks and adds them.".into()),
        ),
        choice(
            "abandon",
            "Forget it",
            Some("The disks stay as they are; Storage inventory lists them.".into()),
        ),
    ];
    let values = cx
        .flow
        .input_group(
            &format!("New vdev {} isn't finished", addition.number),
            Some(&format!(
                "An earlier run chose {} for a new {} {} vdev and stopped before adding it.",
                disks.join(", "),
                addition.class,
                addition.kind
            )),
            vec![how],
            |_| Ok(()),
        )
        .await?;
    if text(&values, "choice") == "abandon" {
        cx.update(|s| {
            s.finish_addition(addition.number);
            Ok(())
        })?;
        cx.flow
            .info(format!("Forgot new vdev {}.", addition.number));
        return Ok(None);
    }
    Ok(Some(addition))
}

async fn choose(cx: &Context<'_>, snapshot: &Snapshot) -> Result<Addition, Stop> {
    let fields = add_form(snapshot, &cx.pool);
    if fields[2].options.is_empty() {
        return Err(Stop::failed(
            "No disk can be used for a new vdev: every disk is in a pool, in use, chosen by \
             an unfinished workflow, or not recognizable by WWN or serial. Storage inventory \
             says which.",
        ));
    }
    let values = cx
        .flow
        .input_group(
            "The new vdev",
            Some(&form_description(snapshot, &cx.pool)),
            fields,
            |values| proposal(snapshot, &cx.pool, values).map(|_| ()),
        )
        .await?;
    let (vdev, identities, findings) = proposal(snapshot, &cx.pool, &values)
        .map_err(|e| Stop::failed(e.into_values().collect::<Vec<_>>().join(" ")))?;
    let mut details: Vec<String> = identities
        .iter()
        .filter_map(|i| snapshot.disk_with(i))
        .map(super::describe_disk)
        .collect();
    details.extend(
        findings
            .warnings
            .iter()
            .map(|w| format!("Warning: {}", w.message)),
    );
    details.extend(consequences(snapshot, &cx.pool, &vdev));
    details.push(plan(snapshot, &vdev));
    let confirmed = cx
        .flow
        .confirm(
            "Erase the disks and add the vdev",
            &format!(
                "Everything on these {} disks is erased, and they become a new {} of {}.",
                identities.len(),
                vdev.describe(),
                cx.pool
            ),
            Severity::Destructive,
            Some("erase"),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    for identity in &identities {
        let disk = cx.resolve(identity)?;
        cx.flow
            .info(format!("Erasing {}.", super::describe_disk(&disk)));
        cx.wipe_metadata(&disk).await?;
    }
    let number =
        cx.update(|s| s.begin_addition(vdev.class, vdev.kind, identities.clone(), cx.now()))?;
    Ok(Addition {
        number,
        class: vdev.class,
        kind: vdev.kind,
        members: identities,
        recorded_at: cx.now(),
    })
}

/// The steps, as the confirmation lists them.
fn plan(snapshot: &Snapshot, vdev: &Vdev) -> String {
    if snapshot.encryption() == Encryption::Luks2 {
        format!(
            "Plan: erase each disk; ask for the passphrase at the console and prove it opens \
             the pool's members; give each disk one partition and LUKS2; list them in crypttab \
             and prove the rebuilt initramfs unlocks them; then zpool add {} and back up the \
             new LUKS headers.",
            vdev.kind
        )
    } else {
        format!(
            "Plan: erase each disk, give it one partition, then zpool add {}.",
            vdev.kind
        )
    }
}

pub async fn add_vdev(cx: &Context<'_>) -> Result<(), Stop> {
    cx.phase(CHOOSE);
    let snapshot = cx.snapshot().await?;
    let addition = match pending(cx, &snapshot).await? {
        Some(addition) => addition,
        None => {
            let snapshot = cx.snapshot().await?;
            choose(cx, &snapshot).await?
        }
    };
    cx.flow
        .checkpoints()
        .record(RESUME, json!(addition.number))?;
    cx.done(CHOOSE);

    let snapshot = cx.snapshot().await?;
    let mut members = Vec::new();
    for identity in &addition.members {
        let disk = cx.resolve(identity)?;
        members.push(
            super::by_id(&disk).ok_or_else(|| {
                Stop::failed(format!("{} has no /dev/disk/by-id name", disk.name))
            })?,
        );
    }
    let vdev = Vdev::new(addition.class, addition.kind, members);
    let layout = snapshot.pool_layout(&cx.pool, Some(vdev.clone()));
    let index = layout.vdevs.len() - 1;
    let storage = cx.storage(&layout);
    let (prepared, mappers) = cx
        .with_key(&snapshot, async {
            cx.phase(PREPARE);
            let prepared = storage.prepare(index).await?;
            cx.done(PREPARE);
            cx.phase(POOL_PHASE);
            let mappers = storage.add_vdev(index).await?;
            cx.done(POOL_PHASE);
            Ok((prepared, mappers))
        })
        .await?;

    cx.phase(FINISH);
    if prepared.encrypted {
        cx.backup_headers(&prepared.members).await?;
    }
    cx.update(|s| {
        s.finish_addition(addition.number);
        Ok(())
    })?;
    for line in consequences(&snapshot, &cx.pool, &vdev) {
        cx.flow.info(line);
    }
    cx.flow
        .info(format!("Added a {} to {}.", vdev.describe(), cx.pool));
    cx.flow.result(json!({
        "vdev": vdev.describe(),
        "disks": addition.members.iter().map(|m| m.value().to_string()).collect::<Vec<_>>(),
        "mappers": mappers,
    }));
    cx.done(FINISH);
    Ok(())
}

// ---- attach ----

/// Whether ZFS can attach a disk to this vdev now; why not otherwise.
pub fn attachable(top: &Top<'_>, raidz_expansion: bool) -> Result<(), String> {
    if top.holds_esp {
        return Err("it is the boot vdev; replace its members instead".into());
    }
    if top.removing {
        return Err("it is being removed".into());
    }
    if top.replacing {
        return Err("a member is being replaced".into());
    }
    if !top.members.iter().all(online) {
        return Err("not every member is online".into());
    }
    match (top.class, top.kind) {
        (VdevClass::Cache | VdevClass::Spare, _) => {
            Err(format!("a {} device can't be widened", top.class))
        }
        (_, Some(VdevType::Single | VdevType::Mirror)) => Ok(()),
        (VdevClass::Data, Some(k)) if k.is_raidz() => {
            if raidz_expansion {
                Ok(())
            } else {
                Err("this host's OpenZFS lacks the raidz_expansion feature".into())
            }
        }
        _ => Err("BMAC doesn't widen this kind of vdev".into()),
    }
}

/// What the new disk's member can hold, after its partition and LUKS
/// header.
pub fn member_bytes(size: u64, encryption: Encryption) -> Option<u64> {
    let partition = bmac_storage::partition::member_partition_bytes(&[size]).ok()?;
    Some(match encryption {
        Encryption::Luks2 => partition.saturating_sub(LUKS2_HEADER_BYTES),
        Encryption::None => partition,
    })
}

/// Whether `size` is big enough to join `top`, whose members it must
/// match.
pub fn big_enough(top: &Top<'_>, size: u64, encryption: Encryption) -> Result<(), String> {
    let needed = top
        .members
        .iter()
        .filter_map(|m| m.member_size)
        .min()
        .unwrap_or(0);
    let has = member_bytes(size, encryption).unwrap_or(0);
    if has < needed {
        return Err(format!(
            "it would hold {}, and {} needs {}",
            bmac_core::Bytes(has),
            top.name,
            bmac_core::Bytes(needed)
        ));
    }
    Ok(())
}

/// Whether this host's OpenZFS can add a disk to a raidz vdev.
async fn raidz_expansion(cx: &Context<'_>) -> bool {
    cx.run(
        bmac_exec::Command::new(bmac_adapters::programs::ZPOOL).args([
            "get",
            "-H",
            "-o",
            "value",
            "feature@raidz_expansion",
            cx.pool.as_str(),
        ]),
    )
    .await
    .is_ok_and(|v| matches!(v.trim(), "enabled" | "active"))
}

const ATTACH: &str = "attach";

pub async fn attach_disk(cx: &Context<'_>) -> Result<(), Stop> {
    cx.phase(CHOOSE);
    let snapshot = cx.snapshot().await?;
    if snapshot.layout.pool.removal_in_progress {
        return Err(Stop::failed(
            "A vdev removal is in progress; ZFS changes no vdev's members until it finishes.",
        ));
    }
    let checkpoints = cx.flow.checkpoints();
    let (vdev_name, identity) = match checkpoints.get(ATTACH) {
        Some(saved) => (
            saved["vdev"].as_str().unwrap_or_default().to_string(),
            serde_json::from_value::<DiskIdentity>(saved["disk"].clone())
                .map_err(|e| Stop::failed(format!("the job's record is unreadable: {e}")))?,
        ),
        None => choose_attach(cx, &snapshot).await?,
    };
    cx.done(CHOOSE);

    let snapshot = cx.snapshot().await?;
    let top = snapshot
        .top(&vdev_name)
        .ok_or_else(|| Stop::failed(format!("{vdev_name} is no longer in {}", cx.pool)))?;
    let disk = cx.resolve(&identity)?;
    let by_id = super::by_id(&disk)
        .ok_or_else(|| Stop::failed(format!("{} has no /dev/disk/by-id name", disk.name)))?;
    let layout = PoolLayout {
        ashift: snapshot.ashift,
        ..PoolLayout::new(
            cx.pool.clone(),
            snapshot.encryption(),
            vec![Vdev::new(top.class, VdevType::Single, vec![by_id])],
        )
    };
    let storage = cx.storage(&layout);
    let target = match top.kind {
        Some(k) if k.is_raidz() => top.name.to_string(),
        _ => top
            .members
            .iter()
            .find(|m| online(m))
            .map(|m| m.path.clone())
            .ok_or_else(|| Stop::failed(format!("{} has no online member", top.name)))?,
    };
    let raidz = top.kind.is_some_and(VdevType::is_raidz);
    let prepared = cx
        .with_key(&snapshot, async {
            cx.phase(PREPARE);
            let prepared = storage.prepare(0).await?;
            cx.done(PREPARE);
            cx.phase(POOL_PHASE);
            if !prepared.in_pool {
                if prepared.encrypted {
                    storage.register(&prepared.members).await?;
                }
                let device = prepared.devices().remove(0);
                cx.flow.info(format!("Attaching {device} to {}.", top.name));
                cx.zpool(&["attach", cx.pool.as_str(), &target, &device])
                    .await?;
            }
            cx.done(POOL_PHASE);
            Ok(prepared)
        })
        .await?;
    cx.phase(FINISH);
    if prepared.encrypted {
        cx.backup_headers(&prepared.members).await?;
    }
    cx.flow.info(if raidz {
        format!(
            "{} is expanding onto the new disk in the background; the Storage page shows its \
             progress. Data written before keeps its old data-to-parity ratio until it is \
             rewritten.",
            top.name
        )
    } else {
        format!(
            "ZFS is resilvering {} onto the new disk; the Storage page shows its progress.",
            top.name
        )
    });
    cx.flow
        .result(json!({ "vdev": top.name, "disk": identity.value() }));
    cx.done(FINISH);
    Ok(())
}

async fn choose_attach(
    cx: &Context<'_>,
    snapshot: &Snapshot,
) -> Result<(String, DiskIdentity), Stop> {
    let expansion = if snapshot
        .tops()
        .iter()
        .any(|t| t.kind.is_some_and(VdevType::is_raidz))
    {
        raidz_expansion(cx).await
    } else {
        false
    };
    let tops = snapshot.tops();
    let mut vdev = field("vdev", FieldType::Select, "Vdev");
    vdev.options = tops
        .iter()
        .filter(|t| attachable(t, expansion).is_ok())
        .map(|t| {
            let kind = if t.kind.is_some_and(VdevType::is_raidz) {
                "Adds the disk to the raidz vdev; existing data keeps its old parity ratio."
            } else if t.kind == Some(VdevType::Single) {
                "Turns the single disk into a two-way mirror."
            } else {
                "Adds another copy to the mirror."
            };
            choice(t.name, &t.describe(), Some(kind.into()))
        })
        .collect();
    let refused: Vec<String> = tops
        .iter()
        .filter_map(|t| {
            attachable(t, expansion)
                .err()
                .map(|why| format!("{}: {why}", t.name))
        })
        .collect();
    if vdev.options.is_empty() {
        return Err(Stop::failed(format!(
            "No vdev can take another disk now. {}",
            refused.join("; ")
        )));
    }
    let mut disk = field("disk", FieldType::Select, "New disk");
    disk.options = disk_choices(snapshot);
    if disk.options.is_empty() {
        return Err(Stop::failed(
            "No disk can be used: every disk is in a pool, in use, chosen by an unfinished \
             workflow, or not recognizable by WWN or serial.",
        ));
    }
    let encryption = snapshot.encryption();
    let check = |values: &Map<String, Value>| -> Result<(), BTreeMap<String, String>> {
        let top = snapshot
            .top(&text(values, "vdev"))
            .ok_or_else(|| refuse("vdev", "Choose a vdev."))?;
        let candidates = snapshot.candidates();
        let c = candidates
            .iter()
            .find(|c| c.by_id.as_str() == text(values, "disk"))
            .ok_or_else(|| refuse("disk", "Choose a disk."))?;
        big_enough(&top, c.disk.size_bytes, encryption)
            .map_err(|why| refuse("disk", format!("Too small: {why}.")))
    };
    let values = cx
        .flow
        .input_group(
            "Attach a disk",
            Some(&if refused.is_empty() {
                "Choose the vdev and the disk to add to it.".to_string()
            } else {
                format!("Not offered: {}.", refused.join("; "))
            }),
            vec![vdev, disk],
            check,
        )
        .await?;
    let name = text(&values, "vdev");
    let candidates = snapshot.candidates();
    let chosen = candidates
        .iter()
        .find(|c| c.by_id.as_str() == text(&values, "disk"))
        .ok_or_else(|| Stop::failed("the chosen disk is gone"))?;
    let confirmed = cx
        .flow
        .confirm(
            "Erase the disk and attach it",
            &format!(
                "Everything on {} is erased, and it joins {name}.",
                chosen.describe()
            ),
            Severity::Destructive,
            Some("erase"),
            vec![chosen.describe()],
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    let identity = chosen.identity.clone();
    let disk = cx.resolve(&identity)?;
    cx.flow
        .info(format!("Erasing {}.", super::describe_disk(&disk)));
    cx.wipe_metadata(&disk).await?;
    cx.flow
        .checkpoints()
        .record(ATTACH, json!({ "vdev": name, "disk": identity }))?;
    Ok((name, identity))
}
