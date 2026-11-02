import { useEffect, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { type Look, loadLook } from "./look";

type Packet = { kind: "proximity" | "drop" | "leave"; normalizedY: number; proximity: number };
type Ripple = { y: number; born: number; strength: number };

/** "#rrggbb" mixed toward white: the hot core of the glow */
const core = (hex: string) => {
  const n = parseInt(hex.slice(1), 16);
  const mix = (c: number) => Math.round(c + (255 - c) * 0.8);
  return `rgb(${mix(n >> 16)} ${mix((n >> 8) & 255)} ${mix(n & 255)})`;
};

/** The blocked reason on a dark pill just inside the edge, along it (turned for side edges, since the window is only `reach`
 *  deep), word-wrapped to the lines that fit and centred near the cursor `y` (0..1). */
function drawLabel(ctx: CanvasRenderingContext2D, text: string, alpha: number, side: string, w: number, h: number, y: number) {
  const vertical = side === "left" || side === "right";
  const L = vertical ? h : w, D = vertical ? w : h;
  const lh = 15, pad = 7, gap = 10;
  ctx.font = '600 12px "Segoe UI Variable", system-ui, -apple-system, sans-serif';
  const max = L - 2 * pad - 8;
  const rows = Math.max(1, Math.floor((D - gap - 2 * pad) / lh));
  const lines: string[] = [];
  for (const word of text.split(" ")) {
    const last = lines.length ? `${lines[lines.length - 1]} ${word}` : word;
    if (lines.length && ctx.measureText(last).width <= max) lines[lines.length - 1] = last;
    else lines.push(word);
  }
  if (lines.length > rows) {
    lines.length = rows;
    let end = lines[rows - 1];
    while (end && ctx.measureText(end + "…").width > max) end = end.slice(0, -1);
    lines[rows - 1] = end + "…";
  }
  const bw = Math.min(max, Math.max(...lines.map((l) => ctx.measureText(l).width))) + 2 * pad;
  const bh = lines.length * lh + 2 * pad;
  const along = Math.min(Math.max(y * L, bw / 2 + 4), L - bw / 2 - 4);
  const depth = gap + bh / 2;
  ctx.save();
  ctx.globalAlpha = alpha;
  ctx.translate(side === "left" ? depth : side === "right" ? w - depth : along, side === "top" ? depth : side === "bottom" ? h - depth : along);
  if (vertical) ctx.rotate(side === "left" ? -Math.PI / 2 : Math.PI / 2);
  ctx.fillStyle = "rgba(5, 6, 12, 0.88)"; // ink
  ctx.beginPath();
  if (ctx.roundRect) ctx.roundRect(-bw / 2, -bh / 2, bw, bh, 9);
  else ctx.rect(-bw / 2, -bh / 2, bw, bh); // WebKit before Safari 16
  ctx.fill();
  ctx.fillStyle = "#eceff7"; // fg
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  lines.forEach((l, i) => ctx.fillText(l, 0, -bh / 2 + pad + lh * (i + 0.5), max));
  ctx.restore();
}

/**
 * Neon edge in the user's colour. Motion "ripple": NameDrop-style liquid line that bulges toward the
 * cursor, radiates waves while it is near and splashes when the cursor crosses or a file drops.
 * "pulse": straight line whose glow swells with proximity and flashes on crossing. "calm": steady line.
 * `blocked`: the device behind it can't be driven (`stateReason`): dimmed, dashed, and the reason beside the edge when the cursor is close.
 */
export function BezelPortal({ side, event, blocked = null }: { side: "left" | "right" | "top" | "bottom"; event: string; blocked?: string | null }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const note = useRef(blocked);
  note.current = blocked;

  useEffect(() => {
    const el = canvas.current!;
    const ctx = el.getContext("2d")!;
    const target = { y: 0.5, p: 0 };
    const cur = { y: 0.5, p: 0 };
    const ripples: Ripple[] = [];
    const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
    let look = loadLook();
    let motion = reduced ? "calm" : look.motion;
    let lastEmit = 0;
    let flash = -1e9;
    let raf = 0, nap = 0;
    // an idle glow redraws at 10 fps (1 fps hidden) instead of 60; any event wakes it
    const wake = () => {
      if (!nap) return;
      clearTimeout(nap);
      nap = 0;
      raf = requestAnimationFrame(frame);
    };
    // file transfer progress drawn along the edge, summed over files in flight; `end` starts the fade-out
    const xfer = { pct: 0, end: -1e9, active: false };
    const files = new Map<number, [number, number]>();
    // auto-hide: how near the cursor is (tracker `hover`, button or not) and how visible the glow is (-1: not yet drawn)
    const hover = { p: 0 };
    let shown = -1, applied = "";
    let label = 0; // how visible the blocked reason is

    // crossing / drop / preview
    const splash = (y: number) => {
      if (motion === "ripple") ripples.push({ y, born: performance.now(), strength: 2.2 });
      if (motion === "pulse") flash = performance.now();
    };

    const unlisteners = [
      // packets are addressed to this window: with several portals each glows only for its own
      getCurrentWebviewWindow().listen<Packet>(event, ({ payload }) => {
        if (payload.kind === "leave") target.p = 0;
        else {
          target.y = payload.normalizedY;
          target.p = payload.proximity;
        }
        if (payload.kind === "drop") splash(payload.normalizedY);
        wake();
      }),
      getCurrentWebviewWindow().listen<[number, number]>("hover", ({ payload }) => {
        hover.p = payload[1];
        wake();
      }),
      listen<{ active: boolean; y?: number }>("kvm", ({ payload }) => payload.active && (splash(payload.y ?? cur.y), wake())),
      listen<{ type: string; id?: number; done?: number; total?: number }>("transfer", ({ payload: e }) => {
        if (e.type === "text") return;
        wake();
        if (e.type === "progress") files.set(e.id!, [e.done!, e.total!]);
        else if (files.delete(e.id!) && e.type !== "failed") Object.assign(xfer, { pct: 1 });
        let done = 0, total = 0;
        for (const [d, t] of files.values()) (done += d), (total += t);
        if (files.size) Object.assign(xfer, { pct: total ? done / total : 1, active: true });
        else if (xfer.active) Object.assign(xfer, { active: false, end: performance.now() });
      }),
      listen<Look>("look", ({ payload }) => {
        // preview the crossing in the new style (and show a hidden glow once)
        const preview = payload.color !== look.color || payload.motion !== look.motion || (payload.autoHide && !look.autoHide);
        look = payload;
        motion = reduced ? "calm" : look.motion;
        if (preview) splash(cur.y);
        wake();
      }),
    ];

    const frame = (now: number) => {
      const dpr = devicePixelRatio || 1;
      const w = el.clientWidth, h = el.clientHeight;
      if (el.width !== w * dpr || el.height !== h * dpr) {
        el.width = w * dpr;
        el.height = h * dpr;
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);

      // critically-damped-ish follow so 60 Hz packets look continuous at any refresh rate
      cur.y += (target.y - cur.y) * 0.25;
      cur.p += (target.p - cur.p) * 0.15;

      if (motion === "ripple" && cur.p > 0.15 && now - lastEmit > 420) {
        ripples.push({ y: cur.y, born: now, strength: cur.p });
        lastEmit = now;
      }
      for (let i = ripples.length - 1; i >= 0; i--) if (now - ripples[i].born > 1600) ripples.splice(i, 1);

      // edge-local frame: t runs along the edge (0..L), depth grows into the screen (0..D)
      const vertical = side === "left" || side === "right";
      const L = vertical ? h : w, D = vertical ? w : h;
      const at = (t: number, depth: number): [number, number] =>
        side === "left" ? [depth, t] : side === "right" ? [w - depth, t] : side === "top" ? [t, depth] : [t, h - depth];
      const maxBulge = motion === "ripple" ? Math.min(D * 0.7, 90) : 0;
      const sigma = L * 0.09;
      const cy = cur.y * L;

      const offsetAt = (y: number) => {
        let o = cur.p * maxBulge * Math.exp(-((y - cy) ** 2) / (2 * sigma * sigma));
        for (const r of ripples) {
          const age = (now - r.born) / 1000;
          const front = age * L * 0.9; // wave front speed: ~0.9 edge-lengths/s
          const d = Math.abs(y - r.y * L) - front;
          const env = Math.exp(-(d * d) / (2 * (L * 0.05) ** 2)) * (1 - age / 1.6);
          o += r.strength * 14 * env * Math.sin(d * 0.08);
        }
        return Math.max(0, o);
      };

      const breathe = motion === "calm" ? 0.4 : 0.35 + 0.1 * Math.sin(now / 900);
      const flare = Math.max(0, 1 - (now - flash) / 700);
      const glow = Math.min(1, breathe + cur.p * 0.65 + ripples.length * 0.05 + flare);
      // auto-hide: fades in as the cursor nears; crossings, drops and transfers always show
      const busy = cur.p > 0.02 || ripples.length > 0 || flare > 0 || xfer.active || now - xfer.end < 900;
      const want = !look.autoHide || busy ? 1 : Math.min(1, hover.p * 1.5);
      shown = shown < 0 ? want : shown + (want - shown) * 0.12;
      const why = note.current;
      const wantLabel = why && hover.p > 0.6 ? 1 : 0;
      label = reduced ? wantLabel : label + (wantLabel - label) * 0.15;
      // the reason stays readable even with a faint or auto-hidden glow
      const op = Math.max(look.opacity * shown, label * 0.95).toFixed(3); // only touch the style when it changes
      if (op !== applied) el.style.opacity = applied = op;
      const dim = why ? 0.35 : 1;
      const thick = look.width;
      const grad = vertical ? ctx.createLinearGradient(0, 0, 0, h) : ctx.createLinearGradient(0, 0, w, 0);
      // the chosen colour along the edge, a white-hot core where the cursor is
      grad.addColorStop(0, look.color);
      grad.addColorStop(Math.max(0.01, Math.min(0.99, cur.y)), core(look.color));
      grad.addColorStop(1, look.color);

      ctx.beginPath();
      for (let t = 0; t <= L; t += 2) {
        const [x, y] = at(t, 3 + offsetAt(t));
        if (t === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.lineCap = "round";
      ctx.strokeStyle = grad;
      ctx.globalCompositeOperation = "lighter";
      if (why) ctx.setLineDash([12 * thick, 10 * thick]);
      for (const [width, blur, alpha] of [[10, 28, 0.35], [5, 14, 0.6], [2, 4, 1]] as const) {
        ctx.lineWidth = width * thick * (1 + flare);
        ctx.shadowBlur = blur * thick * (1 + flare);
        ctx.shadowColor = look.color;
        ctx.globalAlpha = alpha * glow * dim;
        ctx.stroke();
      }
      ctx.setLineDash([]);
      ctx.globalAlpha = 1;
      ctx.shadowBlur = 0;

      const xAlpha = xfer.active ? 1 : Math.max(0, 1 - (now - xfer.end) / 900);
      if (xAlpha > 0) {
        ctx.beginPath();
        const [x0, y0] = at(0, 3), [x1, y1] = at(xfer.pct * L, 3);
        ctx.moveTo(x0, y0);
        ctx.lineTo(x1, y1);
        ctx.strokeStyle = core(look.color);
        ctx.shadowColor = look.color;
        ctx.shadowBlur = 16;
        ctx.lineWidth = 4 * thick;
        ctx.globalAlpha = xAlpha;
        ctx.stroke();
        ctx.globalAlpha = 1;
        ctx.shadowBlur = 0;
      }
      ctx.globalCompositeOperation = "source-over";
      if (why && label > 0.01) drawLabel(ctx, why, label, side, w, h, cur.y);
      const idle = !busy && target.p < 0.01 && Math.abs(want - shown) < 0.01 && Math.abs(wantLabel - label) < 0.01;
      if (idle) nap = window.setTimeout(() => ((nap = 0), (raf = requestAnimationFrame(frame))), shown < 0.01 ? 1000 : 100);
      else raf = requestAnimationFrame(frame);
    };
    raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      clearTimeout(nap);
      unlisteners.forEach((u) => u.then((f) => f()));
    };
  }, [side, event]);

  return <canvas ref={canvas} className="block h-full w-full" />;
}
