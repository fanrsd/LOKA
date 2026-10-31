<div align="center">

<img src="docs/icon.png" width="96" alt="LOKA icon: Oka, a cursor with a face, next to a glowing edge">

# LOKA

**One mouse and keyboard for every computer on your desk.**
Push the cursor through a glowing edge of your screen and it lands on the device next to it.

**English** · [Bahasa Indonesia](README.id.md) · [Website](https://loka.margingroup.id) · [Download](https://github.com/fanrsd/LOKA/releases/latest)

<img src="docs/screenshots/calibrate.webp" width="640" alt="LOKA paired with Studio PC, with the glowing edge set on the right side of screen 1">

</div>

## What it does

- **Shared mouse & keyboard, any direction.** Every computer running LOKA can drive the others and be driven by them: push the cursor over from whichever one your hands are on.
- **The Bezel Portal.** You mark the stretch of your screen edge where the other device physically sits. Push the cursor into that glowing segment and it moves over. Push it out the far side of that device and it comes home. **Ctrl+Alt+Esc** or a double tap of **Ctrl** always brings it back. Place several devices on different edges, and the cursor can keep going from one to the next (PC → Mac → laptop).
- **Feels like one computer.** The cursor disappears from the screen you're driving from. Ctrl works as ⌘ on a Mac (and ⌘ as Ctrl on a PC), so shortcuts just work. Set the pointer speed over there and an optional edge delay so a quick flick past the edge stays put. **Scroll Lock** (F14 on a Mac) locks the pointer on the screen it's on. Mac trackpad swipes between desktops, Mission Control and pinch-to-zoom work on the other computer too.
- **The keyboard and clipboard follow the cursor.** Whatever you type goes to the device the cursor is on. Copy on one computer, move over, and paste on the other, both directions: text, pictures (screenshots too, up to 10 MB) and files you copied in Explorer or Finder. Anything a password manager marks as secret stays home. **Recent clipboard** keeps the last 12 things you copied, pictures included: click one to copy it again.
- **Drag files across the edge.** Pick up files in Explorer or Finder and keep dragging past the glowing edge: the cursor carries them over, and you drop them in any folder or app on the other computer. Big files stream: the drop starts right away and fills in as they arrive.
- **Drop files on the edge, both ways.** Drag files or whole folders to the glowing edge and let go. They arrive in `Downloads/LOKA` (or a folder you choose) on the paired device, folders with their structure, several files at once. While you control another computer, its edge leading back glows too: drag files there to send them the other way. Existing files are never overwritten. The glow fills up as the files travel, and both computers show progress, speed and time left. You can cancel, open the folder when it lands, and get a system notification on arrival. **Ctrl+Alt+V** pastes what just arrived into the app in front (a chat, an email). Or send from Explorer's **Send to → LOKA** or Finder's **Open With → LOKA**.
- **LocalSend-style pairing.** Devices on the same network find each other. Click one, then confirm on the other device. On networks that block discovery (guest Wi-Fi), add the other computer by its IP address. Pairing shows the same 6-digit code on both screens, so you know you paired the right one, and nobody on the network can fake it.
- **Updates itself.** When a new version is out, Oka pops up with what's new; one click downloads, installs and restarts LOKA (Windows, macOS, Linux). **About** shows the version and the changelog.
- **Make it yours.** Pick the glow colour, which also becomes the app's accent, how see-through and how thick the glow is, whether it shows always or only when the cursor comes near, and the crossing animation: Ripple, Pulse or Calm. Every click in LOKA makes a soft tick; the speaker in the top-right corner mutes it.
- **Your Android phone too.** Push the computer's cursor off the edge where the phone sits and it lands on the phone: tap, scroll and type there with the computer's mouse and keyboard (right button = Back, middle = Home). Or use the phone as a touchpad and keyboard for the computer, and send files from it. LOKA sets it up from the phone itself through Wireless debugging (Android 11+), no cable or PC tools needed.
- **Share more, only if you want to.** Each computer can share a folder (it opens as a drive on the other one), its sound (play it through the other computer's speakers), its notifications (Windows), its microphone (use it as the mic in calls on the other computer, through the free VB-CABLE or BlackHole), and a terminal. Everything starts off.
- **A link that notices.** If the other computer sleeps, drops off Wi-Fi or freezes, the cursor comes home within about 3 seconds and nothing stays pressed over there. Switching Wi-Fi mid-session keeps going.
- **Wake it up.** A paired computer that went to sleep gets a **Wake up** button (Wake-on-LAN).
- **Lives in the tray.** Closing the window keeps LOKA sharing in the background. The tray menu can **Control** any placed device without touching the edge, **Disconnect**, open LOKA again, or **Quit LOKA**.

Meet **Oka**, the little one from LOKA: a cursor, the thing that actually travels between your screens. Every device gets its own colour, and its face shows what's going on: searching, waiting, paired or offline.

| | |
|---|---|
| <img src="docs/screenshots/home.webp" alt="Nearby devices, each shown as an Oka"> | <img src="docs/screenshots/pairing.webp" alt="Pairing prompt with two Okas facing each other"> |
| <img src="docs/screenshots/waiting.webp" alt="Waiting for the MacBook Air to accept, with the pairing code to compare"> | <img src="docs/screenshots/glow-violet.webp" alt="The same screen with a violet glow"> |

## Security

- **Encrypted, always.** Every link between devices (input, clipboard, files, services) is QUIC over **TLS 1.3**, with certificates on both ends.
- **Pinned to the device, not the address.** Each LOKA has its own certificate; its SHA-256 **fingerprint** is its identity. A paired device is remembered by that fingerprint, so a computer that merely takes its IP address gets nowhere, and an unpaired one is refused.
- **A pairing code nobody can fake.** Both screens show the same 6-digit code, derived from both fingerprints and a random number from each side. The asker commits to its number before seeing the other's, so a man in the middle can't try certificates until the codes happen to match.
- **Nothing leaves your network.** No account, no server, no cloud. Sharing a folder, sound, notifications, the mic or a terminal is off until you turn it on.

## Platforms

| Platform | Status |
|---|---|
| Windows 10/11 | Works |
| macOS | Code complete, still being tested on real hardware. Needs the **Accessibility** permission. |
| Linux | New: X11 and Wayland desktops, and servers without a desktop (`loka --headless`). Still being tested on real hardware. |
| Android 11+ | New: be controlled by a computer (set up once through Wireless debugging), use the phone as touchpad and keyboard, send files. Still being tested on real phones. |
| iOS | Not planned: iOS does not allow apps to control the device's input |

## Install

Download the installer from [Releases](https://github.com/fanrsd/LOKA/releases/latest):

- **Windows:** `LOKA_x.y.z_x64-setup.exe`, or the `.msi`. If SmartScreen warns, choose **More info → Run anyway**. When Windows Firewall asks, tick both **Private** and **Public**. Devices still don't see each other? In LOKA open **About → Allow through Windows Firewall**.
- **macOS:** `LOKA_x.y.z_universal.dmg` (Apple Silicon and Intel). Open it, drag LOKA onto **Applications**, and open it from Applications. LOKA is not notarized yet, so the first launch is blocked: click **Done** (not Move to Trash), then:
  - macOS 15 and newer: System Settings → Privacy & Security → scroll down → **Open Anyway**.
  - macOS 14 and older: right-click LOKA in Applications → **Open** → **Open**.
  - "LOKA is damaged and can't be opened" (or any version): run in Terminal

    ```bash
    xattr -dr com.apple.quarantine /Applications/LOKA.app
    ```

  Allow **Accessibility** when asked (System Settings → Privacy & Security → Accessibility), on every Mac; on macOS 15+ also allow **Local Network**. After updating, if the mouse stops crossing, remove LOKA from the Accessibility list with **−** and turn it on again.
- **Linux:** `LOKA_x.y.z_amd64.deb` (Debian, Ubuntu), the `.rpm` (Fedora), or the `.AppImage`. LOKA reads the mouse and keyboard below the desktop, so it needs permission once: **About → Allow input access** (asks for your password).
- **Linux server without a desktop:** install the `.deb` or `.rpm`, then run `sudo loka --headless` on it (or `sudo loka --allow-input` once, then `loka --headless` as your user). Pair from LOKA on another computer and answer the code in the terminal. Its keyboard input lands on the server's console.
- **Android:** `LOKA_x.y.z_android.apk` on the phone (allow installing from your browser or Files when asked). Open LOKA, pair with a computer, then tap **Set up** so the computer can move into the phone: LOKA walks you through Developer options → Wireless debugging → pairing code, typed straight into LOKA's notification. Xiaomi/POCO also need **USB debugging (Security settings)**.

Then:

1. Open LOKA on both computers.
2. On one, drag the other's Oka next to the screen edge it sits beside on your desk (or click it). Check that both show the same code, then accept on the other.
3. Slide its box along the edge until the glow lines up with where the other device sits. The other computer puts this one on its matching edge by itself.

Both devices must be on the same local network. LOKA uses UDP 53318 (discovery) and TCP 53319–53321 (telemetry, files, input). Guest Wi-Fi or routers with client isolation block discovery: LOKA shows **This computer's address** under **Add by IP**; type it into **Add by IP** on the other computer.

## Build from source

Requirements: [Node.js](https://nodejs.org) 20+, [Rust](https://rustup.rs) (stable), and the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS.

```bash
npm install
npm run tauri dev        # run
npm run sim              # run with a simulated phone client
npm run tauri build      # installers in src-tauri/target/release/bundle
```

## License

[Apache License 2.0](LICENSE). Forks and modified versions must keep the [NOTICE](NOTICE) file and credit the original project:

> Based on LOKA by Aufa Nirsad: https://github.com/fanrsd/LOKA
