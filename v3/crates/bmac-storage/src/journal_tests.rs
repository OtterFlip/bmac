// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::os::unix::fs::PermissionsExt;

use super::*;

fn disk(serial: &str) -> DiskIdentity {
    DiskIdentity::Serial(serial.into())
}

fn prod(n: u32) -> GuestName {
    GuestName::Production { number: n }
}

fn member(serial: Option<&str>) -> RemovedMember {
    RemovedMember {
        identity: serial.map(disk),
        mapper: serial.map(|s| format!("bmac-rpool-s{s}")),
        size: Some(1 << 30),
    }
}

fn journal() -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::new(dir.path().join("bmac").join("storage-state.json"));
    (dir, journal)
}

#[test]
fn a_missing_journal_reads_as_empty_and_the_first_change_creates_it_privately() {
    let (_dir, journal) = journal();
    assert_eq!(journal.read().unwrap(), State::default());
    journal
        .update(|s| {
            s.record_trims(&[prod(1)], 100);
            Ok(())
        })
        .unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(journal.path()), 0o600);
    assert_eq!(mode(journal.path().parent().unwrap()), 0o700);
    assert!(journal.read().unwrap().recently_trimmed(prod(1), 100));
    let text = std::fs::read_to_string(journal.path()).unwrap();
    assert!(text.contains("\"prod1\""), "{text}");
}

#[test]
fn a_failed_change_writes_nothing() {
    let (_dir, journal) = journal();
    journal
        .update(|s| s.record_scrub("scrub repaired 0B with 0 errors", 1))
        .unwrap();
    let before = std::fs::read(journal.path()).unwrap();
    let refused = journal.update(|s| {
        s.record_trims(&[prod(2)], 5);
        s.record_scrub("two\nlines", 5)
    });
    assert!(refused.is_err());
    assert_eq!(std::fs::read(journal.path()).unwrap(), before);
}

#[test]
fn a_symlinked_or_unknown_journal_is_refused() {
    let (dir, journal) = journal();
    std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
    let elsewhere = dir.path().join("elsewhere.json");
    std::fs::write(&elsewhere, "{\"schema_version\": 1}").unwrap();
    std::os::unix::fs::symlink(&elsewhere, journal.path()).unwrap();
    assert!(
        journal
            .read()
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
    std::fs::remove_file(journal.path()).unwrap();
    std::fs::write(journal.path(), "{\"schema_version\": 2}").unwrap();
    assert!(journal.read().unwrap_err().to_string().contains("schema 2"));
    std::fs::write(journal.path(), "{\"schema_version\": 1, \"extra\": 1}").unwrap();
    assert!(journal.read().is_err(), "unknown fields are refused");
}

#[test]
fn trims_and_clean_scrubs_are_reused_for_a_day() {
    let mut s = State::default();
    s.record_trims(&[prod(1), prod(2)], 1_000);
    assert!(s.recently_trimmed(prod(1), 1_000 + RECENT_SECONDS - 1));
    assert!(!s.recently_trimmed(prod(1), 1_000 + RECENT_SECONDS));
    assert!(!s.recently_trimmed(prod(3), 1_000));

    s.record_scrub("scrub repaired 0B in 00:01:02 with 0 errors on Tue", 2_000)
        .unwrap();
    assert!(s.recent_clean_scrub(2_100).is_some());
    assert!(s.recent_clean_scrub(2_000 + RECENT_SECONDS).is_none());
    s.record_scrub("scrub repaired 0B in 00:01:02 with 3 errors on Tue", 2_200)
        .unwrap();
    assert!(
        s.recent_clean_scrub(2_300).is_none(),
        "the last scrub counts"
    );
    for n in 0..30 {
        s.record_scrub("scrub repaired 0B with 0 errors", 3_000 + n)
            .unwrap();
    }
    assert_eq!(s.scrubs.len(), MAX_SCRUBS);
    assert_eq!(s.scrubs[0].completed_at, 3_010);
}

#[test]
fn one_removal_at_a_time_with_its_members_kept() {
    let mut s = State::default();
    assert!(
        s.record_removal("mirror-1", vec![member(None)], 1).is_err(),
        "a vdev whose disks are all gone can't be tracked"
    );
    let id = s
        .record_removal("mirror-1", vec![member(Some("A")), member(None)], 10)
        .unwrap();
    assert_eq!(id, "removal-10-mirror-1");
    assert_eq!(s.removal_in_progress().unwrap().members.len(), 2);
    let second = s.record_removal("mirror-2", vec![member(Some("B"))], 11);
    assert!(
        second
            .unwrap_err()
            .to_string()
            .contains("mirror-1 is still in progress")
    );

    s.set_removal(&id, RemovalState::Failed, Some("zpool remove refused"), 12)
        .unwrap();
    assert!(s.removal_in_progress().is_none());
    assert_eq!(s.removals[0].notes[0].note, "zpool remove refused");
    assert_eq!(s.removals[0].updated_at, 12);
    let next = s
        .record_removal("mirror-2", vec![member(Some("B"))], 13)
        .unwrap();
    s.set_removal(&next, RemovalState::Retired, None, 20)
        .unwrap();
    assert!(
        s.set_removal("removal-0-x", RemovalState::Retired, None, 21)
            .is_err()
    );
    assert!(s.validate().is_ok());
}

fn replacement(leaf: &str, vdev: &str) -> Replacement {
    Replacement {
        leaf: leaf.into(),
        old: None,
        mapper: None,
        vdev: vdev.into(),
        recorded_at: 5,
    }
}

#[test]
fn a_replacement_is_remembered_by_the_new_disk_with_the_member_it_replaces() {
    let mut s = State::default();
    s.record_replacement(
        &disk("NEW"),
        replacement("/dev/mapper/bmac-rpool-sOLD", "mirror-0"),
    )
    .unwrap();
    let r = s.replacing(&disk("NEW")).unwrap();
    assert_eq!(
        (r.leaf.as_str(), r.vdev.as_str()),
        ("/dev/mapper/bmac-rpool-sOLD", "mirror-0")
    );
    assert!(
        s.record_replacement(
            &disk("OTHER"),
            replacement("/dev/mapper/bmac-rpool-sOLD", "mirror-0")
        )
        .is_err(),
        "two disks can't replace one member"
    );
    s.record_replacement(
        &disk("X"),
        Replacement {
            old: Some(disk("X")),
            ..replacement("/dev/x", "mirror-1")
        },
    )
    .unwrap();
    s.record_replacement(&disk("NEW"), replacement("/dev/y", "mirror-0"))
        .unwrap();
    assert_eq!(
        s.replacing(&disk("NEW")).unwrap().leaf,
        "/dev/y",
        "the choice changed"
    );
    s.clear_replacement(&disk("NEW"));
    s.clear_replacement(&disk("X"));
    assert!(s.replacements.is_empty());
}

#[test]
fn additions_are_numbered_without_reusing_a_number() {
    let mut s = State::default();
    let first = s
        .begin_addition(
            VdevClass::Data,
            VdevType::Raidz2,
            vec![disk("A"), disk("B"), disk("C"), disk("D")],
            1,
        )
        .unwrap();
    assert_eq!(first, 1);
    assert!(
        s.begin_addition(VdevClass::Log, VdevType::Single, vec![disk("C")], 2)
            .is_err(),
        "a disk chosen for one new vdev can't be chosen for another"
    );
    s.finish_addition(first);
    let second = s
        .begin_addition(VdevClass::Log, VdevType::Single, vec![disk("C")], 3)
        .unwrap();
    assert_eq!(second, 2);
    assert!(
        s.begin_addition(VdevClass::Spare, VdevType::Single, vec![], 4)
            .is_err()
    );
    s.last_addition = 1;
    assert!(s.validate().is_err(), "a number that was never handed out");
}

#[test]
fn one_failure_drill_at_a_time() {
    let mut s = State::default();
    let drill = Simulation {
        vdev: "mirror-1".into(),
        leaf: "/dev/mapper/bmac-rpool-sA".into(),
        disk: disk("A"),
        mapper: Some("bmac-rpool-sA".into()),
        stage: SimulationStage::Offline,
        started_at: 1,
    };
    s.begin_simulation(drill.clone()).unwrap();
    assert!(
        s.begin_simulation(drill)
            .unwrap_err()
            .to_string()
            .contains("already under way")
    );
}

#[test]
fn concurrent_changes_are_serialized_by_the_lock() {
    let (_dir, journal) = journal();
    let threads: Vec<_> = (0..8)
        .map(|n| {
            let journal = journal.clone();
            std::thread::spawn(move || {
                journal
                    .update(|s| {
                        s.begin_addition(
                            VdevClass::Cache,
                            VdevType::Single,
                            vec![disk(&format!("D{n}"))],
                            1,
                        )
                    })
                    .unwrap()
            })
        })
        .collect();
    let mut numbers: Vec<u64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=8).collect::<Vec<_>>());
    assert_eq!(journal.read().unwrap().additions.len(), 8);
}
