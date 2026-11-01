import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { Face } from "./Face";
import type { Portal } from "./calibration";

/** `proto`: wire version (`kvm::PROTOCOL`); devices with another one can't pair or connect. `aspectRatio`: its screen's width / height.
 *  `controllable`: whether it can be driven right now (false: an Android phone not set up, a Linux without input access; absent on older LOKA = yes).
 *  `text`: it takes typed text whatever its keyboard layout (absent on older LOKA). */
export type DeviceInfo = { alias: string; deviceType: "desktop" | "mobile"; os: string; fingerprint: string; proto: number; aspectRatio: number; controllable?: boolean; text?: boolean };
export type Peer = { info: DeviceInfo; addr: string };
export type PairedDevice = { fingerprint: string; alias: string };
export type CursorPrefs = { speed: number; edgeDelayMs: number; swapCtrlCmd: boolean };
export type Sharing = { terminal: boolean; folder: string | null; sound: boolean; notifications: boolean; mic: boolean };
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

/** How to make a device drivable, per OS (`controllable: false`). */
export const setupHow = (os: string) =>
  os === "android"
    ? "On the phone: open LOKA → Set up"
    : os === "linux"
      ? "On it: About → Allow input access, or run sudo loka --allow-input"
      : os === "macos"
        ? "On the Mac: LOKA → About → Allow Accessibility (again after each LOKA update)"
        : "Open LOKA there and follow its setup";

export type DeviceState = "ready" | "setup" | "offline" | "update";

/** Whether a device can be driven right now: the one check behind every badge, note and overlay. `peer` absent = not on the network;
 *  setup only matters once paired. */
export const deviceState = (peer: Peer | undefined, paired: boolean, me: DeviceInfo): DeviceState =>
  !peer ? "offline" : peer.info.proto !== me.proto ? "update" : paired && peer.info.controllable === false ? "setup" : "ready";

export const STATE_LABEL: Record<DeviceState, string> = { ready: "Ready", setup: "Needs setup", offline: "Offline", update: "Needs update" };

/** One line: why `alias` can't be driven and the fix (`os` for setup); `null` when ready. */
export const stateReason = (state: DeviceState, alias: string, os = "") =>
  state === "setup"
    ? `${alias} isn't ready. ${setupHow(os)}`
    : state === "update"
      ? `${alias} needs a LOKA update: install the newest LOKA on both devices`
      : state === "offline"
        ? `${alias} isn't on the network: turn it on and open LOKA there`
        : null;

/** Pill naming a blocked state (danger) or offline (muted); `dot` for cramped spots, labelled for screen readers and on hover. */
export function StateBadge({ state, title, dot }: { state: DeviceState; title?: string | null; dot?: boolean }) {
  if (state === "ready") return null;
  const off = state === "offline";
  if (dot)
    return <span role="img" aria-label={STATE_LABEL[state]} title={title ?? STATE_LABEL[state]} className={`block h-2.5 w-2.5 rounded-full ring-2 ring-raised ${off ? "bg-muted" : "bg-danger"}`} />;
  return <span className={`whitespace-nowrap rounded-full px-2 py-0.5 text-[11px] font-semibold ${off ? "bg-white/10 text-muted" : "bg-danger/15 text-danger"}`}>{STATE_LABEL[state]}</span>;
}

/** The selected device's state spelled out: badge, reason + fix, and Wake up when it's offline and wakeable. */
export function StateNote({ state, alias, os, onWake }: { state: DeviceState; alias: string; os?: string; onWake?: () => void }) {
  const reason = stateReason(state, alias, os);
  if (!reason) return null;
  return (
    <div role="status" className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-[20px] bg-white/[0.04] p-4 text-sm leading-relaxed">
      <StateBadge state={state} />
      <p className="min-w-0 flex-1 text-muted">{reason}.</p>
      {state === "offline" && onWake && (
        <button onClick={onWake} className={quietButton + " min-h-11 ring-1 ring-line"}>
          Wake up
        </button>
      )}
    </div>
  );
}

/** `check_device`: one QUIC probe per channel; `ms` = round trip. */
export type Probe = { ok: boolean; ms: number | null; error: string | null };
export type Check = { kvm: Probe; transfer: Probe; hint: string | null };

const probe = (name: string, p: Probe) => `${name}: ${p.ok ? `reached${p.ms != null ? ` in ${Math.round(p.ms)} ms` : ""}` : `not reached${p.error ? ` (${p.error})` : ""}`}`;

/** "Test connection" for a paired device: whether mouse/keyboard and files get through, and the likely fix when not. */
export function ConnectionCheck({ fingerprint }: { fingerprint: string }) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<Check | string | null>(null);
  const run = () => {
    setBusy(true);
    invoke<Check>("check_device", { fingerprint })
      .then(setResult, (e) => setResult(String(e)))
      .finally(() => setBusy(false));
  };
  return (
    <div className="min-w-0">
      <button onClick={run} disabled={busy} className={quietButton + " min-h-11 ring-1 ring-line disabled:opacity-50"}>
        {busy ? "Testing…" : "Test connection"}
      </button>
      <div role="status" className="px-1 text-xs leading-relaxed text-muted">
        {typeof result === "string" ? (
          <p className="mt-2 text-danger">{result}</p>
        ) : result ? (
          <div className="mt-2 space-y-1">
            <p className={result.kvm.ok ? "" : "text-danger"}>{probe("Mouse and keyboard", result.kvm)}</p>
            <p className={result.transfer.ok ? "" : "text-danger"}>{probe("Files", result.transfer)}</p>
            {result.hint && <p className="text-fg">{result.hint}</p>}
          </div>
        ) : null}
      </div>
    </div>
  );
}

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
