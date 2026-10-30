import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { About, useUpdate } from "./About";
import { Calibrate } from "./Calibrate";
import { Face, type Mood } from "./Face";
import { PairPrompt, osLabel, quietButton, type Device, type Peer } from "./Pairing";
import { ClipHistory } from "./ClipHistory";
import { Outbox } from "./Outbox";
import { Pointer } from "./Pointer";
import { Remote } from "./Remote";
import { Sharing } from "./Sharing";
import { Transfers } from "./Transfers";

const SELECTED_KEY = "loka.selected";

export function Home() {
  const [device, setDevice] = useState<Device | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [about, setAbout] = useState(false);
  const update = useUpdate();
  const refreshDevice = () => invoke<Device>("get_device").then(setDevice);

  useEffect(() => {
    const refreshPeers = () => invoke<Peer[]>("peers").then(setPeers);
    refreshDevice();
    // subscribe before the first fetch so nothing announced in between is missed
    const unPeer = listen("peer", refreshPeers);
    unPeer.then(refreshPeers);
    const unPaired = listen("paired", refreshDevice);
    const unDevice = listen("device", refreshDevice);
    const unAbout = listen("about", () => setAbout(true)); // tray menu
    const prune = setInterval(refreshPeers, 5000); // devices that went quiet drop off
    return () => {
      unPeer.then((f) => f());
      unPaired.then((f) => f());
      unDevice.then((f) => f());
      unAbout.then((f) => f());
      clearInterval(prune);
    };
  }, []);

  if (!device) return null;
  const { alias, fingerprint } = device.info;

  return (
    <main className="mx-auto flex min-h-full max-w-3xl flex-col px-8 pb-6 pt-6">
      <header className="flex items-center gap-3">
        <Face seed={fingerprint} size={34} />
        <div className="min-w-0 flex-1 leading-tight">
          <p className="truncate font-display text-[15px] font-semibold">{alias}</p>
          <p className="text-xs text-muted">This computer</p>
        </div>
        <button
          onClick={() => setAbout(true)}
          aria-label={update.available ? `About LOKA, ${update.available.name} available` : "About LOKA"}
          className={`${quietButton} relative`}
        >
          About
          {update.available && <span aria-hidden className="absolute right-2 top-2 h-2 w-2 rounded-full bg-accent" />}
        </button>
      </header>
      <Devices device={device} peers={peers} onChange={refreshDevice} />
      <Sharing key={String(device.sharing.folder)} initial={device.sharing} received={device.received} mac={device.info.os === "macos"} onChange={refreshDevice} />
      <ClipHistory />
      <Transfers />
      <PairPrompt selfSeed={fingerprint} />
      <footer className="mt-auto flex justify-end pt-10">
        <button onClick={() => invoke("start_sim")} className="text-xs text-faint underline-offset-2 hover:text-muted hover:underline">
          Developer: open phone simulator
        </button>
      </footer>

      <Outbox device={device} peers={peers} />
      {about && <About update={update} seed={fingerprint} windows={device.info.os === "windows"} onClose={() => setAbout(false)} />}
    </main>
  );
}

type PairAnswer = "accepted" | "declined" | "noAnswer" | "unknown";
type PairState = { state: "waiting"; code?: string } | { state: Exclude<PairAnswer, "accepted"> } | { state: "error"; message: string };

/** Every LOKA nearby. Any of them can be paired and placed around this computer's screens; any paired one can drive this one too. */
function Devices({ device, peers, onChange }: { device: Device; peers: Peer[]; onChange: () => void }) {
  const [selected, setSelected] = useState<string | null>(() => localStorage.getItem(SELECTED_KEY));
  const [pairing, setPairing] = useState<Record<string, PairState>>({});
  const [kvm, setKvm] = useState<{ active: boolean; error?: string | null; device?: string }>({ active: false });
  const [locked, setLocked] = useState(false);
  const mac = device.info.os === "macos";

  useEffect(() => {
    const un = listen<{ active: boolean; error?: string | null; device?: string }>("kvm", (e) => setKvm(e.payload));
    const unLock = listen<boolean>("lock", (e) => setLocked(e.payload));
    // the requester's half of the pairing code arrives once both nonces are exchanged
    const unCode = listen<{ fingerprint: string; code: string }>("pair-code", ({ payload: { fingerprint: f, code } }) =>
      setPairing((s) => (s[f]?.state === "waiting" ? { ...s, [f]: { state: "waiting", code } } : s)),
    );
    return () => {
      un.then((f) => f());
      unLock.then((f) => f());
      unCode.then((f) => f());
    };
  }, []);

  const isPaired = (f: string) => device.paired.some((p) => p.fingerprint === f);
  const outdated = (p: Peer) => p.info.proto !== device.info.proto;
  const select = (f: string | null) => {
    setSelected(f);
    if (f) localStorage.setItem(SELECTED_KEY, f);
    else localStorage.removeItem(SELECTED_KEY);
  };

  const offlinePaired = device.paired.filter((p) => !peers.some((c) => c.info.fingerprint === p.fingerprint));
  const current = peers.find((c) => c.info.fingerprint === selected && isPaired(selected) && !outdated(c));
  const placed = (f: string) => f in device.portals;

  const pair = async (p: Peer) => {
    const f = p.info.fingerprint;
    setPairing((s) => ({ ...s, [f]: { state: "waiting" } }));
    try {
      const answer = await invoke<PairAnswer>("pair", { fingerprint: f });
      setPairing(({ [f]: _, ...rest }) => (answer === "accepted" ? rest : { ...rest, [f]: { state: answer } }));
      if (answer === "accepted") select(f);
    } catch (e) {
      setPairing((s) => ({ ...s, [f]: { state: "error", message: String(e) } }));
    }
  };

  const view = (p: Peer): { mood: Mood; note: string; tone: "fg" | "muted" | "danger" } => {
    const f = p.info.fingerprint;
    const s = pairing[f];
    if (outdated(p)) return { mood: "sad", note: "Needs LOKA update", tone: "muted" };
    if (s?.state === "waiting") return { mood: "waiting", note: s.code ? `Code ${s.code}` : "Asking…", tone: "fg" };
    if (isPaired(f)) return { mood: selected === f ? "happy" : "idle", note: placed(f) ? "Paired" : "Paired · place it", tone: selected === f ? "fg" : "muted" };
    if (s?.state === "declined") return { mood: "sad", note: "Declined", tone: "danger" };
    if (s?.state === "noAnswer") return { mood: "sad", note: "No answer", tone: "danger" };
    if (s?.state === "unknown") return { mood: "sad", note: "Can't see this computer", tone: "danger" };
    if (s?.state === "error") return { mood: "sad", note: "Couldn't reach it", tone: "danger" };
    return { mood: "idle", note: "Click to pair", tone: "muted" };
  };

  const empty = peers.length === 0 && offlinePaired.length === 0;
  const waiting = Object.values(pairing).find((s) => s.state === "waiting");
  const waitingCode = waiting?.state === "waiting" ? waiting.code : undefined;
  const unseen = peers.find((p) => pairing[p.info.fingerprint]?.state === "unknown");

  return (
    <>
      <section aria-labelledby="nearby" className="mt-12">
        <h1 id="nearby" className="rise font-display text-[44px] font-extrabold leading-[0.95] tracking-[-0.04em]">
          {current ? (
            <>
              Paired with
              <br />
              <span className="text-accent">{current.info.alias}</span>
            </>
          ) : (
            "Nearby"
          )}
        </h1>
        <p className="rise mt-3 max-w-[52ch] text-[15px] leading-relaxed text-muted" style={{ "--i": 1 } as React.CSSProperties}>
          {waiting
            ? waitingCode
              ? `Check that the other device shows ${waitingCode}, then accept there.`
              : "Asking the other device…"
            : unseen
              ? `${unseen.info.alias} can't see this computer on the network. Check both are on the same network, or add this computer by IP there.`
              : current
                ? "Push your cursor off the glowing edge to move over. Push it back out the far side to come home. The other device can reach this one the same way."
                : "Every computer running LOKA shows up here. Click one to pair, then place it around your screens."}
        </p>

        {empty ? (
          <div className="rise mt-10 flex items-center gap-5 rounded-[24px] bg-raised p-6 ring-1 ring-line" style={{ "--i": 2 } as React.CSSProperties}>
            <Face seed={device.info.fingerprint} size={56} mood="searching" />
            <div>
              <p className="font-display text-lg font-semibold">Looking around your network…</p>
              <p className="mt-1 text-sm text-muted">Open LOKA on the other computer.</p>
            </div>
          </div>
        ) : (
          <ul className="mt-9 flex flex-wrap gap-x-4 gap-y-6">
            {peers.map((p, i) => {
              const f = p.info.fingerprint;
              const v = view(p);
              const active = selected === f && isPaired(f);
              return (
                <li key={f} className="rise" style={{ "--i": 2 + i } as React.CSSProperties}>
                  <button
                    disabled={pairing[f]?.state === "waiting" || outdated(p)}
                    onClick={() => (isPaired(f) ? select(f) : pair(p))}
                    aria-pressed={active}
                    aria-label={`${p.info.alias}, ${osLabel(p.info.os)}. ${v.note}`}
                    className={`group flex w-32 flex-col items-center rounded-[22px] px-2 pb-3 pt-4 transition-[background-color,transform] duration-300 ease-[var(--ease-spring)] hover:bg-white/[0.04] active:scale-[0.96] focus-visible:outline-2 focus-visible:outline-accent ${outdated(p) ? "disabled:cursor-not-allowed" : "disabled:cursor-wait"}`}
                  >
                    <span className={`rounded-[36%] p-1 ring-2 transition-[box-shadow,transform] duration-300 ease-[var(--ease-spring)] group-hover:-translate-y-0.5 ${active ? "ring-accent" : "ring-transparent"}`}>
                      <Face seed={f} size={68} mood={v.mood} />
                    </span>
                    <span className="mt-3 w-full truncate text-center text-sm font-semibold">{p.info.alias}</span>
                    <span className={`mt-0.5 text-xs tabular-nums ${v.tone === "danger" ? "text-danger" : v.tone === "fg" ? "text-fg" : "text-muted"}`}>{v.note}</span>
                  </button>
                </li>
              );
            })}
            {offlinePaired.map((p, i) => (
              <li key={p.fingerprint} className="rise flex w-32 flex-col items-center px-2 pb-3 pt-4 opacity-60" style={{ "--i": 2 + peers.length + i } as React.CSSProperties}>
                <span className="p-1 grayscale">
                  <Face seed={p.fingerprint} size={68} mood="sad" />
                </span>
                <span className="mt-3 w-full truncate text-center text-sm font-semibold">{p.alias}</span>
                <span className="mt-0.5 text-xs text-muted">Offline</span>
                {device.wakeable.includes(p.fingerprint) && (
                  <button onClick={() => invoke("wake", { fingerprint: p.fingerprint })} className="mt-1 text-xs text-accent underline-offset-2 hover:underline">
                    Wake up
                  </button>
                )}
                <button onClick={() => invoke("unpair", { fingerprint: p.fingerprint })} className="mt-1 text-xs text-faint underline-offset-2 hover:text-danger hover:underline">
                  Forget
                </button>
              </li>
            ))}
          </ul>
        )}
        <AddHost hosts={device.hosts} ips={device.ips} onChange={onChange} />
      </section>

      {current && selected && (
        <>
          <Calibrate key={selected} fingerprint={selected} alias={current.info.alias} saved={device.portals[selected]} />
          <Pointer initial={device.cursor} mac={mac} />
          <Remote key={selected} fingerprint={selected} alias={current.info.alias} mac={mac} />
          <div className="mt-4 flex justify-end">
            <button
              onClick={() => {
                invoke("unpair", { fingerprint: selected });
                select(null);
              }}
              className="text-xs text-faint underline-offset-2 hover:text-danger hover:underline"
            >
              Unpair {current.info.alias}
            </button>
          </div>
        </>
      )}

      {/* status toast while input is on another device, locked, or why it couldn't move */}
      {(kvm.active || kvm.error || locked) && (
        <div
          role="status"
          className="fixed bottom-5 left-1/2 flex -translate-x-1/2 items-center gap-3 rounded-full bg-raised py-2 pl-2 pr-4 text-sm ring-1 ring-line"
          style={{ animation: "sheet-in 0.4s var(--ease-spring)" }}
        >
          {kvm.active && kvm.device ? <Face seed={kvm.device} size={26} mood="happy" /> : <Face seed={kvm.error ? "error" : device.info.fingerprint} size={26} mood={kvm.error ? "sad" : "idle"} />}
          {kvm.active ? (
            <span>
              Using {peers.find((p) => p.info.fingerprint === kvm.device)?.info.alias ?? "the other device"}
              {locked && " · locked"} · <kbd className="rounded-md bg-white/10 px-1.5 py-0.5 font-sans text-xs text-muted">{mac ? "⌃ ⌥ Esc" : "Ctrl Alt Esc"}</kbd> to return
            </span>
          ) : kvm.error ? (
            <span className="text-danger">Couldn't move over: {kvm.error}</span>
          ) : (
            <span>
              Pointer locked here · <kbd className="rounded-md bg-white/10 px-1.5 py-0.5 font-sans text-xs text-muted">{mac ? "F14" : "Scroll Lock"}</kbd> to release
            </span>
          )}
        </div>
      )}
    </>
  );
}

/** Fallback for networks that drop discovery (guest Wi-Fi, client isolation): reach a device by its address. */
function AddHost({ hosts, ips, onChange }: { hosts: string[]; ips: string[]; onChange: () => void }) {
  const [ip, setIp] = useState("");
  const [error, setError] = useState("");

  const add = (e: React.FormEvent) => {
    e.preventDefault();
    invoke("add_host", { ip }).then(
      () => {
        setIp("");
        setError("");
        onChange();
      },
      (err) => setError(String(err)),
    );
  };

  return (
    <div className="mt-8 text-sm">
      <form onSubmit={add} className="flex flex-wrap items-center gap-2">
        <label htmlFor="add-ip" className="text-muted">
          Not showing up? Add by IP
        </label>
        <input
          id="add-ip"
          value={ip}
          onChange={(e) => setIp(e.target.value)}
          placeholder="192.168.1.20"
          inputMode="decimal"
          spellCheck={false}
          aria-describedby={error ? "add-ip-error" : undefined}
          className="w-36 rounded-full bg-white/[0.06] px-4 py-2 font-mono text-fg ring-1 ring-line placeholder:text-faint focus-visible:outline-2 focus-visible:outline-accent"
        />
        <button type="submit" disabled={!ip.trim()} className={quietButton + " ring-1 ring-line disabled:opacity-50"}>
          Add
        </button>
      </form>
      {ips.length > 0 && (
        <p className="mt-2 text-xs text-faint">
          This computer's address: <span className="select-text font-mono text-fg">{ips.join(", ")}</span>
        </p>
      )}
      {error && (
        <p id="add-ip-error" role="alert" className="mt-2 text-xs text-danger">
          {error}
        </p>
      )}
      {hosts.length > 0 && (
        <ul className="mt-3 flex flex-wrap gap-2" aria-label="Added by IP">
          {hosts.map((h) => (
            <li key={h} className="flex items-center gap-1 rounded-full bg-white/[0.04] py-1 pl-3 pr-1 font-mono text-xs text-muted ring-1 ring-line">
              {h}
              <button
                onClick={() => invoke("remove_host", { ip: h }).then(onChange)}
                aria-label={`Remove ${h}`}
                className="grid h-6 w-6 place-items-center rounded-full hover:bg-white/10 hover:text-danger focus-visible:outline-2 focus-visible:outline-accent"
              >
                ×
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
