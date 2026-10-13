// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The capacity function against `STORAGE.md`'s examples table, read from
//! the document itself, and its bounds over every supported geometry.

use bmac_storage::geometry::{BlockSize, Geometry, PoolGeometry, Ratio};
use bmac_storage::layout::VdevType;

fn storage_md() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../design/STORAGE.md");
    std::fs::read_to_string(path).unwrap()
}

fn geometry_for(label: &str) -> Geometry {
    if let Some(width) = label.strip_suffix("-way mirror") {
        return Geometry::new(VdevType::Mirror, width.parse().unwrap(), 12);
    }
    let (kind, width) = label.split_once(", ").unwrap();
    let kind = match kind {
        "raidz1" => VdevType::Raidz1,
        "raidz2" => VdevType::Raidz2,
        "raidz3" => VdevType::Raidz3,
        other => panic!("unknown layout {other}"),
    };
    Geometry::new(kind, width.trim_end_matches(" disks").parse().unwrap(), 12)
}

fn percent(r: Ratio) -> String {
    format!("{:.1}%", r.as_f64() * 100.0)
}

#[test]
fn storage_md_examples_table_is_exact() {
    let text = storage_md();
    let start = text
        .find("### Examples (ashift 12, 4 KiB sectors)")
        .expect("examples heading");
    let rows: Vec<Vec<String>> = text[start..]
        .lines()
        .skip_while(|l| !l.starts_with("| Layout"))
        .skip(2)
        .take_while(|l| l.starts_with("| "))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect();
    assert_eq!(rows.len(), 10, "the table lost or gained rows");
    let k = |n: u64| n * 1024;
    for row in rows {
        let g = geometry_for(&row[0]);
        let computed = vec![
            row[0].clone(),
            percent(g.efficiency(k(16))),
            percent(g.efficiency(k(64))),
            percent(g.efficiency(k(128))),
            g.inflation(BlockSize::K16).to_string(),
            g.inflation(BlockSize::K64).to_string(),
        ];
        assert_eq!(computed, row, "{}", row[0]);
    }
}

fn every_geometry() -> impl Iterator<Item = Geometry> {
    (9..=16u8).flat_map(|ashift| {
        let raidz = [VdevType::Raidz1, VdevType::Raidz2, VdevType::Raidz3]
            .into_iter()
            .flat_map(move |kind| {
                (kind.min_members() as u32..=24)
                    .map(move |width| Geometry::new(kind, width, ashift))
            });
        let mirrors = (2..=4).map(move |width| Geometry::new(VdevType::Mirror, width, ashift));
        raidz
            .chain(mirrors)
            .chain([Geometry::new(VdevType::Single, 1, ashift)])
    })
}

fn every_block() -> impl Iterator<Item = BlockSize> {
    (9..=17).map(|shift| BlockSize::new(1 << shift).unwrap())
}

#[test]
fn efficiency_never_exceeds_the_ideal_ratio() {
    for g in every_geometry() {
        for bytes in (9..=24).map(|s| 1u64 << s).chain([4096 * 3, 100_000, 1]) {
            let e = g.efficiency(bytes);
            assert!(e <= g.ideal_efficiency(), "{g:?} at {bytes}: {e:?}");
            assert!(e.num > 0, "{g:?} at {bytes}");
        }
    }
}

#[test]
fn inflation_is_never_below_one() {
    for g in every_geometry() {
        for block in every_block() {
            let i = g.inflation(block);
            assert!(i >= Ratio::ONE, "{g:?} at {block}: {i:?}");
            if !g.kind.is_raidz() {
                assert_eq!(i, Ratio::ONE, "{g:?} at {block}");
            }
        }
        assert_eq!(g.inflation(BlockSize::K128), Ratio::ONE);
    }
}

#[test]
fn space_needed_covers_the_volume_and_vm_space_fits_the_report() {
    let tib = 1u64 << 40;
    for g in every_geometry() {
        let pool = PoolGeometry { data: vec![g] };
        for block in every_block() {
            assert!(pool.space_needed(tib, block) > tib, "{g:?} at {block}");
            assert!(
                g.vm_bytes(tib, block) <= g.reported_bytes(tib),
                "{g:?} at {block}"
            );
            assert!(g.reported_bytes(tib) <= g.nominal_bytes(tib), "{g:?}");
        }
    }
}
