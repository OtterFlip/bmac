// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmac capacity --json`: this host's pool space and data vdev geometry,
//! and, for a zvol size and block size, whether a new copy fits above the
//! 10% reserve, using the capacity function of `bmac-storage`
//! (`STORAGE.md`, "One capacity function"). Script operations call it on
//! each placement host instead of doing their own arithmetic.

use bmac_adapters::AdapterError;
use bmac_adapters::zfs::Zfs;
use bmac_adapters::zpool::{ConfigNode, PoolStatus, VdevClass, Zpool, int_property};
use bmac_core::{DatasetName, HostSlot, PoolName};
use bmac_exec::CommandRunner;
use bmac_storage::capacity::{Growth, HostPool, Placement, growth_allowance};
use bmac_storage::{BlockSize, Geometry, PoolGeometry, PoolSpace, VdevType, ZvolAllocation};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fit {
    pub volsize: u64,
    pub block_size: u32,
    /// `V × I(b) × (1 + m(b))` on this pool.
    pub needed: u64,
    pub fits: bool,
    /// Bytes left above the reserve after the copy, when it fits.
    pub spare: Option<u64>,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capacity {
    pub host: HostSlot,
    pub pool: String,
    pub space: PoolSpace,
    pub total: u64,
    pub reserve: u64,
    pub headroom: u64,
    pub geometry: PoolGeometry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
}

fn vdev_geometry(vdev: &ConfigNode, ashift: u8) -> Result<Geometry, String> {
    let kind = match vdev.vdev_type().as_str() {
        "disk" => VdevType::Single,
        "mirror" => VdevType::Mirror,
        "raidz" | "raidz1" => VdevType::Raidz1,
        "raidz2" => VdevType::Raidz2,
        "raidz3" => VdevType::Raidz3,
        other => return Err(format!("{}: vdev type {other} is not supported", vdev.name)),
    };
    // A `replacing-N` or `spare-N` group stands for one member.
    let width = vdev.children.len().max(1);
    if width < kind.min_members() {
        return Err(format!(
            "{}: {width} members is too few for {kind}",
            vdev.name
        ));
    }
    Ok(Geometry::new(kind, width as u32, ashift))
}

/// The data vdevs' geometry. `ashift` is the pool's property; 0 means ZFS
/// chose per vdev and the sector size isn't known, so it is refused.
pub fn geometry(status: &PoolStatus, ashift: u8) -> Result<PoolGeometry, String> {
    if !(9..=16).contains(&ashift) {
        return Err(format!(
            "pool {} has ashift {ashift}; BMAC pools are created with an explicit ashift",
            status.pool
        ));
    }
    let data = status
        .vdevs(VdevClass::Data)
        .iter()
        .map(|vdev| vdev_geometry(vdev, ashift))
        .collect::<Result<Vec<_>, _>>()?;
    if data.is_empty() {
        return Err(format!("pool {} has no data vdevs", status.pool));
    }
    Ok(PoolGeometry { data })
}

pub fn compute(
    host: HostSlot,
    status: &PoolStatus,
    ashift: u8,
    space: PoolSpace,
    zvol: Option<(u64, BlockSize)>,
) -> Result<Capacity, String> {
    let geometry = geometry(status, ashift)?;
    let fit = zvol.map(|(volsize, block)| {
        let needed = geometry.space_needed(volsize, block);
        let placement = Placement::check(space, needed);
        Fit {
            volsize,
            block_size: block.into(),
            needed,
            fits: placement.fits(),
            spare: placement.spare(),
            explanation: placement.explain(host),
        }
    });
    Ok(Capacity {
        host,
        pool: status.pool.clone(),
        space,
        total: space.total(),
        reserve: space.reserve(),
        headroom: space.headroom(),
        geometry,
        fit,
    })
}

/// Reads `pool` on this host.
pub async fn collect(
    runner: &dyn CommandRunner,
    host: HostSlot,
    pool: &PoolName,
    zvol: Option<(u64, BlockSize)>,
) -> Result<Capacity, String> {
    let text = |e: AdapterError| e.to_string();
    let zpool = Zpool::new(runner);
    let status = zpool.status(pool).await.map_err(text)?;
    let properties = zpool.properties(pool, &["ashift"]).await.map_err(text)?;
    let ashift = int_property(&properties, "ashift")
        .and_then(|a| u8::try_from(a).ok())
        .ok_or_else(|| format!("pool {pool} reported no ashift"))?;
    let dataset: DatasetName = pool.as_str().parse().map_err(|e| format!("{e}"))?;
    let space = Zfs::new(runner).space(&dataset).await.map_err(text)?;
    compute(
        host,
        &status,
        ashift,
        PoolSpace {
            used: space.used,
            available: space.available,
        },
        zvol,
    )
}

/// `bmac growth --json`: how far a disk may grow on every placement host,
/// from each host's `bmac capacity --json` output, concatenated in `input`.
pub fn growth(
    input: &str,
    allocation: ZvolAllocation,
    volsize: u64,
    block: BlockSize,
) -> Result<Growth, String> {
    let hosts = serde_json::Deserializer::from_str(input)
        .into_iter::<HostPool>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("not bmac capacity --json output: {e}"))?;
    let mut seen = std::collections::BTreeSet::new();
    if let Some(pool) = hosts.iter().find(|pool| !seen.insert(pool.host)) {
        return Err(format!("{} is listed twice", pool.host));
    }
    growth_allowance(allocation, volsize, block, &hosts).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use bmac_adapters::programs;
    use bmac_adapters::zpool::parse_status;
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn mox(n: u8) -> HostSlot {
        HostSlot::new(n).unwrap()
    }

    fn status(config: &str) -> PoolStatus {
        parse_status(&format!(
            "  pool: rpool\n state: ONLINE\nconfig:\n\n\
             \tNAME        STATE     READ WRITE CKSUM\n\
             \trpool       ONLINE       0     0     0\n{config}\nerrors: No known data errors\n"
        ))
        .unwrap()
    }

    const MIRROR: &str = "\t  mirror-0  ONLINE       0     0     0\n\
                          \t    /dev/a  ONLINE       0     0     0\n\
                          \t    /dev/b  ONLINE       0     0     0\n";
    const RAIDZ2_5: &str = "\t  raidz2-0  ONLINE       0     0     0\n\
                            \t    /dev/a  ONLINE       0     0     0\n\
                            \t    /dev/b  ONLINE       0     0     0\n\
                            \t    /dev/c  ONLINE       0     0     0\n\
                            \t    /dev/d  ONLINE       0     0     0\n\
                            \t    /dev/e  ONLINE       0     0     0\n";

    #[test]
    fn a_mirror_charges_only_the_metadata_allowance() {
        let space = PoolSpace {
            used: 100 * GIB,
            available: 900 * GIB,
        };
        let capacity = compute(
            mox(1),
            &status(MIRROR),
            12,
            space,
            Some((100 * GIB, BlockSize::K16)),
        )
        .unwrap();
        assert_eq!(
            capacity.geometry.data,
            [Geometry::new(VdevType::Mirror, 2, 12)]
        );
        assert_eq!((capacity.total, capacity.reserve), (1000 * GIB, 100 * GIB));
        let fit = capacity.fit.unwrap();
        assert_eq!(fit.needed, 100 * GIB / 64 * 65);
        assert!(fit.fits);
        assert_eq!(fit.spare, Some(800 * GIB - fit.needed));
    }

    #[test]
    fn small_blocks_on_raidz_need_more_and_may_not_fit() {
        let space = PoolSpace {
            used: 0,
            available: 1000 * GIB,
        };
        let raidz = compute(
            mox(2),
            &status(RAIDZ2_5),
            12,
            space,
            Some((800 * GIB, BlockSize::K16)),
        )
        .unwrap();
        let fit = raidz.fit.unwrap();
        assert!(fit.needed > 800 * GIB, "raidz2 inflates 16 KiB blocks");
        assert!(!fit.fits);
        assert_eq!(fit.spare, None);
        assert!(fit.explanation.starts_with("mox2: "), "{}", fit.explanation);
        let larger = compute(
            mox(2),
            &status(RAIDZ2_5),
            12,
            space,
            Some((800 * GIB, BlockSize::K128)),
        )
        .unwrap();
        assert!(larger.fit.unwrap().needed < fit.needed);
    }

    #[test]
    fn unknown_sector_sizes_and_layouts_are_refused() {
        let space = PoolSpace {
            used: 0,
            available: GIB,
        };
        let error = compute(mox(1), &status(MIRROR), 0, space, None).unwrap_err();
        assert!(error.contains("ashift 0"), "{error}");
        let draid = "\t  draid1:2d:4c:0s-0  ONLINE  0 0 0\n\t    /dev/a  ONLINE  0 0 0\n";
        assert!(
            compute(mox(1), &status(draid), 12, space, None)
                .unwrap_err()
                .contains("not supported")
        );
        let lone = compute(
            mox(1),
            &status("\t  /dev/a  ONLINE  0 0 0\n"),
            12,
            space,
            None,
        )
        .unwrap();
        assert_eq!(lone.geometry.data, [Geometry::new(VdevType::Single, 1, 12)]);
    }

    #[tokio::test]
    async fn reads_the_pool_with_read_only_commands() {
        let fake = FakeRunner::new();
        let status_text = format!(
            "  pool: rpool\n state: ONLINE\nconfig:\n\n\
             \tNAME        STATE     READ WRITE CKSUM\n\
             \trpool       ONLINE       0     0     0\n{MIRROR}\nerrors: No known data errors\n"
        );
        fake.expect(
            programs::ZPOOL,
            ["status", "-P", "-p", "rpool"],
            Reply::ok(status_text),
        )
        .expect(
            programs::ZPOOL,
            ["get", "-H", "-p", "-o", "property,value", "ashift", "rpool"],
            Reply::ok("ashift\t12\n"),
        )
        .expect(
            programs::ZFS,
            ["list", "-H", "-p", "-o", "used,available", "rpool"],
            Reply::ok(format!("{}\t{}\n", 10 * GIB, 90 * GIB)),
        );
        let capacity = collect(&fake, mox(1), &"rpool".parse().unwrap(), None)
            .await
            .unwrap();
        fake.assert_done();
        assert_eq!(capacity.headroom, 80 * GIB);
        let json = serde_json::to_value(&capacity).unwrap();
        assert_eq!(
            json["space"],
            serde_json::json!({"used": 10 * GIB, "available": 90 * GIB})
        );
        assert!(json.get("fit").is_none());
    }

    fn capacity_json(host: u8, config: &str, used: u64, available: u64) -> String {
        let space = PoolSpace { used, available };
        let capacity = compute(mox(host), &status(config), 12, space, None).unwrap();
        serde_json::to_string_pretty(&capacity).unwrap()
    }

    #[test]
    fn growth_takes_the_smallest_allowance_of_the_placement_hosts() {
        let input = capacity_json(1, MIRROR, 100 * GIB, 900 * GIB)
            + "\n"
            + &capacity_json(2, MIRROR, 500 * GIB, 500 * GIB);
        let sparse = ZvolAllocation::Sparse { guest_free: GIB };
        let growth = growth(&input, sparse, 64 * GIB, BlockSize::K16).unwrap();
        assert_eq!(growth.hosts.len(), 2);
        assert_eq!(growth.hosts[1].headroom, 400 * GIB);
        let expected = (400 * GIB / 65 * 64 - GIB) / (1024 * 1024) * (1024 * 1024);
        assert_eq!(growth.allowed, expected);
        assert!(growth.hosts[0].allowed > growth.hosts[1].allowed);
    }

    #[test]
    fn growth_on_raidz_charges_the_inflation() {
        let mirror = capacity_json(1, MIRROR, 0, 1000 * GIB);
        let raidz = capacity_json(1, RAIDZ2_5, 0, 1000 * GIB);
        let reserved = ZvolAllocation::Reserved {
            refreservation: 64 * GIB,
        };
        let on_mirror = growth(&mirror, reserved, 64 * GIB, BlockSize::K16).unwrap();
        let on_raidz = growth(&raidz, reserved, 64 * GIB, BlockSize::K16).unwrap();
        assert!(on_raidz.allowed < on_mirror.allowed);
    }

    #[test]
    fn growth_refuses_bad_input() {
        let sparse = ZvolAllocation::Sparse { guest_free: 0 };
        let one = capacity_json(1, MIRROR, 0, GIB);
        for (input, why) in [
            ("", "at least one placement host"),
            ("{\"host\":\"mox1\"}", "not bmac capacity"),
            (&*format!("{one}{one}"), "listed twice"),
        ] {
            let error = growth(input, sparse, GIB, BlockSize::K16).unwrap_err();
            assert!(error.contains(why), "{input}: {error}");
        }
    }
}
