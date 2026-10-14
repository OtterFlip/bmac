// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import "./styleNonce";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { App } from "./App";
import { createApi } from "./lib/api";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App api={createApi()} />
  </StrictMode>,
);
