import { useEffect, useState } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";

export type Edge = "left" | "right" | "top" | "bottom";
export type Rect = { x: number; y: number; w: number; h: number };
/** As stored by the backend: which stretch of which monitor edge leads to a paired device. */
export type Portal = { edge: Edge; monitor: Rect; start: number; end: number; reach: number };

/** px from the edge where the portal reacts (physical) */
export const REACH = 120;

export const opposite: Record<Edge, Edge> = { left: "right", right: "left", top: "bottom", bottom: "top" };

/** This window's portal edge, live: set at creation (`?edge=`), updated by `portal` events sent to it. */
export function useEdge(): Edge {
  const [edge, setEdge] = useState<Edge>(() => (new URLSearchParams(location.search).get("edge") as Edge | null) ?? "right");
  useEffect(() => {
    const un = getCurrentWebviewWindow().listen<Edge>("portal", (e) => setEdge(e.payload));
    return () => void un.then((f) => f());
  }, []);
  return edge;
}
