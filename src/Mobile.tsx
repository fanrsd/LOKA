import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Wizard, useControl, type Control } from "./Control";
import { Face, type Mood } from "./Face";
import { AddHost } from "./Home";
import { ConnectionCheck, PairPrompt, StateBadge, deviceState, osLabel, primaryButton, quietButton, stateReason, type Device, type PairedDevice, type Peer } from "./Pairing";
import { REACH, opposite, type Edge, type Portal } from "./calibration";
import { About, useUpdate, type Update } from "./About";
import { UpdatePrompt } from "./UpdatePrompt";

type PairAnswer = "accepted" | "declined" | "noAnswer" | "unknown";
type Pairing = { state: "waiting"; code?: string } | { state: "failed"; note: string };
const FAILED: Record<Exclude<PairAnswer, "accepted">, string> = { declined: "Declined", noAnswer: "No answer", unknown: "It can't see this phone" };

/** The phone app: computers nearby, pairing, and the touchpad that drives a paired one. */
export function Mobile() {
  const [device, setDevice] = useState<Device | null>(null);
  const [peers, setPeers] = useState<Peer[]>([]);
  const [pairing, setPairing] = useState<Record<string, Pairing>>({});
  const [driving, setDriving] = useState<Peer | null>(null);
  const [screen, setScreen] = useState<"home" | "wizard" | "settings">("home");
  const [about, setAbout] = useState(false);
  // a computer was just paired: point at the setup that lets it move in
  const [fresh, setFresh] = useState(false);
  const pairedCount = useRef(-1);
  const [note, setNote] = useState("");
  const control = useControl();
  // once per launch, as on computers; phones can't update in place, so Download fetches the APK
  const [prompt, setPrompt] = useState(true);
  const update = useUpdate();
  const apk = update.available && `https://github.com/fanrsd/LOKA/releases/download/${update.available.tag}/LOKA_${update.available.tag.slice(1)}_android.apk`;
  const files = useRef<HTMLInputElement>(null);
  const sendTo = useRef("");
  const refreshDevice = () =>
    invoke<Device>("get_device").then((d) => {
      if (pairedCount.current >= 0 && d.paired.length > pairedCount.current) setFresh(true);
      pairedCount.current = d.paired.length;
      setDevice(d);
    });

  useEffect(() => {
    const refreshPeers = () => invoke<Peer[]>("peers").then(setPeers);
    refreshDevice();
    const unPeer = listen("peer", refreshPeers);
    unPeer.then(refreshPeers);
    const unPaired = listen("paired", refreshDevice);
    const unDevice = listen("device", refreshDevice);
    const unCode = listen<{ fingerprint: string; code: string }>("pair-code", ({ payload: { fingerprint: f, code } }) =>
      setPairing((s) => (s[f]?.state === "waiting" ? { ...s, [f]: { state: "waiting", code } } : s)),
    );
    const prune = setInterval(refreshPeers, 5000); // devices that went quiet drop off
    return () => {
      for (const un of [unPeer, unPaired, unDevice, unCode]) un.then((f) => f());
      clearInterval(prune);
    };
  }, []);

  if (!device) return null;
  if (driving) return <Touchpad peer={driving} onClose={() => setDriving(null)} />;
  const first = device.paired[0]?.alias ?? "your computer";
  if (screen === "wizard")
    return (
      <Wizard
        computer={first}
        onClose={() => {
          setScreen("home");
          control.refresh();
        }}
      />
    );
  if (screen === "settings")
    return (
      <>
        <Settings device={device} control={control} update={update} onSetup={() => setScreen("wizard")} onAbout={() => setAbout(true)} onClose={() => setScreen("home")} onChange={refreshDevice} />
        {about && (
          <About
            update={update}
            seed={device.info.fingerprint}
            os={device.info.os}
            downloadUrl={apk}
            extra={`Phone control: ${control.status ? JSON.stringify(control.status) : "unavailable"}`}
            onClose={() => setAbout(false)}
            onUpdate={() => setAbout(false)}
          />
        )}
      </>
    );
  const controlCard = device.paired.length > 0 && (
    <ControlCard
      device={device}
      computer={device.paired[0]}
      control={control}
      fresh={fresh}
      onSetup={() => {
        setFresh(false);
        setScreen("wizard");
      }}
      onChange={refreshDevice}
    />
  );
  const running = !!control.status?.running;

  /** Files picked with Android's picker go one by one; the web view gets their bytes, not paths. */
  const send = async (list: FileList | null) => {
    const fingerprint = sendTo.current;
    for (const f of Array.from(list ?? [])) {
      setNote(`Sending ${f.name}…`);
      try {
        // data URL → its base64 part (FileReader encodes natively, unlike a JS loop)
        // the project's TS lib predates Promise.withResolvers
        const data = await new Promise<string>((resolve, reject) => {
          const r = new FileReader();
          r.onload = () => resolve(String(r.result).split(",", 2)[1] ?? "");
          r.onerror = () => reject(r.error);
          r.readAsDataURL(f);
        });
        await invoke("send_upload", { fingerprint, name: f.name, data });
        setNote(`Sent ${f.name}`);
      } catch (e) {
        setNote(`Couldn't send ${f.name}: ${e}`);
      }
    }
  };

  const isPaired = (f: string) => device.paired.some((p) => p.fingerprint === f);
  const outdated = (p: Peer) => deviceState(p, false, device.info) === "update";
  const nearby = peers.filter((p) => !isPaired(p.info.fingerprint));

  const pair = async (p: Peer) => {
    const f = p.info.fingerprint;
    setPairing((s) => ({ ...s, [f]: { state: "waiting" } }));
    try {
      const answer = await invoke<PairAnswer>("pair", { fingerprint: f });
      setPairing(({ [f]: _, ...rest }) => (answer === "accepted" ? rest : { ...rest, [f]: { state: "failed", note: FAILED[answer] } }));
    } catch (e) {
      setPairing((s) => ({ ...s, [f]: { state: "failed", note: String(e) } }));
    }
  };

  return (
    <main className="min-h-full px-5 pb-10 pt-14">
      <header className="flex items-center gap-3">
        <Face seed={device.info.fingerprint} size={40} />
        <div className="min-w-0 flex-1">
          <p className="truncate font-semibold">{device.info.alias}</p>
          <p className="text-xs text-muted">Visible on this Wi-Fi</p>
        </div>
        <button onClick={() => setScreen("settings")} aria-label="Settings" className={quietButton + " grid h-11 w-11 place-items-center p-0"}>
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" aria-hidden>
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" />
          </svg>
        </button>
      </header>

      {!running && controlCard}

      <h1 className="mt-8 font-display text-[32px] font-extrabold leading-none tracking-[-0.035em]">Your computers</h1>

      {device.paired.length === 0 && nearby.length === 0 && (
        <div className="mt-10 flex flex-col items-center text-center">
          <Face seed={device.info.fingerprint} size={72} mood="searching" />
          <p className="mt-6 font-semibold">Looking for computers</p>
          <p className="mt-1 max-w-[30ch] text-sm leading-relaxed text-muted">Open LOKA on your computer, on the same Wi-Fi as this phone.</p>
        </div>
      )}

      <ul className="mt-6 space-y-3">
        {device.paired.map((d, i) => {
          const peer = peers.find((p) => p.info.fingerprint === d.fingerprint);
          const state = deviceState(peer, true, device.info);
          const usable = peer && state !== "update";
          const reason = stateReason(state, d.alias, peer?.info.os);
          return (
            <li key={d.fingerprint} className="rise rounded-[24px] bg-raised p-4 ring-1 ring-line" style={{ "--i": i } as React.CSSProperties}>
              <div className="flex items-center gap-3">
                <Face seed={d.fingerprint} size={44} mood={state === "ready" ? "idle" : "sad"} />
                <div className="min-w-0 flex-1">
                  <p className="truncate font-semibold">{d.alias}</p>
                  <p className="text-xs text-muted">
                    {!peer ? "Not on this Wi-Fi" : state === "update" ? osLabel(peer.info.os) : <><span className="text-accent">Online</span> · {osLabel(peer.info.os)}</>}
                  </p>
                </div>
                <StateBadge state={state} />
              </div>
              {reason && <p className="mt-3 text-sm leading-relaxed text-muted">{reason}.</p>}
              <div className="mt-4 flex gap-2">
                <button disabled={state !== "ready"} onClick={() => peer && setDriving(peer)} className={primaryButton + " min-h-11 flex-1"}>
                  Use as touchpad
                </button>
                <button
                  disabled={!usable}
                  onClick={() => {
                    sendTo.current = d.fingerprint;
                    files.current?.click();
                  }}
                  className={quietButton + " min-h-11 ring-1 ring-line disabled:opacity-50"}
                >
                  Send files
                </button>
              </div>
              <div className="mt-3">
                <ConnectionCheck fingerprint={d.fingerprint} />
              </div>
            </li>
          );
        })}
      </ul>
      <input ref={files} type="file" multiple hidden onChange={(e) => send(e.currentTarget.files).finally(() => (e.target.value = ""))} />
      {note && (
        <p role="status" className="mt-3 px-1 text-sm text-muted">
          {note}
        </p>
      )}

      {running && controlCard}

      {nearby.length > 0 && (
        <>
          <p className="mt-8 px-1 text-xs text-muted">Nearby, not paired</p>
          <ul className="mt-2 space-y-2">
            {nearby.map((p) => {
              const s = pairing[p.info.fingerprint];
              const mood: Mood = s?.state === "waiting" ? "waiting" : s ? "sad" : "idle";
              const note = outdated(p) ? "Needs LOKA update" : s?.state === "waiting" ? (s.code ? `Code ${s.code}: check it matches` : "Asking…") : s ? s.note : osLabel(p.info.os);
              return (
                <li key={p.info.fingerprint} className="flex items-center gap-3 rounded-[20px] bg-raised p-3 pl-4 ring-1 ring-line">
                  <Face seed={p.info.fingerprint} size={36} mood={mood} />
                  <div className="min-w-0 flex-1">
                    <p className="truncate font-semibold">{p.info.alias}</p>
                    <p className={"text-xs " + (s?.state === "failed" ? "text-danger" : "text-muted")}>{note}</p>
                  </div>
                  <button disabled={outdated(p) || s?.state === "waiting"} onClick={() => pair(p)} className={quietButton + " min-h-11 ring-1 ring-line disabled:opacity-50"}>
                    Pair
                  </button>
                </li>
              );
            })}
          </ul>
        </>
      )}

      <AddHost hosts={device.hosts} ips={device.ips} onChange={refreshDevice} />
      <PairPrompt selfSeed={device.info.fingerprint} />
      {prompt && update.available && <UpdatePrompt update={update} seed={device.info.fingerprint} downloadUrl={apk} onClose={() => setPrompt(false)} />}
    </main>
  );
}

const SIDES: Edge[] = ["left", "top", "bottom", "right"];

/** Whether paired computers can move into this phone, which side of the computer it sits on, and the way to set it up.
 *  `fresh`: a computer was just paired, so the setup is what to do next. */
function ControlCard({ device, computer, control, fresh, onSetup, onChange }: { device: Device; computer: PairedDevice; control: Control; fresh: boolean; onSetup: () => void; onChange: () => void }) {
  const [placeNote, setPlaceNote] = useState("");
  const setup = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!fresh) return;
    setup.current?.scrollIntoView({ block: "center", behavior: "smooth" });
    setup.current?.focus({ preventScroll: true });
  }, [fresh, control.status?.running]);
  const s = control.status;
  if (!s) return null;
  const { alias, fingerprint } = computer;
  // the phone's portal edge faces the computer: sitting left of it, the computer is off the phone's right edge
  const edge = device.portals[fingerprint]?.edge;
  const side = edge ? opposite[edge] : null;
  const place = async (where: Edge) => {
    const monitor = { x: 0, y: 0, w: Math.round(screen.width * devicePixelRatio), h: Math.round(screen.height * devicePixelRatio) };
    const portal: Portal = { edge: opposite[where], monitor, start: 0, end: 1, reach: REACH };
    try {
      setPlaceNote((await invoke<string | null>("set_portal", { fingerprint, portal, sync: true })) ?? "");
    } catch (e) {
      setPlaceNote(String(e));
    }
    onChange();
  };
  if (s.running)
    return (
      <div className="mt-6 rounded-[24px] bg-raised p-4 ring-1 ring-line">
        <p className="text-sm text-muted">
          <span className="font-semibold text-fg">{alias} can move into this phone.</span> Push its cursor past the side this phone sits on.
        </p>
        <p id="side-label" className="mt-3 text-xs text-muted">
          This phone sits on {alias}'s
        </p>
        <div className="mt-2 flex flex-wrap gap-2" role="group" aria-labelledby="side-label">
          {SIDES.map((w) => (
            <button
              key={w}
              aria-pressed={side === w}
              onClick={() => place(w)}
              className={"min-h-11 rounded-full px-4 text-sm capitalize ring-1 ring-line transition-colors " + (side === w ? "bg-fg font-semibold text-ink" : "text-muted")}
            >
              {w}
            </button>
          ))}
        </div>
        <p role="status" className="mt-2 min-h-4 text-xs text-muted">
          {placeNote}
        </p>
        <Battery control={control} />
      </div>
    );
  return (
    <div className={"mt-6 rounded-[24px] bg-raised p-4 ring-1 transition-shadow " + (fresh ? "ring-2 ring-accent" : "ring-line")}>
      <p className="font-semibold">{s.paired ? `${alias} can't move into this phone right now` : `${alias} can't move into this phone yet`}</p>
      <p className={"mt-1 text-sm leading-relaxed " + (control.error ? "text-danger" : "text-muted")}>
        {s.paired
          ? (control.error ?? "Starting…")
          : "Touchpad and Send files already work. To let its cursor move in past the screen edge, do the one-time setup in Android settings."}
      </p>
      <button ref={setup} onClick={s.paired && control.error ? control.retry : onSetup} className={(fresh ? primaryButton : quietButton + " text-fg ring-1 ring-line") + " mt-3 min-h-11"}>
        {s.paired && control.error ? "Try again" : "Set up"}
      </button>
      <Battery control={control} />
    </div>
  );
}

/** Android's battery optimisation may stop LOKA in the background, and with it every way in. */
function Battery({ control, className = "mt-3 border-t border-line pt-3" }: { control: Control; className?: string }) {
  if (!control.status || control.status.batteryExempt) return null;
  // back from Android's dialog, useControl's visibilitychange refresh picks up the answer
  const allow = () => invoke("plugin:loka|exemptBattery").then(control.refresh, () => {});
  return (
    <div className={"flex items-center gap-3 " + className}>
      <p className="flex-1 text-sm text-muted">Android may close LOKA in the background</p>
      <button onClick={allow} className={quietButton + " min-h-11 text-fg ring-1 ring-line"}>
        Allow
      </button>
    </div>
  );
}

function Settings({ device, control, update, onSetup, onAbout, onClose, onChange }: {
  device: Device;
  control: Control;
  update: Update;
  onSetup: () => void;
  onAbout: () => void;
  onClose: () => void;
  onChange: () => void;
}) {
  const s = control.status;
  return (
    <main className="min-h-full px-5 pb-10 pt-12">
      <header className="flex items-center gap-2">
        <button onClick={onClose} aria-label="Back" className={quietButton + " grid h-11 w-11 place-items-center p-0 text-lg"}>
          ‹
        </button>
        <h1 className="font-semibold">Settings</h1>
      </header>

      <p className="mt-6 px-1 text-xs text-muted">Computers controlling this phone</p>
      <div className="mt-2 flex items-center gap-3 rounded-[20px] bg-raised p-4 ring-1 ring-line">
        <p className="flex-1 text-sm">
          {!s ? "Not available" : s.running ? "On: paired computers can move in" : s.paired ? (control.error ?? "Starting…") : "Not set up"}
        </p>
        <button onClick={onSetup} className={quietButton + " min-h-11 ring-1 ring-line"}>
          {s?.paired ? "Set up again" : "Set up"}
        </button>
      </div>
      <Battery control={control} className="mt-2 rounded-[20px] bg-raised p-4 ring-1 ring-line" />

      <p className="mt-6 px-1 text-xs text-muted">Received files</p>
      <p className="mt-2 break-all rounded-[20px] bg-raised p-4 font-mono text-xs text-muted ring-1 ring-line">{device.received}</p>

      <p className="mt-6 px-1 text-xs text-muted">Paired</p>
      {device.paired.length === 0 ? (
        <p className="mt-2 px-1 text-sm text-faint">No computers yet.</p>
      ) : (
        <ul className="mt-2 divide-y divide-line rounded-[20px] bg-raised ring-1 ring-line">
          {device.paired.map((d) => (
            <li key={d.fingerprint} className="flex items-center gap-3 p-3 pl-4">
              <Face seed={d.fingerprint} size={32} />
              <p className="min-w-0 flex-1 truncate">{d.alias}</p>
              <button onClick={() => invoke("unpair", { fingerprint: d.fingerprint }).then(onChange)} className={quietButton + " min-h-11"}>
                Unpair
              </button>
            </li>
          ))}
        </ul>
      )}

      <button
        onClick={onAbout}
        className="mt-6 flex min-h-11 w-full items-center gap-3 rounded-[20px] bg-raised p-4 text-left ring-1 ring-line transition-colors hover:bg-white/[0.04] focus-visible:outline-2 focus-visible:outline-accent"
      >
        <span className="flex-1 text-sm font-medium">About LOKA</span>
        {update.available && <span aria-hidden className="h-2 w-2 rounded-full bg-accent" />}
        <span className="text-xs text-muted">{update.available ? `${update.available.name} available` : update.current ? `Version ${update.current}` : ""}</span>
      </button>
    </main>
  );
}

// PC/AT set-1 scancodes of the US layout, row by row; the shifted characters sit at the same index
const PLAIN = "1234567890-=qwertyuiop[]asdfghjkl;'`\\zxcvbnm,./";
const SHIFTED = '!@#$%^&*()_+QWERTYUIOP{}ASDFGHJKL:"~|ZXCVBNM<>?';
const ROW_START = [0x02, 0x10, 0x1e, 0x2b];
const SHIFT = 0x2a;
const ENTER = 0x1c;
const BACKSPACE = 0x0e;

/** The key (and whether Shift is needed) that types `ch`. */
function scanOf(ch: string): [number, boolean] | null {
  if (ch === " ") return [0x39, false];
  if (ch === "\n") return [ENTER, false];
  for (const [chars, shift] of [[PLAIN, false], [SHIFTED, true]] as const) {
    const i = chars.indexOf(ch);
    if (i >= 0) return [ROW_START[Math.floor(i / 12)] + (i % 12), shift];
  }
  return null;
}

type Input =
  | { type: "move"; dx: number; dy: number }
  | { type: "button"; button: number; down: boolean }
  | { type: "wheel"; dx: number; dy: number }
  | { type: "key"; scan: number; down: boolean }
  | { type: "text"; text: string };
const send = (event: Input) => invoke("touch", { event });
const tapKey = (scan: number, shift = false) => {
  if (shift) send({ type: "key", scan: SHIFT, down: true });
  send({ type: "key", scan, down: true });
  send({ type: "key", scan, down: false });
  if (shift) send({ type: "key", scan: SHIFT, down: false });
};
const click = (button: number) => {
  send({ type: "button", button, down: true });
  send({ type: "button", button, down: false });
};
/** Types `text` on the computer: as text when it takes any character (its own layout can't garble it), else as US keys. Line breaks are Enter. */
const typeText = (text: string, asText: boolean) =>
  text.split("\n").forEach((line, i) => {
    if (i) tapKey(ENTER);
    if (!line) return;
    if (asText) send({ type: "text", text: line });
    else
      for (const ch of line) {
        const k = scanOf(ch);
        if (k) tapKey(...k);
      }
  });

/** Cursor px per CSS px of finger travel. */
const SPEED = 2.5;
/** Finger travel (CSS px) per wheel notch. */
const NOTCH = 30;

type Session = { state: "connecting" } | { state: "on" } | { state: "back" } | { state: "error"; message: string };

/** The phone as touchpad and keyboard of computer `peer`: one finger moves, two scroll, a tap clicks (two fingers: right click). */
function Touchpad({ peer, onClose }: { peer: Peer; onClose: () => void }) {
  const [session, setSession] = useState<Session>({ state: "connecting" });
  const [typing, setTyping] = useState(false);
  const keys = useRef<HTMLTextAreaElement>(null);
  /** what the box held after the last input */
  const box = useRef(" ");
  const fingers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef({ start: 0, moved: 0, most: 0, rest: { x: 0, y: 0, wheel: 0 } });
  const fp = peer.info.fingerprint;

  const start = () => {
    setSession({ state: "connecting" });
    invoke("control", { fingerprint: fp }).catch((e) => setSession({ state: "error", message: String(e) }));
  };

  useEffect(() => {
    const un = listen<{ active: boolean; error?: string | null }>("kvm", ({ payload: e }) =>
      setSession(e.active ? { state: "on" } : e.error ? { state: "error", message: e.error } : { state: "back" }),
    );
    un.then(start);
    return () => {
      un.then((f) => f());
      invoke("disconnect");
    };
  }, [fp]);

  const down = (e: React.PointerEvent) => {
    e.currentTarget.setPointerCapture(e.pointerId);
    const f = fingers.current;
    if (f.size === 0) gesture.current = { start: e.timeStamp, moved: 0, most: 0, rest: { x: 0, y: 0, wheel: 0 } };
    f.set(e.pointerId, { x: e.clientX, y: e.clientY });
    gesture.current.most = Math.max(gesture.current.most, f.size);
  };

  const move = (e: React.PointerEvent) => {
    const f = fingers.current;
    const last = f.get(e.pointerId);
    if (!last) return;
    const [dx, dy] = [e.clientX - last.x, e.clientY - last.y];
    f.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const g = gesture.current;
    g.moved += Math.hypot(dx, dy);
    const r = g.rest;
    if (f.size === 1) {
      // sub-pixel rests carry over so slow strokes still move
      r.x += dx * SPEED;
      r.y += dy * SPEED;
      const [mx, my] = [Math.trunc(r.x), Math.trunc(r.y)];
      r.x -= mx;
      r.y -= my;
      if (mx || my) send({ type: "move", dx: mx, dy: my });
    } else if (e.pointerId === f.keys().next().value) {
      // two fingers scroll, natural direction (content follows the fingers); one finger's motion is enough
      r.wheel += (dy / NOTCH) * 120;
      const w = Math.trunc(r.wheel);
      r.wheel -= w;
      if (w) send({ type: "wheel", dx: 0, dy: w });
    }
  };

  const up = (e: React.PointerEvent) => {
    const f = fingers.current;
    f.delete(e.pointerId);
    const g = gesture.current;
    if (f.size === 0 && g.moved < 12 && e.timeStamp - g.start < 250) click(g.most >= 2 ? 2 : 1);
  };

  const typed = (e: React.FormEvent<HTMLTextAreaElement>) => {
    // the box starts as one space, so Backspace still shows on an "empty" box. What changed since the last input
    // goes out: removed characters as Backspace, new ones as typed text (so IME words, autocorrect and emoji work)
    const t = e.currentTarget;
    const [v, old] = [t.value, box.current];
    let p = 0;
    while (p < old.length && p < v.length && old[p] === v[p]) p++;
    if (p > 0 && /[\uD800-\uDBFF]/.test(old[p - 1])) p--; // never half an emoji
    for (const _ of old.slice(p)) tapKey(BACKSPACE);
    typeText(v.slice(p), peer.info.text === true);
    // a word still being composed stays in the box, so the keyboard can keep changing it
    if ((e.nativeEvent as InputEvent).isComposing) box.current = v;
    else t.value = box.current = " ";
  };

  const toggleKeys = () => {
    const t = keys.current!;
    if (typing) t.blur();
    else {
      t.value = box.current = " ";
      t.focus();
    }
    setTyping(!typing);
  };

  const hold = (button: number) => ({
    onPointerDown: () => send({ type: "button", button, down: true }),
    onPointerUp: () => send({ type: "button", button, down: false }),
    onPointerCancel: () => send({ type: "button", button, down: false }),
  });

  const status =
    session.state === "on" ? null : session.state === "connecting" ? "Connecting…" : session.state === "back" ? `${peer.info.alias} took its cursor back` : session.message;

  return (
    <main className="flex h-full flex-col px-4 pb-6 pt-12">
      <header className="flex items-center gap-2">
        <button onClick={onClose} aria-label="Back" className={quietButton + " grid h-11 w-11 place-items-center p-0 text-lg"}>
          ‹
        </button>
        <Face seed={fp} size={28} mood={session.state === "on" ? "happy" : session.state === "connecting" ? "waiting" : "sad"} />
        <p className="truncate font-semibold">{peer.info.alias}</p>
      </header>

      <div
        onPointerDown={down}
        onPointerMove={move}
        onPointerUp={up}
        onPointerCancel={up}
        className="relative mt-3 grid flex-1 touch-none select-none place-items-center rounded-[28px] bg-raised ring-1 ring-line"
      >
        {status ? (
          <div className="px-8 text-center" role="status">
            <p className={session.state === "error" ? "text-danger" : "text-muted"}>{status}</p>
            {session.state !== "connecting" && (
              <button onPointerDown={(e) => e.stopPropagation()} onClick={start} className={primaryButton + " mt-4 min-h-11"}>
                Control again
              </button>
            )}
          </div>
        ) : (
          <p className="pointer-events-none text-center text-sm leading-relaxed text-faint">
            One finger moves
            <br />
            Two fingers scroll
            <br />
            Tap to click
          </p>
        )}
      </div>

      <div className="mt-3 grid grid-cols-2 gap-3">
        <button {...hold(1)} className="min-h-14 touch-none rounded-[20px] bg-raised text-sm text-muted ring-1 ring-line active:bg-white/10">
          Left
        </button>
        <button {...hold(2)} className="min-h-14 touch-none rounded-[20px] bg-raised text-sm text-muted ring-1 ring-line active:bg-white/10">
          Right
        </button>
      </div>
      <div className="mt-2 flex justify-center gap-2">
        <button onClick={toggleKeys} aria-pressed={typing} className={quietButton + " min-h-11 aria-pressed:text-fg"}>
          Keyboard
        </button>
        <button onClick={onClose} className={quietButton + " min-h-11"}>
          Stop
        </button>
      </div>
      {/* off-screen box the Android keyboard types into; its text goes out as key presses */}
      <textarea
        ref={keys}
        onInput={typed}
        onBlur={() => setTyping(false)}
        aria-label="Type on the computer"
        autoCapitalize="off"
        autoCorrect="off"
        spellCheck={false}
        className="fixed -left-[200vw] top-1/2 h-10 w-40 opacity-0"
      />
    </main>
  );
}
