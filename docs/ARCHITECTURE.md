# httpmate — Architecture

An HTTP debugging proxy for macOS, built with Tauri (Rust backend, React frontend).

**Status:** Draft v1 · 2026-06-11

---

## 1. Goals and scope

### v1 functional requirements

1. **Run a local proxy service** — start/stop an HTTP(S) proxy listening on a configurable local port.
2. **Intercept and monitor traffic** — capture every request/response flowing through the proxy, including HTTPS via TLS interception (MITM) with a locally generated CA.
3. **Inspect HTTP content** — view headers, bodies (with pretty-printing for common content types), and metadata (timing, sizes, status, TLS info) in the GUI.

### Future requirements the architecture must accommodate

| # | Feature | Architectural seam (see §6) |
|---|---------|------------------------------|
| F1 | Regex match + rewrite + forward of requests | Interceptor chain |
| F2 | CLI in addition to the GUI | Core/shell split (Rust workspace) |
| F3 | Mock HTTP server | Interceptor chain (terminal interceptor) + rule store |
| F4 | LLM-agent-assisted end-to-end testing | Control API + event bus |
| F5 | Traffic metrics visualization | SQLite store + query layer |

### Non-goals (v1)

- HTTP/3 / QUIC interception (HTTP/1.1 and HTTP/2 only).
- WebSocket *message* inspection (the upgrade and connection are recorded; frame-level capture is a later increment).
- Non-macOS platforms. Nothing should gratuitously block Linux/Windows, but macOS integration (Keychain, system proxy) is first-class and untested elsewhere.
- Acting as a remote/shared proxy. The proxy binds to `127.0.0.1` by default; LAN exposure is an explicit opt-in.

---

## 2. Key decisions

These were settled up front and the rest of the document assumes them:

| Decision | Choice | Rationale |
|----------|--------|-----------|
| HTTPS interception | **MITM in v1** with a local CA | Most real traffic is TLS; a proxy that can't show HTTPS bodies isn't useful for debugging. The CA/cert machinery also gates several future features. |
| Captured-traffic storage | **SQLite-backed** (bodies over a threshold spill to files) | Survives restarts, handles long sessions without unbounded RAM, and gives F5 (metrics) a queryable store for free. |
| Proxy engine | **Custom engine on hyper + tokio + rustls** | Full control of the request lifecycle. The interceptor chain — the extensibility seam for F1/F3/F4 — is easiest to get right when we own the lifecycle rather than adapting to a third-party handler model. |
| System proxy configuration | **Optional auto-configure** via macOS SystemConfiguration, plus manual mode | One-click capture for system-wide traffic; manual port mode for targeting a single app or CI use without elevated privileges. |

---

## 3. High-level architecture

```
┌─────────────────────────────── Tauri app (macOS) ───────────────────────────────┐
│                                                                                 │
│  ┌────────────── WebView (React + TS) ──────────────┐                           │
│  │  Traffic table · detail pane · proxy controls    │                           │
│  │  CA onboarding · settings                        │                           │
│  └───────▲───────────────────────────────┬──────────┘                           │
│          │ Tauri events (push)           │ Tauri commands (invoke)              │
│  ┌───────┴───────────────────────────────▼──────────────────────────────────┐   │
│  │                        httpmate-app  (Tauri shell, Rust)                 │   │
│  │   command handlers · event forwarding · app lifecycle · macOS glue       │   │
│  └───────▲───────────────────────────────┬──────────────────────────────────┘   │
└──────────│───────────────────────────────│──────────────────────────────────────┘
           │ broadcast events              │ ProxyController API
┌──────────┴───────────────────────────────▼──────────────────────────────────────┐
│                          httpmate-core  (Rust library crate)                    │
│                                                                                  │
│  ┌─────────────┐   ┌──────────────────────────────┐   ┌──────────────────────┐  │
│  │ Listener    │──▶│      Interceptor chain       │──▶│  Upstream client     │  │
│  │ (hyper,     │   │  recorder · [rewrite] ·      │   │  (hyper client,      │  │
│  │  CONNECT,   │   │  [mock] · forwarder          │   │   conn pooling)      │  │
│  │  TLS MITM)  │   └──────────────┬───────────────┘   └──────────────────────┘  │
│  └──────┬──────┘                  │                                              │
│         │                  ┌──────▼───────┐    ┌───────────────────────────┐    │
│  ┌──────▼──────┐           │  Event bus   │    │  Storage (SQLite + blob   │    │
│  │ Cert        │           │  (broadcast) │    │  files, WAL mode)         │    │
│  │ authority   │           └──────────────┘    └───────────────────────────┘    │
│  │ (rcgen)     │                                                                 │
│  └─────────────┘   ┌──────────────────────────┐                                  │
│                    │ macos module: system     │                                  │
│                    │ proxy toggle · Keychain  │                                  │
│                    └──────────────────────────┘                                  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

The load-bearing boundary is **`httpmate-core` vs `httpmate-app`**: everything that captures, stores, or transforms traffic lives in the core library and is callable without a GUI. The Tauri shell is a thin adapter (commands in, events out). This is what makes F2 (CLI) and F4 (agent control) cheap later — they are just additional shells over the same core.

### Repository layout (Cargo workspace)

```
httpmate/
├── crates/
│   ├── httpmate-core/        # proxy engine, interceptors, storage, CA, macOS glue
│   │   └── src/
│   │       ├── proxy/        # listener, CONNECT/MITM handling, upstream client
│   │       ├── intercept/    # Interceptor trait, chain, recorder, forwarder
│   │       ├── ca/           # root CA lifecycle, leaf-cert minting + cache
│   │       ├── store/        # SQLite schema, repository API, blob spillover
│   │       ├── events/       # event types, broadcast bus
│   │       └── macos/        # system proxy toggle, Keychain trust (cfg(target_os))
│   └── httpmate-cli/         # F2 — future; thin clap-based shell over core
├── src-tauri/                # httpmate-app: Tauri shell (commands, event bridge)
├── src/                      # React frontend (TypeScript, Vite)
└── docs/
```

---

## 4. Backend design (`httpmate-core`)

### 4.1 Proxy engine

Tokio runtime; hyper for both the server (accepting proxied requests) and the upstream client.

**Plain HTTP** — requests arrive in absolute-URI form (`GET http://example.com/ HTTP/1.1`); they are normalized and pushed through the interceptor chain.

**HTTPS (CONNECT + MITM)** — on `CONNECT host:443`:

1. Reply `200 Connection Established`, take ownership of the raw socket.
2. TLS-accept the client side with rustls, presenting a **leaf certificate minted on the fly** for the requested host (see §4.3). ALPN negotiates `h2`/`http/1.1`.
3. TLS-connect to the upstream origin with rustls (real certificate validation against webpki roots; failures surfaced as recorded errors, with an opt-out toggle per host).
4. Serve the decrypted client stream with hyper; each decrypted request flows through the same interceptor chain as plain HTTP. HTTP/2 on the client side maps onto pooled upstream connections.

**Passthrough list** — hosts matching user-configured patterns (and, by default, hosts that fail the TLS handshake because of certificate pinning) are tunneled opaquely instead of MITM'd; the transaction is still recorded as a `tunnel` entry with timing/byte counts.

**Body handling** — bodies are streamed, not buffered: the engine forwards chunks to the destination as they arrive while a tee copies them into the recorder up to a configurable cap (default 10 MB; beyond the cap the body is forwarded but recorded as truncated). This keeps large downloads/uploads from stalling or exhausting memory, and is also the property the future rewrite feature must respect (a rewrite rule that needs the full body forces buffering *for matching transactions only*).

### 4.2 Interceptor chain — the extensibility seam

Every transaction passes through an ordered chain of interceptors. This is the single abstraction that v1 monitoring and future features F1/F3/F4 all plug into.

```rust
#[async_trait]
pub trait Interceptor: Send + Sync {
    /// Inspect/modify the request, or answer it directly (short-circuit).
    async fn on_request(&self, tx: &mut TransactionCtx, req: Request) -> RequestAction;
    /// Inspect/modify the response on its way back to the client.
    async fn on_response(&self, tx: &mut TransactionCtx, resp: Response) -> Response;
}

pub enum RequestAction {
    Continue(Request),   // pass (possibly modified) request down the chain
    Respond(Response),   // short-circuit: reply without contacting upstream (mocks, blocks)
}
```

- `TransactionCtx` carries the transaction id, timing marks, client info, TLS metadata, and a tag map interceptors can use to communicate (e.g. the future rewrite engine tagging which rule fired, so the UI can badge it).
- **v1 chain:** `recorder` (captures everything, emits events, never modifies) → `forwarder` (terminal: sends upstream and returns the response).
- **F1 rewrite** becomes a `rewrite` interceptor before the forwarder: compiled regex rules against method/URL/headers/body, applying modifications and continuing.
- **F3 mock server** becomes a `mock` interceptor that returns `Respond(..)` for matching rules — the "mock server" is the proxy answering for the origin; no separate listener needed (though a standalone-port mode can reuse the same rule engine later).
- **F4 agent hooks** can register a dynamic interceptor that pauses matching transactions and waits for a verdict over the control API (human or agent "breakpoints").

The chain is rebuilt atomically when configuration changes (`ArcSwap`), so rule edits never require a proxy restart.

### 4.3 Certificate authority

- On first run, generate a root CA (rcgen, ECDSA P-256, ~10-year validity, CN "httpmate Root CA"). The CA private key is stored in the **macOS Keychain**; only the public certificate is written to disk for export.
- Onboarding flow installs trust via `SecTrustSettingsSetTrustSettings` (or shells out to `security add-trusted-cert`), which triggers the system admin prompt. The UI shows clear trust state: missing / installed-untrusted / trusted.
- Leaf certificates are minted per host on first CONNECT, cached in memory (LRU) keyed by SNI, including SAN handling for wildcard reuse.
- The UI exposes "export CA cert" for installing on iOS simulators/devices and other clients.

### 4.4 Storage

SQLite via `rusqlite` (WAL mode), one database per session directory under `~/Library/Application Support/httpmate/`.

```sql
CREATE TABLE transactions (
  id            INTEGER PRIMARY KEY,          -- monotonic, also the UI ordering key
  started_at    INTEGER NOT NULL,             -- unix millis
  duration_ms   INTEGER,
  kind          TEXT NOT NULL,                -- 'http' | 'tunnel' | 'ws-upgrade'
  scheme        TEXT, method TEXT, host TEXT, path TEXT, query TEXT,
  status        INTEGER,
  req_header_blob   BLOB,                     -- raw header bytes, order-preserving
  resp_header_blob  BLOB,
  req_body_ref      TEXT,                     -- inline:<rowid> | file:<relpath> | truncated:<...>
  resp_body_ref     TEXT,
  req_size      INTEGER, resp_size INTEGER,
  client_addr   TEXT, tls_version TEXT, alpn TEXT,
  error         TEXT,                         -- upstream/TLS failure detail, if any
  tags          TEXT                          -- JSON: interceptor annotations (rule hits, etc.)
);
CREATE TABLE bodies (rowid INTEGER PRIMARY KEY, data BLOB);  -- bodies ≤ 256 KB
CREATE INDEX idx_tx_started ON transactions(started_at);
CREATE INDEX idx_tx_host    ON transactions(host);
```

- Bodies ≤ 256 KB are stored inline in `bodies`; larger ones spill to content-addressed files next to the database. Bodies are stored as received (decompressed-on-view in the UI, with the original encoding noted).
- Writes go through a single writer task fed by a bounded channel, batching inserts per ~50 ms tick — the proxy hot path never blocks on disk.
- Retention: configurable cap (default 50k transactions / 2 GB per session) enforced by background pruning; "clear session" truncates.
- F5 (metrics) reads the same database: aggregates by host/status/time are plain SQL, computed off the hot path.

### 4.5 Event bus and control API

- `tokio::sync::broadcast` channel of `ProxyEvent` values: `TransactionStarted`, `TransactionUpdated` (response headers arrived), `TransactionCompleted`, `ProxyStateChanged`, `CaTrustChanged`. Events carry summaries only (no bodies); consumers fetch detail from the store on demand.
- `ProxyController` is the imperative API surface of the core: `start(config) / stop() / status() / set_system_proxy(bool) / query(filter) / get_transaction(id) / install_ca_trust()`. The Tauri shell maps commands onto it 1:1.
- **F2/F4 forward path:** the same controller can later be served over a local gRPC or JSON-RPC socket (`~/Library/Application Support/httpmate/control.sock`), letting the CLI attach to a running GUI instance and letting agent frameworks (e.g. via an MCP server wrapping the socket) drive the proxy, query traffic, and set breakpoints. Nothing in v1 builds the socket, but because every capability already flows through `ProxyController` + the event bus, exposing it is additive.

### 4.6 macOS integration

- **System proxy toggle:** set/unset HTTP + HTTPS proxies on active network services. Primary path is the SystemConfiguration framework (`SCNetworkConfiguration` via the `system-configuration` crate); fallback is `networksetup -setwebproxy`. The previous proxy state is snapshotted and restored on toggle-off and on app exit. A guard file detects a crashed previous run and offers to restore network settings on next launch.
- **Keychain:** CA private key storage and trust installation (§4.3).
- Both live behind small traits in `core::macos` so the core stays compilable (with no-op impls) on other platforms.

---

## 5. Tauri shell and frontend

### 5.1 Shell (`src-tauri`)

Thin by design — owns the `ProxyController`, registers Tauri commands that delegate to it, and runs one task that forwards bus events to the WebView. App-lifecycle responsibilities: restore system proxy on quit, warn when quitting with the proxy capturing system-wide traffic.

Event forwarding **coalesces**: bus events are batched and emitted to the WebView at most every ~100 ms as `traffic:batch` payloads. Under load (hundreds of requests/sec) this keeps the IPC channel and React render loop healthy.

### 5.2 Frontend (React + TypeScript + Vite)

- **State:** Zustand store holding proxy status, the transaction summary list, filters, and selection. Transaction *detail* (headers, bodies) is fetched lazily via `invoke('get_transaction', id)` when a row is selected — the list stays light.
- **Traffic table:** virtualized (TanStack Virtual) — only visible rows render; the store keeps summaries for the current session window and pages older history from SQLite on scroll/search.
- **Detail pane:** request/response tabs; header table preserving order and duplicates; body viewer with type-aware rendering (JSON tree, form-urlencoded table, image preview, hex fallback), decompression, and copy-as-cURL.
- **Filtering:** client-side quick filter (host/method/status/text) over the in-memory window; full-history search compiles to SQL against the store.
- **Onboarding:** first-run wizard for CA generation → trust installation → optional system proxy enable, with live trust-state checks.
- UI components: Tailwind + shadcn/ui (or equivalent); no heavyweight state framework — the backend is the source of truth.

### 5.3 IPC contract

Commands (request/response): `start_proxy`, `stop_proxy`, `get_status`, `set_system_proxy`, `query_transactions`, `get_transaction`, `clear_session`, `generate_ca`, `install_ca_trust`, `export_ca`, `get_settings`, `set_settings`.
Events (push): `traffic:batch`, `proxy:state`, `ca:state`.
Types are defined once in Rust and exported to TypeScript via `specta`/`tauri-specta` so the contract can't drift.

---

## 6. How future features land

- **F1 — Regex rewrite:** new `rewrite` interceptor (§4.2) + a `rules` table in SQLite + a rules editor UI. The chain, tagging, and atomic-reload mechanics already exist. Decision deferred to F1: full-body matching forces buffering for matching rules (cap applies).
- **F2 — CLI:** new `httpmate-cli` crate. Standalone mode links `httpmate-core` directly (headless capture, HAR/JSONL export to stdout); attach mode talks to a running GUI over the control socket (§4.5). No core changes.
- **F3 — Mock server:** `mock` interceptor returning `Respond(..)` from a rule store; shares the matcher engine with F1. Optionally a second listener that serves *only* mock rules for use without proxy configuration.
- **F4 — Agent integration:** an MCP server (or plain JSON-RPC client) over the control socket exposing tools like `query_traffic`, `get_transaction`, `set_breakpoint`, `resume_with_modification`, `add_mock`. Agents observe and steer traffic during E2E tests without any privileged hooks into the engine — they're just another controller client.
- **F5 — Metrics:** read-only SQL aggregations over the transactions table, surfaced as a dashboard tab (requests/sec, latency percentiles by host, error rates, size histograms). A small `metrics` module in core owns the queries so the CLI can print the same numbers.

---

## 7. Security considerations

The tool is a deliberate local MITM; the design keeps that power scoped to the user's own machine and session:

- CA **private key never touches disk** — Keychain only; exported artifact is the public cert.
- Proxy and (future) control socket bind to localhost by default; LAN exposure is explicit and warns about open relaying.
- Upstream TLS is **verified by default**; disabling verification is per-host and visually flagged on affected transactions.
- Sensitive header values (`Authorization`, `Cookie`, etc.) are maskable in the UI by default with click-to-reveal; exports (cURL/HAR) warn when they include credentials.
- Stored traffic lives under the user's Application Support directory with `0600`-style permissions; "clear session" deletes blobs as well as rows.
- System-proxy changes are always reversible: prior state snapshot + crash-recovery restore (§4.6).

---

## 8. Technology summary

| Concern | Choice |
|---|---|
| App shell | Tauri 2.x |
| Async runtime / HTTP | tokio, hyper 1.x (server + client), tower for middleware utilities |
| TLS | rustls (client + server), rcgen for CA/leaf minting |
| Storage | rusqlite (bundled SQLite, WAL) + blob spillover files |
| macOS glue | system-configuration, security-framework crates |
| IPC typing | specta + tauri-specta (Rust → TS types) |
| Frontend | React 18, TypeScript, Vite, Zustand, TanStack Virtual, Tailwind |
| CLI (F2) | clap, linking httpmate-core |

## 9. Open questions / risks

- **Certificate pinning** (many native apps, all of Apple's own services) defeats MITM by design — mitigated by automatic passthrough + clear per-transaction "pinned, tunneled" labeling, but user expectations need managing in the UI.
- **HTTP/2 edge cases** (trailers, server push remnants, flow-control under tee'd bodies) need a dedicated test suite against real origins; budget integration tests with a local h2 origin early.
- **websocket frames, HTTP/3:** explicitly deferred; revisit listener design when QUIC interception becomes worth it.
- **Performance target** to validate in v1: ≥ 500 req/s sustained through MITM with recording on, UI remaining responsive (coalesced batching, §5.1).

## 10. Suggested build order

1. Workspace scaffold; core crate with plain-HTTP proxy + recorder + in-memory bus; Tauri shell streaming a live table.
2. SQLite store + lazy detail pane + filters.
3. CA module + CONNECT/MITM path + onboarding flow.
4. System proxy toggle + crash-restore; passthrough rules; polish (copy-as-cURL, body viewers).
5. Hardening: h2 test suite, throughput benchmark, retention/pruning.
