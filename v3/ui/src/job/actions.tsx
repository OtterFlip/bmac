// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// What the job view needs from the app around it. Each app (the Web
// Dashboard, the appliance UI) provides these from its own backend.

import { createContext, useContext, type ReactNode } from "react";
import type { Answer, OperationView } from "../generated";

export interface JobActions {
  /** The operations this user can see, by id. */
  operations: Record<string, OperationView>;
  /** Sends an answer; false when it could not be sent (the app has said why). */
  answer(jobId: string, requestId: string, answer: Answer): Promise<boolean>;
  cancel(jobId: string, force: boolean): Promise<boolean>;
  /** Continues an interrupted job from its last checkpoint. */
  resume(jobId: string): Promise<boolean>;
  /** Marks an interrupted job failed once someone has checked the live state, releasing its resources. */
  reconcile(jobId: string): Promise<boolean>;
  /** Opens the launch dialog for an operation with these parameters filled in. */
  openLaunch(operation: string, args: Record<string, string>): void;
  notify(kind: "info" | "error", title: string, body?: string): void;
}

const Context = createContext<JobActions | null>(null);

export function JobActionsProvider({ value, children }: { value: JobActions; children: ReactNode }) {
  return <Context.Provider value={value}>{children}</Context.Provider>;
}

export function useJobActions(): JobActions {
  const actions = useContext(Context);
  if (!actions) throw new Error("the job view needs a JobActionsProvider");
  return actions;
}
