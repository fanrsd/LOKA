import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { availableMonitors, type Monitor } from "@tauri-apps/api/window";
import { type Edge, type Portal, REACH } from "./calibration";
import { type Look, MOTIONS, SWATCHES, loadLook, saveLook } from "./look";

const EDGES: Edge[] = ["left", "top", "bottom", "right"];
const MAP_W = 560, MAP_H = 240;

type Calibration = { monitor: number; edge: Edge; start: number; length: number };
const DEFAULT: Calibration = { monitor: 0, edge: "right", start: 0.35, length: 0.3 };

/** Where on this computer's screens device `fingerprint` sits; every change moves its real portal live. */
export function Calibrate({ fingerprint, alias, saved }: { fingerprint: string; alias: string; saved?: Portal }) {
  const [monitors, setMonitors] = useState<Monitor[]>([]);
  // null until the monitors are known, so the saved portal maps onto the right screen
  const [cal, setCal] = useState<Calibration | null>(null);
  const update = (patch: Partial<Calibration>) => setCal((c) => c && { ...c, ...patch });
  const [look, setLook] = useState<Look>(loadLook);
  const restyle = (patch: Partial<Look>) => {
    const next = { ...look, ...patch };
    setLook(next);
    saveLook(next);
  };

  useEffect(() => {
    availableMonitors().then((ms) => {
      setMonitors(ms);
      const i = saved ? ms.findIndex((m) => m.position.x === saved.monitor.x && m.position.y === saved.monitor.y) : -1;
      setCal(saved && i >= 0 ? { monitor: i, edge: saved.edge, start: saved.start, length: saved.end - saved.start } : DEFAULT);
    });
    // another device chosen: start from its own portal
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fingerprint]);

  const mon = cal && (monitors[cal.monitor] ?? monitors[0]);

  // live-apply: the real overlay jumps to the new segment while the bar is dragged
  useEffect(() => {
    if (!cal || !mon) return;
    const portal: Portal = {
      edge: cal.edge,
      monitor: { x: mon.position.x, y: mon.position.y, w: mon.size.width, h: mon.size.height },
      start: cal.start,
      end: cal.start + cal.length,
      reach: REACH,
    };
    invoke("set_portal", { fingerprint, portal });
  }, [cal, mon, fingerprint]);

  if (!cal) return null;

  // fit every monitor into the map box
  const minX = Math.min(...monitors.map((m) => m.position.x));
  const minY = Math.min(...monitors.map((m) => m.position.y));
  const maxX = Math.max(...monitors.map((m) => m.position.x + m.size.width));
  const maxY = Math.max(...monitors.map((m) => m.position.y + m.size.height));
  const k = monitors.length ? Math.min(MAP_W / (maxX - minX), MAP_H / (maxY - minY)) : 1;

  return (
    <section aria-labelledby="cal-title" className="rise mt-12 rounded-[28px] bg-raised p-7 ring-1 ring-line" style={{ "--i": 3 } as React.CSSProperties}>
      <h2 id="cal-title" className="font-display text-2xl font-bold tracking-[-0.025em]">
        Where does {alias} sit?
      </h2>
      <p className="mt-1.5 max-w-[56ch] text-sm leading-relaxed text-muted">
        Pick the screen and the edge it's next to, then slide the glowing bar until it lines up with the device on your desk.
      </p>

      {/* box shrinks to the fitted layout so mx-auto centres the screens themselves */}
      <div className="relative mx-auto mt-8" style={{ width: monitors.length ? (maxX - minX) * k : MAP_W, height: monitors.length ? (maxY - minY) * k : MAP_H }}>
        {monitors.map((m, i) => (
          <button
            key={i}
            aria-label={`Screen ${i + 1}${m.name ? `, ${m.name}` : ""}`}
            aria-pressed={i === cal.monitor}
            onClick={() => update({ monitor: i })}
            className={`absolute grid place-items-center rounded-[14px] font-display text-sm font-semibold transition-[background-color,box-shadow,color] duration-300 focus-visible:outline-2 focus-visible:outline-accent ${i === cal.monitor ? "bg-white/[0.07] text-fg ring-1 ring-white/25" : "bg-white/[0.03] text-faint ring-1 ring-line hover:bg-white/[0.05]"}`}
            style={{ left: (m.position.x - minX) * k, top: (m.position.y - minY) * k, width: m.size.width * k - 6, height: m.size.height * k - 6 }}
          >
            {i + 1}
          </button>
        ))}
        {mon && (
          <EdgeSlider
            edge={cal.edge}
            box={{ left: (mon.position.x - minX) * k, top: (mon.position.y - minY) * k, width: mon.size.width * k - 6, height: mon.size.height * k - 6 }}
            start={cal.start}
            length={cal.length}
            onChange={(start) => update({ start })}
          />
        )}
      </div>

      <div className="mt-8 grid grid-cols-[6rem_1fr] items-center gap-x-4 gap-y-4 text-sm">
        <span id="edge-label" className="text-muted">Edge</span>
        <div role="radiogroup" aria-labelledby="edge-label" className="flex flex-wrap gap-1.5">
          {EDGES.map((e) => (
            <button
              key={e}
              role="radio"
              aria-checked={cal.edge === e}
              onClick={() => update({ edge: e })}
              className={`rounded-full px-3.5 py-1.5 font-medium capitalize transition-[background-color,color,transform] duration-200 active:scale-[0.96] focus-visible:outline-2 focus-visible:outline-accent ${cal.edge === e ? "bg-fg text-ink" : "bg-white/[0.05] text-muted hover:text-fg"}`}
            >
              {e}
            </button>
          ))}
        </div>

        <label htmlFor="len" className="text-muted">
          Size
        </label>
        <input
          id="len"
          type="range"
          min={0.05}
          max={1}
          step={0.01}
          value={cal.length}
          onChange={(e) => {
            const length = +e.target.value;
            update({ length, start: Math.min(cal.start, 1 - length) });
          }}
          className="loka-range"
          style={{ "--fill": `${((cal.length - 0.05) / 0.95) * 100}%` } as React.CSSProperties}
        />

        <span id="glow-label" className="text-muted">Glow</span>
        <div role="radiogroup" aria-labelledby="glow-label" className="flex flex-wrap items-center gap-2.5">
          {Object.entries(SWATCHES).map(([name, c]) => (
            <button
              key={c}
              role="radio"
              aria-checked={look.color === c}
              aria-label={name}
              onClick={() => restyle({ color: c })}
              className={`h-7 w-7 rounded-full transition-[transform,box-shadow] duration-200 ease-[var(--ease-spring)] hover:scale-110 active:scale-95 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-fg ${look.color === c ? "ring-2 ring-fg ring-offset-[3px] ring-offset-raised" : ""}`}
              style={{ background: c, filter: `drop-shadow(0 0 6px ${c}99)` }}
            />
          ))}
          {/* any colour: the native picker behind a rainbow swatch */}
          <label
            className={`relative h-7 w-7 cursor-pointer rounded-full bg-[conic-gradient(#ff6fae,#ffb547,#c3f24a,#5ef0b0,#4cc9ff,#a98bff,#ff6fae)] transition-transform duration-200 hover:scale-110 has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-fg ${Object.values(SWATCHES).includes(look.color) ? "" : "ring-2 ring-fg ring-offset-[3px] ring-offset-raised"}`}
          >
            <input
              type="color"
              aria-label="Custom colour"
              value={look.color}
              onChange={(e) => restyle({ color: e.target.value })}
              className="absolute inset-0 h-full w-full cursor-pointer opacity-0"
            />
          </label>
        </div>

        <label htmlFor="glow-opacity" className="text-muted">Opacity</label>
        <div className="flex items-center gap-3">
          <input
            id="glow-opacity"
            type="range"
            min={0.1}
            max={1}
            step={0.05}
            value={look.opacity}
            onChange={(e) => restyle({ opacity: +e.target.value })}
            aria-valuetext={`${Math.round(look.opacity * 100)}%`}
            className="loka-range flex-1"
            style={{ "--fill": `${((look.opacity - 0.1) / 0.9) * 100}%` } as React.CSSProperties}
          />
          <span className="w-10 text-right tabular-nums text-muted">{Math.round(look.opacity * 100)}%</span>
        </div>

        <span id="motion-label" className="text-muted">Crossing</span>
        <div role="radiogroup" aria-labelledby="motion-label" className="flex flex-wrap gap-1.5">
          {MOTIONS.map((m) => (
            <button
              key={m}
              role="radio"
              aria-checked={look.motion === m}
              onClick={() => restyle({ motion: m })}
              className={`rounded-full px-3.5 py-1.5 font-medium capitalize transition-[background-color,color,transform] duration-200 active:scale-[0.96] focus-visible:outline-2 focus-visible:outline-accent ${look.motion === m ? "bg-fg text-ink" : "bg-white/[0.05] text-muted hover:text-fg"}`}
            >
              {m}
            </button>
          ))}
        </div>
      </div>
    </section>
  );
}

/** Neon bar riding along one edge of the selected monitor; drag or arrow keys move it. */
function EdgeSlider(props: {
  edge: Edge;
  box: { left: number; top: number; width: number; height: number };
  start: number;
  length: number;
  onChange: (start: number) => void;
}) {
  const { edge, box, start, length, onChange } = props;
  const vertical = edge === "left" || edge === "right";
  const span = vertical ? box.height : box.width;
  const grab = useRef<number | null>(null);
  const clamp = (s: number) => Math.min(1 - length, Math.max(0, s));
  const T = 6;

  const style = vertical
    ? { left: box.left + (edge === "left" ? -T / 2 : box.width - T / 2), top: box.top + start * span, width: T, height: length * span }
    : { top: box.top + (edge === "top" ? -T / 2 : box.height - T / 2), left: box.left + start * span, height: T, width: length * span };

  const pos = (e: React.PointerEvent) => (vertical ? e.clientY : e.clientX) / span;

  return (
    <div
      role="slider"
      tabIndex={0}
      aria-label="Device position along the edge"
      aria-valuemin={0}
      aria-valuemax={Math.round((1 - length) * 100)}
      aria-valuenow={Math.round(start * 100)}
      className="absolute cursor-grab touch-none rounded-full bg-accent shadow-[0_0_10px_2px_color-mix(in_oklab,var(--color-accent)_70%,transparent),0_0_26px_6px_color-mix(in_oklab,var(--color-accent)_30%,transparent)] transition-[width,height] duration-300 focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-fg active:cursor-grabbing"
      style={style}
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture(e.pointerId);
        grab.current = pos(e) - start;
      }}
      onPointerMove={(e) => grab.current !== null && onChange(clamp(pos(e) - grab.current))}
      onPointerUp={() => (grab.current = null)}
      onKeyDown={(e) => {
        const d = { ArrowUp: -0.01, ArrowLeft: -0.01, ArrowDown: 0.01, ArrowRight: 0.01 }[e.key];
        if (d) {
          e.preventDefault();
          onChange(clamp(start + d * (e.shiftKey ? 5 : 1)));
        }
      }}
    />
  );
}
