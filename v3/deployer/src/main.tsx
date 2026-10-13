// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import "./index.css";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/App";
import { createTauriBackend, type Backend } from "./lib/backend";
import { MockBackend } from "./lib/mock";

// Outside Tauri (`pnpm dev` in a browser) the window runs on canned data.
const inTauri = "__TAURI_INTERNALS__" in window;
const backend: Backend = inTauri ? createTauriBackend() : new MockBackend();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App backend={backend} />
  </StrictMode>,
);
