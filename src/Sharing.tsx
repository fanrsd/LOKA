import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { quietButton, type Sharing as SharingPrefs } from "./Pairing";

/** On/off switch in the panel style (label + optional hint to its right). */
export function Toggle({ on, onChange, label, hint, disabled }: { on: boolean; onChange: (on: boolean) => void; label: string; hint?: string; disabled?: boolean }) {
  return (
    <button role="switch" aria-checked={on} disabled={disabled} onClick={() => onChange(!on)} className="group flex items-start gap-3 text-left focus-visible:outline-none disabled:opacity-50">
      <span className={`relative mt-0.5 h-6 w-10 shrink-0 rounded-full transition-colors duration-200 group-focus-visible:outline-2 group-focus-visible:outline-offset-2 group-focus-visible:outline-accent ${on ? "bg-accent" : "bg-white/15"}`}>
        <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-fg transition-[left] duration-200 ease-[var(--ease-spring)] ${on ? "left-[18px]" : "left-0.5"}`} />
      </span>
      <span>
        <span className="block text-sm text-fg">{label}</span>
        {hint && <span className="mt-0.5 block text-xs text-faint">{hint}</span>}
      </span>
    </button>
  );
}

const folderName = (p: string) => p.split(/[\\/]/).filter(Boolean).pop() ?? p;

/** What paired devices may open on this computer, and where received files go. Everything starts off. */
export function Sharing({ initial, received, mac, onChange }: { initial: SharingPrefs; received: string; mac: boolean; onChange: () => void }) {
  const [s, setS] = useState(initial);
  const change = (patch: Partial<SharingPrefs>) => {
    const next = { ...s, ...patch };
    setS(next);
    invoke("set_sharing", { sharing: next }).then(onChange);
  };
  const pick = async () => (await open({ directory: true })) as string | null;
  const kbd = "rounded-md bg-white/10 px-1.5 py-0.5 font-sans text-xs text-fg";

  return (
    <section aria-labelledby="sharing-title" className="rise mt-6 rounded-[28px] bg-raised p-7 ring-1 ring-line" style={{ "--i": 6 } as React.CSSProperties}>
      <h2 id="sharing-title" className="font-display text-2xl font-bold tracking-[-0.025em]">
        This computer shares
      </h2>
      <p className="mt-1 text-sm text-muted">Only with paired devices, over their encrypted link.</p>

      <div className="mt-6 flex flex-col gap-5">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <Toggle
            on={!!s.folder}
            label="A folder"
            hint={s.folder ? `${folderName(s.folder)} opens as a drive on the other computer` : "Opens as a drive on the other computer"}
            onChange={async (on) => {
              if (!on) return change({ folder: null });
              const folder = await pick();
              if (folder) change({ folder });
            }}
          />
          {s.folder && (
            <button className={quietButton} onClick={async () => { const folder = await pick(); if (folder) change({ folder }); }}>
              Change…
            </button>
          )}
        </div>
        <Toggle on={s.sound} onChange={(sound) => change({ sound })} label="Its sound" hint="Paired devices can play what this computer plays" />
        <Toggle on={s.notifications} onChange={(notifications) => change({ notifications })} label="Its notifications" hint="Shown on paired devices that ask for them" />
        <Toggle on={s.terminal} onChange={(terminal) => change({ terminal })} label="A terminal" hint="Paired devices can run commands here as you. Turn on only for your own computers." />
      </div>

      <div className="mt-7 flex flex-wrap items-center justify-between gap-3 border-t border-line pt-5 text-sm">
        <p className="min-w-0 text-muted">
          Received files go to <span className="text-fg">{folderName(received)}</span>
        </p>
        <div className="flex gap-1">
          <button
            className={quietButton}
            onClick={async () => {
              const folder = await pick();
              if (folder) invoke("set_received", { folder }).then(onChange);
            }}
          >
            Change…
          </button>
          <button className={quietButton} onClick={() => invoke("set_received", { folder: null }).then(onChange)}>
            Downloads
          </button>
        </div>
      </div>
      <p className="mt-3 text-xs text-faint">
        <kbd className={kbd}>{mac ? "⌃ ⌥ V" : "Ctrl Alt V"}</kbd> pastes the files that arrived last into the app in front.
      </p>
    </section>
  );
}
