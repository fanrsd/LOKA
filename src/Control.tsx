import { useEffect, useState } from "react";
import { addPluginListener, invoke } from "@tauri-apps/api/core";
import { Face } from "./Face";
import { primaryButton, quietButton } from "./Pairing";

/** What LokaPlugin.kt reports about Wireless debugging and the injector. */
export type ControlStatus = { sdk: number; maker: string; devOptions: boolean; wirelessDebugging: boolean; paired: boolean; running: boolean };

const status = () => invoke<ControlStatus>("plugin:loka|status");
const openSettings = (page: "about" | "developer" | "wireless" | "security") => invoke("plugin:loka|openSettings", { page });

/** Start the injector (it dials back to Rust with the ticket). Rejects with a readable reason (a string). */
export async function startControl() {
  const ticket = await invoke<[number, string] | null>("injector_ticket");
  if (!ticket) throw "LOKA isn't ready yet";
  // Kotlin rejections arrive as objects
  await invoke("plugin:loka|start", { port: ticket[0], token: ticket[1] }).catch((e) => {
    throw typeof e === "string" ? e : (e?.message ?? JSON.stringify(e));
  });
}

/** Whether computers can drive this phone right now, and why not. */
export type Control = { status: ControlStatus | null; error: string | null; refresh: () => Promise<void>; retry: () => void };

/** Live status; `start` once at launch when the phone was set up before (Wireless debugging may need turning back on). */
export function useControl(): Control {
  const [s, setS] = useState<ControlStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const refresh = () => status().then(setS, () => setS(null));

  useEffect(() => {
    refresh().then(async () => {
      const now = await status().catch(() => null);
      if (now?.paired && !now.running) startControl().then(refresh, (e) => setError(String(e)));
    });
    const un = addPluginListener("loka", "injector", () => refresh());
    const back = () => document.visibilityState === "visible" && refresh();
    document.addEventListener("visibilitychange", back);
    return () => {
      un.then((l) => l.unregister());
      document.removeEventListener("visibilitychange", back);
    };
  }, []);

  const retry = () => {
    setError(null);
    startControl().then(refresh, (e) => setError(String(e)));
  };
  return { status: s, error, refresh, retry };
}

const XIAOMI = /xiaomi|redmi|poco/;

type Step = "intro" | "dev" | "wifi" | "old" | "code" | "mi" | "done";

/** The next thing to do, from what Android reports: the wizard moves on by itself when the user comes back. */
function next(s: ControlStatus, miDone: boolean): Step {
  if (!s.devOptions) return "dev";
  if (s.sdk < 30) return "old";
  if (!s.paired) return s.wirelessDebugging ? "code" : "wifi";
  if (XIAOMI.test(s.maker) && !miDone) return "mi";
  return "done";
}

/** One-time setup so computers can move into this phone: Developer options, Wireless debugging, pairing code. */
export function Wizard({ computer, onClose }: { computer: string; onClose: () => void }) {
  const [s, setS] = useState<ControlStatus | null>(null);
  const [started, setStarted] = useState(false);
  const [miDone, setMiDone] = useState(false);
  const [code, setCode] = useState("");
  const [codeError, setCodeError] = useState("");
  const [busy, setBusy] = useState(false);
  const [startError, setStartError] = useState("");

  useEffect(() => {
    const refresh = () => status().then(setS);
    refresh();
    // coming back from Settings: check again
    const tick = setInterval(() => document.visibilityState === "visible" && refresh(), 1000);
    const un = addPluginListener<{ ok: boolean; error?: string }>("loka", "pair", (r) => {
      setBusy(false);
      if (r.ok) refresh();
      else setCodeError(/code|auth|spake|password/i.test(r.error ?? "") ? "That code didn't work. Android shows a new code each time the window opens: use the newest one." : (r.error ?? "Pairing failed"));
    });
    return () => {
      clearInterval(tick);
      un.then((l) => l.unregister());
    };
  }, []);

  const step: Step = !s ? "intro" : !started ? "intro" : next(s, miDone);
  const steps: Step[] = ["dev", "wifi", "code"];
  const index = steps.indexOf(step === "old" ? "wifi" : step === "mi" ? "code" : step);

  useEffect(() => {
    if (step !== "done" || s?.running) return;
    startControl().then(
      () => status().then(setS),
      (e) => setStartError(String(e)),
    );
  }, [step]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!/^\d{6}$/.test(code)) return setCodeError("Enter all 6 digits.");
    setCodeError("");
    setBusy(true);
    invoke("plugin:loka|pairCode", { code });
  };

  const xiaomi = s ? XIAOMI.test(s.maker) : false;
  const samsung = s?.maker === "samsung";

  return (
    <main className="flex min-h-full flex-col px-5 pb-8 pt-12">
      <header className="flex items-center gap-2">
        <button onClick={onClose} aria-label="Back" className={quietButton + " grid h-11 w-11 place-items-center p-0 text-lg"}>
          ‹
        </button>
        {step !== "intro" && step !== "done" && index >= 0 && (
          <>
            <p className="text-sm font-semibold">Step {index + 1} of 3</p>
            <div className="ml-auto flex w-28 gap-1" aria-hidden>
              {steps.map((_, i) => (
                <span key={i} className={"h-1 flex-1 rounded-full " + (i <= index ? "bg-accent" : "bg-white/10")} />
              ))}
            </div>
          </>
        )}
      </header>

      {step === "intro" && (
        <Body
          title={`Let ${computer} use this phone`}
          text="Its mouse and keyboard work here: push its cursor past the screen edge and keep working on the phone. Android allows this only through a one-time setup in its own settings."
          face
        >
          <ul className="mt-6 space-y-3 text-sm text-muted">
            <li>1 · Turn on Developer options</li>
            <li>2 · Turn on Wireless debugging</li>
            <li>3 · Pair LOKA with a code</li>
          </ul>
          <Primary onClick={() => setStarted(true)}>Start setup</Primary>
        </Body>
      )}

      {step === "dev" && (
        <Body title="Turn on Developer options" text="">
          <ol className="mt-4 space-y-3 text-[15px]">
            <li>
              Open <b>Settings › About phone</b>
              {samsung && " › Software information"}
            </li>
            <li>
              Tap <b>{xiaomi ? "OS version (MIUI or HyperOS)" : "Build number"}</b> 7 times
            </li>
            <li>Come back here when it says "You are now a developer"</li>
          </ol>
          <Waiting>Waiting for Developer options. LOKA moves on by itself.</Waiting>
          <Primary onClick={() => openSettings("about")}>Open About phone</Primary>
        </Body>
      )}

      {step === "old" && (
        <Body
          title="This phone can't be controlled"
          text={`Wireless debugging arrived in Android 11; this phone runs an older Android. You can still use it as ${computer}'s touchpad and send files.`}
        >
          <Primary onClick={onClose}>OK</Primary>
        </Body>
      )}

      {step === "wifi" && (
        <Body title="Turn on Wireless debugging" text="">
          <ol className="mt-4 space-y-3 text-[15px]">
            <li>
              In <b>Developer options</b>, find <b>Wireless debugging</b>
            </li>
            <li>Turn it on and allow it on this Wi-Fi</li>
          </ol>
          <Waiting>Waiting for Wireless debugging.</Waiting>
          <Primary onClick={() => openSettings("wireless")}>Open Developer options</Primary>
        </Body>
      )}

      {step === "code" && (
        <Body title="Enter the pairing code" text="">
          <ol className="mt-4 space-y-3 text-[15px]">
            <li>
              In Wireless debugging, tap <b>Pair device with pairing code</b>
            </li>
            <li>
              Pull down the notification shade and type the 6 digits into <b>LOKA's notification</b>: the code window closes if you switch apps
            </li>
          </ol>
          <form onSubmit={submit} className="mt-6">
            <label htmlFor="adb-code" className="text-xs text-muted">
              Or type it here (split screen)
            </label>
            <input
              id="adb-code"
              value={code}
              onChange={(e) => setCode(e.target.value.replace(/\D/g, "").slice(0, 6))}
              inputMode="numeric"
              autoComplete="one-time-code"
              placeholder="123456"
              aria-invalid={!!codeError}
              aria-describedby={codeError ? "adb-code-error" : undefined}
              className="mt-2 w-full rounded-2xl bg-raised px-4 py-3 text-center font-mono text-2xl tracking-[0.3em] text-fg ring-1 ring-line placeholder:text-faint focus-visible:outline-2 focus-visible:outline-accent"
            />
            {codeError && (
              <p id="adb-code-error" role="alert" className="mt-2 text-sm text-danger">
                {codeError}
              </p>
            )}
            <button type="submit" disabled={busy} className={quietButton + " mt-2 min-h-11 w-full ring-1 ring-line"}>
              {busy ? "Pairing…" : "Pair"}
            </button>
          </form>
          <Primary
            onClick={async () => {
              await invoke("plugin:loka|askCode");
              openSettings("wireless");
            }}
          >
            Open Wireless debugging
          </Primary>
        </Body>
      )}

      {step === "mi" && (
        <Body
          title="One more switch on Xiaomi"
          text="MIUI and HyperOS block typing and clicks from LOKA until this is on."
        >
          <ol className="mt-4 space-y-3 text-[15px]">
            <li>
              In Developer options, turn on <b>USB debugging (Security settings)</b>
            </li>
            <li>Xiaomi may ask you to sign in to a Mi account first</li>
          </ol>
          <Primary onClick={() => openSettings("developer")}>Open Developer options</Primary>
          <button onClick={() => setMiDone(true)} className={quietButton + " mt-2 min-h-11 w-full"}>
            It's on
          </button>
        </Body>
      )}

      {step === "done" && (
        <Body
          title={startError ? "Almost there" : "Ready"}
          text={startError || `Push ${computer}'s cursor past the edge where this phone sits in its layout. A cursor shows up here.`}
          face
        >
          {startError ? (
            <Primary onClick={() => startControl().then(() => setStartError(""), (e) => setStartError(String(e)))}>Try again</Primary>
          ) : (
            <>
              <p className="mt-3 text-center text-sm text-faint">LOKA keeps Wireless debugging on, also after a restart.</p>
              <Primary onClick={onClose}>Done</Primary>
            </>
          )}
        </Body>
      )}
    </main>
  );
}

function Body({ title, text, face, children }: { title: string; text: string; face?: boolean; children: React.ReactNode }) {
  return (
    <section className={"flex flex-1 flex-col " + (face ? "items-center justify-center text-center" : "mt-6")}>
      {face && (
        <div className="mb-6 flex gap-4" aria-hidden>
          <Face seed="computer" size={56} look="right" mood="happy" />
          <Face seed="phone" size={56} look="left" mood="happy" />
        </div>
      )}
      <h1 className="font-display text-[30px] font-extrabold leading-[1.05] tracking-[-0.035em]">{title}</h1>
      {text && <p className="mt-3 max-w-[34ch] leading-relaxed text-muted">{text}</p>}
      <div className={"flex w-full flex-1 flex-col " + (face ? "" : "")}>{children}</div>
    </section>
  );
}

function Waiting({ children }: { children: React.ReactNode }) {
  return (
    <p role="status" className="mt-6 flex items-center gap-3 rounded-2xl bg-raised p-4 text-sm text-muted ring-1 ring-line">
      <span aria-hidden className="h-4 w-4 shrink-0 animate-spin rounded-full border-2 border-white/20 border-t-accent" />
      {children}
    </p>
  );
}

function Primary({ onClick, children }: { onClick: () => void; children: React.ReactNode }) {
  return (
    <button onClick={onClick} className={primaryButton + " mt-auto min-h-12 w-full"}>
      {children}
    </button>
  );
}
