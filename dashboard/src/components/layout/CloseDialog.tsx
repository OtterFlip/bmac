import { useState } from "react";
import { Power } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { Dialog } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { selectActiveRuns, useStore } from "@/state/store";
import { api } from "@/lib/api";

/** Shown when the window is closed while scripts are still running. */
export function CloseDialog() {
  const n = useStore((s) => s.closeRequested);
  const setCloseRequested = useStore((s) => s.setCloseRequested);
  const active = useStore(useShallow(selectActiveRuns));
  const [closing, setClosing] = useState(false);
  return (
    <Dialog
      open={n > 0}
      onOpenChange={(o) => !o && !closing && setCloseRequested(0)}
      tone="warn"
      icon={<Power />}
      title={`${active.length || n} operation${(active.length || n) === 1 ? " is" : "s are"} still running`}
      description="Quitting interrupts them the same way Ctrl-C does in a terminal, and waits up to 20 seconds for their cleanup. Work a script already did stays in place."
      footer={
        <>
          <Button variant="ghost" disabled={closing} onClick={() => setCloseRequested(0)}>
            Keep the dashboard open
          </Button>
          <Button
            variant="danger"
            disabled={closing}
            onClick={async () => {
              setClosing(true);
              await api().quit(true);
            }}
          >
            {closing ? "Stopping…" : "Interrupt and quit"}
          </Button>
        </>
      }
    >
      <ul className="space-y-1">
        {active.map((r) => (
          <li key={r.run_id} className="truncate rounded-md bg-surface-2 px-2.5 py-1.5 font-mono text-[12px] text-fg-muted">
            {r.command_line}
          </li>
        ))}
      </ul>
    </Dialog>
  );
}
