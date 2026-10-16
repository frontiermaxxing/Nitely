import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { ownTitleBar } from "./WindowControls";
import "./styles/tokens.css";
import "./styles.css";

// The app draws its own title bar on Windows; the layout leaves room for it (styles.css).
if (ownTitleBar) document.documentElement.classList.add("own-titlebar");

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
