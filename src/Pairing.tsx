import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { Face } from "./Face";
import type { Portal } from "./calibration";

export type DeviceInfo = { alias: string; deviceType: "desktop" | "mobile"; os: string; fingerprint: string };
export type Peer = { info: DeviceInfo; addr: string };
export type PairedDevice = { fingerprint: string; alias: string };
export type CursorPrefs = { speed: number; edgeDelayMs: number; swapCtrlCmd: boolean };
export type Sharing = { terminal: boolean; folder: string | null; sound: boolean; notifications: boolean };
export type Device = {
  info: DeviceInfo;
  paired: PairedDevice[];
  portals: Record<string, Portal>;
  ips: string[];
  hosts: string[];
  cursor: CursorPrefs;
  sharing: Sharing;
  received: string;
  wakeable: string[];
};

const OS_LABEL: Record<string, string> = { windows: "Windows", macos: "macOS", linux: "Linux", android: "Android", ios: "iOS", sim: "Simulator" };
export const osLabel = (os: string) => OS_LABEL[os] ?? os;

/** Primary action: a white pill, like the rest of the system. */
export const primaryButton =
  "rounded-full bg-fg px-5 py-2.5 text-sm font-semibold text-ink transition-[transform,background-color] duration-200 ease-out hover:bg-white active:scale-[0.97] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent disabled:opacity-50";
export const quietButton =
  "rounded-full px-4 py-2.5 text-sm font-medium text-muted transition-[color,background-color,transform] duration-200 hover:bg-white/5 hover:text-fg active:scale-[0.97] focus-visible:outline-2 focus-visible:outline-accent";

/** Accept/decline sheet for an incoming pairing request addressed to this window. */
export function PairPrompt({ selfSeed }: { selfSeed: string }) {
  const [req, setReq] = useState<{ id: number; peer: Peer; code: string } | null>(null);

  useEffect(() => {
    const win = getCurrentWebviewWindow(); // the simulated phone's prompts are its own
    const open = win.listen<{ id: number; peer: Peer; code: string }>("pair-request", (e) => setReq(e.payload));
    // the backend times requests out; close the sheet with it
    const closed = win.listen<number>("pair-request-closed", (e) => setReq((r) => (r?.id === e.payload ? null : r)));
    return () => {
      open.then((f) => f());
      closed.then((f) => f());
    };
  }, []);

  if (!req) return null;
  const answer = (accept: boolean) => {
    invoke("answer_pair", { id: req.id, accept });
    setReq(null);
  };
  const { alias, fingerprint, os } = req.peer.info;

  return (
    <div className="fixed inset-0 z-10 grid place-items-center bg-ink/70 p-5 backdrop-blur-md" style={{ animation: "fade-in 0.2s ease-out" }}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="pair-title"
        aria-describedby="pair-body"
        className="w-full max-w-sm rounded-[28px] bg-raised p-7 text-center ring-1 ring-line"
        style={{ animation: "sheet-in 0.45s var(--ease-spring)" }}
      >
        {/* the two devices look at each other */}
        <div className="flex items-center justify-center gap-3">
          <Face seed={fingerprint} size={64} look="right" />
          <span aria-hidden className="flex gap-1">
            {[0, 1, 2].map((i) => (
              <span key={i} className="h-1 w-1 rounded-full bg-accent" style={{ opacity: 0.35 + i * 0.25 }} />
            ))}
          </span>
          <Face seed={selfSeed} size={64} look="left" />
        </div>
        <h2 id="pair-title" className="mt-6 font-display text-[26px] font-bold leading-tight tracking-[-0.025em]">
          Pair with {alias}?
        </h2>
        <p id="pair-body" className="mx-auto mt-2 max-w-[30ch] text-sm leading-relaxed text-muted">
          {alias} ({osLabel(os)}, {req.peer.addr}) will be able to use this device's mouse and keyboard and send it files. Pair only if it shows the same code:
        </p>
        <p className="mt-4 font-display text-[34px] font-bold tabular-nums tracking-[0.06em] text-fg">{req.code}</p>
        <div className="mt-7 flex flex-col gap-1">
          <button onClick={() => answer(true)} className={primaryButton}>
            Pair
          </button>
          {/* default focus on the safe choice: a stray Enter must not hand over control */}
          <button autoFocus onClick={() => answer(false)} className={quietButton}>
            Not now
          </button>
        </div>
      </div>
    </div>
  );
}
