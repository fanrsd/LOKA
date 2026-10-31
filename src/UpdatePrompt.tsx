import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Face } from "./Face";
import { Notes, type Update } from "./About";
import { primaryButton, quietButton } from "./Pairing";

type Phase = { at: "ready" } | { at: "downloading"; done: number; total?: number } | { at: "installing" } | { at: "failed"; error: string };

/** A newer LOKA: Oka hops in with what's new; installs in place where the updater can, else opens the download page. */
export function UpdatePrompt({ update, seed, onClose }: { update: Update; seed: string; onClose: () => void }) {
  const [phase, setPhase] = useState<Phase>({ at: "ready" });
  const busy = phase.at === "downloading" || phase.at === "installing";
  const version = update.installable?.version ?? update.available!.name.replace(/^LOKA\s*/i, "").replace(/^v/, "");
  const notes = update.available?.notes || update.installable?.notes || "";

  useEffect(() => {
    const un = listen<{ done: number; total?: number }>("update", ({ payload: p }) =>
      setPhase(p.total && p.done >= p.total ? { at: "installing" } : { at: "downloading", ...p }),
    );
    const esc = (e: KeyboardEvent) => e.key === "Escape" && !busy && onClose();
    window.addEventListener("keydown", esc);
    return () => {
      un.then((f) => f());
      window.removeEventListener("keydown", esc);
    };
  }, [busy, onClose]);

  const install = () => {
    setPhase({ at: "downloading", done: 0 });
    // resolves only on failure: success restarts LOKA (Windows: its installer closes and reopens it)
    invoke("install_update").catch((e) => setPhase({ at: "failed", error: String(e) }));
  };
  const download = () => invoke("open_url", { url: update.available?.url ?? "https://github.com/fanrsd/LOKA/releases/latest" });
  const pct = phase.at === "downloading" && phase.total ? Math.round((phase.done / phase.total) * 100) : null;

  return (
    <div className="fixed inset-x-0 bottom-0 top-(--titlebar) z-20 flex items-center justify-center bg-ink/70 p-5 backdrop-blur-md" style={{ animation: "fade-in 0.2s ease-out" }} onClick={() => !busy && onClose()}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="update-title"
        onClick={(e) => e.stopPropagation()}
        className="flex max-h-full w-full max-w-sm flex-col items-center rounded-[28px] bg-raised px-7 pb-6 pt-8 text-center ring-1 ring-line"
        style={{ animation: "sheet-in 0.45s var(--ease-spring)" }}
      >
        {/* Oka hops on a pulse of the accent, like a crossing */}
        <div className="relative grid h-28 w-28 place-items-center">
          {[0, 0.8].map((d) => (
            <span key={d} aria-hidden className="absolute inset-3 rounded-full border-2 border-accent/60" style={{ animation: `ripple 1.6s ease-out ${d}s infinite` }} />
          ))}
          <div style={{ animation: phase.at === "failed" ? undefined : "hop 1.6s ease-in-out infinite" }}>
            <Face seed={seed} size={84} mood={phase.at === "failed" ? "sad" : busy ? "waiting" : "happy"} />
          </div>
        </div>

        <p className="mt-3 rounded-full bg-accent/15 px-2.5 py-0.5 text-xs font-semibold text-accent">New version</p>
        <h2 id="update-title" className="mt-2 font-display text-[28px] font-bold leading-tight tracking-[-0.025em]">
          LOKA {version} is here
        </h2>
        {update.current && <p className="mt-1 text-sm text-muted">You have {update.current}.</p>}

        {notes && (
          <section className="mt-5 max-h-48 w-full overflow-y-auto rounded-[20px] bg-white/[0.04] p-4 text-left">
            <h3 className="text-xs font-medium text-faint">What's new</h3>
            <div className="mt-1.5 space-y-1.5 text-sm leading-relaxed text-muted">
              <Notes text={notes} />
            </div>
          </section>
        )}

        <div aria-live="polite" className="mt-6 w-full">
          {busy ? (
            <>
              <div className="h-2 w-full overflow-hidden rounded-full bg-white/[0.06]" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined} aria-label="Update progress">
                <div
                  className="h-full rounded-full bg-accent shadow-[0_0_12px_color-mix(in_oklab,var(--color-accent)_60%,transparent)] transition-[width] duration-300"
                  style={{ width: phase.at === "installing" ? "100%" : `${pct ?? 8}%` }}
                />
              </div>
              <p className="mt-2 text-sm text-muted">
                {phase.at === "installing" ? "Installing… LOKA restarts by itself" : pct === null ? "Downloading…" : `Downloading… ${pct}%`}
              </p>
            </>
          ) : phase.at === "failed" ? (
            <>
              <p className="text-sm text-danger">Couldn't update: {phase.error}</p>
              <div className="mt-4 flex justify-center gap-2">
                <button onClick={onClose} className={quietButton}>
                  Close
                </button>
                <button onClick={download} className={primaryButton}>
                  Download instead
                </button>
              </div>
            </>
          ) : (
            <div className="flex justify-center gap-2">
              {/* "Later" first and focused: a stray Enter never restarts LOKA mid-session */}
              <button autoFocus onClick={onClose} className={quietButton}>
                Later
              </button>
              <button onClick={update.installable ? install : download} className={primaryButton}>
                {update.installable ? "Update now" : "Download"}
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
