import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { CursorPrefs } from "./Pairing";

/** How this computer's mouse and keyboard behave on the device they drive. Saved in the Rust settings. */
export function Pointer({ initial, mac }: { initial: CursorPrefs; mac: boolean }) {
  const [p, setP] = useState(initial);
  const change = (patch: Partial<CursorPrefs>) => {
    const next = { ...p, ...patch };
    setP(next);
    invoke("set_cursor_prefs", { prefs: next });
  };
  const kbd = "rounded-md bg-white/10 px-1.5 py-0.5 font-sans text-xs text-fg";

  return (
    <section aria-labelledby="pointer-title" className="rise mt-6 rounded-[28px] bg-raised p-7 ring-1 ring-line" style={{ "--i": 4 } as React.CSSProperties}>
      <h2 id="pointer-title" className="font-display text-2xl font-bold tracking-[-0.025em]">
        How it feels over there
      </h2>

      <div className="mt-6 grid grid-cols-[6rem_1fr] items-center gap-x-4 gap-y-4 text-sm">
        <label htmlFor="speed" className="text-muted">Speed</label>
        <div className="flex items-center gap-3">
          <input
            id="speed"
            type="range"
            min={0.5}
            max={2}
            step={0.05}
            value={p.speed}
            onChange={(e) => change({ speed: +e.target.value })}
            aria-valuetext={`${p.speed.toFixed(2)} times`}
            className="loka-range flex-1"
            style={{ "--fill": `${((p.speed - 0.5) / 1.5) * 100}%` } as React.CSSProperties}
          />
          <span className="w-12 text-right tabular-nums text-muted">{p.speed.toFixed(2)}×</span>
        </div>

        <label htmlFor="delay" className="text-muted">Edge delay</label>
        <div className="flex items-center gap-3">
          <input
            id="delay"
            type="range"
            min={0}
            max={600}
            step={25}
            value={p.edgeDelayMs}
            onChange={(e) => change({ edgeDelayMs: +e.target.value })}
            aria-valuetext={p.edgeDelayMs ? `${p.edgeDelayMs} milliseconds` : "off"}
            aria-describedby="delay-hint"
            className="loka-range flex-1"
            style={{ "--fill": `${(p.edgeDelayMs / 600) * 100}%` } as React.CSSProperties}
          />
          <span className="w-12 text-right tabular-nums text-muted">{p.edgeDelayMs ? `${p.edgeDelayMs} ms` : "Off"}</span>
        </div>
        <span />
        <p id="delay-hint" className="-mt-2 text-xs text-faint">How long the cursor rests on the glow before it moves over, so a quick flick past the edge stays here.</p>

        <span id="swap-label" className="text-muted">Shortcuts</span>
        <button
          role="switch"
          aria-checked={p.swapCtrlCmd}
          aria-labelledby="swap-label swap-text"
          onClick={() => change({ swapCtrlCmd: !p.swapCtrlCmd })}
          className="group flex items-center gap-3 text-left focus-visible:outline-none"
        >
          <span
            className={`relative h-6 w-10 shrink-0 rounded-full transition-colors duration-200 group-focus-visible:outline-2 group-focus-visible:outline-offset-2 group-focus-visible:outline-accent ${p.swapCtrlCmd ? "bg-accent" : "bg-white/15"}`}
          >
            <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-fg transition-[left] duration-200 ease-[var(--ease-spring)] ${p.swapCtrlCmd ? "left-[18px]" : "left-0.5"}`} />
          </span>
          <span id="swap-text" className="text-fg">
            {mac ? "⌘ acts as Ctrl on a PC" : "Ctrl acts as ⌘ on a Mac"}
          </span>
        </button>
      </div>

      <p className="mt-6 text-xs leading-relaxed text-faint">
        <kbd className={kbd}>{mac ? "F14" : "Scroll Lock"}</kbd> keeps the pointer on the screen it's on.{" "}
        Tap <kbd className={kbd}>{mac ? "⌃" : "Ctrl"}</kbd> twice, or <kbd className={kbd}>{mac ? "⌃ ⌥ Esc" : "Ctrl Alt Esc"}</kbd>, to bring it home. Trackpad swipes between desktops, Mission Control and pinch-to-zoom work over there too.
      </p>
    </section>
  );
}
