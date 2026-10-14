// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The capacity function against the lab's measurements (M8.5,
//! `lab/scenarios/m8_capacity.py`): fully written 512 MiB zvols of every
//! offered block size on raidz1, raidz2, and raidz3 pools of several
//! widths, ashift 12, compression off. The prediction must be within 1% of
//! what ZFS reports as `used`, and never below it.

use bmac_storage::geometry::{BlockSize, Geometry, PoolGeometry};
use bmac_storage::layout::VdevType;
use serde::Deserialize;

#[derive(Deserialize)]
struct Row {
    kind: String,
    width: u32,
    ashift: u8,
    volsize: u64,
    block_size: u32,
    used: u64,
}

fn rows() -> Vec<Row> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/lab/capacity.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn predictions_are_within_one_percent_above_the_lab_measurements() {
    let rows = rows();
    assert_eq!(rows.len(), 44, "11 layouts × 4 block sizes");
    let mut failures = Vec::new();
    for row in rows {
        let kind = match row.kind.as_str() {
            "raidz1" => VdevType::Raidz1,
            "raidz2" => VdevType::Raidz2,
            "raidz3" => VdevType::Raidz3,
            other => panic!("unknown layout {other}"),
        };
        let pool = PoolGeometry {
            data: vec![Geometry::new(kind, row.width, row.ashift)],
        };
        let block = BlockSize::new(row.block_size).unwrap();
        let predicted = pool.space_needed(row.volsize, block);
        let error = (predicted as f64 - row.used as f64) / row.used as f64;
        if !(0.0..=0.01).contains(&error) {
            failures.push(format!(
                "{} of {}, {block}: used {}, predicted {predicted} ({:+.2}%)",
                row.kind,
                row.width,
                row.used,
                error * 100.0
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
