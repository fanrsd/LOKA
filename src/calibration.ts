import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

export type Edge = "left" | "right" | "top" | "bottom";
export type Rect = { x: number; y: number; w: number; h: number };
/** As stored by the backend: which stretch of which monitor edge leads to a paired device. */
export type Portal = { edge: Edge; monitor: Rect; start: number; end: number; reach: number };

/** px from the edge where the portal reacts (physical) */
export const REACH = 120;

export const opposite: Record<Edge, Edge> = { left: "right", right: "left", top: "bottom", bottom: "top" };

export const sameRect = (a: Rect, b: Rect) => a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;

/** Stretches of `edge` of screen `m` (fractions along it) a portal may use: not against another screen
 * (the cursor just moves on there) and not under one of `taken`. Mirrors `platform::free_spans`. */
export function freeSpans(screens: Rect[], m: Rect, edge: Edge, taken: Portal[]): [number, number][] {
  const vertical = edge === "left" || edge === "right";
  const [lo, len] = vertical ? [m.y, m.h] : [m.x, m.w];
  const blocked: [number, number][] = taken.filter((p) => p.edge === edge && sameRect(p.monitor, m)).map((p) => [p.start, p.end]);
  for (const n of screens) {
    if (sameRect(n, m)) continue;
    const touches = { left: n.x + n.w === m.x, right: n.x === m.x + m.w, top: n.y + n.h === m.y, bottom: n.y === m.y + m.h }[edge];
    const [a, b] = vertical ? [n.y, n.y + n.h] : [n.x, n.x + n.w];
    if (touches) blocked.push([(a - lo) / len, (b - lo) / len]);
  }
  blocked.sort((a, b) => a[0] - b[0]);
  const free: [number, number][] = [];
  let at = 0;
  for (const [a, b] of [...blocked, [1, 1]]) {
    if (Math.min(a, 1) > at) free.push([at, Math.min(a, 1)]);
    at = Math.max(at, b);
  }
  return free;
}

/** Shortest a portal may be (fraction of its edge). */
export const MIN_LENGTH = 0.05;

/** A `length` long segment as near `centre` as the free stretch of `edge` around it allows. */
export function slot(screens: Rect[], m: Rect, edge: Edge, taken: Portal[], centre: number, length: number): Portal | null {
  const spans = freeSpans(screens, m, edge, taken).filter(([a, b]) => b - a >= MIN_LENGTH);
  const dist = ([a, b]: [number, number]) => (centre < a ? a - centre : centre > b ? centre - b : 0);
  const span = spans.sort((x, y) => dist(x) - dist(y))[0];
  if (!span) return null;
  const len = Math.min(length, span[1] - span[0]);
  const start = Math.min(Math.max(centre - len / 2, span[0]), span[1] - len);
  return { edge, monitor: m, start, end: start + len, reach: REACH };
}

/** Where a device put on `edge` without a spot goes: the middle of the longest free stretch of that edge
 * on any screen, 30 % long at most. Mirrors `platform::fit`. */
export function fit(screens: Rect[], edge: Edge, taken: Portal[]): Portal | null {
  let best: [Rect, [number, number]] | null = null;
  for (const m of screens)
    for (const s of freeSpans(screens, m, edge, taken)) if (!best || s[1] - s[0] > best[1][1] - best[1][0] + 1e-9) best = [m, s];
  if (!best) return null;
  const [m, [a, b]] = best;
  const len = Math.min(b - a, 0.3);
  if (len < MIN_LENGTH) return null;
  const start = Math.min(Math.max(0.5 - len / 2, a), b - len);
  return { edge, monitor: m, start, end: start + len, reach: REACH };
}

/** This window's portal edge, live: set at creation (`?edge=`), updated by `portal` events sent to it. */
export function useEdge(): Edge {
  const [edge, setEdge] = useState<Edge>(() => (new URLSearchParams(location.search).get("edge") as Edge | null) ?? "right");
  useEffect(() => {
    const w = getCurrentWebviewWindow();
    const un = w.listen<Edge>("portal", (e) => setEdge(e.payload));
    // an edge sent while this page was still loading never arrived (a device moved right after its window
    // opened): once listening, take the stored one. Portal windows are `portal-<fingerprint prefix>`.
    const prefix = w.label.startsWith("portal-") ? w.label.slice("portal-".length) : null;
    if (prefix)
      un.then(() => invoke<{ portals: Record<string, Portal> }>("get_device")).then(({ portals }) => {
        const stored = Object.entries(portals).find(([f]) => f.startsWith(prefix))?.[1];
        if (stored) setEdge(stored.edge);
      });
    return () => void un.then((f) => f());
  }, []);
  return edge;
}
