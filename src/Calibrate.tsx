import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { availableMonitors } from "@tauri-apps/api/window";
import { type Edge, type Portal, type Rect, fit, sameRect, slot } from "./calibration";
import { Face } from "./Face";
import { type Look, MOTIONS, SWATCHES, loadLook, saveLook } from "./look";
import type { Device, Peer } from "./Pairing";

const EDGES: Edge[] = ["left", "top", "bottom", "right"];
/** where a device clicked (or picked with the keyboard) goes first */
const FIRST_EDGES: Edge[] = ["right", "left", "top", "bottom"];
const MAP_W = 420, MAP_H = 200;
/** room around the screens for the devices next to them (map px) */
const PAD = 96;
/** a dragged device snaps to an edge this close (map px) */
const SNAP = 80;
const BAR = 6;

const vertical = (e: Edge) => e === "left" || e === "right";

/** A device on the map or waiting to go on it; `peer` set: not paired yet (dropping it pairs first). */
type Item = { fingerprint: string; alias: string; aspect: number; peer?: Peer };
type Point = { x: number; y: number };
type Drag = { item: Item; from: Portal | null; length: number; at: Point; origin: Point; to: Portal | null; moved: boolean };

/** This computer's screens with every device placed around them, like the desk. Dragging one to an edge
 * puts its portal there (the other computer puts this one on its matching edge); dragging a nearby
 * device in pairs it first. */
export function Calibrate({ device, peers, selected, onSelect, onPair, onChange }: {
  device: Device;
  peers: Peer[];
  selected: string | null;
  onSelect: (fingerprint: string) => void;
  onPair: (peer: Peer) => Promise<boolean>;
  onChange: () => void;
}) {
  const [screens, setScreens] = useState<Rect[] | null>(null);
  // changes not in `device` yet (null: taken off), so the map follows a drag without a refetch per move
  const [live, setLive] = useState<Record<string, Portal | null>>({});
  const [drag, setDragState] = useState<Drag | null>(null);
  const dragRef = useRef<Drag | null>(null);
  const setDrag = (d: Drag | null) => {
    dragRef.current = d;
    setDragState(d);
  };
  const dragged = useRef(false); // the click ending a drag is no click
  const [note, setNote] = useState<string | null>(null);
  const box = useRef<HTMLDivElement>(null);
  const tray = useRef<HTMLDivElement>(null);
  // one set_portal at a time, so a late move never lands after the drop
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const [look, setLook] = useState<Look>(loadLook);
  const restyle = (patch: Partial<Look>) => {
    const next = { ...look, ...patch };
    setLook(next);
    saveLook(next);
  };

  useEffect(() => {
    availableMonitors().then((ms) => setScreens(ms.map((m) => ({ x: m.position.x, y: m.position.y, w: m.size.width, h: m.size.height }))));
  }, []);
  useEffect(() => setLive({}), [device]);

  if (!screens?.length) return null;

  const portals: Record<string, Portal> = {};
  for (const [f, p] of Object.entries({ ...device.portals, ...live })) {
    // a portal on a screen that's gone waits in the tray
    if (p && screens.some((s) => sameRect(s, p.monitor))) portals[f] = p;
  }
  const others = (f: string) => Object.entries(portals).flatMap(([g, p]) => (g === f ? [] : [p]));
  const aspectOf = (f: string) => peers.find((p) => p.info.fingerprint === f)?.info.aspectRatio || 16 / 10;
  const paired: Item[] = device.paired.map((d) => ({ fingerprint: d.fingerprint, alias: d.alias, aspect: aspectOf(d.fingerprint) }));
  const placed = paired.filter((i) => portals[i.fingerprint]);
  const waiting: Item[] = [
    ...paired.filter((i) => !portals[i.fingerprint]),
    ...peers
      .filter((p) => !device.paired.some((d) => d.fingerprint === p.info.fingerprint) && p.info.proto === device.info.proto)
      .map((p) => ({ fingerprint: p.info.fingerprint, alias: p.info.alias, aspect: p.info.aspectRatio || 16 / 10, peer: p })),
  ];

  // fit every screen into the map, with room around them
  const minX = Math.min(...screens.map((s) => s.x));
  const minY = Math.min(...screens.map((s) => s.y));
  const maxX = Math.max(...screens.map((s) => s.x + s.w));
  const maxY = Math.max(...screens.map((s) => s.y + s.h));
  const k = Math.min(MAP_W / (maxX - minX), MAP_H / (maxY - minY));
  const mx = (x: number) => (x - minX) * k + PAD;
  const my = (y: number) => (y - minY) * k + PAD;

  const apply = (f: string, portal: Portal | null, sync: boolean) => {
    setLive((l) => ({ ...l, [f]: portal }));
    const call = queue.current.then(() => invoke<string | null>("set_portal", { fingerprint: f, portal, sync }).catch((e) => String(e)));
    queue.current = call;
    if (sync)
      call.then((why) => {
        setNote(why);
        onChange();
      });
  };

  /** Map px (inside the box) of a pointer event. */
  const toMap = (e: React.PointerEvent): Point => {
    const r = box.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  /** The free spot on the screen edge nearest map point `at`, if one is close enough. */
  const snap = (at: Point, f: string, length: number): Portal | null => {
    const px = minX + (at.x - PAD) / k, py = minY + (at.y - PAD) / k;
    let best: [number, Portal] | null = null;
    for (const m of screens)
      for (const e of EDGES) {
        const v = vertical(e);
        const line = { left: m.x, right: m.x + m.w, top: m.y, bottom: m.y + m.h }[e];
        const along = v ? (py - m.y) / m.h : (px - m.x) / m.w;
        const past = Math.max(0, -along, along - 1) * (v ? m.h : m.w);
        const d = Math.hypot(v ? px - line : py - line, past) * k;
        if (d > SNAP || (best && d >= best[0])) continue;
        const p = slot(screens, m, e, others(f), along, length);
        if (p) best = [d, p];
      }
    return best?.[1] ?? null;
  };

  /** Bar on the edge and the device's box beside it, in map px. */
  const layout = (p: Portal, aspect: number) => {
    const m = p.monitor, v = vertical(p.edge);
    const a = v ? my(m.y + p.start * m.h) : mx(m.x + p.start * m.w);
    const len = (p.end - p.start) * (v ? m.h : m.w) * k;
    const depth = Math.min(PAD - 14, Math.max(34, v ? len * aspect : len / aspect));
    const line = { left: mx(m.x), right: mx(m.x + m.w), top: my(m.y), bottom: my(m.y + m.h) }[p.edge];
    const out = p.edge === "left" || p.edge === "top" ? line - 8 - depth : line + 8;
    return {
      bar: v ? { left: line - BAR / 2, top: a, width: BAR, height: len } : { left: a, top: line - BAR / 2, width: len, height: BAR },
      tile: v ? { left: out, top: a, width: depth, height: len } : { left: a, top: out, width: len, height: depth },
    };
  };

  const firstFit = (f: string) => FIRST_EDGES.map((e) => fit(screens, e, others(f))).find(Boolean) ?? null;

  /** Place `item` at `to` (pairing it first if needed); the other device follows when the side changed. */
  const put = async (item: Item, to: Portal, from: Portal | null) => {
    if (item.peer) {
      setNote(`Asking ${item.alias}…`);
      if (!(await onPair(item.peer))) return setNote(null);
      setNote(null);
    }
    apply(item.fingerprint, to, !from || from.edge !== to.edge);
    onSelect(item.fingerprint);
  };

  const grab = (item: Item) => ({
    onPointerDown: (e: React.PointerEvent) => {
      if (e.button !== 0) return;
      e.currentTarget.setPointerCapture(e.pointerId);
      dragged.current = false;
      const at = toMap(e), from = portals[item.fingerprint] ?? null;
      setDrag({ item, from, length: from ? from.end - from.start : 0.3, at, origin: at, to: from, moved: false });
    },
    onPointerMove: (e: React.PointerEvent) => {
      const d = dragRef.current;
      if (!d || d.item.fingerprint !== item.fingerprint) return;
      const at = toMap(e);
      if (!d.moved && Math.hypot(at.x - d.origin.x, at.y - d.origin.y) < 5) return;
      const to = snap(at, item.fingerprint, d.length);
      setDrag({ ...d, at, to: to ?? null, moved: true });
      // the real glow follows a placed device along
      if (d.from && to && JSON.stringify(to) !== JSON.stringify(d.to)) apply(item.fingerprint, to, false);
    },
    onPointerUp: (e: React.PointerEvent) => {
      const d = dragRef.current;
      setDrag(null);
      if (!d?.moved) return;
      dragged.current = true;
      if (d.to) return void put(d.item, d.to, d.from);
      if (!d.from) return;
      const t = tray.current?.getBoundingClientRect();
      const inTray = t && e.clientX >= t.left && e.clientX <= t.right && e.clientY >= t.top && e.clientY <= t.bottom;
      // dropped in the tray: off the map (and off the other device's); anywhere else: back where it was
      apply(d.item.fingerprint, inTray ? null : d.from, !!inTray);
    },
    onPointerCancel: () => {
      const d = dragRef.current;
      setDrag(null);
      if (d?.from) apply(d.item.fingerprint, d.from, false);
    },
  });

  const sel = selected && portals[selected] ? selected : null;
  const selPortal = sel ? portals[sel] : null;
  const selAlias = placed.find((i) => i.fingerprint === sel)?.alias;

  return (
    <section aria-labelledby="cal-title" className="rise mt-12 rounded-[28px] bg-raised p-7 ring-1 ring-line" style={{ "--i": 3 } as React.CSSProperties}>
      <h2 id="cal-title" className="font-display text-2xl font-bold tracking-[-0.025em]">
        Where your devices sit
      </h2>
      <p className="mt-1.5 max-w-[60ch] text-sm leading-relaxed text-muted">
        Drag each device next to the edge it sits beside on your desk. The other computer puts this one on its matching edge. Drag in a device that isn't paired yet to pair it.
      </p>

      <div ref={box} className="relative mx-auto mt-6 touch-none select-none" style={{ width: (maxX - minX) * k + 2 * PAD, height: (maxY - minY) * k + 2 * PAD }}>
        {screens.map((m, i) => (
          <div
            key={i}
            aria-hidden
            className="absolute grid place-items-center rounded-[14px] bg-white/[0.05] font-display text-sm font-semibold text-faint ring-1 ring-white/20"
            style={{ left: mx(m.x) + 3, top: my(m.y) + 3, width: m.w * k - 6, height: m.h * k - 6 }}
          >
            {i + 1}
          </div>
        ))}

        {placed.map((item) => {
          const f = item.fingerprint, p = portals[f];
          const { bar, tile } = layout(p, item.aspect);
          const moving = drag?.item.fingerprint === f && drag.moved;
          const screen = screens.findIndex((s) => sameRect(s, p.monitor)) + 1;
          return (
            <div key={f} className={moving ? "opacity-30" : ""}>
              <div
                aria-hidden
                className={`absolute rounded-full bg-accent transition-opacity ${f === sel ? "shadow-[0_0_10px_2px_color-mix(in_oklab,var(--color-accent)_70%,transparent),0_0_26px_6px_color-mix(in_oklab,var(--color-accent)_30%,transparent)]" : "opacity-50"}`}
                style={bar}
              />
              <button
                {...grab(item)}
                onClick={() => (dragged.current ? (dragged.current = false) : onSelect(f))}
                onKeyDown={(e) => {
                  const dir = ({ ArrowUp: -1, ArrowLeft: -1, ArrowDown: 1, ArrowRight: 1 } as Record<string, number>)[e.key];
                  if (dir) {
                    e.preventDefault();
                    const to = slot(screens, p.monitor, p.edge, others(f), (p.start + p.end) / 2 + dir * (e.shiftKey ? 0.05 : 0.01), p.end - p.start);
                    if (to) apply(f, to, false);
                  } else if (e.key === "Delete" || e.key === "Backspace") {
                    e.preventDefault();
                    apply(f, null, true);
                  }
                }}
                aria-pressed={f === sel}
                aria-label={`${item.alias}, ${p.edge} edge of screen ${screen}. Arrow keys slide it along, Delete takes it off.`}
                className={`absolute flex cursor-grab flex-col items-center justify-center gap-1 overflow-hidden rounded-[12px] bg-white/[0.04] ring-1 transition-[background-color,box-shadow] duration-200 hover:bg-white/[0.08] focus-visible:outline-2 focus-visible:outline-accent active:cursor-grabbing ${f === sel ? "ring-accent" : "ring-line"}`}
                style={tile}
              >
                <Face seed={f} size={Math.max(16, Math.min(40, Math.min(tile.width, tile.height) * 0.55))} mood={f === sel ? "happy" : "idle"} />
                {tile.height > 64 && tile.width > 48 && <span className="w-full truncate px-1 text-center text-[11px] font-semibold">{item.alias}</span>}
              </button>
            </div>
          );
        })}

        {/* where the dragged device lands, or it floating under the pointer */}
        {drag?.moved &&
          (drag.to ? (
            (() => {
              const { bar, tile } = layout(drag.to, drag.item.aspect);
              return (
                <div aria-hidden className="pointer-events-none">
                  <div className="absolute rounded-full bg-accent" style={bar} />
                  <div className="absolute grid place-items-center rounded-[12px] border border-dashed border-accent bg-white/[0.06]" style={tile}>
                    <Face seed={drag.item.fingerprint} size={Math.max(16, Math.min(40, Math.min(tile.width, tile.height) * 0.55))} mood="happy" />
                  </div>
                </div>
              );
            })()
          ) : (
            <div aria-hidden className="pointer-events-none absolute -translate-x-1/2 -translate-y-1/2" style={{ left: drag.at.x, top: drag.at.y }}>
              <Face seed={drag.item.fingerprint} size={40} mood="waiting" />
            </div>
          ))}
      </div>

      <div ref={tray} className="mt-6 rounded-[20px] border border-dashed border-line px-4 py-3">
        <p className="text-xs text-muted">
          {waiting.length ? "Not placed yet: drag one next to a screen, or click it." : "Drag a device here to take it off your screens."}
        </p>
        {waiting.length > 0 && (
          <ul className="mt-2 flex flex-wrap gap-2">
            {waiting.map((item) => (
              <li key={item.fingerprint}>
                <button
                  {...grab(item)}
                  onClick={() => {
                    if (dragged.current) return void (dragged.current = false);
                    const to = firstFit(item.fingerprint);
                    if (to) put(item, to, null);
                    else setNote("Every edge is taken: take a device off first.");
                  }}
                  aria-label={item.peer ? `${item.alias}, not paired. Pair it and place it next to your screens.` : `${item.alias}. Place it next to your screens.`}
                  className={`flex w-24 cursor-grab flex-col items-center gap-1 rounded-[18px] p-2 transition-[background-color,opacity] duration-200 hover:bg-white/[0.05] focus-visible:outline-2 focus-visible:outline-accent active:cursor-grabbing ${drag?.item.fingerprint === item.fingerprint && drag.moved ? "opacity-30" : ""}`}
                >
                  <Face seed={item.fingerprint} size={40} />
                  <span className="w-full truncate text-center text-xs font-semibold">{item.alias}</span>
                  <span className="text-[11px] text-faint">{item.peer ? "Drag to pair" : "Paired"}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <p role="status" className="mt-3 min-h-5 text-sm text-muted">
        {note}
      </p>

      <div className="mt-5 grid grid-cols-[6rem_1fr] items-center gap-x-4 gap-y-4 text-sm">
        {sel && selPortal && (
          <>
            <span id="edge-label" className="text-muted">
              {selAlias} on
            </span>
            <div role="radiogroup" aria-labelledby="edge-label" className="flex flex-wrap gap-1.5">
              {EDGES.map((e) => (
                <button
                  key={e}
                  role="radio"
                  aria-checked={selPortal.edge === e}
                  onClick={() => {
                    if (selPortal.edge === e) return;
                    const to = fit(screens, e, others(sel));
                    if (to) apply(sel, to, true);
                    else setNote(`No room left on the ${e} edge.`);
                  }}
                  className={`rounded-full px-3.5 py-1.5 font-medium capitalize transition-[background-color,color,transform] duration-200 active:scale-[0.96] focus-visible:outline-2 focus-visible:outline-accent ${selPortal.edge === e ? "bg-fg text-ink" : "bg-white/[0.05] text-muted hover:text-fg"}`}
                >
                  {e}
                </button>
              ))}
              <button onClick={() => apply(sel, null, true)} className="rounded-full px-3.5 py-1.5 font-medium text-faint transition-colors hover:text-danger focus-visible:outline-2 focus-visible:outline-accent">
                Take off
              </button>
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
              value={selPortal.end - selPortal.start}
              onChange={(e) => {
                const to = slot(screens, selPortal.monitor, selPortal.edge, others(sel), (selPortal.start + selPortal.end) / 2, +e.target.value);
                if (to) apply(sel, to, false);
              }}
              className="loka-range"
              style={{ "--fill": `${((selPortal.end - selPortal.start - 0.05) / 0.95) * 100}%` } as React.CSSProperties}
            />
          </>
        )}

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
