import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { BezelPortal } from "./BezelPortal";
import { loadCalibration, opposite } from "./calibration";
import { Face } from "./Face";
import { PairPrompt } from "./Pairing";
import type { TransferEvent } from "./Transfers";

type Injected =
  | { type: "moveTo"; x: number; y: number }
  | { type: "button"; button: number; down: boolean }
  | { type: "wheel"; dx: number; dy: number }
  | { type: "key"; scan: number; down: boolean };

/** Simulator only: draws input received over the KVM link instead of moving the real cursor. */
function VirtualCursor() {
  const cursor = useRef<HTMLDivElement>(null);
  const [keys, setKeys] = useState<string[]>([]);
  useEffect(() => {
    const un = listen<Injected>("kvm-input", ({ payload: e }) => {
      const el = cursor.current!;
      // moves arrive at mouse rate; touch the DOM directly instead of re-rendering
      if (e.type === "moveTo") {
        el.style.display = "block";
        el.style.transform = `translate(${e.x}px, ${e.y}px)`;
      } else if (e.type === "button") el.dataset.down = String(e.down);
      else if (e.type === "key" && e.down) setKeys((k) => [...k, e.scan.toString(16)].slice(-8));
    });
    return () => void un.then((f) => f());
  }, []);
  return (
    <>
      <div
        ref={cursor}
        data-down="false"
        className="pointer-events-none absolute -left-2 -top-2 z-20 hidden h-4 w-4 rounded-full bg-white shadow-[0_0_0_3px_rgb(76_201_255/0.45)] transition-[scale] duration-150 data-[down=true]:scale-75"
      />
      {keys.length > 0 && <p className="absolute bottom-4 left-7 font-mono text-[11px] text-faint">keys {keys.join(" ")}</p>}
    </>
  );
}

export function PhoneView() {
  const [progress, setProgress] = useState<{ name: string; pct: number } | null>(null);
  const [items, setItems] = useState<string[]>([]);
  // the phone's glowing edge faces the monitor edge it is calibrated against
  const side = opposite[loadCalibration().edge];

  useEffect(() => {
    const un = listen<TransferEvent>("transfer", ({ payload: e }) => {
      // the simulator shares this process with Main, so skip the sending half
      if (e.type === "progress") {
        if (!e.outgoing) setProgress({ name: e.name, pct: e.total ? e.done / e.total : 1 });
      } else if (e.type === "file" || e.type === "text") {
        setProgress(null);
        setItems((xs) => [e.type === "file" ? e.path : e.text, ...xs].slice(0, 20));
      } else if (e.type === "failed" && !e.outgoing) setProgress(null);
    });
    return () => void un.then((f) => f());
  }, []);

  return (
    <main className="relative h-full overflow-hidden">
      <div className="pointer-events-none absolute inset-0">
        <BezelPortal side={side} event="spatial-remote" />
      </div>
      <section className="relative flex h-full flex-col px-7 pt-10">
        <Face seed="sim-mobile" size={56} mood={progress ? "waiting" : "idle"} />
        <h1 className="mt-5 font-display text-[28px] font-extrabold leading-none tracking-[-0.035em]">Simulated phone</h1>
        <p className="mt-2 text-sm leading-relaxed text-muted">
          {items.length === 0 && !progress ? "Push the cursor or drop a file across the glowing edge." : "Received"}
        </p>
        {progress && (
          <div className="mt-5">
            <p className="truncate text-xs text-muted">{progress.name}</p>
            <div className="mt-2 h-1 rounded-full bg-white/10">
              <div className="h-full rounded-full bg-accent transition-[width] duration-150" style={{ width: `${progress.pct * 100}%` }} />
            </div>
          </div>
        )}
        <ul className="mt-4 space-y-2 overflow-y-auto pb-10">
          {items.map((t, i) => (
            <li key={i} className="rise break-all rounded-2xl bg-raised p-3 text-xs ring-1 ring-line">
              {t}
            </li>
          ))}
        </ul>
      </section>
      <VirtualCursor />
      <PairPrompt selfSeed="sim-mobile" />
    </main>
  );
}
