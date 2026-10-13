// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useEffect } from "react";
import { useStore, type StatusData, type StatusEntry, type StatusKind } from "./store";

/** A status snapshot, fetched while the calling page is shown and kept current. */
export function useStatus<K extends StatusKind>(kind: K): StatusEntry<StatusData[K]> {
  useEffect(() => useStore.getState().wantStatus(kind), [kind]);
  return useStore((s) => s.status[kind]) as StatusEntry<StatusData[K]>;
}
