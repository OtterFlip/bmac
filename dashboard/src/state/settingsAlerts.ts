import { useEffect } from "react";
import { useShallow } from "zustand/react/shallow";
import { configAttention, useStore } from "./store";

export interface SettingsAlerts {
  /** Essential config files still missing or identical to their example. */
  configNeeds: number;
  dependenciesMissing: boolean;
  awaitingQDevice: boolean;
  awaitingFirstHost: boolean;
  /** The Settings page shows an amber alert. */
  any: boolean;
}

/**
 * The Settings page's amber alerts, shared with the sidebar. While no host is
 * detected it keeps the QDevice access check fresh, rechecking when the
 * operator returns from preparing the QDevice in another window.
 */
export function useSettingsAlerts(): SettingsAlerts {
  const state = useStore(
    useShallow((s) => ({
      configNeeds: s.config ? configAttention(s.config).length : null,
      dependenciesChecked: s.preflight.checks !== null,
      dependenciesMissing: s.preflight.checks?.some((c) => !c.config_file && c.status !== "ok") ?? false,
      hostsDetected: (s.sources.list_hosts.data?.hosts.length ?? 0) > 0,
      qdeviceAccessible: s.sources.check_qdevice_access.data?.accessible === true,
      qdeviceKnown: s.sources.check_qdevice_access.data !== null || s.sources.check_qdevice_access.status === "error",
      qdeviceCheckReady: s.ready && !!s.workflows.check_qdevice_access,
    })),
  );
  const refreshSource = useStore((s) => s.refreshSource);
  const awaitingCluster = state.configNeeds === 0 && state.dependenciesChecked && !state.dependenciesMissing && !state.hostsDetected;
  useEffect(() => {
    if (!awaitingCluster || !state.qdeviceCheckReady) return;
    const check = () => void refreshSource("check_qdevice_access", { ifOlderThanMs: 15_000 });
    check();
    window.addEventListener("focus", check);
    return () => window.removeEventListener("focus", check);
  }, [awaitingCluster, state.qdeviceCheckReady, refreshSource]);
  const configNeeds = state.configNeeds ?? 0;
  const awaitingQDevice = awaitingCluster && !state.qdeviceAccessible && state.qdeviceKnown;
  const awaitingFirstHost = awaitingCluster && state.qdeviceAccessible;
  const any = configNeeds > 0 || state.dependenciesMissing || awaitingQDevice || awaitingFirstHost;
  return { configNeeds, dependenciesMissing: state.dependenciesMissing, awaitingQDevice, awaitingFirstHost, any };
}
