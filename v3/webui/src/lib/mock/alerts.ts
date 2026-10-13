// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Sample alerts and notification settings for the mock backend: the demo
// status's two warnings as alerts, one resolved alert in history, and email
// set up apart from the token.

import type { Alert, NotifyView } from "@bmac/ui";

export function demoAlerts(now = Date.now()): NotifyView {
  const replication: Alert = {
    id: "a1b2c3d4e5f6",
    condition: "replication.failing",
    resource: "prod2",
    host: null,
    severity: "warning",
    message: "Replication job 102-1 from mox2 to mox3 is failing: command 'zfs recv' failed: dataset is busy",
    next_step: { text: "Inspect prod2", operation: "diagnostics.prod_vm_state", args: { resource: "prod2" } },
    state: "open",
    opened_ms: now - 20 * 60_000,
    resolved_ms: null,
    acknowledged_by: null,
    acknowledged_ms: null,
    generation: 0,
    notified_ms: now - 20 * 60_000,
  };
  const pool: Alert = {
    id: "b2c3d4e5f6a1",
    condition: "zfs.pool.capacity",
    resource: "mox2/rpool",
    host: "mox2",
    severity: "warning",
    message: "mox2/rpool has 15% free (276 GiB of 1.8 TiB).",
    next_step: { text: "Inspect mox2", operation: "diagnostics.host_state", args: { host: "mox2" } },
    state: "acknowledged",
    opened_ms: now - 3 * 3_600_000,
    resolved_ms: null,
    acknowledged_by: "alice@pve",
    acknowledged_ms: now - 2 * 3_600_000,
    generation: 0,
    notified_ms: now - 3 * 3_600_000,
  };
  const corosync: Alert = {
    id: "c3d4e5f6a1b2",
    condition: "corosync.link",
    resource: "mox3",
    host: "mox3",
    severity: "critical",
    message: "mox3's corosync link 0 to mox1 is down.",
    next_step: null,
    state: "resolved",
    opened_ms: now - 26 * 3_600_000,
    resolved_ms: now - 25 * 3_600_000,
    acknowledged_by: null,
    acknowledged_ms: null,
    generation: 1,
    notified_ms: now - 26 * 3_600_000,
  };
  return {
    alerts: [replication, pool],
    history: [corosync],
    silences: [],
    settings: {
      sender: "bmac@example.com",
      recipients: ["ops@example.com"],
      routes: [],
      throttle_minutes: 60,
      digest_hour: 7,
      dashboard_url: "https://mox1.example.ts.net:8443/",
      disabled_conditions: [],
    },
    settings_revision: 1,
    token_set: false,
    outbox: { pending: 0, held: 0, last_sent_ms: now - 20 * 60_000, problems: [] },
    local: false,
    can_acknowledge: true,
    can_edit: true,
  };
}
