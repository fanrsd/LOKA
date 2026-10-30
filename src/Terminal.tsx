import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { quietButton } from "./Pairing";

/** Full-window shell on a paired device. Esc goes to the shell, so only Close ends it. */
export function Terminal({ fingerprint, alias, onClose }: { fingerprint: string; alias: string; onClose: () => void }) {
  const host = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<"connecting" | "open" | "ended" | { error: string }>("connecting");

  useEffect(() => {
    const css = getComputedStyle(document.documentElement);
    const token = (name: string) => css.getPropertyValue(name).trim();
    const term = new XTerm({
      fontFamily: '"Cascadia Mono", Consolas, "SF Mono", Menlo, ui-monospace, monospace',
      fontSize: 13,
      cursorBlink: true,
      theme: {
        background: token("--color-raised"),
        foreground: token("--color-fg"),
        cursor: token("--color-accent"),
        cursorAccent: token("--color-raised"),
        selectionBackground: "#eceff733",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current!);
    fit.fit();
    term.focus();

    let id: number | undefined;
    let gone = false;
    const onData = new Channel<ArrayBuffer>();
    // an empty buffer is the end marker
    onData.onmessage = (buf) => {
      if (buf.byteLength) return term.write(new Uint8Array(buf));
      id = undefined;
      setStatus("ended");
    };
    invoke<number>("term_open", { fingerprint, cols: term.cols, rows: term.rows, onData }).then(
      (i) => {
        if (gone) return invoke("term_close", { id: i });
        id = i;
        invoke("term_resize", { id, cols: term.cols, rows: term.rows }); // in case it changed while connecting
        setStatus((s) => (s === "ended" ? s : "open"));
      },
      (e) => !gone && setStatus({ error: String(e) }),
    );
    const input = term.onData((data) => id !== undefined && invoke("term_write", { id, data }));
    const resize = term.onResize(({ cols, rows }) => id !== undefined && invoke("term_resize", { id, cols, rows }));
    const observer = new ResizeObserver(() => fit.fit());
    observer.observe(host.current!);
    return () => {
      gone = true;
      observer.disconnect();
      input.dispose();
      resize.dispose();
      if (id !== undefined) invoke("term_close", { id });
      term.dispose();
    };
  }, [fingerprint]);

  const note =
    status === "connecting" ? "Connecting…" : status === "open" ? "Terminal" : status === "ended" ? "Session ended" : status.error;

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="terminal-title"
      className="fixed inset-x-0 bottom-0 top-(--titlebar) z-30 flex flex-col bg-ink"
      style={{ animation: "fade-in 0.2s ease-out" }}
    >
      <header className="flex items-center gap-4 px-6 pb-3 pt-5">
        <div className="min-w-0 flex-1">
          <h2 id="terminal-title" className="truncate font-display text-[22px] font-bold leading-tight tracking-[-0.025em]">
            {alias}
          </h2>
          <p aria-live="polite" className={`truncate text-sm ${typeof status === "object" ? "text-danger" : "text-muted"}`}>
            {note}
          </p>
        </div>
        <button onClick={onClose} className={quietButton}>
          Close
        </button>
      </header>
      <div className="mx-4 mb-4 min-h-0 flex-1 overflow-hidden rounded-[24px] bg-raised p-3 ring-1 ring-line">
        <div ref={host} className="h-full w-full" />
      </div>
    </div>
  );
}
