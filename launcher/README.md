# botwork-launcher (Rust)

This crate provides the Rust `botwork-launcher` implementation.

The HTTP+JSON contract matches the historical Python launcher behavior.

## Build

```bash
cargo build --release
```

The binary is produced at:
- `target/release/botwork-launcher`

## Runtime requirements

`botwork-launcher` must run on the host as root (or with `CAP_SYS_ADMIN`) so it can run `docker`, `mount`, and `umount` operations.

## Socket activation

Socket activation is read through [`listenfd`](https://crates.io/crates/listenfd), which adopts the inherited fd into a stdlib `UnixListener` and validates `AF_UNIX` + `SOCK_STREAM` + sets `FD_CLOEXEC`. The launcher additionally rejects any `LISTEN_FDS` count other than `1` and confirms `SO_ACCEPTCONN` on the inherited fd before promoting it to the tokio runtime.

We switched from `libsystemd` to `listenfd` for two reasons:

* It encapsulates the one mandatory `unsafe { from_raw_fd }` at the fd-adoption boundary so the launcher crate can stay inside the workspace-wide `unsafe_code = "forbid"` lint.
* Its dep tree (`libc`, `uuid`) is a small subset of `libsystemd`'s (which pulls in `hmac`, `sha2`, `nom`, `once_cell`, `thiserror`, `nix`).

`listenfd`'s `LISTEN_PID` handling is a strict superset of `libsystemd`'s — when systemd sets `LISTEN_PID` (always, in production) the semantics are identical; when it is unset/empty (e.g. running under `systemfd` for dev), `listenfd` tolerates it where `libsystemd` errors. Our production unit always sets `LISTEN_PID`, so the production code path is unchanged.

Production should run the launcher via systemd socket activation so systemd owns the socket path lifecycle and passes a single listener fd to the process.
The launcher socket is the whole privilege boundary for `docker run` and `mount`: it must never be world-accessible.

The expected contract is:
- one `Accept=no` socket unit
- `ListenStream=/run/botwork/launcher.sock`
- a single `AF_UNIX` `SOCK_STREAM` listener passed to the launcher

Example socket unit:

```ini
[Socket]
ListenStream=/run/botwork/launcher.sock
SocketMode=0660
SocketGroup=botwork-broker
Accept=no
```

When started without `LISTEN_FDS` (for example under local `cargo run`), the launcher falls back to self-binding the configured socket path.
That self-bind path uses:
- `BOTWORK_LAUNCHER_SOCKET_GROUP=<group name or gid>` to switch the socket to `0660` and chown the socket group
- owner-only `0600` when `BOTWORK_LAUNCHER_SOCKET_GROUP` is unset

The socket-activated path does **not** chmod/chown the inherited fd; set `SocketMode=0660` and `SocketGroup=` in the `.socket` unit to match the broker identity.

## Security-sensitive environment

- `BOTWORK_LAUNCHER_ALLOWED_UID=<uid>`: allow peers whose `SO_PEERCRED` uid matches
- `BOTWORK_LAUNCHER_ALLOWED_GID=<gid>`: allow peers whose `SO_PEERCRED` gid matches
  - if neither is set, the launcher defaults to its own effective uid/gid for local development; production should set the broker uid/gid explicitly
- `BOTWORK_LAUNCHER_PIDS_LIMIT=<count>`: docker `--pids-limit` for plugin containers (default `256`)
- `BOTWORK_LAUNCHER_CPU_LIMIT=<value>`: docker `--cpus` for plugin containers (default `1.0`)
- `BOTWORK_LAUNCHER_MEMORY_LIMIT=<value>`: docker `--memory` for plugin containers (default `512m`)
- `BOTWORK_LAUNCHER_READ_ONLY_ROOTFS=<true|false>`: opt-in `--read-only` root fs for plugin containers; left off by default because some plugins still need writable runtime paths
- `BOTWORK_PLUGIN_UID=<uid>` / `BOTWORK_PLUGIN_GID=<gid>`: uid/gid passed to `docker run --user`
- `BOTWORK_LAUNCHER_IMAGE_ALLOWLIST_REGEX=<regex>`: image allowlist
- `BOTWORK_LAUNCHER_EGRESS_PROXY=<url>` (optional): when set, the
  launcher injects `HTTPS_PROXY`, `HTTP_PROXY` (both equal to
  `<url>`) and `NO_PROXY=localhost,127.0.0.1` into every spawned
  plugin container. The URL must start with `http://` or `https://`,
  must not contain whitespace, and must not include a path (e.g.
  `http://egress_envoy:3128`). When unset (default) no proxy env vars
  are injected and plugins reach the network directly. Intended to be
  set on `botwork-launcher.service` by `vm 0.3.4+` once the egress
  envoy unit lands; see [botworkz/botwork#92] for the cycle 2B
  rollout. Caller-supplied env in the `/launch` payload wins if it
  sets one of these names — the injection is additive, not
  authoritative.

The socket group/mode and the `SO_PEERCRED` allowlist are deliberate belt-and-braces checks: the kernel should block the wrong peers before connect, and the launcher should still reject them if filesystem permissions drift.

## Per-container environment injection

- `/launch` accepts an optional `env` field. When omitted or `null`, launcher behavior is unchanged and no `-e` flags are added to `docker run`.
- When present, `env` must be an array of objects with exactly `{ "name": "...", "value": "..." }` string fields.
- `name` must match `^[A-Z_][A-Z0-9_]*$`, must not start with `DOCKER_`, and must not be one of: `PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`.
- `value` is forwarded verbatim to Docker and accepts all UTF-8 content except embedded NUL (`\0`) bytes.
- Additional guardrails: max 64 env entries, max value length 64 KiB, duplicate names rejected.
- Environment variable values are never logged by launcher. Success logs include only `env_count=<N>`.
- Non-goal: launcher does not fetch secrets. Upstream components (typically `session-broker` using `botwork-auth-broker`) are responsible for obtaining values and providing them in `/launch`.

## Per-launch resource overrides

- `/launch` accepts an optional `resources` object with optional `cpus`, `memory`, and `pids` keys.
- `cpus` and `memory` must be non-empty strings, and `pids` must be an integer in `1..=4294967295`.
- Unknown keys in `resources` are rejected.
- Any omitted key falls back to the launcher's configured defaults (`BOTWORK_LAUNCHER_{CPU,MEMORY,PIDS}_LIMIT` or compile-time defaults).

## Logging

This launcher intentionally logs with plain `println!` to stdout instead of using `tracing` or `log`.
It mirrors the Python implementation's `logging.basicConfig(stream=sys.stdout, format="%(message)s")` behavior, with the same line shape plus the `[botwork-launcher]` prefix.
That keeps output compatible for operators piping either implementation to journald or other log aggregators.

Key operator-visible log lines now include:
- `socket-activated: using fd ...` or `self-bind: no LISTEN_FDS, binding ...`
- `accept loop ready`, `accepted connection ...`, and `accept error: ...`
- `rejected unauthorized peer (...)`
- `request: method=... path=...`
- `error_response: status=... message=...`
- `launch ok: ...`, `bind-agent ok: ...`, `teardown ok: ...`
