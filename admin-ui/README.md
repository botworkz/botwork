# botwork-admin-ui

`botwork-admin-ui` is the operator-facing Leptos client that sits
in front of `botwork-admin-api`. See [RFE #106](https://github.com/botworkz/botwork/issues/106)
for the API design context.

This directory is **two crates and a trunk project**:

```
admin-ui/
├── README.md             ← you are here
├── wasm/                 ← the Leptos CSR client (compiles to wasm32)
│   ├── Cargo.toml        ← cdylib + rlib, leptos = "0.8" csr
│   ├── Trunk.toml        ← build config + dev proxy → admin-api
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
    │   ├── handler.rs    ← include_dir! + /healthz, /admin/*
    │   └── main.rs       ← env-driven bind + axum::serve
    └── tests/
        └── integration.rs
```

## Why two crates?

* `wasm/` targets `wasm32-unknown-unknown`. It depends on
  `leptos`/`wasm-bindgen`/`web-sys` and cannot be built for the
  host. It is excluded from `[workspace.default-members]` so plain
  `cargo build` / `cargo test` at the repo root skip it; CI exercises
  it explicitly with `--target wasm32-unknown-unknown` and via
  `trunk build`.
* `server/` targets the native host. It depends on `axum` and
  `include_dir`, pulls `wasm/dist/` into the binary at compile time,
  and produces a distroless container symmetric with `admin-api`.

Splitting them is the only way to keep `cargo check --workspace`
honest on the host: a single crate that both compiles to wasm AND
links against tokio multi-thread is impossible. Two crates with
disjoint dependency graphs is the standard Leptos-CSR layout (see
`botworkz/gander`'s `gander-chat` for the same split, just without
the embedding server).

## UI surface

* `wasm/`: full operator UI with routed entity pages:
  * Tenants / Workspaces / Plugins / Bindings: full CRUD.
  * Sessions / Workers: read-only list + detail.
  * Dashboard: aggregate counts across all entities.
* `server/`:
  * `GET /healthz` — `{ "status": "ok" }`. Liveness probe for
    systemd + goss.
  * `GET /admin/` and `GET /admin/index.html` — the trunk-emitted
    SPA shell from the embedded bundle.
  * `GET /admin/*path` — any other file from the embedded bundle,
    falling back to `index.html` so client-side router deep links
    survive a hard reload.

`/admin/api/*` is **not** served here. In production the ingress
envoy routes that prefix to `admin_api:9400`; in the dev loop the
trunk dev server proxies it (see `wasm/Trunk.toml`).

## Build

You'll need `trunk` and the wasm32 target installed on your dev box:

```bash
cargo install trunk --version 0.21.5 --locked
rustup target add wasm32-unknown-unknown
curl -fsSL https://github.com/tailwindlabs/tailwindcss/releases/download/v3.4.17/tailwindcss-linux-x64 \
  -o ~/.cargo/bin/tailwindcss
chmod +x ~/.cargo/bin/tailwindcss
```

Then:

```bash
# 1. Build the WASM bundle into admin-ui/wasm/dist/
#    (Trunk pre_build hook runs Tailwind automatically)
cd admin-ui/wasm
trunk build --release

# 2. Build the server that embeds the bundle
cd ../..
cargo build --release -p botwork-admin-ui-server

# 3. Run it (binds 0.0.0.0:9500 by default)
./target/release/botwork-admin-ui-server
```

The container image runs steps 1+2 in one multi-stage Dockerfile and
the runtime stage carries only the final binary plus libgcc_s:

```bash
earthly +admin-ui-image
# or
docker build -t botwork/admin-ui:local -f admin-ui/Dockerfile .
```

## Dev loop

Live-reload, no docker required:

```bash
# Terminal 1 — admin-api against a local postgres
export BOTWORK_DATABASE_URL=postgres://botwork:smoke@127.0.0.1/botwork
cargo run -p botwork-admin-api

# Terminal 2 — trunk dev server with HMR + proxy
cd admin-ui/wasm
trunk serve
```

Open `http://127.0.0.1:8080/`. The browser fetches
`/admin/api/v1/health` → trunk dev server proxies to
`127.0.0.1:9400/admin/api/v1/health`. No CORS. Edit Rust source →
trunk rebuilds wasm, re-runs Tailwind via hook, browser reloads.

## Styling

`admin-ui/wasm` now uses Tailwind CSS + `leptos-shadcn-*` component
crates (pinned versions in `wasm/Cargo.toml`). The Tailwind input is
`wasm/input.css`, with shadcn-style CSS variable tokens and dark mode
enabled by default via `class="dark"` on `<body>` in `wasm/index.html`.

## Production invocation pattern

Mirrors the other broker units (added in the vm-side companion PR):

```bash
docker run --rm --name botwork-admin-ui \
  --network botwork-internal --network-alias admin_ui \
  --user 1100:1100 \
  botwork/admin-ui:local
```

Version probe: `botwork-admin-ui-server --version` (or `-V`).

Operator reach (once the vm-side companion lands):

```
browser  ──TLS──>  envoy (ingress)
                     │
                     ├── /admin/api/*  → admin_api:9400 (JSON)
                     └── /admin/*      → admin_ui:9500  (bundle)
```

## Trust posture

Same as every other broker: docker network is the trust boundary.
No `--publish`. ext_authz at envoy fronts both `/admin/*` and
`/admin/api/*`; admin-ui itself is credless.

## Environment variables

- `BOTWORK_ADMIN_UI_BIND` (default: `0.0.0.0:9500`) — bind address.
  **Do not** add a port publish for this service.
- `RUST_LOG` — standard `tracing-subscriber` filter; defaults to `info`.

## Exit codes

| Code | Meaning                                                              |
|------|----------------------------------------------------------------------|
| 0    | normal exit (currently unreachable — `axum::serve` runs forever).    |
| 4    | Failed to bind `BOTWORK_ADMIN_UI_BIND`.                              |
| 5    | `axum::serve` returned an error (transport / shutdown failure).      |

(No exit codes 2/3 because admin-ui has no DB connection — those
slots are reserved by convention with admin-api.)
