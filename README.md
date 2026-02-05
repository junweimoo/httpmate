# httpmate

An HTTP debugging proxy for macOS, built with Tauri (Rust + React).

httpmate runs a local proxy, intercepts HTTP **and HTTPS** traffic (TLS
interception with a locally generated CA), and shows every request/response —
headers, bodies, timing, TLS metadata — in a live, filterable UI. Sessions are
persisted to SQLite so history survives restarts.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design,
including how the planned features (regex rewrite, CLI, mock server, agent
integration, metrics) plug into the v1 seams.

## Layout

```
crates/httpmate-core/   # everything that captures/stores/transforms traffic:
                        # proxy engine, interceptor chain, MITM CA, SQLite store,
                        # macOS glue. Fully usable without a GUI.
src-tauri/              # thin Tauri shell: commands → Controller, events → WebView
src/                    # React frontend (Vite + TypeScript + Zustand)
```

## Development

Prerequisites: Rust (stable), Node 20+, and on macOS nothing else. On Linux
you additionally need `libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev` to
build the shell (the proxy itself is macOS-focused; system-proxy/keychain
features are inert elsewhere).

```sh
npm install

# run the desktop app (starts vite + tauri)
npx tauri dev          # or: cargo tauri dev

# core engine tests (includes end-to-end proxy + MITM tests)
cargo test

# lint / typecheck
cargo clippy --all-targets
npm run build
```

`cargo check`/`cargo test` default to the core crate only (see
`default-members` in Cargo.toml), so they work on any platform without
webview libraries.

## Using it

1. Start the proxy from the toolbar (default port 8888).
2. Open the **CA** dialog, generate the root certificate and click *Install
   trust* (macOS admin prompt). For simulators/other devices, copy the PEM or
   the cert file from the same dialog.
3. Either flip on **system proxy** (snapshots your current network settings
   and restores them on toggle-off/quit/crash-recovery), or point a single
   client at it:

   ```sh
   curl -x http://127.0.0.1:8888 https://example.com/
   ```

Hosts that pin their certificates are detected (client rejects the minted
cert) and automatically downgraded to opaque tunnels; you can also pre-list
`passthroughHosts` in `settings.json` under the app data directory.

## Status

v1: proxy service, HTTP(S) interception and inspection. The interceptor
chain, controller API and store are the extension points for the roadmap in
the architecture doc. Keychain and `networksetup` integration are
implemented but still need verification on real macOS hardware.
