import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { dragTitleBar, macTitleBar, ownTitleBar } from "./WindowControls";
import "./styles/tokens.css";
import "./styles.css";

// A scroller shows its thumb while it scrolls, by wheel or keys, not only under the pointer
// (styles.css, "scrollbars").
const scrolling = new Map<Element, number>();
document.addEventListener(
  "scroll",
  (e) => {
    const el = e.target instanceof Element ? e.target : document.scrollingElement;
    if (!el) return;
    el.classList.add("scrolling");
    clearTimeout(scrolling.get(el));
    scrolling.set(
      el,
      window.setTimeout(() => {
        el.classList.remove("scrolling");
        scrolling.delete(el);
      }, 900),
    );
  },
  { capture: true, passive: true },
);

// The app draws its own title bar on Windows, and the Mac's lights sit over its top left; the layout
// leaves room for them (styles.css).
if (dragTitleBar) document.documentElement.classList.add("drag-titlebar");
if (ownTitleBar) document.documentElement.classList.add("own-titlebar");
if (macTitleBar) document.documentElement.classList.add("mac-titlebar");

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
