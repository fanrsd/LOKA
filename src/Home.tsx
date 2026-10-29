import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Calibrate } from "./Calibrate";
import { Face, type Mood } from "./Face";
import { PairPrompt, osLabel, quietButton, type Device, type Peer, type Role } from "./Pairing";
import { Transfers } from "./Transfers";

const TARGET_KEY = "loka.target";

export function Home() {
  const [device, setDevice] = useState<Device | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);

  useEffect(() => {
    const refreshDevice = () => invoke<Device>("get_device").then(setDevice);
    const refreshPeers = () => invoke<Peer[]>("peers").then(setPeers);
    refreshDevice();
    // subscribe before the first fetch so nothing announced in between is missed
    const unPeer = listen("peer", refreshPeers);
    unPeer.then(refreshPeers);
    const unPaired = listen("paired", refreshDevice);
    const prune = setInterval(refreshPeers, 5000); // devices that went quiet drop off
    return () => {
      unPeer.then((f) => f());
      unPaired.then((f) => f());
      clearInterval(prune);
    };
  }, []);

  if (!device) return null;
  const setRole = (role: Role) =>
    invoke("set_role", { role }).then(() => setDevice({ ...device, info: { ...device.info, role } }));

  if (!device.info.role) return <RolePicker device={device} onPick={setRole} />;

  const { alias, fingerprint, role } = device.info;
  return (
    <main className="mx-auto flex min-h-full max-w-3xl flex-col px-8 pb-6 pt-6">
      <header className="flex items-center gap-3">
        <Face seed={fingerprint} size={34} />
        <div className="min-w-0 flex-1 leading-tight">
          <p className="truncate font-display text-[15px] font-semibold">{alias}</p>
          <p className="text-xs text-muted">This computer</p>
        </div>
        <RoleSwitch role={role!} onChange={setRole} />
      </header>
      {role === "main" ? <MainHome device={device} peers={peers} /> : <ClientHome device={device} peers={peers} />}
      <Transfers />
    </main>
  );
}

/** Apple-style segmented control with a sliding thumb. */
function RoleSwitch({ role, onChange }: { role: Role; onChange: (r: Role) => void }) {
  const roles: Role[] = ["main", "client"];
  return (
    <div role="radiogroup" aria-label="Role of this computer" className="relative grid grid-cols-2 rounded-full bg-white/[0.06] p-1 text-sm">
      <span
        aria-hidden
        className="absolute inset-y-1 left-1 w-[calc(50%-4px)] rounded-full bg-fg transition-transform duration-500 ease-[var(--ease-spring)]"
        style={{ transform: role === "client" ? "translateX(100%)" : undefined }}
      />
      {roles.map((r) => (
        <button
          key={r}
          role="radio"
          aria-checked={role === r}
          onClick={() => role !== r && onChange(r)}
          className={`relative z-[1] rounded-full px-4 py-1.5 font-medium capitalize transition-colors duration-300 focus-visible:outline-2 focus-visible:outline-accent ${role === r ? "text-ink" : "text-muted hover:text-fg"}`}
        >
          {r}
        </button>
      ))}
    </div>
  );
}

function RolePicker({ device, onPick }: { device: Device; onPick: (role: Role) => void }) {
  const options: { role: Role; title: string; body: string; look: "left" | "right" }[] = [
    { role: "main", title: "Main", body: "Its mouse and keyboard reach the devices around it.", look: "right" },
    { role: "client", title: "Client", body: "It gets controlled by a Main sitting next to it.", look: "left" },
  ];
  return (
    <main className="grid min-h-full place-items-center px-8 py-10">
      <div className="w-full max-w-xl">
        <div className="rise">
          <Face seed={device.info.fingerprint} size={56} />
        </div>
        <h1 className="rise mt-6 font-display text-5xl font-extrabold leading-[0.95] tracking-[-0.04em]" style={{ "--i": 1 } as React.CSSProperties}>
          Hi, I'm {device.info.alias}.
          <br />
          <span className="text-muted">What's my job?</span>
        </h1>
        <p className="rise mt-4 max-w-[46ch] text-[15px] leading-relaxed text-muted" style={{ "--i": 2 } as React.CSSProperties}>
          Pick Main on the computer whose mouse and keyboard you want to share. You can change this any time.
        </p>
        <div className="mt-8 grid gap-3 sm:grid-cols-2">
          {options.map((o, i) => (
            <button
              key={o.role}
              onClick={() => onPick(o.role)}
              className="rise group rounded-[24px] bg-raised p-6 text-left ring-1 ring-line transition-[transform,box-shadow] duration-300 ease-[var(--ease-spring)] hover:-translate-y-1 hover:ring-white/20 active:scale-[0.98] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
              style={{ "--i": 3 + i } as React.CSSProperties}
            >
              <Face seed={o.role === "main" ? "main-role" : "client-role"} size={44} look={o.look} />
              <span className="mt-5 block font-display text-2xl font-bold tracking-[-0.02em]">{o.title}</span>
              <span className="mt-1 block text-sm leading-relaxed text-muted">{o.body}</span>
            </button>
          ))}
        </div>
      </div>
    </main>
  );
}

type PairState = "waiting" | "declined" | string; // any other string is an error message

function MainHome({ device, peers }: { device: Device; peers: Peer[] }) {
  const [target, setTarget] = useState<string | null>(() => localStorage.getItem(TARGET_KEY));
  const [pairing, setPairing] = useState<Record<string, PairState>>({});
  const [kvm, setKvm] = useState<{ active: boolean; error?: string | null }>({ active: false });

  useEffect(() => {
    const un = listen<{ active: boolean; error?: string | null }>("kvm", (e) => setKvm(e.payload));
    return () => void un.then((f) => f());
  }, []);

  const isPaired = (f: string) => device.paired.some((p) => p.fingerprint === f);
  const select = (f: string | null) => {
    setTarget(f);
    if (f) localStorage.setItem(TARGET_KEY, f);
    else localStorage.removeItem(TARGET_KEY);
  };

  const clients = peers.filter((p) => p.info.role === "client");
  const offlinePaired = device.paired.filter((p) => !clients.some((c) => c.info.fingerprint === p.fingerprint));
  const selected = clients.find((c) => c.info.fingerprint === target && isPaired(target));

  const pair = async (p: Peer) => {
    const f = p.info.fingerprint;
    setPairing((s) => ({ ...s, [f]: "waiting" }));
    try {
      const accepted = await invoke<boolean>("pair", { fingerprint: f });
      setPairing((s) => ({ ...s, [f]: accepted ? "" : "declined" }));
      if (accepted) select(f);
    } catch (e) {
      setPairing((s) => ({ ...s, [f]: String(e) }));
    }
  };

  const view = (p: Peer): { mood: Mood; note: string; tone: "fg" | "muted" | "danger" } => {
    const f = p.info.fingerprint;
    const s = pairing[f];
    if (s === "waiting") return { mood: "waiting", note: "Accept on it…", tone: "fg" };
    if (isPaired(f)) return { mood: target === f ? "happy" : "idle", note: target === f ? "Connected" : "Paired", tone: target === f ? "fg" : "muted" };
    if (s === "declined") return { mood: "sad", note: "Declined", tone: "danger" };
    if (s) return { mood: "sad", note: "Couldn't reach it", tone: "danger" };
    return { mood: "idle", note: "Click to pair", tone: "muted" };
  };

  const empty = clients.length === 0 && offlinePaired.length === 0;

  return (
    <>
      <section aria-labelledby="nearby" className="mt-12">
        <h1 id="nearby" className="rise font-display text-[44px] font-extrabold leading-[0.95] tracking-[-0.04em]">
          {selected ? (
            <>
              Paired with
              <br />
              <span className="text-accent">{selected.info.alias}</span>
            </>
          ) : (
            "Nearby"
          )}
        </h1>
        <p className="rise mt-3 max-w-[52ch] text-[15px] leading-relaxed text-muted" style={{ "--i": 1 } as React.CSSProperties}>
          {selected
            ? "Push your cursor off the glowing edge to move over. Push it back out the far side to come home."
            : "Devices running LOKA as Client show up here. Click one to pair."}
        </p>

        {empty ? (
          <div className="rise mt-10 flex items-center gap-5 rounded-[24px] bg-raised p-6 ring-1 ring-line" style={{ "--i": 2 } as React.CSSProperties}>
            <Face seed={device.info.fingerprint} size={56} mood="searching" />
            <div>
              <p className="font-display text-lg font-semibold">Looking around your network…</p>
              <p className="mt-1 text-sm text-muted">Open LOKA on the other computer and choose Client.</p>
            </div>
          </div>
        ) : (
          <ul className="mt-9 flex flex-wrap gap-x-4 gap-y-6">
            {clients.map((p, i) => {
              const f = p.info.fingerprint;
              const v = view(p);
              const active = target === f && isPaired(f);
              return (
                <li key={f} className="rise" style={{ "--i": 2 + i } as React.CSSProperties}>
                  <button
                    disabled={pairing[f] === "waiting"}
                    onClick={() => (isPaired(f) ? select(f) : pair(p))}
                    aria-pressed={active}
                    aria-label={`${p.info.alias}, ${osLabel(p.info.os)}. ${v.note}`}
                    className="group flex w-32 flex-col items-center rounded-[22px] px-2 pb-3 pt-4 transition-[background-color,transform] duration-300 ease-[var(--ease-spring)] hover:bg-white/[0.04] active:scale-[0.96] focus-visible:outline-2 focus-visible:outline-accent disabled:cursor-wait"
                  >
                    <span className={`rounded-[36%] p-1 ring-2 transition-[box-shadow,transform] duration-300 ease-[var(--ease-spring)] group-hover:-translate-y-0.5 ${active ? "ring-accent" : "ring-transparent"}`}>
                      <Face seed={f} size={68} mood={v.mood} />
                    </span>
                    <span className="mt-3 w-full truncate text-center text-sm font-semibold">{p.info.alias}</span>
                    <span className={`mt-0.5 text-xs ${v.tone === "danger" ? "text-danger" : v.tone === "fg" ? "text-fg" : "text-muted"}`}>{v.note}</span>
                  </button>
                </li>
              );
            })}
            {offlinePaired.map((p, i) => (
              <li key={p.fingerprint} className="rise flex w-32 flex-col items-center px-2 pb-3 pt-4 opacity-60" style={{ "--i": 2 + clients.length + i } as React.CSSProperties}>
                <span className="p-1 grayscale">
                  <Face seed={p.fingerprint} size={68} mood="sad" />
                </span>
                <span className="mt-3 w-full truncate text-center text-sm font-semibold">{p.alias}</span>
                <span className="mt-0.5 text-xs text-muted">Offline</span>
                <button onClick={() => invoke("unpair", { fingerprint: p.fingerprint })} className="mt-1 text-xs text-faint underline-offset-2 hover:text-danger hover:underline">
                  Forget
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {selected && target && (
        <>
          <Calibrate target={target} />
          <div className="mt-4 flex justify-end">
            <button
              onClick={() => {
                invoke("unpair", { fingerprint: target });
                select(null);
              }}
              className="text-xs text-faint underline-offset-2 hover:text-danger hover:underline"
            >
              Unpair {selected.info.alias}
            </button>
          </div>
        </>
      )}

      <footer className="mt-auto flex justify-end pt-10">
        <button onClick={() => invoke("start_sim")} className="text-xs text-faint underline-offset-2 hover:text-muted hover:underline">
          Developer: open phone simulator
        </button>
      </footer>

      {/* status toast while input is on the other device, or why it couldn't move */}
      {(kvm.active || kvm.error) && (
        <div
          role="status"
          className="fixed bottom-5 left-1/2 flex -translate-x-1/2 items-center gap-3 rounded-full bg-raised py-2 pl-2 pr-4 text-sm ring-1 ring-line"
          style={{ animation: "sheet-in 0.4s var(--ease-spring)" }}
        >
          {kvm.active && selected ? <Face seed={selected.info.fingerprint} size={26} mood="happy" /> : <Face seed="error" size={26} mood="sad" />}
          {kvm.active ? (
            <span>
              Using {selected?.info.alias ?? "the other device"} ·{" "}
              <kbd className="rounded-md bg-white/10 px-1.5 py-0.5 font-sans text-xs text-muted">Ctrl Alt Esc</kbd> to return
            </span>
          ) : (
            <span className="text-danger">Couldn't move over: {kvm.error}</span>
          )}
        </div>
      )}
    </>
  );
}

function ClientHome({ device, peers }: { device: Device; peers: Peer[] }) {
  const mains = peers.filter((p) => p.info.role === "main");
  const isPaired = (f: string) => device.paired.some((p) => p.fingerprint === f);
  const pairedMain = mains.find((m) => isPaired(m.info.fingerprint));

  return (
    <>
      <section className="flex flex-1 flex-col items-center justify-center py-10 text-center">
        <div className="rise">
          <Face seed={device.info.fingerprint} size={120} mood={pairedMain ? "happy" : "searching"} label={device.info.alias} />
        </div>
        <h1 className="rise mt-8 font-display text-[40px] font-extrabold leading-[0.95] tracking-[-0.04em]" style={{ "--i": 1 } as React.CSSProperties}>
          {pairedMain ? `Ready for ${pairedMain.info.alias}` : "Waiting for a Main"}
        </h1>
        <p className="rise mt-3 max-w-[42ch] text-[15px] leading-relaxed text-muted" style={{ "--i": 2 } as React.CSSProperties}>
          {pairedMain
            ? `Push the cursor off ${pairedMain.info.alias}'s glowing edge and it lands here.`
            : "On your main computer, open LOKA and click this face to pair. You'll confirm it here."}
        </p>
      </section>

      {mains.length > 0 && (
        <section aria-labelledby="mains" className="rise" style={{ "--i": 3 } as React.CSSProperties}>
          <h2 id="mains" className="text-xs font-medium text-faint">Main computers nearby</h2>
          <ul className="mt-2 divide-y divide-line rounded-[20px] bg-raised ring-1 ring-line">
            {mains.map((p) => (
              <li key={p.info.fingerprint} className="flex items-center gap-3 px-4 py-3">
                <Face seed={p.info.fingerprint} size={32} mood={isPaired(p.info.fingerprint) ? "happy" : "idle"} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-semibold">{p.info.alias}</span>
                  <span className="text-xs text-muted">
                    {osLabel(p.info.os)} · {isPaired(p.info.fingerprint) ? "Paired" : "Not paired"}
                  </span>
                </span>
                {isPaired(p.info.fingerprint) && (
                  <button onClick={() => invoke("unpair", { fingerprint: p.info.fingerprint })} className={quietButton}>
                    Unpair
                  </button>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
      <PairPrompt selfSeed={device.info.fingerprint} />
    </>
  );
}
