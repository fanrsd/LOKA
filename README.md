<div align="center">

<img src="docs/icon.png" width="96" alt="LOKA icon: Tuju, a cursor with a face, next to a glowing edge">

# LOKA

**One mouse and keyboard for every computer on your desk.**
Push the cursor through a glowing edge of your screen and it lands on the device next to it.

**English** · [Bahasa Indonesia](README.id.md) · [Website](https://loka.margingroup.id) · [Download](https://github.com/fanrsd/LOKA/releases/latest)

<img src="docs/screenshots/calibrate.webp" width="640" alt="LOKA paired with Studio PC, with the glowing edge set on the right side of screen 1">

</div>

## What it does

- **Shared mouse & keyboard.** One computer is the **Main**: its mouse and keyboard drive the others. Each device nearby runs LOKA as a **Client**.
- **The Bezel Portal.** You mark the stretch of your screen edge where the other device physically sits. Push the cursor into that glowing segment and it moves over. Push it out the far side of that device and it comes home. **Ctrl+Alt+Esc** always brings it back.
- **The keyboard follows the cursor.** Whatever you type goes to the device the cursor is on.
- **Drop files on the edge.** Drag a file to the glowing edge and let go. It arrives in `Downloads/LOKA` on the paired device. Existing files are never overwritten.
- **LocalSend-style pairing.** Devices on the same network find each other. Click one, then confirm on the other device.
- **Make it yours.** Pick the glow colour, which also becomes the app's accent, and the crossing animation: Ripple, Pulse or Calm.

Meet **Tuju**, the mascot: a cursor, the thing that actually travels between your screens. Every device gets its own colour, and its face shows what's going on: searching, waiting, paired or offline.

| | |
|---|---|
| <img src="docs/screenshots/home.webp" alt="Nearby devices, each shown as a Tuju"> | <img src="docs/screenshots/pairing.webp" alt="Pairing prompt with two Tujus facing each other"> |
| <img src="docs/screenshots/client-waiting.webp" alt="A Client waiting for a Main"> | <img src="docs/screenshots/glow-violet.webp" alt="The same screen with a violet glow"> |

## Platforms

| Platform | Status |
|---|---|
| Windows 10/11 | Works (Main and Client) |
| macOS | Code complete, still being tested on real hardware. Needs the **Accessibility** permission. |
| Android | Planned |
| iOS | Not planned: iOS does not allow apps to control the device's input |

## Install

Download the installer from [Releases](https://github.com/fanrsd/LOKA/releases/latest):

- **Windows:** `LOKA_x.y.z_x64-setup.exe`, or the `.msi`
- **macOS:** `LOKA_x.y.z_universal.dmg`. The app is not notarized yet, so right-click it and choose **Open** the first time.

Then:

1. Open LOKA on both computers and pick a role: **Main** on the computer with the mouse and keyboard, **Client** on the other.
2. On the Main, click the Client's Tuju. Accept on the Client.
3. Pick the edge and slide the glowing bar until it lines up with where the other device sits.

Both devices must be on the same local network. LOKA uses UDP 53318 (discovery) and TCP 53319–53321 (telemetry, files, input). Allow them in your firewall.

## Build from source

Requirements: [Node.js](https://nodejs.org) 20+, [Rust](https://rustup.rs) (stable), and the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS.

```bash
npm install
npm run tauri dev        # run
npm run sim              # run with a simulated phone client
npm run tauri build      # installers in src-tauri/target/release/bundle
```

## Security

LOKA is built for a trusted home or office network. Only paired devices can send input or files, but the traffic is not encrypted yet. Avoid public Wi-Fi.

## License

[Apache License 2.0](LICENSE). Forks and modified versions must keep the [NOTICE](NOTICE) file and credit the original project:

> Based on LOKA by Aufa Nirsad: https://github.com/fanrsd/LOKA
