import { useEffect, useId } from "react";

/**
 * Tuju, LOKA's mascot: a chubby cursor arrow, the thing that actually travels between screens.
 * Every device gets its own Tuju (hue from its fingerprint); the face carries state:
 *   idle      blinks now and then, eyes follow the pointer, small smile
 *   waiting   eyes dart side to side, "o" mouth (a pairing request is out)
 *   happy     ^ ^ with an open smile (paired)
 *   sad       half-shut eyes, frown (declined / offline)
 *   searching hovers up and down while rings radiate from its tip, the hotspot (listening on the network)
 */
export type Mood = "idle" | "waiting" | "happy" | "sad" | "searching";

function hash(seed: string) {
  let h = 0;
  for (const c of seed) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return h;
}

// one listener for every mascot: eyes drift toward the pointer
let tracking = false;
function trackPointer() {
  if (tracking || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  tracking = true;
  const root = document.documentElement.style;
  addEventListener("pointermove", (e) => {
    root.setProperty("--lx", ((e.clientX / innerWidth) * 2 - 1).toFixed(3));
    root.setProperty("--ly", ((e.clientY / innerHeight) * 2 - 1).toFixed(3));
  });
}

// classic pointer silhouette; a thick round stroke of the same paint makes it soft and chubby
const BODY = "M24 9 L24 78 L41 63 L51 83 L62 78 L53 59 L78 58 Z";
const INK = "#05060c";
const EYES: [number, number][] = [[36, 45], [51, 45]];

const MOUTH: Record<Mood, React.ReactNode> = {
  idle: <path d="M40.5 57 Q44 60 47.5 57" />,
  searching: <path d="M40.5 57 Q44 60 47.5 57" />,
  waiting: <circle cx="44" cy="58" r="1.9" fill={INK} stroke="none" />,
  happy: <path d="M39.5 56 Q44 63 48.5 56 Z" fill={INK} />,
  sad: <path d="M40.5 59.5 Q44 56.5 47.5 59.5" />,
};

export function Face({
  seed,
  size = 64,
  mood = "idle",
  look,
  label,
}: {
  seed: string;
  size?: number;
  mood?: Mood;
  /** fixed gaze, overriding the pointer; "right" mirrors Tuju so two of them can face each other */
  look?: "left" | "right";
  /** accessible name; omit when a visible name sits next to the mascot */
  label?: string;
}) {
  useEffect(trackPointer, []);
  const id = useId();
  const h = hash(seed);
  const hue = h % 360;
  const blinkDelay = `${(h % 4000) / 1000}s`;
  const open = mood === "idle" || mood === "searching" || mood === "waiting";

  const eye = ([x, y]: [number, number]) =>
    mood === "happy" ? (
      <path key={x} d={`M${x - 3.6} ${y + 1.5} Q${x} ${y - 4} ${x + 3.6} ${y + 1.5}`} />
    ) : mood === "sad" ? (
      <path key={x} d={`M${x - 3.4} ${y + 0.5} Q${x} ${y + 2.5} ${x + 3.4} ${y + 0.5}`} />
    ) : (
      <g key={x}>
        <ellipse cx={x} cy={y} rx="3.4" ry="4.6" fill={INK} stroke="none" />
        <circle cx={x + 1.2} cy={y - 1.7} r="1.25" fill="white" stroke="none" />
      </g>
    );

  return (
    <span
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      className="relative inline-block shrink-0"
      style={{ width: size, height: size }}
    >
      {mood === "searching" &&
        [0, 1].map((i) => (
          <span
            key={i}
            className="absolute left-[20%] top-[6%] h-[44%] w-[44%] -translate-x-1/2 -translate-y-1/2 rounded-full border border-accent/50"
            style={{ animation: `ripple 2.6s ${i * 1.3}s var(--ease-out-soft) infinite` }}
          />
        ))}
      <svg
        viewBox="0 0 100 100"
        className="relative block h-full w-full overflow-visible drop-shadow-[0_0.06em_0.08em_rgb(0_0_0/0.5)]"
        style={{
          transform: look === "right" ? "scaleX(-1)" : undefined,
          animation: mood === "searching" ? "hop 2.4s ease-in-out infinite" : undefined,
        }}
      >
        <defs>
          <linearGradient id={id} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0" style={{ stopColor: `oklch(0.88 0.09 ${hue})` }} />
            <stop offset="1" style={{ stopColor: `oklch(0.66 0.15 ${hue})` }} />
          </linearGradient>
        </defs>
        <g transform="translate(-1 4)">
          <path d={BODY} fill={`url(#${id})`} stroke={`url(#${id})`} strokeWidth="15" strokeLinejoin="round" />
          {/* soft rim light along the leading edge */}
          <path d="M22 16 L22 40" stroke="white" strokeOpacity="0.45" strokeWidth="3" strokeLinecap="round" />
          <g fill="none" stroke={INK} strokeWidth="2.6" strokeLinecap="round">
            <g
              className="transition-[translate] duration-300 ease-out"
              style={{
                translate: look ? "-2.5px 0" : "calc(var(--lx, 0) * 2.5px) calc(var(--ly, 0) * 2px)",
                animation: mood === "waiting" ? "glance 1.8s ease-in-out infinite" : undefined,
              }}
            >
              <g
                style={{
                  transformBox: "fill-box",
                  transformOrigin: "center",
                  animation: open && mood !== "waiting" ? `blink 6s ${blinkDelay} infinite` : undefined,
                }}
              >
                {EYES.map(eye)}
              </g>
            </g>
            {MOUTH[mood]}
          </g>
        </g>
      </svg>
    </span>
  );
}
