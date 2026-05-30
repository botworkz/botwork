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
- `BOTWORK_LAUNCHER_MEMORY_LIMIT=<value>`: docker `--memory` for plugin containers (default `512m`)
- `BOTWORK_LAUNCHER_READ_ONLY_ROOTFS=<true|false>`: opt-in `--read-only` root fs for plugin containers; left off by default because some plugins still need writable runtime paths
- `BOTWORK_PLUGIN_UID=<uid>` / `BOTWORK_PLUGIN_GID=<gid>`: uid/gid passed to `docker run --user`
- `BOTWORK_LAUNCHER_IMAGE_ALLOWLIST_REGEX=<regex>`: image allowlist

The socket group/mode and the `SO_PEERCRED` allowlist are deliberate belt-and-braces checks: the kernel should block the wrong peers before connect, and the launcher should still reject them if filesystem permissions drift.

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
