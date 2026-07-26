# botwork-ui

`botwork-ui` is the operator-facing Leptos client that sits in front of
`botwork-api`. See [RFE #106](https://github.com/botworkz/botwork/issues/106) for
the API design context and
[botworkz/space#311](https://github.com/botworkz/space/issues/311)
for the Phase 2 URL reshape that this crate implements.

This directory is **two crates and a trunk project**:

```
ui/
├── README.md             ← you are here
├── wasm/                 ← the Leptos CSR client (compiles to wasm32)
│   ├── Cargo.toml        ← cdylib + rlib, leptos = "0.8" csr
│   ├── Trunk.toml        ← build config + dev proxy → api
│   ├── index.html        ← trunk template
│   ├── input.css         ← Tailwind entry + shadcn theme tokens
│   ├── tailwind.config.js
│   ├── src/
│   │   └── lib.rs        ← App component + #[wasm_bindgen(start)]
│   └── dist/             ← (gitignored) trunk build --release output
└── server/               ← tiny axum binary that embeds dist/ and serves it
    ├── Cargo.toml
    ├── src/
    │   ├── lib.rs        ← module docs + build_router export
    │   ├── handler.rs    ← include_dir! + /health, /login, /{tenant}/*
    │   └── main.rs       ← env-driven bind + axum::serve
    └── tests/
        └── integration.rs
```

## Why two crates?

* `wasm/` targets `wasm32-unknown-unknown`. It depends on
  `leptos`/`wasm-bindgen`/`web-sys` and cannot be built for the host. It is
  excluded from `[workspace.default-members]` so plain `cargo build` / `cargo
  test` at the repo root skip it; CI exercises it explicitly with
  `--target wasm32-unknown-unknown` and via `trunk build`.
* `server/` targets the native host. It depends on `axum` and `include_dir`,
  pulls `wasm/dist/` into the binary at compile time, and produces a distroless
  container symmetric with `api`.

## UI surface (Phase 2)

### Server routes (`server/`)

| Path | Behaviour |
|------|-----------|
| `GET /health` | `{ "status": "ok" }` — liveness probe |
| `GET /login` | SPA shell (login page) |
| `GET /static/*` | Static assets from embedded bundle |
| `GET /{tenant}` | SPA shell (redirects to `/{tenant}/` client-side) |
| `GET /{tenant}/` | SPA shell |
| `GET /{tenant}/*rest` | SPA shell (deep-link fallback for client-side router) |

**Deleted in Phase 2:** `/admin/*` and `/admin/index.html`. No compat shim.

### Client-side routes (`wasm/`)

| Path | Page |
|------|------|
| `/login` | Login form — tenant + password; `POST /api/auth/login`; on success navigates to `/{tenant}/` |
| `/{tenant}/` | Dashboard — aggregate counts |
| `/{tenant}/workspaces` | Workspace list + create |
| `/{tenant}/workspaces/{id}` | Workspace detail + edit + delete |
| `/{tenant}/bindings` | Workspace-plugin binding list + create |
| `/{tenant}/bindings/{wid}/{pid}` | Binding detail + edit + delete |
| `/{tenant}/sessions` | Agent session list (read-only) |
| `/{tenant}/sessions/{id}` | Session detail |
| `/{tenant}/workers` | Session worker list (read-only) |
| `/{tenant}/workers/{id}` | Worker detail |

**Deleted in Phase 2:** `/admin/*` client routes. The tenant is now a first-class
router parameter in every page component.

## Login flow

1. On app boot, `/api/auth/whoami` is probed. If it returns 200, the `{tenant}`
   from the response is used to navigate to `/{tenant}/`. If it returns 401,
   the router redirects to `/login`.
2. The login page POSTs `{ tenant, password }` JSON to `POST /api/auth/login`
   (implemented in `botwork-extra`'s auth-broker, proxied by envoy). On success
   the browser receives an HttpOnly `botwork_cap` cookie and JSON `{ bearer,
   tenant, lease_id, expires_at }`. The SPA navigates to `/{tenant}/`.
3. The logout button (top nav, visible when authenticated) POSTs to
   `POST /api/auth/logout`. The cap cookie is cleared server-side and the SPA
   navigates to `/login`.

Cookie name: `botwork_cap`. All fetch calls use `credentials: 'include'` so the
browser attaches the cookie automatically.

## API calls

All tenant-scoped API calls target `/api/tenant/{tenant}/*` where `{tenant}`
is extracted from the current URL params. The SPA never embeds the tenant in
request bodies. See `wasm/src/api.rs` for the full call surface.

## Build

You'll need `trunk` and the wasm32 target:

```bash
cargo install trunk --version 0.21.5 --locked
rustup target add wasm32-unknown-unknown
curl -fsSL https://github.com/tailwindlabs/tailwindcss/releases/download/v3.4.17/tailwindcss-linux-x64 \
  -o ~/.cargo/bin/tailwindcss
chmod +x ~/.cargo/bin/tailwindcss
```

Then:

```bash
# 1. Build the WASM bundle into ui/wasm/dist/
cd ui/wasm
trunk build --release

# 2. Build the server that embeds the bundle
cd ../..
cargo build --release -p botwork-ui-server

# 3. Run it (binds 0.0.0.0:9500 by default)
./target/release/botwork-ui-server
```

## Dev loop

Live-reload, no docker required:

```bash
# Terminal 1 — api against a local postgres
export BOTWORK_DATABASE_URL=******127.0.0.1/botwork
cargo run -p botwork-api

# Terminal 2 — trunk dev server with HMR + proxy
cd ui/wasm
trunk serve
```

Open `http://127.0.0.1:8080/`. Trunk proxies `/api/*` to `127.0.0.1:9400`.

## Trust posture

docker network is the trust boundary. No `--publish`. envoy fronts
`/{tenant}/*` and `/login`; ext_authz at envoy fronts `/api/*`.
The SPA is credless — it piggybacks the `botwork_cap` cookie or
uses a bearer from local state.

## Environment variables

- `BOTWORK_UI_BIND` (default: `0.0.0.0:9500`) — bind address (never published).
- `RUST_LOG` — tracing-subscriber filter; defaults to `info`.

## Exit codes

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| 0    | normal exit (currently unreachable — `axum::serve` runs forever).    |
| 4    | Failed to bind `BOTWORK_UI_BIND`.                                    |
| 5    | `axum::serve` returned an error.                                     |

## References

- [botworkz/space#311](https://github.com/botworkz/space/issues/311) — Phase 2 URL reshape
- [RFE #106](https://github.com/botworkz/botwork/issues/106) — original admin-api RFE
