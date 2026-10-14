// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmac-backupd init`: the appliance's identity and certificate, and its
//! binding to an imported pool by GUID, with the `bmac` datasets and the
//! journal. A pool is never created or imported here, and one that already
//! has another appliance's journal is refused (that is `adopt`).

use std::time::Duration;

use bmac_exec::{Command, CommandRunner};
use uuid::Uuid;

use crate::journal::{Journal, Layout, Metadata};
use crate::pool::{ZFS, pool_guid};
use crate::state::{self, Identity, Paths, PoolBinding, SCHEMA};

/// Makes the identity and certificate, unless they exist.
pub fn identity(paths: &Paths, name: &str, now_ms: i64) -> Result<Identity, String> {
    if let Some(existing) = state::load::<Identity>(&paths.identity()).map_err(|e| e.to_string())? {
        if existing.name != name {
            return Err(format!(
                "this appliance is already {}; its name doesn't change",
                existing.name
            ));
        }
        return Ok(existing);
    }
    make_identity(paths, name, Uuid::new_v4(), now_ms)
}

fn make_identity(
    paths: &Paths,
    name: &str,
    appliance_uuid: Uuid,
    now_ms: i64,
) -> Result<Identity, String> {
    if !state::is_name(name) {
        return Err("the name must be a lowercase DNS label".into());
    }
    let (cert, key) = state::certificate(name, &appliance_uuid)?;
    crate::fsx::write_atomic(&paths.key(), key.as_bytes(), 0o600).map_err(|e| e.to_string())?;
    crate::fsx::write_atomic(&paths.cert(), cert.as_bytes(), 0o644).map_err(|e| e.to_string())?;
    let identity = Identity {
        schema_version: SCHEMA,
        appliance_uuid,
        name: name.to_string(),
        created_at_ms: now_ms,
    };
    state::save(&paths.identity(), &identity).map_err(|e| e.to_string())?;
    Ok(identity)
}

async fn zfs(runner: &dyn CommandRunner, args: &[&str]) -> Result<String, String> {
    runner
        .run(
            Command::new(ZFS)
                .args(args.iter().copied())
                .timeout(Duration::from_secs(120)),
        )
        .await
        .map(|o| o.stdout_text())
        .map_err(|e| e.to_string())
}

async fn exists(runner: &dyn CommandRunner, dataset: &str) -> Result<bool, String> {
    match zfs(runner, &["list", "-H", "-o", "name", dataset]).await {
        Ok(_) => Ok(true),
        Err(e) if e.contains("does not exist") => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether the metadata dataset is mounted. The service never mounts it:
/// its mount namespace would keep the mount from everyone else.
pub async fn metadata_mounted(runner: &dyn CommandRunner, layout: &Layout) -> bool {
    zfs(
        runner,
        &["get", "-H", "-o", "value", "mounted", &layout.metadata()],
    )
    .await
    .is_ok_and(|m| m.trim() == "yes")
}

/// Mounts the metadata dataset at the state directory's `metadata`.
pub async fn mount_metadata(
    runner: &dyn CommandRunner,
    paths: &Paths,
    layout: &Layout,
) -> Result<(), String> {
    let mounted = zfs(
        runner,
        &["get", "-H", "-o", "value", "mounted", &layout.metadata()],
    )
    .await?;
    if mounted.trim() == "yes" {
        return Ok(());
    }
    let mountpoint = paths.metadata();
    zfs(
        runner,
        &[
            "set",
            &format!("mountpoint={}", mountpoint.display()),
            &layout.metadata(),
        ],
    )
    .await?;
    zfs(runner, &["mount", &layout.metadata()]).await.map(drop)
}

/// Binds `pool` (imported) to this appliance and prepares its datasets.
pub async fn bind(
    runner: &dyn CommandRunner,
    paths: &Paths,
    identity: &Identity,
    pool: &str,
) -> Result<PoolBinding, String> {
    let guid = pool_guid(runner, pool)
        .await?
        .ok_or_else(|| format!("no pool {pool} is imported"))?;
    if let Some(bound) = state::load::<PoolBinding>(&paths.pool()).map_err(|e| e.to_string())?
        && (bound.pool_guid != guid || bound.pool != pool)
    {
        return Err(format!(
            "this appliance is bound to pool {} (GUID {}); it never writes another",
            bound.pool, bound.pool_guid
        ));
    }
    let layout = Layout {
        pool: pool.to_string(),
    };
    let base = layout.base();
    if !exists(runner, &base).await? {
        zfs(
            runner,
            &[
                "create",
                "-o",
                "mountpoint=none",
                "-o",
                "canmount=off",
                "-o",
                "compression=on",
                "-o",
                "dedup=off",
                &base,
            ],
        )
        .await?;
    }
    let clusters = format!("{base}/clusters");
    if !exists(runner, &clusters).await? {
        zfs(
            runner,
            &[
                "create",
                "-o",
                "mountpoint=none",
                "-o",
                "canmount=off",
                &clusters,
            ],
        )
        .await?;
    }
    if !exists(runner, &layout.metadata()).await? {
        let mountpoint = format!("mountpoint={}", paths.metadata().display());
        zfs(runner, &["create", "-o", &mountpoint, &layout.metadata()]).await?;
    }
    mount_metadata(runner, paths, &layout).await?;
    let metadata = Metadata::new(paths.metadata());
    match metadata.load().map_err(|e| e.to_string())? {
        Some(journal) if journal.appliance_uuid != identity.appliance_uuid => {
            return Err(format!(
                "pool {pool} holds appliance {}'s backups; run bmac-backupd adopt to take it over",
                journal.appliance_uuid
            ));
        }
        Some(journal) if journal.pool_guid != guid => {
            return Err(format!(
                "pool {pool}'s journal names pool GUID {}, not {guid}",
                journal.pool_guid
            ));
        }
        Some(_) => {}
        None => metadata
            .save(&Journal::new(identity.appliance_uuid, &guid))
            .map_err(|e| format!("cannot start the journal: {e}"))?,
    }
    crate::admin::save_note(&paths.metadata(), identity.appliance_uuid, &identity.name)?;
    let binding = PoolBinding {
        schema_version: SCHEMA,
        pool: pool.to_string(),
        pool_guid: guid,
    };
    state::save(&paths.pool(), &binding).map_err(|e| e.to_string())?;
    Ok(binding)
}

/// How `adopt` learns that the administrator is there.
pub enum Credential<'a> {
    /// Checked against the hash in the metadata dataset.
    Password(&'a str),
    /// At the console, accepted only when no password was ever set.
    Console,
}

/// `bmac-backupd adopt`: after an OS reinstall, takes over an imported pool
/// another install of this appliance wrote. The appliance keeps the
/// journal's UUID and its name, gets a new certificate, and must enroll
/// again with each cluster.
pub async fn adopt(
    runner: &dyn CommandRunner,
    paths: &Paths,
    pool: &str,
    credential: Credential<'_>,
    now_ms: i64,
) -> Result<(Identity, PoolBinding), String> {
    if let Some(existing) = state::load::<Identity>(&paths.identity()).map_err(|e| e.to_string())? {
        return Err(format!(
            "this OS is already appliance {} ({}); adopt is for a fresh install",
            existing.name, existing.appliance_uuid
        ));
    }
    let guid = pool_guid(runner, pool)
        .await?
        .ok_or_else(|| format!("no pool {pool} is imported; run bmac-backupd unlock first"))?;
    let layout = Layout {
        pool: pool.to_string(),
    };
    if !exists(runner, &layout.metadata()).await? {
        return Err(format!(
            "pool {pool} has no {}; it isn't a backup pool",
            layout.metadata()
        ));
    }
    mount_metadata(runner, paths, &layout).await?;
    let metadata = Metadata::new(paths.metadata());
    let journal = metadata
        .load()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("pool {pool} has no journal"))?;
    if journal.pool_guid != guid {
        return Err(format!(
            "pool {pool}'s journal names pool GUID {}, not {guid}",
            journal.pool_guid
        ));
    }
    let admin = crate::admin::load(&paths.metadata())?;
    match (&admin, credential) {
        (Some(admin), Credential::Password(password))
            if crate::admin::matches(&admin.password_hash, password) => {}
        (Some(_), _) => return Err("that isn't the administrator's password".into()),
        (None, Credential::Console) => {}
        (None, Credential::Password(_)) => {
            return Err(
                "no administrator password was ever set on this pool; run adopt at the console"
                    .into(),
            );
        }
    }
    let note = crate::admin::note(&paths.metadata())?
        .filter(|n| n.appliance_uuid == journal.appliance_uuid)
        .ok_or("the pool doesn't record this appliance's name")?;
    let identity = make_identity(paths, &note.name, journal.appliance_uuid, now_ms)?;
    let binding = PoolBinding {
        schema_version: SCHEMA,
        pool: pool.to_string(),
        pool_guid: guid,
    };
    state::save(&paths.pool(), &binding).map_err(|e| e.to_string())?;
    let _ = crate::audit::Audit::new(&paths.metadata()).record(
        now_ms,
        "adopt",
        "console",
        serde_json::json!({
            "appliance_uuid": identity.appliance_uuid,
            "pool_guid": binding.pool_guid,
            "fingerprint": crate::state::pem_fingerprint(
                &std::fs::read_to_string(paths.cert()).map_err(|e| e.to_string())?
            )?,
        }),
    );
    Ok((identity, binding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_made_once_and_keeps_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let first = identity(&paths, "vault", 1).unwrap();
        assert_eq!(identity(&paths, "vault", 2).unwrap(), first);
        assert!(identity(&paths, "other", 3).is_err());
        assert!(paths.cert().exists() && paths.key().exists());
        let other = Paths::at(dir.path().join("x"));
        assert!(identity(&other, "Bad_Name", 1).is_err());
    }

    fn mounted_pool(fake: &bmac_exec::FakeRunner) {
        use bmac_exec::Reply;
        fake.expect(
            crate::pool::ZPOOL,
            ["list", "-Hp", "-o", "name,guid"],
            Reply::ok("rpool\t42\nbk\t777\n"),
        );
        fake.expect(
            ZFS,
            ["list", "-H", "-o", "name", "bk/bmac/metadata"],
            Reply::ok("bk/bmac/metadata\n"),
        );
        fake.expect(
            ZFS,
            ["get", "-H", "-o", "value", "mounted", "bk/bmac/metadata"],
            Reply::ok("yes\n"),
        );
    }

    #[tokio::test]
    async fn adopt_keeps_the_appliance_uuid_only_with_the_administrators_password() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path());
        let old = Uuid::from_u128(42);
        Metadata::new(paths.metadata())
            .save(&Journal::new(old, "777"))
            .unwrap();
        crate::admin::save_note(&paths.metadata(), old, "vault").unwrap();
        crate::admin::set_password(&paths.metadata(), "the admin password", 1, 1000).unwrap();

        let fake = bmac_exec::FakeRunner::new();
        mounted_pool(&fake);
        let refused = adopt(
            &fake,
            &paths,
            "bk",
            Credential::Password("guess guess guess"),
            2,
        )
        .await
        .unwrap_err();
        assert!(
            refused.contains("isn't the administrator's password"),
            "{refused}"
        );
        mounted_pool(&fake);
        let refused = adopt(&fake, &paths, "bk", Credential::Console, 2)
            .await
            .unwrap_err();
        assert!(
            refused.contains("isn't the administrator's password"),
            "{refused}"
        );
        assert!(!paths.identity().exists());

        mounted_pool(&fake);
        let (identity, binding) = adopt(
            &fake,
            &paths,
            "bk",
            Credential::Password("the admin password"),
            3,
        )
        .await
        .unwrap();
        fake.assert_done();
        assert_eq!(
            (identity.appliance_uuid, identity.name.as_str()),
            (old, "vault")
        );
        assert_eq!(binding.pool_guid, "777");
        use rustls::pki_types::pem::PemObject;
        let pem = std::fs::read_to_string(paths.cert()).unwrap();
        let der = rustls::pki_types::CertificateDer::from_pem_slice(pem.as_bytes()).unwrap();
        assert!(state::claimed(&der).contains(&old.to_string()));
        let audit = crate::audit::Audit::new(&paths.metadata())
            .recent(5)
            .unwrap();
        assert_eq!(audit.last().unwrap().event, "adopt");

        let again = adopt(&fake, &paths, "bk", Credential::Console, 4)
            .await
            .unwrap_err();
        assert!(again.contains("already appliance vault"), "{again}");
    }
}
