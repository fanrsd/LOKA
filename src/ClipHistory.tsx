import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Entry = { id: number; kind: "text" | "image"; preview: string; time: number };

/** Recent clipboard: the last things copied here (or arrived from the other device); click one to copy it again. */
export function ClipHistory() {
  const [list, setList] = useState<Entry[]>([]);
  const [copied, setCopied] = useState<number | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    const load = () => void invoke<Entry[]>("clip_history").then(setList);
    load();
    const un = listen("clip-history", load);
    return () => void un.then((f) => f());
  }, []);

  useEffect(() => {
    if (copied === null) return;
    const t = setTimeout(() => setCopied(null), 1200);
    return () => clearTimeout(t);
  }, [copied]);

  const restore = (id: number) =>
    invoke("clip_restore", { id }).then(
      () => (setError(""), setCopied(id)),
      (e) => setError(String(e)),
    );

  return (
    <section aria-labelledby="clip-title" className="rounded-[24px] bg-raised p-5 ring-1 ring-line">
      <h2 id="clip-title" className="font-display text-lg font-bold tracking-tight">Recent clipboard</h2>
      {list.length === 0 ? (
        <p className="mt-2 text-sm text-muted">Things you copy show up here.</p>
      ) : (
        <ul className="mt-3 flex flex-col gap-1.5">
          {list.map((e) => (
            <li key={e.id}>
              <button
                className="flex w-full items-center gap-3 rounded-2xl px-3 py-2 text-left ring-1 ring-line transition-colors hover:bg-white/5 focus-visible:outline-2 focus-visible:outline-accent"
                onClick={() => restore(e.id)}
              >
                {e.kind === "image" ? (
                  <img src={e.preview} alt="Copied picture" className="max-h-16 max-w-24 rounded-lg object-contain" />
                ) : (
                  <span className="line-clamp-2 min-w-0 flex-1 text-sm break-words whitespace-pre-wrap">{e.preview}</span>
                )}
                <span aria-live="polite" className="ml-auto shrink-0 text-xs text-accent">
                  {copied === e.id ? "Copied" : ""}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      {error && <p role="alert" className="mt-2 text-xs text-danger">{error}</p>}
    </section>
  );
}
