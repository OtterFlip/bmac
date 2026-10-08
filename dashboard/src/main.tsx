import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app/App";
import "./index.css";

document.addEventListener("contextmenu", (e) => {
  if (!(e.target instanceof HTMLElement && e.target.closest(".selectable, input, textarea"))) e.preventDefault();
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
