// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup appliance service.

use std::process::ExitCode;

use bmac_backup::retention::ApplianceRetention;
use bmac_backupd::audit::Audit;
use bmac_backupd::grants::Grants;
use bmac_backupd::peers::{self, Peers, Probe};
use bmac_backupd::platform::{IoPolicy, Platform};
use bmac_backupd::pool::RealPool;
use bmac_backupd::retention::{self, Change, Settings};
use bmac_backupd::scrub::{IoGate, ScrubState};
use bmac_backupd::state::{self, Identity, Paths, PoolBinding};
use bmac_backupd::{admin, daemon, enroll, journal::Metadata, luks, setup, terminal, watchdog};
use bmac_exec::RealRunner;
use bmac_proto::backups::display_fingerprint;

const USAGE: &str = "usage:
  bmac-backupd create-pool POOL LAYOUT /dev/disk/by-id/DISK...
                                   erase the disks, encrypt them with LUKS2 and make the pool
                                   (LAYOUT is stripe, mirror, raidz1, raidz2 or raidz3; console only)
  bmac-backupd unlock              after a reboot: one passphrase opens and imports the pool (console only)
  bmac-backupd init NAME POOL      make this appliance's identity and bind an imported pool
  bmac-backupd adopt POOL          after an OS reinstall: take the pool back with the administrator's password
  bmac-backupd admin-password      set the password for the appliance UI and adopt
  bmac-backupd enroll BUNDLE [--address HOST[:PORT]]
                                   ask a cluster to back up to this appliance
  bmac-backupd run                 check in with the enrolled clusters and pull their PITs
  bmac-backupd status              this appliance's identity, pool and clusters
  bmac-backupd retention           the retention policy and what a prune would delete
  bmac-backupd retention set DAILY WEEKLY MONTHLY [MONTH_DAYS yearly|no-yearly]
                                   change the policy (typed here, it needs no delay)
  bmac-backupd retention floor DAILY WEEKLY MONTHLY
                                   the least the appliance UI may set
  bmac-backupd test-day-seconds SECONDS|off
                                   for tests only: retention's days last SECONDS
  bmac-backupd hold PIT            keep a PIT whatever the policy says
  bmac-backupd unhold PIT
  bmac-backupd prune               prune now, as the service does every hour
  bmac-backupd peers               this appliance's fingerprint, its trusted peers, recent refusals
  bmac-backupd trust UUID FINGERPRINT ADDRESS
                                   trust a peer, once its certificate matches FINGERPRINT
  bmac-backupd untrust UUID
  bmac-backupd grant CLUSTER HOURS PIT...
                                   let a cluster's restore job read these PITs; shows the token once
  bmac-backupd grants
  bmac-backupd revoke-grant ID
  bmac-backupd settings            scrubs, the I/O policy, the watchdog and its email
  bmac-backupd settings scrub-days DAYS | io pulls-first|scrub-first|together
  bmac-backupd settings watchdog-minutes MINUTES | url add URL | url remove URL
  bmac-backupd settings email FROM TO...
  bmac-backupd settings postmark-token [--clear]
                                   the token is read from the terminal or stdin, never an argument
  bmac-backupd scrub [now]         the last scrub and suspect PITs; now starts one
  bmac-backupd audit [COUNT]       the newest audit log entries
  bmac-backupd --version";

static RUNNER: RealRunner = RealRunner;

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

fn identity(paths: &Paths) -> Result<Identity, String> {
    state::load(&paths.identity())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "this appliance has no identity; run bmac-backupd init first".into())
}

async fn init(paths: &Paths, name: &str, pool: &str) -> Result<(), String> {
    let identity = setup::identity(paths, name, now_ms())?;
    let binding = setup::bind(&RUNNER, paths, &identity, pool).await?;
    let cert = std::fs::read_to_string(paths.cert()).map_err(|e| e.to_string())?;
    println!(
        "appliance {} ({}) is bound to pool {} (GUID {})",
        identity.name, identity.appliance_uuid, binding.pool, binding.pool_guid
    );
    println!(
        "certificate fingerprint: {}",
        display_fingerprint(&state::pem_fingerprint(&cert)?)
    );
    if admin::load(&paths.metadata())?.is_none() {
        println!("set the administrator's password next: bmac-backupd admin-password");
    }
    Ok(())
}

async fn enroll(paths: &Paths, bundle: &str, address: Option<&str>) -> Result<(), String> {
    let identity = identity(paths)?;
    let text = std::fs::read_to_string(bundle).map_err(|e| format!("{bundle}: {e}"))?;
    let bundle = enroll::read_bundle(&text, now_ms())?;
    let address = address
        .map(str::parse::<bmac_core::ApplianceAddress>)
        .transpose()
        .map_err(|e| format!("--address: {e}"))?;
    let enrolled =
        enroll::enroll(paths, &identity, &bundle, address, enroll::PORT, now_ms()).await?;
    println!(
        "asked cluster {} (through {}) to back up to this appliance; request {}",
        enrolled.cluster_name, enrolled.answered_by, enrolled.request_id
    );
    println!(
        "approve it in the Dashboard after checking that it shows this certificate fingerprint:\n  {}",
        display_fingerprint(&enrolled.fingerprint)
    );
    Ok(())
}

fn status(paths: &Paths) -> Result<(), String> {
    let identity = identity(paths)?;
    println!("appliance {} ({})", identity.name, identity.appliance_uuid);
    match state::load::<PoolBinding>(&paths.pool()).map_err(|e| e.to_string())? {
        Some(b) => println!("pool {} (GUID {})", b.pool, b.pool_guid),
        None => println!("no pool is bound"),
    }
    let journal = Metadata::new(paths.metadata()).load().ok().flatten();
    for enrollment in state::enrollments(paths).map_err(|e| e.to_string())? {
        let pits = journal
            .as_ref()
            .map(|j| {
                j.pits
                    .iter()
                    .filter(|p| p.cluster_uuid == enrollment.cluster_uuid)
                    .count()
            })
            .unwrap_or(0);
        println!(
            "cluster {} ({}): {pits} PITs, {} hosts known",
            enrollment.cluster_name,
            enrollment.cluster_uuid,
            enrollment.hosts.len()
        );
    }
    if let Some(j) = journal {
        println!(
            "journal revision {}: {} receiving, {} quarantined",
            j.revision,
            j.receiving.len(),
            j.quarantined.len()
        );
    }
    Ok(())
}

/// The console's user, for the audit log.
fn console() -> String {
    format!(
        "console:{}",
        std::env::var("SUDO_USER").unwrap_or_else(|_| "root".into())
    )
}

/// `D W M [MONTH_DAYS yearly|no-yearly]`; without the last two, `base`'s.
fn policy(counts: &[&str], base: &ApplianceRetention) -> Result<ApplianceRetention, String> {
    let n = |s: &str| {
        s.parse::<u32>()
            .ok()
            .filter(|n| *n <= 1000)
            .ok_or_else(|| format!("{s:?} isn't a count from 0 to 1000"))
    };
    let (month_days, yearly) = match counts {
        [_, _, _] => (base.month_days, base.yearly),
        [_, _, _, days, yearly] => (
            n(days)?,
            match *yearly {
                "yearly" => true,
                "no-yearly" => false,
                other => return Err(format!("{other:?} isn't yearly or no-yearly")),
            },
        ),
        _ => return Err("give D W M, or D W M MONTH_DAYS yearly|no-yearly".into()),
    };
    let policy = ApplianceRetention {
        daily: n(counts[0])?,
        weekly: n(counts[1])?,
        monthly: n(counts[2])?,
        month_days,
        yearly,
    };
    policy.validate()?;
    Ok(policy)
}

fn describe(p: &ApplianceRetention) -> String {
    let months = if p.yearly {
        format!(
            "{} monthly of {} days, then yearly",
            p.monthly, p.month_days
        )
    } else {
        format!("every month of {} days", p.month_days)
    };
    format!("{} daily, {} weekly, {months}", p.daily, p.weekly)
}

fn bound(paths: &Paths) -> Result<(PoolBinding, Metadata), String> {
    let binding: PoolBinding = state::load(&paths.pool())
        .map_err(|e| e.to_string())?
        .ok_or("this appliance has no pool; run bmac-backupd init first")?;
    let metadata = Metadata::new(paths.metadata());
    if metadata.load().map_err(|e| e.to_string())?.is_none() {
        return Err(format!(
            "{} has no journal; is the pool imported and its metadata mounted?",
            paths.metadata().display()
        ));
    }
    Ok((binding, metadata))
}

/// Changes the settings under the journal lock, then records it.
async fn settings(
    paths: &Paths,
    event: serde_json::Value,
    change: impl FnOnce(&mut Settings, &bmac_backupd::journal::Journal) -> Result<String, String>,
) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
    let journal = metadata
        .load()
        .map_err(|e| e.to_string())?
        .ok_or("no journal")?;
    let mut settings = Settings::load(&metadata).map_err(|e| e.to_string())?;
    let said = change(&mut settings, &journal)?;
    settings.save(&metadata).map_err(|e| e.to_string())?;
    let mut details = event;
    details["retention"] = serde_json::to_value(settings.policy).unwrap_or_default();
    details["floor"] = serde_json::to_value(settings.floor).unwrap_or_default();
    let _ = Audit::new(&paths.metadata()).record(now_ms(), "policy", &console(), details);
    println!("{said}");
    Ok(())
}

fn find_pit(
    journal: &bmac_backupd::journal::Journal,
    pit: &str,
) -> Result<(String, uuid::Uuid), String> {
    let id: uuid::Uuid = pit
        .parse()
        .map_err(|_| format!("{pit:?} isn't a PIT UUID"))?;
    journal
        .pits
        .iter()
        .find(|p| p.pit_uuid == id)
        .map(|p| (p.cluster_uuid.clone(), id))
        .ok_or_else(|| format!("PIT {id} isn't published here"))
}

async fn show_retention(paths: &Paths) -> Result<(), String> {
    let (binding, metadata) = bound(paths)?;
    let journal = metadata
        .load()
        .map_err(|e| e.to_string())?
        .ok_or("no journal")?;
    let settings = Settings::load(&metadata).map_err(|e| e.to_string())?;
    println!("policy: {}", describe(&settings.policy));
    println!("floor: {}", describe(&settings.floor));
    if let Some(p) = &settings.pending {
        println!(
            "pending: {}, from {} (requested by {})",
            describe(&p.policy),
            jiff::Timestamp::from_millisecond(p.effective_at_ms).map_err(|e| e.to_string())?,
            p.by
        );
    }
    for h in &settings.holds {
        println!(
            "held: PIT {} of cluster {} (by {})",
            h.pit_uuid, h.cluster_uuid, h.by
        );
    }
    let pool = RealPool {
        runner: &RUNNER,
        pool: binding.pool,
    };
    let zones = retention::Zones::load(&metadata).map_err(|e| e.to_string())?;
    for (cluster, zone) in &zones.clusters {
        match &zone.pending {
            Some(p) => println!(
                "cluster {cluster}: days in {}, then {} from {}",
                zone.zone,
                p.zone,
                jiff::Timestamp::from_millisecond(p.effective_at_ms).map_err(|e| e.to_string())?
            ),
            None => println!("cluster {cluster}: days in {}", zone.zone),
        }
    }
    if let Some(day) = settings.test_day_s {
        println!("for tests, a day lasts {day} seconds");
    }
    let mut plan = retention::plan(&journal, &settings, &zones, &Default::default(), now_ms());
    retention::estimate(&pool, &mut plan).await;
    for c in &plan.coverage {
        println!(
            "VM {} of cluster {}: {} PITs kept, {} to {}",
            c.vm_uuid,
            c.cluster_uuid,
            c.pits,
            jiff::Timestamp::from_second(c.oldest_s).map_err(|e| e.to_string())?,
            jiff::Timestamp::from_second(c.newest_s).map_err(|e| e.to_string())?
        );
    }
    for d in &plan.prune {
        println!("would prune PIT {} of VM {}", d.pit_uuid, d.vm_uuid);
    }
    println!(
        "a prune now would delete {} PITs and free about {} bytes",
        plan.prune.len(),
        plan.reclaim_bytes
    );
    Ok(())
}

async fn prune(paths: &Paths) -> Result<(), String> {
    let (binding, metadata) = bound(paths)?;
    let pool = RealPool {
        runner: &RUNNER,
        pool: binding.pool,
    };
    let audit = Audit::new(&paths.metadata());
    let report = retention::prune(&pool, &metadata, &audit, &Default::default(), now_ms()).await?;
    println!(
        "pruned {} PITs (about {} bytes); {} stay after a failure",
        report.pruned.len(),
        report.reclaim_bytes,
        report.skipped.len()
    );
    for (pit, why) in &report.skipped {
        println!("  {pit}: {why}");
    }
    Ok(())
}

fn when(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| ms.to_string())
}

fn show_peers(paths: &Paths) -> Result<(), String> {
    let identity = identity(paths)?;
    let (_, metadata) = bound(paths)?;
    let cert = std::fs::read_to_string(paths.cert()).map_err(|e| e.to_string())?;
    println!(
        "this appliance: {} ({})",
        identity.name, identity.appliance_uuid
    );
    println!(
        "its fingerprint: {}",
        display_fingerprint(&state::pem_fingerprint(&cert)?)
    );
    let peers = Peers::load(&metadata).map_err(|e| e.to_string())?;
    if peers.peers.is_empty() {
        println!("no trusted peers");
    }
    for e in &peers.peers {
        println!(
            "trusted: {} at {} ({}); last read here {}; trusts this appliance: {}",
            e.peer.appliance_uuid,
            e.peer.address,
            display_fingerprint(&e.peer.fingerprint),
            e.last_served_ms.map(when).unwrap_or_else(|| "never".into()),
            match e.trusts_us {
                Some(true) => "yes",
                Some(false) => "no, it refused this appliance's last handshake",
                None => "not known yet",
            }
        );
    }
    for r in peers.refusals.iter().rev().take(10) {
        println!(
            "refused: {} from {} claiming {:?} at {}",
            display_fingerprint(&r.fingerprint),
            r.from,
            r.claimed,
            when(r.at_ms)
        );
    }
    Ok(())
}

async fn trust(paths: &Paths, uuid: &str, fingerprint: &str, address: &str) -> Result<(), String> {
    let identity = identity(paths)?;
    let (_, metadata) = bound(paths)?;
    let uuid: uuid::Uuid = uuid
        .parse()
        .map_err(|_| format!("{uuid:?} isn't an appliance UUID"))?;
    let address = address
        .parse::<bmac_core::ApplianceAddress>()
        .map_err(|e| format!("{address}: {e}"))?;
    let wanted = bmac_proto::appliance::normalized_fingerprint(fingerprint)
        .ok_or("a fingerprint is 64 hexadecimal digits")?;
    let cert = std::fs::read_to_string(paths.cert()).map_err(|e| e.to_string())?;
    let key = std::fs::read_to_string(paths.key()).map_err(|e| e.to_string())?;
    match peers::probe(&wanted, &address, (&cert, &key), daemon::EXPORT_PORT).await {
        Probe::Matches => {}
        Probe::Mismatch { presented } => {
            return Err(format!(
                "WARNING: {address} presented a certificate with fingerprint\n  {}\nnot the one typed. Nothing was saved.",
                display_fingerprint(&presented)
            ));
        }
        Probe::Unreachable(e) => {
            return Err(format!(
                "{address} can't be reached: {e}; nothing was saved"
            ));
        }
    }
    let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
    let mut peers = Peers::load(&metadata).map_err(|e| e.to_string())?;
    peers.trust(
        uuid,
        &wanted,
        address.clone(),
        &state::pem_fingerprint(&cert)?,
        &console(),
        now_ms(),
    )?;
    peers.save(&metadata).map_err(|e| e.to_string())?;
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "trust",
        &console(),
        serde_json::json!({"appliance_uuid": uuid, "fingerprint": wanted, "address": address.to_string()}),
    );
    println!(
        "{address} presented that certificate; appliance {uuid} is now a trusted peer of {}.",
        identity.name
    );
    println!("{}", peers::TRUST_REMINDER);
    Ok(())
}

async fn untrust(paths: &Paths, uuid: &str) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
    let mut peers = Peers::load(&metadata).map_err(|e| e.to_string())?;
    let entry = peers
        .untrust(uuid)
        .ok_or_else(|| format!("{uuid} isn't a trusted peer"))?;
    peers.save(&metadata).map_err(|e| e.to_string())?;
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "untrust",
        &console(),
        serde_json::json!({"appliance_uuid": uuid, "fingerprint": entry.peer.fingerprint}),
    );
    println!("appliance {uuid} is no longer trusted; its open connections close within seconds");
    Ok(())
}

async fn grant(paths: &Paths, cluster: &str, hours: &str, pits: &[&str]) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let hours: u32 = hours
        .parse()
        .map_err(|_| format!("{hours:?} isn't a number of hours"))?;
    let pits = pits
        .iter()
        .map(|p| {
            p.parse::<uuid::Uuid>()
                .map_err(|_| format!("{p:?} isn't a PIT UUID"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
    let journal = metadata
        .load()
        .map_err(|e| e.to_string())?
        .ok_or("no journal")?;
    let mut grants = Grants::load(&metadata).map_err(|e| e.to_string())?;
    let (grant, token) =
        grants.issue(&journal, cluster, &pits, Some(hours), &console(), now_ms())?;
    grants.save(&metadata).map_err(|e| e.to_string())?;
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "grant",
        &console(),
        serde_json::json!({"grant_id": grant.grant_id, "cluster_uuid": cluster, "pits": grant.pits, "expires_at_ms": grant.expires_at_ms}),
    );
    println!(
        "restore grant {} until {}; paste this into the restore wizard (it isn't shown again):",
        grant.grant_id,
        when(grant.expires_at_ms)
    );
    println!("{token}");
    Ok(())
}

fn show_grants(paths: &Paths) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let now = now_ms();
    for g in Grants::load(&metadata).map_err(|e| e.to_string())?.grants {
        println!(
            "{} for cluster {}: {} PITs, {} until {}{}",
            g.grant_id,
            g.cluster_uuid,
            g.pits.len(),
            if g.revoked {
                "revoked"
            } else if g.live(now) {
                "live"
            } else {
                "ended"
            },
            when(g.expires_at_ms),
            g.last_used_ms
                .map(|t| format!(", last used {}", when(t)))
                .unwrap_or_default()
        );
    }
    Ok(())
}

async fn revoke_grant(paths: &Paths, id: &str) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
    let mut grants = Grants::load(&metadata).map_err(|e| e.to_string())?;
    if !grants.revoke(id) {
        return Err(format!("{id} isn't a live grant"));
    }
    grants.save(&metadata).map_err(|e| e.to_string())?;
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "revoke_grant",
        &console(),
        serde_json::json!({"grant_id": id}),
    );
    println!("restore grant {id} is revoked");
    Ok(())
}

async fn create_pool(pool: &str, layout: &str, disks: &[&str]) -> Result<(), String> {
    terminal::at_console(|k| std::env::var(k).ok())?;
    let layout = luks::Layout::parse(layout)
        .ok_or("the layout is stripe, mirror, raidz1, raidz2, or raidz3")?;
    let disks: Vec<String> = disks.iter().map(|d| d.to_string()).collect();
    let guid = luks::create(&RUNNER, &mut terminal::Tty, pool, layout, &disks).await?;
    println!(
        "pool {pool} (GUID {guid}) is encrypted and ready; next: bmac-backupd init NAME {pool}"
    );
    Ok(())
}

async fn unlock(paths: &Paths) -> Result<(), String> {
    terminal::at_console(|k| std::env::var(k).ok())?;
    let binding: Option<PoolBinding> = state::load(&paths.pool()).map_err(|e| e.to_string())?;
    let done = luks::unlock(
        &RUNNER,
        &mut terminal::Tty,
        binding
            .as_ref()
            .map(|b| (b.pool.as_str(), b.pool_guid.as_str())),
    )
    .await?;
    if binding.is_none() {
        println!(
            "pool {} (GUID {}) is open and imported; this install isn't bound to it: run bmac-backupd adopt {}",
            done.pool, done.pool_guid, done.pool
        );
        return Ok(());
    }
    let layout = bmac_backupd::journal::Layout {
        pool: done.pool.clone(),
    };
    setup::mount_metadata(&RUNNER, paths, &layout).await?;
    println!(
        "pool {} (GUID {}) is unlocked; the service resumes at its next check-in",
        done.pool, done.pool_guid
    );
    Ok(())
}

async fn adopt(paths: &Paths, pool: &str) -> Result<(), String> {
    let console = terminal::at_console(|k| std::env::var(k).ok()).is_ok();
    let mut tty = terminal::Tty;
    let password = terminal::Prompter::secret(
        &mut tty,
        "Administrator password (empty if none was ever set): ",
    )
    .map_err(|e| e.to_string())?
    .unwrap_or_default();
    let credential = match (password.is_empty(), console) {
        (false, _) => setup::Credential::Password(&password),
        (true, true) => setup::Credential::Console,
        (true, false) => {
            return Err(
                "without the administrator's password, adopt runs only at the console".into(),
            );
        }
    };
    let (identity, binding) = setup::adopt(&RUNNER, paths, pool, credential, now_ms()).await?;
    let cert = std::fs::read_to_string(paths.cert()).map_err(|e| e.to_string())?;
    println!(
        "this install is appliance {} ({}) again, on pool {} (GUID {})",
        identity.name, identity.appliance_uuid, binding.pool, binding.pool_guid
    );
    println!(
        "its new certificate fingerprint: {}\nenroll it again with each cluster (bmac-backupd enroll BUNDLE)",
        display_fingerprint(&state::pem_fingerprint(&cert)?)
    );
    Ok(())
}

fn admin_password(paths: &Paths) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let mut tty = terminal::Tty;
    let mut read = |prompt: &str| {
        terminal::Prompter::secret(&mut tty, prompt)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no password was typed".to_string())
    };
    let first = read("New administrator password: ")?;
    admin::check_strength(&first)?;
    if read("Again: ")? != first {
        return Err("the passwords differ".into());
    }
    admin::set_password(metadata.root(), &first, now_ms(), admin::ROUNDS)?;
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "admin_password",
        &console(),
        serde_json::json!({}),
    );
    println!("the administrator's password is set");
    Ok(())
}

fn show_settings(paths: &Paths) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let p = Platform::load(&metadata).map_err(|e| e.to_string())?;
    println!(
        "scrubs: every {} days{}",
        p.scrub_every_days,
        if p.scrub_every_days == 0 {
            " (off)"
        } else {
            ""
        }
    );
    println!(
        "I/O policy: {}",
        serde_json::to_value(p.io_policy)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
            .replace('_', "-")
    );
    println!(
        "watchdog: email after {} minutes without an answer",
        p.watchdog.down_after_minutes
    );
    for url in &p.watchdog.urls {
        println!("  checks {url}");
    }
    println!(
        "email: from {} to {}; Postmark token {}",
        if p.notify.from.is_empty() {
            "(unset)"
        } else {
            &p.notify.from
        },
        if p.notify.to.is_empty() {
            "(nobody)".to_string()
        } else {
            p.notify.to.join(", ")
        },
        if Platform::token(&metadata).is_some() {
            "set"
        } else {
            "not set"
        }
    );
    let watch = watchdog::WatchState::load(&metadata).map_err(|e| e.to_string())?;
    for c in &watch.clusters {
        match c.down_since_ms {
            Some(t) => println!(
                "cluster {}: no host has answered since {}",
                c.cluster_name,
                when(t)
            ),
            None => println!("cluster {}: answering", c.cluster_name),
        }
    }
    for u in watch.urls.iter().filter(|u| u.failing_since_ms.is_some()) {
        println!(
            "{}: failing since {} ({})",
            u.url,
            when(u.failing_since_ms.unwrap_or(0)),
            u.last_error.as_deref().unwrap_or("")
        );
    }
    if let Some(e) = &watch.notify_error {
        println!("email is stopped: {e}");
    }
    Ok(())
}

async fn change_settings(
    paths: &Paths,
    change: impl FnOnce(&mut Platform) -> Result<(), String>,
) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
    let mut p = Platform::load(&metadata).map_err(|e| e.to_string())?;
    change(&mut p)?;
    p.save(&metadata)?;
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "settings",
        &console(),
        serde_json::to_value(&p).unwrap_or_default(),
    );
    drop(_guard);
    show_settings(paths)
}

fn postmark_token(paths: &Paths, clear: bool) -> Result<(), String> {
    let (_, metadata) = bound(paths)?;
    if clear {
        Platform::clear_token(&metadata)?;
    } else {
        let token = terminal::Prompter::secret(&mut terminal::Tty, "Postmark server token: ")
            .map_err(|e| e.to_string())?
            .ok_or("no token was given")?;
        Platform::set_token(&metadata, &token)?;
    }
    let _ = Audit::new(&paths.metadata()).record(
        now_ms(),
        "postmark_token",
        &console(),
        serde_json::json!({"set": !clear}),
    );
    println!(
        "the Postmark token is {}",
        if clear { "removed" } else { "set" }
    );
    Ok(())
}

fn count(text: &str, what: &str, max: u32) -> Result<u32, String> {
    text.parse::<u32>()
        .ok()
        .filter(|n| *n <= max)
        .ok_or_else(|| format!("{what} is a number from 0 to {max}"))
}

async fn scrub(paths: &Paths, now: bool) -> Result<(), String> {
    let (binding, metadata) = bound(paths)?;
    if now {
        let _guard = metadata.lock().await.map_err(|e| e.to_string())?;
        let mut state = ScrubState::load(&metadata).map_err(|e| e.to_string())?;
        state.requested = true;
        state.save(&metadata).map_err(|e| e.to_string())?;
    }
    let state = bmac_backupd::scrub::tick(
        &RUNNER,
        &binding.pool,
        &metadata,
        &Audit::new(&paths.metadata()),
        &IoGate::default(),
        now_ms(),
    )
    .await?;
    match &state.report {
        None => println!("no scrub has run"),
        Some(r) => {
            println!(
                "scrub {}{}; started {}{}",
                r.state,
                r.done_permille
                    .map(|p| format!(" ({:.1}% done)", f64::from(p) / 10.0))
                    .unwrap_or_default(),
                r.started_at_ms.map(when).unwrap_or_else(|| "?".into()),
                r.finished_at_ms
                    .map(|t| format!(", finished {}", when(t)))
                    .unwrap_or_default()
            );
            println!(
                "{} bytes repaired, {} unrepairable errors",
                r.repaired_bytes, r.errors
            );
            for a in &r.affected {
                println!("  damaged: {a}");
            }
        }
    }
    for s in &state.suspects {
        println!("suspect: PIT {} of cluster {}", s.pit_uuid, s.cluster_uuid);
    }
    Ok(())
}

fn show_audit(paths: &Paths, limit: &str) -> Result<(), String> {
    let limit = count(limit, "the count", 10_000)? as usize;
    let mut entries = Audit::new(&paths.metadata())
        .recent(limit)
        .map_err(|e| e.to_string())?;
    entries.reverse();
    for e in entries {
        println!("{} {} {} {}", when(e.at_ms), e.event, e.by, e.details);
    }
    Ok(())
}

async fn dispatch(args: &[String]) -> Result<(), String> {
    let paths = Paths::system();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["create-pool", pool, layout, disks @ ..] if !disks.is_empty() => {
            create_pool(pool, layout, disks).await
        }
        ["unlock"] => unlock(&paths).await,
        ["adopt", pool] => adopt(&paths, pool).await,
        ["admin-password"] => admin_password(&paths),
        ["init", name, pool] => init(&paths, name, pool).await,
        ["enroll", bundle] => enroll(&paths, bundle, None).await,
        ["enroll", bundle, "--address", address] => enroll(&paths, bundle, Some(address)).await,
        ["run"] => daemon::run(paths, terminated()).await,
        ["status"] => status(&paths),
        ["retention"] => show_retention(&paths).await,
        ["retention", "set", counts @ ..] => {
            settings(&paths, serde_json::json!({"change": "policy"}), |s, _| {
                let new = policy(counts, &s.policy)?;
                Ok(match s.change(new, &console(), now_ms(), true)? {
                    Change::Applied => format!("retention is now {}", describe(&new)),
                    Change::Delayed { .. } => unreachable!("confirmed at the console"),
                })
            })
            .await
        }
        ["retention", "floor", d, w, m] => {
            settings(&paths, serde_json::json!({"change": "floor"}), |s, _| {
                let floor = policy(&[*d, *w, *m], &s.floor)?;
                s.set_floor(floor)?;
                Ok(format!("the floor is now {}", describe(&floor)))
            })
            .await
        }
        ["test-day-seconds", seconds] => {
            let day = match *seconds {
                "off" => None,
                n => Some(
                    n.parse::<i64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("{n:?} isn't a number of seconds or off"))?,
                ),
            };
            settings(
                &paths,
                serde_json::json!({"change": "test_day_s", "test_day_s": day}),
                |s, _| {
                    s.test_day_s = day;
                    Ok(match day {
                        Some(n) => format!("for tests, a day now lasts {n} seconds"),
                        None => "days are calendar days again".into(),
                    })
                },
            )
            .await
        }
        ["hold", pit] => {
            settings(
                &paths,
                serde_json::json!({"change": "hold", "pit": pit}),
                |s, j| {
                    let (cluster, id) = find_pit(j, pit)?;
                    s.hold(&cluster, id, &console(), now_ms());
                    Ok(format!("PIT {id} is held"))
                },
            )
            .await
        }
        ["unhold", pit] => {
            settings(
                &paths,
                serde_json::json!({"change": "unhold", "pit": pit}),
                |s, j| {
                    let (cluster, id) = find_pit(j, pit)?;
                    if !s.release(&cluster, &id) {
                        return Err(format!("PIT {id} isn't held"));
                    }
                    Ok(format!("PIT {id} is no longer held"))
                },
            )
            .await
        }
        ["prune"] => prune(&paths).await,
        ["peers"] => show_peers(&paths),
        ["trust", uuid, fingerprint, address] => trust(&paths, uuid, fingerprint, address).await,
        ["untrust", uuid] => untrust(&paths, uuid).await,
        ["grant", cluster, hours, pits @ ..] if !pits.is_empty() => {
            grant(&paths, cluster, hours, pits).await
        }
        ["settings"] => show_settings(&paths),
        ["settings", "scrub-days", n] => {
            let n = count(n, "the days", 366)?;
            change_settings(&paths, |p| {
                p.scrub_every_days = n;
                Ok(())
            })
            .await
        }
        ["settings", "io", policy] => {
            let policy = match *policy {
                "pulls-first" => IoPolicy::PullsFirst,
                "scrub-first" => IoPolicy::ScrubFirst,
                "together" => IoPolicy::Together,
                _ => return Err("the I/O policy is pulls-first, scrub-first or together".into()),
            };
            change_settings(&paths, |p| {
                p.io_policy = policy;
                Ok(())
            })
            .await
        }
        ["settings", "watchdog-minutes", n] => {
            let n = count(n, "the minutes", 1440)?;
            change_settings(&paths, |p| {
                p.watchdog.down_after_minutes = n;
                Ok(())
            })
            .await
        }
        ["settings", "url", "add", url] => {
            change_settings(&paths, |p| {
                if !p.watchdog.urls.iter().any(|u| u == url) {
                    p.watchdog.urls.push(url.to_string());
                }
                Ok(())
            })
            .await
        }
        ["settings", "url", "remove", url] => {
            change_settings(&paths, |p| {
                let before = p.watchdog.urls.len();
                p.watchdog.urls.retain(|u| u != url);
                if p.watchdog.urls.len() == before {
                    return Err(format!("{url} isn't checked"));
                }
                Ok(())
            })
            .await
        }
        ["settings", "email", from, to @ ..] => {
            let (from, to) = (from.to_string(), to.iter().map(|t| t.to_string()).collect());
            change_settings(&paths, |p| {
                p.notify = bmac_backupd::platform::Notify { from, to };
                Ok(())
            })
            .await
        }
        ["settings", "postmark-token"] => postmark_token(&paths, false),
        ["settings", "postmark-token", "--clear"] => postmark_token(&paths, true),
        ["scrub"] => scrub(&paths, false).await,
        ["scrub", "now"] => scrub(&paths, true).await,
        ["audit"] => show_audit(&paths, "50"),
        ["audit", n] => show_audit(&paths, n),
        ["grants"] => show_grants(&paths),
        ["revoke-grant", id] => revoke_grant(&paths, id).await,
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("bmac-backupd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bmac-backupd: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dispatch(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bmac-backupd: {e}");
            ExitCode::FAILURE
        }
    }
}
