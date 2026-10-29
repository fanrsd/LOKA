import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { quietButton } from "./Pairing";

export type TransferEvent =
  | { type: "progress"; id: number; outgoing: boolean; name: string; done: number; total: number }
  | { type: "file"; id: number; path: string }
  | { type: "sent"; id: number }
  | { type: "failed"; id: number; outgoing: boolean; name: string; error: string }
  | { type: "text"; kind: "text" | "url"; text: string };

type Xfer = {
  key: string;
  id: number;
  outgoing: boolean;
  name: string;
  done: number;
  total: number;
  start: number;
  status: "active" | "done" | "failed";
  path?: string;
  error?: string;
};

const units = ["B", "KB", "MB", "GB", "TB"];
export const bytes = (n: number) => {
  let i = 0;
  while (n >= 1024 && i < units.length - 1) (n /= 1024), i++;
  return `${n < 10 && i ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
};
const duration = (s: number) => (s < 60 ? `${Math.ceil(s)} s` : `${Math.ceil(s / 60)} min`);

function apply(xs: Xfer[], e: TransferEvent): Xfer[] {
  if (e.type === "text") return xs;
  const outgoing = e.type === "sent" || (e.type !== "file" && e.outgoing);
  const key = `${outgoing ? "out" : "in"}-${e.id}`;
  const old = xs.find((x) => x.key === key);
  if (!old && e.type === "sent") return xs;
  const base: Xfer = old ?? { key, id: e.id, outgoing, name: "", done: 0, total: 0, start: performance.now(), status: "active" };
  const next: Xfer =
    e.type === "progress" ? { ...base, name: e.name, done: e.done, total: e.total }
    : e.type === "failed" ? { ...base, name: e.name, status: "failed", error: e.error }
    : e.type === "file" ? { ...base, status: "done", path: e.path, done: base.total }
    : { ...base, status: "done", done: base.total };
  return old ? xs.map((x) => (x === old ? next : x)) : [next, ...xs].slice(0, 4);
}

/** Downloads-style stack: every file crossing the portal, sending or receiving, with speed, time left and cancel. */
export function Transfers() {
  const [list, setList] = useState<Xfer[]>([]);
  const [, tick] = useState(0);

  useEffect(() => {
    const un = listen<TransferEvent>("transfer", ({ payload }) => setList((xs) => apply(xs, payload)));
    const t = setInterval(() => tick((n) => n + 1), 1000); // keeps "time left" honest between events
    return () => {
      clearInterval(t);
      void un.then((f) => f());
    };
  }, []);

  if (!list.length) return null;
  const dismiss = (key: string) => setList((xs) => xs.filter((x) => x.key !== key));

  return (
    <ul aria-label="Transfers" className="fixed bottom-5 right-5 z-20 flex w-80 flex-col gap-2">
      {list.map((x) => {
        const pct = x.total ? x.done / x.total : x.status === "done" ? 1 : 0;
        const secs = (performance.now() - x.start) / 1000;
        const rate = secs > 0.3 ? x.done / secs : 0;
        const status =
          x.status === "failed" ? (x.error === "cancelled" ? "Cancelled" : `Failed: ${x.error}`)
          : x.status === "done" ? (x.outgoing ? `Sent · ${bytes(x.total)}` : "Saved to Downloads/LOKA")
          : [`${x.outgoing ? "Sending" : "Receiving"} ${Math.floor(pct * 100)}%`, rate && `${bytes(rate)}/s`, rate && `${duration((x.total - x.done) / rate)} left`]
              .filter(Boolean)
              .join(" · ");
        return (
          <li key={x.key} className="rounded-2xl border border-white/10 bg-ink/95 p-3.5 shadow-2xl backdrop-blur">
            <div className="flex items-center gap-2">
              <p className="min-w-0 flex-1 truncate text-sm font-semibold" title={x.name}>{x.name}</p>
              {x.status === "active" ? (
                <button className={`${quietButton} -my-1.5 !px-3 !py-1.5 text-xs`} onClick={() => invoke("cancel_transfer", { id: x.id })}>
                  Cancel
                </button>
              ) : (
                <button aria-label={`Dismiss ${x.name}`} className={`${quietButton} -my-1.5 !px-2.5 !py-1.5 text-xs`} onClick={() => dismiss(x.key)}>
                  ✕
                </button>
              )}
            </div>
            <div
              role="progressbar"
              aria-label={x.name}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.floor(pct * 100)}
              className="mt-2 h-1.5 overflow-hidden rounded-full bg-white/10"
            >
              <div
                className={`h-full rounded-full transition-[width] duration-150 ${x.status === "failed" ? "bg-danger" : "bg-accent"}`}
                style={{ width: `${pct * 100}%` }}
              />
            </div>
            <div className="mt-1.5 flex items-center gap-2">
              <p aria-live={x.status === "active" ? "off" : "polite"} className={`min-w-0 flex-1 truncate text-xs ${x.status === "failed" && x.error !== "cancelled" ? "text-danger" : "text-muted"}`}>{status}</p>
              {x.path && (
                <button className="text-xs font-medium text-accent hover:underline focus-visible:outline-2 focus-visible:outline-accent" onClick={() => invoke("reveal", { path: x.path })}>
                  Show in folder
                </button>
              )}
            </div>
          </li>
        );
      })}
    </ul>
  );
}
