import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { BezelPortal } from "./BezelPortal";
import { Home } from "./Home";
import { PhoneView } from "./PhoneView";
import { useEdge } from "./calibration";
import { type Look, applyAccent, loadLook } from "./look";
import { listen } from "@tauri-apps/api/event";
import "./index.css";

const label = getCurrentWindow().label;
const isPhone = /Android|iPhone|iPad/i.test(navigator.userAgent);
applyAccent(loadLook().color);
listen<Look>("look", (e) => applyAccent(e.payload.color));

// a native app has no "Back / Reload / Print / Inspect" menu or browser shortcuts (dev builds keep them for debugging)
const BROWSER_KEYS: Record<string, true> = { r: true, f: true, g: true, p: true, s: true, u: true, j: true, h: true, o: true, n: true, "+": true, "-": true, "=": true, "0": true };
if (!import.meta.env.DEV) {
  addEventListener("contextmenu", (e) => e.preventDefault());
  addEventListener("keydown", (e) => {
    const ctrl = (e.ctrlKey || e.metaKey) && BROWSER_KEYS[e.key.toLowerCase()];
    if (ctrl || ["F3", "F5", "F7", "F12"].includes(e.key)) e.preventDefault();
  });
}

function Overlay() {
  return <BezelPortal side={useEdge()} event="spatial" />;
}

function App() {
  if (label === "edge-overlay") return <Overlay />;
  if (label === "mobile-view" || isPhone) return <PhoneView />;
  return <Home />;
}

if (label === "edge-overlay") document.body.classList.add("transparent");
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
