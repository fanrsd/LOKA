import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { setSoundOn, soundOn, tick } from "./sound";

export const isMac = navigator.userAgent.includes("Mac");
const win = getCurrentWindow();

// Windows' own caption glyphs (Segoe Fluent Icons on 11, MDL2 Assets on 10)
const GLYPH = { minimize: "\uE921", maximize: "\uE922", restore: "\uE923", close: "\uE8BB" };
const glyphFont = { fontFamily: "Segoe Fluent Icons, Segoe MDL2 Assets" };
const control =
  "grid h-full w-[46px] place-items-center text-[10px] text-muted transition-colors duration-100 hover:bg-white/[0.06] hover:text-fg active:bg-white/[0.04] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent";

/**
 * The window's own title bar: drag area (double-click maximizes), click-sound toggle and, on Windows,
 * minimize / maximize / close. A Mac keeps its traffic lights (overlay title bar).
 */
export function TitleBar() {
  const [sound, setSound] = useState(soundOn);
  const [maximized, setMaximized] = useState(false);

  useEffect(() => {
    if (isMac) return;
    const sync = () => void win.isMaximized().then(setMaximized);
    sync();
    const un = win.onResized(sync);
    return () => void un.then((f) => f());
  }, []);

  const toggleSound = () => {
    setSoundOn(!sound);
    setSound(!sound);
    tick(); // only audible when turning sound on
  };

  return (
    <div data-tauri-drag-region className={`relative z-40 flex h-(--titlebar) shrink-0 items-center bg-ink ${isMac ? "pl-[78px]" : "pl-4"}`}>
      <span className="pointer-events-none flex-1 font-display text-xs font-semibold tracking-[-0.01em] text-faint">LOKA</span>
      <button
        data-silent
        onClick={toggleSound}
        aria-pressed={!sound}
        aria-label={sound ? "Mute click sounds" : "Unmute click sounds"}
        title={sound ? "Mute click sounds" : "Unmute click sounds"}
        className={`${control} ${isMac ? "mr-2 rounded-md" : ""}`}
      >
        <SpeakerIcon muted={!sound} />
      </button>
      {!isMac && (
        <>
          <button onClick={() => win.minimize()} aria-label="Minimize" className={control} style={glyphFont}>
            {GLYPH.minimize}
          </button>
          <button
            onClick={() => win.toggleMaximize()}
            aria-label={maximized ? "Restore" : "Maximize"}
            className={control}
            style={glyphFont}
          >
            {maximized ? GLYPH.restore : GLYPH.maximize}
          </button>
          <button
            onClick={() => win.close()}
            aria-label="Close"
            className={`${control} hover:bg-[#c42b1c] hover:text-white active:bg-[#c42b1c]/90`}
            style={glyphFont}
          >
            {GLYPH.close}
          </button>
        </>
      )}
    </div>
  );
}

function SpeakerIcon({ muted }: { muted: boolean }) {
  return (
    <svg aria-hidden width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
      <path d="M2.5 6h2.2L8 3.3v9.4L4.7 10H2.5z" fill="currentColor" fillOpacity="0.15" />
      {muted ? <path d="M11 6.2l3.6 3.6M14.6 6.2L11 9.8" /> : <path d="M10.6 5.6a3.4 3.4 0 0 1 0 4.8M12.4 3.9a5.8 5.8 0 0 1 0 8.2" />}
    </svg>
  );
}
