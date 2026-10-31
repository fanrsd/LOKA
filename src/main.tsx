import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { BezelPortal } from "./BezelPortal";
import { Home } from "./Home";
import { PhoneView } from "./PhoneView";
import { Mobile } from "./Mobile";
import { TitleBar, isMac } from "./TitleBar";
import { tickOnPress } from "./sound";
import { useEdge, useOnline } from "./calibration";
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
  // an offline device keeps its doorway (crossing may still work when only discovery is blocked), faded
  return (
    <div className="h-full w-full transition-opacity duration-700" style={{ opacity: useOnline() ? 1 : 0.25 }}>
      <BezelPortal side={useEdge()} event="spatial" />
    </div>
  );
}

// the band (being driven) and one window per portal of the device map
const overlay = label === "edge-overlay" || label.startsWith("portal-");

function App() {
  if (overlay) return <Overlay />;
  if (label === "mobile-view") return <PhoneView />;
  if (isPhone) return <Mobile />;
  return (
    <div className="flex h-full flex-col">
      <TitleBar />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <Home />
      </div>
    </div>
  );
}

if (label === "main") {
  tickOnPress();
  if (isMac) document.documentElement.classList.add("mac");
}

if (overlay) document.body.classList.add("transparent");
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
