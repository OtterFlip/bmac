// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Demo backups: two appliances, a VM whose backups continued on a new branch
// after a rollback, a restore test, and recent transfers.

import type { ApplianceList, BackupsView, PitNodeView } from "@bmac/ui";

const VAULT = "6c2f0a3e-0000-4000-8000-00000000000a";
const OFFSITE = "6c2f0a3e-0000-4000-8000-00000000000b";
const PROD1 = "d4e83cc4-3ecf-4003-9684-ffd01c4f40ec";
const MAIN = "2cfe516c-becb-4cca-b47b-aa4f8387a1e8";
const AFTER_ROLLBACK = "7a1d0c55-1111-4b2e-9c3d-5e6f7a8b9c0d";

function uuid(n: number) {
  return `00000000-0000-4000-8000-${n.toString(16).padStart(12, "0")}`;
}

function pitName(takenS: number, n: number) {
  const iso = new Date(takenS * 1000).toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");
  return `bmac-pit-${iso}-${n.toString(16).padStart(6, "0")}`;
}

function demoPits(nowS: number): PitNodeView[] {
  const day = 86_400;
  const pits: PitNodeView[] = [];
  const add = (n: number, branch: string, parent: number | null, daysAgo: number, offers: [string, string, number | null][]) =>
    pits.push({
      pit_uuid: uuid(n),
      branch_uuid: branch,
      parent: parent === null ? null : uuid(parent),
      name: pitName(nowS - daysAgo * day, n),
      taken_s: nowS - daysAgo * day,
      presence: daysAgo > 2 ? "bookmark" : "snapshot",
      consistency: "filesystem",
      offers: offers.map(([appliance_uuid, state, bytes]) => ({ appliance_uuid, state, bytes })),
      release_blocked: null,
    });
  const both: [string, string, number | null][] = [
    [VAULT, "published", null],
    [OFFSITE, "published", null],
  ];
  add(1, MAIN, null, 6, both);
  add(2, MAIN, 1, 5, both);
  add(3, MAIN, 2, 4, both);
  add(4, AFTER_ROLLBACK, 2, 3, both);
  add(5, AFTER_ROLLBACK, 4, 2, both);
  add(6, AFTER_ROLLBACK, 5, 1, [
    [VAULT, "published", null],
    [OFFSITE, "claimed", 1_342_177_280],
  ]);
  return pits;
}

export function demoAppliances(now = Date.now()): ApplianceList {
  const appliance = (appliance_uuid: string, name: string, address: string, fingerprint: string) => ({
    appliance_uuid,
    name,
    fingerprint,
    address,
    enrollment: "approved",
    enrolled_at_ms: now - 30 * 86_400_000,
    last_checkin_ms: now - 40_000,
    version: "3.0.0",
    locked: false,
    paused: false,
    retired: false,
  });
  return {
    appliances: [
      appliance(VAULT, "vault", "100.64.0.20", "3F:9A:11:C2:7B:0E:44:5D:A8:19:E2:6C:0B:93:7F:21:55:AC:D0:48:1E:6B:F2:39:C7:84:0A:5E:B1:66:2D:F3"),
      appliance(OFFSITE, "offsite", "100.64.0.21", "A1:04:7C:3E:92:5B:D8:16:4F:E0:2A:B7:69:C3:08:5D:F1:4A:93:2E:B6:70:1C:D5:8F:3B:E9:62:07:A4:5C:D8"),
    ],
    requests: [],
    can_manage: false,
  };
}

export function demoBackups(now = Date.now()): BackupsView {
  const nowS = Math.floor(now / 1000);
  const pits = demoPits(nowS);
  const appliances = demoAppliances(now).appliances;
  const status = (a: (typeof appliances)[number], state: string) => ({
    appliance_uuid: a.appliance_uuid,
    name: a.name,
    state,
    locked: false,
    last_checkin_ms: now - 40_000,
    scrub_errors: 0,
    suspect_pits: 0,
    other_copies: [],
  });
  return {
    at_ms: now,
    appliances: [
      { appliance: appliances[0], status: status(appliances[0], "ACTIVE"), live: [] },
      {
        appliance: appliances[1],
        status: status(appliances[1], "ACTIVE"),
        live: [{ vm: "prod1", vm_uuid: PROD1, pit_uuid: pits[5].pit_uuid, pit_name: pits[5].name, bytes: 1_342_177_280, progress_s: nowS - 5 }],
      },
    ],
    vms: [
      {
        vm: "prod1",
        vm_uuid: PROD1,
        vmid: 100,
        policy: {
          revision: 4,
          enabled: true,
          appliances: [VAULT, OFFSITE],
          local_newest: 2,
          local_days: 0,
          local_weeks: 0,
          every: "daily at 03:00",
          every_json: JSON.stringify({ kind: "daily", at: "03:00" }),
          freeze: "if_available",
          accept_multi_pool: false,
          updated_by: "alice@pve",
          updated_at_ms: now - 7 * 86_400_000,
        },
        status: {
          vm: "prod1",
          vm_uuid: PROD1,
          enabled: true,
          last_attempt: { outcome: "captured", at_ms: now - 86_400_000, message: null },
          pits: pits.length,
          snapshots: 2,
          release_blocked: [],
          healthy_appliances: 2,
          min_healthy: 1,
          overdue: [],
          unclaimed: [],
        },
        pits,
        branch_uuid: AFTER_ROLLBACK,
        local: [
          { host: "mox1", snapshots: 2, bytes: 734_003_200 },
          { host: "mox2", snapshots: 2, bytes: 734_003_200 },
          { host: "mox3", snapshots: 2, bytes: 734_003_200 },
        ],
      },
      { vm: "prod2", vm_uuid: uuid(0x200), vmid: 101, policy: null, status: null, pits: [], branch_uuid: null, local: [] },
    ],
    restores: [
      {
        restore_uuid: uuid(0x300),
        vm_uuid: PROD1,
        vm: "prod1",
        pit_uuid: pits[4].pit_uuid,
        appliance_uuid: VAULT,
        mode: "test",
        host: "mox2",
        vmid: 9001,
        name: "bmac-test-prod1",
        methods: { scsi0: 3, efidisk0: 3 },
        started_at_ms: now - 26 * 3_600_000,
        finished_at_ms: now - 26 * 3_600_000 + 540_000,
        ended_at_ms: now - 26 * 3_600_000 + 600_000,
        error: null,
        test: { booted: true, agent: true, agent_after_s: 21 },
      },
      {
        restore_uuid: uuid(0x301),
        vm_uuid: PROD1,
        vm: "prod1",
        pit_uuid: pits[1].pit_uuid,
        appliance_uuid: VAULT,
        mode: "rollback",
        host: "mox1",
        vmid: 100,
        name: "prod1",
        methods: { scsi0: 1, efidisk0: 1 },
        started_at_ms: now - 3.2 * 86_400_000,
        finished_at_ms: now - 3.2 * 86_400_000 + 20_000,
        ended_at_ms: null,
        error: null,
        test: null,
      },
    ],
    transfers: [0, 1, 2].map((i) => ({
      at_ms: now - (i + 1) * 86_400_000 + 600_000,
      host: "mox1",
      appliance: i === 0 ? "vault" : "offsite",
      vm_uuid: PROD1,
      pit_uuid: pits[5 - i].pit_uuid,
      volume_uuid: uuid(0x400),
      bytes: 412_000_000 - i * 50_000_000,
      duration_ms: 95_000 + i * 4000,
      error: null,
    })),
    can_manage: false,
    errors: [],
  };
}
