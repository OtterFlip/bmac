import { create } from "zustand";

export interface Toast {
  id: number;
  kind: "success" | "error" | "info";
  title: string;
  body?: string;
  runId?: string;
}

interface ToastState {
  toasts: Toast[];
  push(t: Omit<Toast, "id">): void;
  dismiss(id: number): void;
}

let next = 1;

export const useToasts = create<ToastState>((set) => ({
  toasts: [],
  push: (t) => {
    const id = next++;
    set((s) => ({ toasts: [...s.toasts.slice(-4), { ...t, id }] }));
    setTimeout(() => set((s) => ({ toasts: s.toasts.filter((x) => x.id !== id) })), t.kind === "error" ? 9000 : 5000);
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((x) => x.id !== id) })),
}));

export const toast = {
  success: (title: string, body?: string, runId?: string) => useToasts.getState().push({ kind: "success", title, body, runId }),
  error: (title: string, body?: string, runId?: string) => useToasts.getState().push({ kind: "error", title, body, runId }),
  info: (title: string, body?: string, runId?: string) => useToasts.getState().push({ kind: "info", title, body, runId }),
};
