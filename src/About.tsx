import { useEffect, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { Face } from "./Face";
import { primaryButton, quietButton } from "./Pairing";

const REPO = "fanrsd/LOKA";
const LINKS = [
  ["Website", "https://loka.margingroup.id"],
  ["GitHub", `https://github.com/${REPO}`],
  ["Report a problem", `https://github.com/${REPO}/issues`],
  ["License", `https://github.com/${REPO}/blob/main/LICENSE`],
] as const;

type Release = { tag: string; name: string; url: string; date: string; notes: string };

/** "v0.1.10" vs "0.1.9": true when `tag` is a later version than `current`. */
export function isNewer(tag: string, current: string) {
  const a = tag.replace(/^v/, "").split(".").map((n) => parseInt(n) || 0);
  const b = current.split(".").map((n) => parseInt(n) || 0);
  for (let i = 0; i < Math.max(a.length, b.length); i++) if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) > (b[i] ?? 0);
  return false;
}

async function fetchReleases(): Promise<Release[]> {
  const r = await fetch(`https://api.github.com/repos/${REPO}/releases?per_page=10`, { headers: { Accept: "application/vnd.github+json" } });
  // unauthenticated API: 60 requests/hour per IP, shared behind VPNs like Cloudflare WARP or office NAT
  if (r.status === 403 || r.status === 429) throw new Error("GitHub is limiting checks from this network right now. Try again later.");
  if (!r.ok) throw new Error(`GitHub answered ${r.status}.`);
  const list: { tag_name: string; name: string | null; html_url: string; published_at: string; body: string | null; draft: boolean; prerelease: boolean }[] = await r.json();
  return list
    .filter((x) => !x.draft && !x.prerelease)
    .map((x) => ({ tag: x.tag_name, name: x.name || x.tag_name, url: x.html_url, date: x.published_at, notes: x.body ?? "" }));
}

export type Update = { current?: string; releases?: Release[]; error?: string; checking: boolean; available?: Release; check: () => void };

/** Checks GitHub Releases once on mount (60 unauthenticated requests/hour is plenty) and on demand. */
export function useUpdate(): Update {
  const [s, setS] = useState<Omit<Update, "available" | "check">>({ checking: true });
  const check = () => {
    setS((x) => ({ ...x, checking: true, error: undefined }));
    const version = getVersion();
    Promise.all([version, fetchReleases()])
      .then(([current, releases]) => setS({ current, releases, checking: false }))
      .catch((e) => version.then((current) => setS({ current, checking: false, error: e instanceof Error ? e.message : String(e) })));
  };
  useEffect(check, []);
  const latest = s.releases?.[0];
  return { ...s, available: latest && s.current && isNewer(latest.tag, s.current) ? latest : undefined, check };
}

/** One-time permission fix per OS: Windows Firewall, Linux input devices. */
const FIX: Record<string, { title: string; text: string; button: string; command: string; failed: string }> = {
  windows: {
    title: "Network",
    text: "Other computers can't find this one, or can't move over? Windows Firewall may be blocking LOKA on this network.",
    button: "Allow through Windows Firewall",
    command: "allow_firewall",
    failed: "Windows Firewall was not changed",
  },
  linux: {
    title: "Mouse and keyboard",
    text: "LOKA reads the mouse and keyboard and types for other devices below the desktop, so it works on X11, Wayland and the console. That needs your permission once.",
    button: "Allow input access",
    command: "allow_input",
    failed: "Input access was not allowed",
  },
};

export function About({ update, seed, os, onClose }: { update: Update; seed: string; os: string; onClose: () => void }) {
  const [fixed, setFixed] = useState<"idle" | "busy" | "done" | string>("idle");
  const fix = FIX[os];

  useEffect(() => {
    const esc = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", esc);
    return () => window.removeEventListener("keydown", esc);
  }, [onClose]);

  // the rule outlives the app: show what Windows Firewall has now, not what this window last did
  useEffect(() => {
    if (os !== "windows") return;
    invoke<string>("firewall_status").then((s) => {
      if (s === "allowed") setFixed("done");
      if (s === "blocked") setFixed("A Windows Firewall rule blocks LOKA: allowing replaces it");
    }, () => {});
  }, [os]);

  const allow = () => {
    setFixed("busy");
    invoke(fix.command).then(() => setFixed("done"), (e) => setFixed(String(e) || fix.failed));
  };

  return (
    <div className="fixed inset-x-0 bottom-0 top-(--titlebar) z-20 flex items-center justify-center bg-ink/70 p-5 backdrop-blur-md" style={{ animation: "fade-in 0.2s ease-out" }} onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="about-title"
        onClick={(e) => e.stopPropagation()}
        className="flex max-h-full w-full max-w-md flex-col rounded-[28px] bg-raised ring-1 ring-line"
        style={{ animation: "sheet-in 0.45s var(--ease-spring)" }}
      >
        <header className="flex items-center gap-4 px-7 pt-7">
          <Face seed={seed} size={52} mood={update.available ? "waiting" : "happy"} />
          <div className="min-w-0 flex-1">
            <h2 id="about-title" className="font-display text-[26px] font-bold leading-tight tracking-[-0.025em]">
              LOKA
            </h2>
            <p className="text-sm text-muted">Version {update.current ?? "…"}</p>
          </div>
          <button autoFocus onClick={onClose} className={quietButton}>
            Close
          </button>
        </header>

        <div className="mt-5 overflow-y-auto px-7 pb-7">
          <section aria-live="polite" className="rounded-[20px] bg-white/[0.04] p-4">
            {update.checking ? (
              <p className="text-sm text-muted">Checking for updates…</p>
            ) : update.available ? (
              <div className="flex items-center gap-3">
                <p className="flex-1 text-sm">
                  <span className="font-semibold text-accent">{update.available.name}</span> is available.
                </p>
                <button onClick={() => invoke("open_url", { url: update.available!.url })} className={primaryButton}>
                  Download
                </button>
              </div>
            ) : (
              <div className="flex items-center gap-3">
                <p className={`flex-1 text-sm ${update.error ? "text-danger" : "text-muted"}`}>
                  {update.error ? `Couldn't check for updates. ${update.error}` : "You have the latest version."}
                </p>
                {update.error ? (
                  <button onClick={() => invoke("open_url", { url: `https://github.com/${REPO}/releases` })} className={quietButton}>
                    Open releases
                  </button>
                ) : (
                  <button onClick={update.check} className={quietButton}>
                    Check again
                  </button>
                )}
              </div>
            )}
          </section>

          {fix && (
            <section className="mt-6">
              <h3 className="text-xs font-medium text-faint">{fix.title}</h3>
              <p className="mt-2 text-sm leading-relaxed text-muted">{fix.text}</p>
              <div className="mt-3 flex items-center gap-3">
                <button onClick={allow} disabled={fixed === "busy" || fixed === "done"} className={quietButton + " ring-1 ring-line"}>
                  {fix.button}
                </button>
                <span role="status" className={`text-xs ${fixed === "done" ? "text-fg" : "text-danger"}`}>
                  {fixed === "done" ? "Allowed" : fixed === "idle" || fixed === "busy" ? "" : fixed}
                </span>
              </div>
            </section>
          )}

          {update.releases && update.releases.length > 0 && (
            <section className="mt-6">
              <h3 className="text-xs font-medium text-faint">What's new</h3>
              <ol className="mt-2 space-y-5">
                {update.releases.map((r) => (
                  <li key={r.tag}>
                    <p className="flex items-baseline justify-between gap-3">
                      <span className="font-display font-semibold">{r.name}</span>
                      <time dateTime={r.date} className="text-xs text-faint">
                        {new Date(r.date).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" })}
                      </time>
                    </p>
                    <div className="mt-1.5 space-y-1.5 text-sm leading-relaxed text-muted">
                      <Notes text={r.notes} />
                    </div>
                  </li>
                ))}
              </ol>
            </section>
          )}

          <nav aria-label="LOKA links" className="mt-7 flex flex-wrap gap-x-4 gap-y-2 text-xs">
            {LINKS.map(([label, url]) => (
              <button key={url} onClick={() => invoke("open_url", { url })} className="text-faint underline-offset-2 hover:text-fg hover:underline focus-visible:outline-2 focus-visible:outline-accent">
                {label}
              </button>
            ))}
            <span className="text-faint">Apache 2.0 · © Aufa Nirsad</span>
          </nav>
        </div>
      </div>
    </div>
  );
}

/** The small markdown subset release notes use: paragraphs, "- " lists, **bold**, `code`. Install steps are skipped. */
function Notes({ text }: { text: string }) {
  const out: ReactNode[] = [];
  let items: string[] = [];
  const flush = () => {
    if (items.length) out.push(<ul key={out.length} className="list-disc space-y-1 pl-5">{items.map((t, i) => <li key={i}>{inline(t)}</li>)}</ul>);
    items = [];
  };
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim().replace(/^#+\s*/, "");
    if (!line || /^\*\*(Windows|macOS|Linux):\*\*/.test(line)) continue;
    if (/^[-*] /.test(line)) items.push(line.slice(2));
    else {
      flush();
      out.push(<p key={out.length}>{inline(line)}</p>);
    }
  }
  flush();
  return <>{out}</>;
}

const inline = (t: string) =>
  t.split(/(\*\*[^*]+\*\*|`[^`]+`)/).map((part, i) =>
    part.startsWith("**") ? <strong key={i} className="font-semibold text-fg">{part.slice(2, -2)}</strong>
    : part.startsWith("`") ? <code key={i} className="rounded bg-white/10 px-1 text-xs">{part.slice(1, -1)}</code>
    : part.replace(/\*([^*]+)\*/g, "$1"),
  );
