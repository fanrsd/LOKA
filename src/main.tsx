import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { BezelPortal } from "./BezelPortal";
import { Home } from "./Home";
import { PhoneView } from "./PhoneView";
import { loadCalibration } from "./calibration";
import { type Look, applyAccent, loadLook } from "./look";
import { listen } from "@tauri-apps/api/event";
import "./index.css";

const label = getCurrentWindow().label;
const isPhone = /Android|iPhone|iPad/i.test(navigator.userAgent);
applyAccent(loadLook().color);
listen<Look>("look", (e) => applyAccent(e.payload.color));

function App() {
  if (label === "edge-overlay") return <BezelPortal side={loadCalibration().edge} event="spatial" />;
  if (label === "mobile-view" || isPhone) return <PhoneView />;
  return <Home />;
}

if (label === "edge-overlay") document.body.classList.add("transparent");
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
