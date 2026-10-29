import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

export type Edge = "left" | "right" | "top" | "bottom";
export type Calibration = { monitor: number; edge: Edge; start: number; length: number };

const KEY = "loka.calibration";
export const DEFAULT: Calibration = { monitor: 0, edge: "right", start: 0.35, length: 0.3 };
/** px from the edge where the portal reacts (physical) */
export const REACH = 120;

// all windows share one origin, so the overlay and simulated phone read what the main window saved
export const loadCalibration = (): Calibration => ({ ...DEFAULT, ...JSON.parse(localStorage.getItem(KEY) ?? "{}") });
export const saveCalibration = (c: Calibration) => localStorage.setItem(KEY, JSON.stringify(c));

export const opposite: Record<Edge, Edge> = { left: "right", right: "left", top: "bottom", bottom: "top" };

/** Calibrated edge, live: the overlay and simulated phone outlive edge changes made in the main window. */
export function useEdge(): Edge {
  const [edge, setEdge] = useState(() => loadCalibration().edge);
  useEffect(() => {
    const un = listen<Edge>("portal", (e) => setEdge(e.payload));
    return () => void un.then((f) => f());
  }, []);
  return edge;
}
