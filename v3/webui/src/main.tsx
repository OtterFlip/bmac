// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import "./styleNonce";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { App } from "./app/App";
import { createHttpBackend } from "./lib/backend";
import { setBackend } from "./state/store";

async function start() {
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("mock")) {
    const { MockBackend } = await import("./lib/mock/backend");
    const { seedDemo } = await import("./lib/mock/demo");
    const mock = new MockBackend();
    seedDemo(mock);
    setBackend(mock);
  } else {
    setBackend(createHttpBackend());
  }
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void start();
