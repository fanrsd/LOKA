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
