import { emit } from "@tauri-apps/api/event";

/** ripple: liquid bulge + waves, splash on crossing · pulse: glow swells, flash on crossing · calm: steady line */
export type Motion = "ripple" | "pulse" | "calm";
export type Look = { color: string; motion: Motion };

const KEY = "loka.look";
// all >= 7:1 on the raised surface, so the accent stays readable as text
export const SWATCHES: Record<string, string> = { Cyan: "#4cc9ff", Mint: "#5ef0b0", Lime: "#c3f24a", Amber: "#ffb547", Pink: "#ff6fae", Violet: "#a98bff" };
export const MOTIONS: Motion[] = ["ripple", "pulse", "calm"];

export const loadLook = (): Look => ({ color: SWATCHES.Cyan, motion: "ripple", ...JSON.parse(localStorage.getItem(KEY) ?? "{}") });

/** the UI accent follows the glow, like a system accent colour */
export const applyAccent = (color: string) => document.documentElement.style.setProperty("--color-accent", color);

/** saves, recolours this window, and tells the overlay + phone windows (they replay the crossing as a preview) */
export function saveLook(look: Look) {
  localStorage.setItem(KEY, JSON.stringify(look));
  applyAccent(look.color);
  emit("look", look);
}
