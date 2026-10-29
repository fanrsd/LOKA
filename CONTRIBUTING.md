# Contributing to LOKA

Thanks for helping. Bug reports, fixes and small features are all welcome.

## Before you start

- **Bugs:** open an issue with your OS, LOKA version (About), the role of each computer (Main/Client) and what you did.
- **Bigger changes:** open an issue first so we can agree on the approach before you spend time on it.
- Windows and macOS are the targets. Android is planned later; iOS is out of scope (no public API to inject input).

## Setup

Needs Node 22+, Rust stable and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS.

```sh
npm install
npm run tauri dev   # run the app
npm run sim         # app + a simulated second device, handy with only one computer
```

## Before opening a pull request

```sh
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
```

CI runs the same two commands on every pull request.

- Keep a pull request to one change, and say how you tested it (which OSes, one or two machines).
- OS-specific code stays in `src-tauri/src/platform/` behind `cfg`, so every OS still compiles.
- Changing the network protocol (`kvm.rs`, `discovery.rs`, `telemetry.rs`, `transfer.rs`) breaks older versions on the other computer: mention it in the pull request.

By contributing you agree your work is licensed under [Apache 2.0](LICENSE).
