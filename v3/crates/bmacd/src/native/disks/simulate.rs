// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `storage.simulate_failure` (v2 `simulate_disk_failure_and_replacement.sh`
//! and `simulated_disk_failure.sh`): the failure drill for hosts whose disks
//! can't be pulled while they run. One member of a healthy redundant vdev
//! goes offline and its LUKS mapping closes, as if it had been pulled. Then
//! the disk is either erased, so it is a new blank disk for Replace a disk,
//! or brought back online. The journal holds the drill until one of those
//! finishes it, and a later run carries on from there.

use bmac_adapters::layout::Member;
use bmac_core::job::{FieldType, Severity};
use bmac_exec::Command;
use bmac_storage::journal::{Simulation, SimulationStage};
use bmac_storage::{Encryption, VdevClass};
use serde_json::json;

use super::{Context, Snapshot, Top, choice, describe_disk, field, mapper_of, named, online, text};
use crate::native::Stop;
use crate::native::storage::{CRYPTSETUP, UDEVADM};

const CHOOSE: (&str, &str) = ("choose", "Choose the member");
const OFFLINE: (&str, &str) = ("offline", "Take the member offline");
const ERASE: (&str, &str) = ("erase", "Erase the disk");
const RESTORE: (&str, &str) = ("restore", "Bring the member back");
const CLOSE_TRIES: u32 = 5;

/// Why no drill can start on this pool now.
pub fn pool_ready(snapshot: &Snapshot) -> Result<(), String> {
    let pool = &snapshot.layout.pool;
    if pool.state.as_deref() != Some("ONLINE") {
        return Err(format!(
            "the pool is {}, not ONLINE",
            pool.state.as_deref().unwrap_or("unknown")
        ));
    }
    if pool
        .scan
        .as_deref()
        .is_some_and(|s| s.contains("in progress"))
        || pool.removal_in_progress
    {
        return Err("a resilver, scrub, or removal is running".into());
    }
    Ok(())
}

/// Whether this member can be drilled: its vdev is redundant and every
/// member is online, and its disk is recognizable.
pub fn drillable(top: &Top<'_>, member: &Member) -> Result<(), String> {
    if !matches!(
        top.class,
        VdevClass::Data | VdevClass::Special | VdevClass::Log
    ) {
        return Err(format!("a {} device isn't drilled", top.class));
    }
    if !top.redundant() {
        return Err(format!("{} has no redundancy", top.name));
    }
    if top.replacing || top.removing || !top.members.iter().all(online) {
        return Err(format!("{} isn't whole", top.name));
    }
    if member.identity.is_none() {
        return Err("its disk has no WWN or serial".into());
    }
    Ok(())
}

pub async fn simulate_failure(cx: &Context<'_>) -> Result<(), Stop> {
    cx.phase(CHOOSE);
    let snapshot = cx.snapshot().await?;
    let drill = match snapshot.journal.simulation.clone() {
        Some(drill) => {
            cx.flow.info(format!(
                "Carrying on with the drill on {} ({}), started earlier.",
                drill.leaf,
                named(&drill.disk)
            ));
            drill
        }
        None => choose(cx, &snapshot).await?,
    };
    cx.done(CHOOSE);
    if drill.stage == SimulationStage::Erased {
        cx.flow.info(format!(
            "The disk {} was erased. Put it back with Replace a disk: choose {} and that disk.",
            named(&drill.disk),
            drill.leaf
        ));
        cx.flow.next_step("Run Replace a disk to finish the drill.");
        return Ok(());
    }

    cx.phase(OFFLINE);
    offline(cx, &drill).await?;
    cx.done(OFFLINE);

    let mut how = field("choice", FieldType::Select, "Next");
    how.default = Some(json!("erase"));
    how.options = vec![
        choice(
            "erase",
            "Erase the disk",
            Some("Then Replace a disk puts it back as a new disk, as after a real swap.".into()),
        ),
        choice(
            "restore",
            "Bring it back online",
            Some("ZFS resilvers what changed while it was offline.".into()),
        ),
        choice(
            "later",
            "Leave it offline",
            Some("Run this again to choose later.".into()),
        ),
    ];
    let values = cx
        .flow
        .input_group(
            &format!("{} is offline", drill.leaf),
            Some(&format!(
                "{} runs with one disk less redundancy until the member is back.",
                drill.vdev
            )),
            vec![how],
            |_| Ok(()),
        )
        .await?;
    match text(&values, "choice").as_str() {
        "erase" => erase(cx, &drill).await,
        "restore" => restore(cx, &snapshot, &drill).await,
        _ => {
            cx.flow
                .info("The member stays offline; run this again to finish the drill.");
            Ok(())
        }
    }
}

async fn choose(cx: &Context<'_>, snapshot: &Snapshot) -> Result<Simulation, Stop> {
    pool_ready(snapshot).map_err(|why| Stop::failed(format!("No drill now: {why}.")))?;
    let tops = snapshot.tops();
    let mut offered = Vec::new();
    for top in &tops {
        for member in top.members {
            if drillable(top, member).is_ok() {
                offered.push((top.clone(), member.clone()));
            }
        }
    }
    if offered.is_empty() {
        return Err(Stop::failed(
            "No member can be drilled: that needs a redundant vdev with every member online.",
        ));
    }
    let mut member = field("member", FieldType::Select, "Member");
    member.options = offered
        .iter()
        .map(|(top, m)| {
            let disk = m
                .disk
                .as_deref()
                .and_then(|d| snapshot.disk_named(d))
                .map(describe_disk)
                .unwrap_or_default();
            choice(
                &m.path,
                &format!("{} in {}: {disk}", m.path, top.name),
                top.holds_esp
                    .then(|| "A boot member: its boot partition is erased with it.".to_string()),
            )
        })
        .collect();
    let values = cx
        .flow
        .input_group(
            "Simulate a disk failure",
            Some(
                "Choose the member to fail. It goes offline at once; erasing it comes after a \
                 second confirmation.",
            ),
            vec![member],
            |_| Ok(()),
        )
        .await?;
    let leaf = text(&values, "member");
    let (top, member) = offered
        .iter()
        .find(|(_, m)| m.path == leaf)
        .ok_or_else(|| Stop::failed("that member is no longer offered"))?;
    let identity = member.identity.clone().expect("drillable members have one");
    let mut details = vec![format!("{} in {}", member.path, top.name)];
    if top.holds_esp {
        details.push(
            "It is a boot member: the host still boots from the other members, and Replace a \
             disk restores its boot partition."
                .into(),
        );
    }
    let confirmed = cx
        .flow
        .confirm(
            "Take the member offline",
            &format!(
                "{} goes offline and {} loses one disk of redundancy until the drill ends. Type \
                 the disk's identity to go on.",
                member.path, top.name
            ),
            Severity::Critical,
            Some(identity.value()),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    let drill = Simulation {
        vdev: top.name.to_string(),
        leaf: member.path.clone(),
        disk: identity,
        mapper: mapper_of(member),
        stage: SimulationStage::Offline,
        started_at: cx.now(),
    };
    cx.update(|s| s.begin_simulation(drill.clone()))?;
    Ok(drill)
}

async fn offline(cx: &Context<'_>, drill: &Simulation) -> Result<(), Stop> {
    let snapshot = cx.snapshot().await?;
    let leaf = snapshot
        .top(&drill.vdev)
        .and_then(|t| t.members.iter().find(|m| m.path == drill.leaf).cloned());
    if leaf.as_ref().is_some_and(online) {
        cx.flow.info(format!("Taking {} offline.", drill.leaf));
        cx.zpool(&["offline", cx.pool.as_str(), &drill.leaf])
            .await?;
    }
    cx.run(
        Command::new(UDEVADM)
            .arg("settle")
            .timeout(std::time::Duration::from_secs(120)),
    )
    .await?;
    let Some(mapper) = &drill.mapper else {
        return Ok(());
    };
    let path = cx.root.join("dev/mapper").join(mapper);
    for attempt in 1..=CLOSE_TRIES {
        if !path.exists() {
            break;
        }
        match cx
            .run(Command::new(CRYPTSETUP).args(["close", mapper.as_str()]))
            .await
        {
            Ok(_) => break,
            Err(e) if attempt == CLOSE_TRIES => return Err(e),
            Err(_) => tokio::time::sleep(cx.poll).await,
        }
    }
    cx.flow
        .info(format!("Closed {mapper}, as if the disk had been pulled."));
    Ok(())
}

async fn erase(cx: &Context<'_>, drill: &Simulation) -> Result<(), Stop> {
    let disk = cx.resolve(&drill.disk)?;
    let confirmed = cx
        .flow
        .confirm(
            "Erase the disk",
            &format!(
                "Everything on {} is erased. Only Replace a disk brings it back into {}.",
                describe_disk(&disk),
                drill.vdev
            ),
            Severity::Destructive,
            Some("erase"),
            vec![describe_disk(&disk)],
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    cx.phase(ERASE);
    let snapshot = cx.snapshot().await?;
    if snapshot
        .top(&drill.vdev)
        .and_then(|t| t.members.iter().find(|m| m.path == drill.leaf).cloned())
        .is_some_and(|m| online(&m))
    {
        return Err(Stop::failed(format!(
            "{} is online again; nothing was erased",
            drill.leaf
        )));
    }
    if let Some(mapper) = &drill.mapper
        && cx.root.join("dev/mapper").join(mapper).exists()
    {
        return Err(Stop::failed(format!(
            "{mapper} is still open; nothing was erased"
        )));
    }
    cx.update(|s| {
        if let Some(d) = s.simulation.as_mut() {
            d.stage = SimulationStage::Erased;
        }
        Ok(())
    })?;
    cx.flow.info(format!("Erasing {}.", describe_disk(&disk)));
    cx.erase(&disk).await?;
    cx.done(ERASE);
    cx.flow.info(format!(
        "{} is blank, like a new disk in the old one's bay.",
        describe_disk(&disk)
    ));
    cx.flow.next_step(format!(
        "Run Replace a disk: choose {} in {} and the disk {}.",
        drill.leaf,
        drill.vdev,
        named(&drill.disk)
    ));
    cx.flow
        .result(json!({ "drill": "erased", "leaf": drill.leaf, "disk": drill.disk.value() }));
    Ok(())
}

async fn restore(cx: &Context<'_>, snapshot: &Snapshot, drill: &Simulation) -> Result<(), Stop> {
    cx.phase(RESTORE);
    if let Some(mapper) = &drill.mapper
        && !cx.root.join("dev/mapper").join(mapper).exists()
    {
        let disk = cx.resolve(&drill.disk)?;
        let partition = disk
            .partitions
            .iter()
            .find(|p| p.luks.is_some())
            .map(|p| {
                format!(
                    "{}-part{}",
                    super::by_id(&disk).map(|b| b.path()).unwrap_or_default(),
                    p.number
                )
            })
            .ok_or_else(|| Stop::failed(format!("{} has no LUKS partition", disk.name)))?;
        let layout = snapshot.pool_layout(&cx.pool, None);
        let storage = cx.storage(&layout);
        cx.with_key(snapshot, async {
            if snapshot.encryption() == Encryption::Luks2 {
                storage.open(&partition, mapper).await?;
            }
            Ok(())
        })
        .await?;
    }
    cx.zpool(&["online", cx.pool.as_str(), &drill.leaf]).await?;
    cx.update(|s| {
        s.simulation = None;
        Ok(())
    })?;
    cx.done(RESTORE);
    cx.flow.info(format!(
        "{} is back online; ZFS resilvers what changed while it was away.",
        drill.leaf
    ));
    cx.flow
        .result(json!({ "drill": "restored", "leaf": drill.leaf }));
    Ok(())
}
