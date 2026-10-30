const KEY = "loka.sound";
let audio: AudioContext | undefined;

export const soundOn = () => localStorage.getItem(KEY) !== "off";
export const setSoundOn = (on: boolean) => localStorage.setItem(KEY, on ? "on" : "off");

/** Soft UI click, synthesized (no asset): a short sine blip falling 1.6 → 0.9 kHz. */
export function tick() {
  if (!soundOn()) return;
  audio ??= new AudioContext();
  const t = audio.currentTime;
  const osc = audio.createOscillator();
  const gain = audio.createGain();
  osc.frequency.setValueAtTime(1600, t);
  osc.frequency.exponentialRampToValueAtTime(900, t + 0.035);
  gain.gain.setValueAtTime(0.0001, t);
  gain.gain.exponentialRampToValueAtTime(0.07, t + 0.003);
  gain.gain.exponentialRampToValueAtTime(0.0001, t + 0.06);
  osc.connect(gain).connect(audio.destination);
  osc.start(t);
  osc.stop(t + 0.07);
}

// a label's click is re-dispatched to its input, so labels are left out (one tick per click)
const PRESSABLE = "button, a[href], [role=button], [role=switch], [role=tab], [role=radio], input[type=checkbox], input[type=radio], select, summary";

/** Every press in this window ticks; keyboard activation (Enter/Space) fires `click` too. */
export function tickOnPress() {
  addEventListener(
    "click",
    (e) => {
      const pressed = (e.target as Element).closest?.(PRESSABLE);
      if (pressed && !pressed.hasAttribute("data-silent")) tick();
    },
    true,
  );
}
