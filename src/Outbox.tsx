import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Face } from "./Face";
import { quietButton, type Device, type Peer } from "./Pairing";

/** Files handed to LOKA by Explorer's Send to or Finder's Open With: pick the paired device they go to. */
export function Outbox({ device, peers }: { device: Device; peers: Peer[] }) {
  const [files, setFiles] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const take = () => invoke<string[]>("take_outbox").then((f) => f.length && setFiles((old) => [...old, ...f]));
    const off = listen("outbox", take);
    off.then(take); // files LOKA was started with
    return () => {
      off.then((f) => f());
    };
  }, []);

  if (!files.length) return null;
  const targets = peers.filter((p) => device.paired.some((d) => d.fingerprint === p.info.fingerprint));
  const name = (p: string) => p.split(/[\\/]/).pop();
  const send = (fingerprint: string) =>
    invoke("send_paths", { fingerprint, paths: files }).then(
      () => setFiles([]),
      (e) => setError(String(e)),
    );

  return (
    <div className="fixed inset-0 z-10 grid place-items-center bg-ink/70 p-5 backdrop-blur-md" style={{ animation: "fade-in 0.2s ease-out" }}>
      <div role="dialog" aria-modal="true" aria-labelledby="outbox-title" className="w-full max-w-sm rounded-[28px] bg-raised p-7 ring-1 ring-line" style={{ animation: "sheet-in 0.45s var(--ease-spring)" }}>
        <h2 id="outbox-title" className="font-display text-[26px] font-bold leading-tight tracking-[-0.025em]">
          Send {files.length === 1 ? name(files[0]) : `${files.length} items`} to…
        </h2>
        {targets.length ? (
          <ul className="mt-5 flex flex-col gap-1">
            {targets.map((p) => (
              <li key={p.info.fingerprint}>
                <button onClick={() => send(p.info.fingerprint)} className="flex w-full items-center gap-3 rounded-2xl px-3 py-2 text-left transition-colors hover:bg-white/5 focus-visible:outline-2 focus-visible:outline-accent">
                  <Face seed={p.info.fingerprint} size={36} />
                  <span className="font-semibold">{p.info.alias}</span>
                </button>
              </li>
            ))}
          </ul>
        ) : (
          <p className="mt-3 text-sm text-muted">No paired device is on the network right now.</p>
        )}
        {error && (
          <p role="alert" className="mt-3 text-sm text-danger">
            {error}
          </p>
        )}
        <button autoFocus onClick={() => setFiles([])} className={`${quietButton} mt-5 w-full`}>
          Cancel
        </button>
      </div>
    </div>
  );
}
