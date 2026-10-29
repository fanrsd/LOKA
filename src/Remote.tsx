import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { quietButton } from "./Pairing";
import { Toggle } from "./Sharing";
import { Terminal } from "./Terminal";

type Stopped = { device: string; on: boolean; error?: string };

/** What this computer can use on the selected paired device (if that device shares it). */
export function Remote({ fingerprint, alias }: { fingerprint: string; alias: string }) {
  const [sound, setSound] = useState(false);
  const [mirror, setMirror] = useState(false);
  const [terminal, setTerminal] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    // the backend stops them itself when the link drops or the other side stops sharing
    const offSound = listen<Stopped>("sound", (e) => {
      if (e.payload.device !== fingerprint) return;
      setSound(e.payload.on);
      if (e.payload.error) setError(e.payload.error);
    });
    const offMirror = listen<Stopped>("mirror", (e) => {
      if (e.payload.device !== fingerprint) return;
      setMirror(e.payload.on);
      if (e.payload.error) setError(e.payload.error);
    });
    return () => {
      offSound.then((f) => f());
      offMirror.then((f) => f());
    };
  }, [fingerprint]);

  const run = async (f: () => Promise<unknown>) => {
    setError(null);
    setBusy(true);
    try {
      await f();
    } catch (e) {
      setError(String(e));
      return false;
    } finally {
      setBusy(false);
    }
    return true;
  };
  const toggle = (cmd: string, set: (on: boolean) => void) => async (on: boolean) => {
    if (await run(() => invoke(cmd, { fingerprint, on }))) set(on);
  };

  return (
    <section aria-labelledby="remote-title" className="rise mt-6 rounded-[28px] bg-raised p-7 ring-1 ring-line" style={{ "--i": 5 } as React.CSSProperties}>
      <h2 id="remote-title" className="font-display text-2xl font-bold tracking-[-0.025em]">
        From {alias}
      </h2>
      <p className="mt-1 text-sm text-muted">Works when {alias} shares it (LOKA there → This computer shares).</p>

      <div className="mt-6 flex flex-col gap-5">
        <Toggle on={sound} disabled={busy} onChange={toggle("set_listening", setSound)} label="Play its sound here" />
        <Toggle on={mirror} disabled={busy} onChange={toggle("set_mirroring", setMirror)} label="Show its notifications here" />
      </div>
      <div className="mt-6 flex flex-wrap gap-1">
        <button className={quietButton} disabled={busy} onClick={() => run(() => invoke("open_share", { fingerprint }))}>
          Open its shared folder
        </button>
        <button className={quietButton} onClick={() => setTerminal(true)}>
          Open a terminal
        </button>
      </div>
      {error && (
        <p role="alert" className="mt-4 text-sm text-danger">
          {error}
        </p>
      )}
      {/* outside the panel: its entrance transform would trap a fixed sheet */}
      {terminal && createPortal(<Terminal fingerprint={fingerprint} alias={alias} onClose={() => setTerminal(false)} />, document.body)}
    </section>
  );
}
