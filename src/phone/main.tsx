import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { Phone } from "./Phone";
import "../styles/tokens.css";
import "./phone.css";

function render() {
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <Phone />
    </StrictMode>,
  );
}

// The browser preview (`/phone.html?fixtures`) answers from sample data instead of a PC.
if (import.meta.env.DEV && new URLSearchParams(location.search).has("fixtures")) {
  import("./fake").then((f) => {
    f.installFake();
    render();
  });
} else {
  render();
}
