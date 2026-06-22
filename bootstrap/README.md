# `bootstrap/` — load `bootstrap.yaml` into the database

The bootstrap oneshot is the bridge between today's deploy-time
YAML-as-source-of-truth and the new runtime DB-as-source-of-truth.
It runs at every boot via the systemd oneshot
`botwork-bootstrap.service`, ordered between `botwork-db-migrate`
(which lands the schema) and `botwork-config-broker` (which reads from
the DB post-cutover).

See [RFE #101](https://github.com/botworkz/botwork/issues/101) for
design context. v0 schema is documented in `db/README.md`.

## Lifetime

Bootstrap is **deliberately throwaway**. The plan is for the future
admin-api to own the entity lifecycle (create/update/delete via
authenticated HTTP, with audit + validation). When that lands, this
crate goes away. The convention is to keep the surface narrow so the
deletion is mechanical:

* one config file, one shape;
* one subcommand-less binary;
* no clever "reconciliation": delete-on-diff is out of scope; v0
  only upserts. Removing rows requires the admin-api.

## `bootstrap.yaml` shape

Top-level `tenants:` (each with its workspaces and plugin bindings)
plus a flat top-level `plugins:` list. Per RFE #101 § "plugins are
global, workspaces reference them": the package definition lives once
under top-level `plugins:`, the per-(tenant, workspace) binding sits
inside the tenant tree with an optional override `config:`.

```yaml
tenants:
- name: phlax
  workspaces:
  - name: mcp
    plugins:
    - name: mcp-bash
    - name: mcp-fetch
      config:
        url: https://example.com

plugins:
- name: mcp-bash
  image: ghcr.io/example/mcp-bash:1.0
  egress:
    mode: none
- name: mcp-fetch
  image: ghcr.io/example/mcp-fetch:1.0
  egress:
    allow:
    - host: example.com
      ports: [443]
```

Validation is intentionally narrow (see `config.rs`):

* every `tenants[].workspaces[].plugins[].name` must appear under
  top-level `plugins:`;
* `tenants[].name` is globally unique;
* `tenants[].workspaces[].name` is unique within a tenant;
* `tenants[].workspaces[].plugins[].name` is unique within a workspace;
* `plugins[].name` is globally unique;
* `serde(deny_unknown_fields)` — typos become load errors, not silent
  drops.

Deep schema validation of the `egress` block lives in config-broker
(and will keep living there post-cutover): bootstrap stores it as
opaque JSON, the same way the DB does.

## Idempotency

Every operation is `find-then-INSERT-or-UPDATE` on the join keys:

* `tenant` keyed on `name`,
* `workspace` keyed on `(tenant_id, name)`,
* `plugin` keyed on `name`,
* `workspace_plugin` keyed on `(workspace_id, plugin_id)`.

Re-running with an unchanged yaml is a no-op observable only in
`updated_at` bumps — and even those don't change unless the
comparable columns differ. That property matters: the systemd unit
restarts at every boot, and we want "we re-ran bootstrap" to never
be a behaviour change.

The whole apply runs in a single transaction. Either the boot sees the
full new state of bootstrap.yaml or it sees the previous state — never
a partial merge. config-broker's hot-path reads happen against the
committed state.

## Wire / env contract

| Env var                       | Default                          | Notes                                   |
|-------------------------------|----------------------------------|-----------------------------------------|
| `BOTWORK_DATABASE_URL`        | _required_                       | Same shape as every other consumer.     |
| `BOTWORK_BOOTSTRAP_CONFIG`    | `/etc/botwork/bootstrap.yaml`    | Override-only; production renders the default. |

## Exit codes (production oneshot)

| Code | Meaning                                                       |
|------|---------------------------------------------------------------|
| 0    | Apply succeeded (no-op or mutations both count as success).   |
| 2    | `BOTWORK_DATABASE_URL` is not set.                            |
| 3    | Connection to postgres failed.                                |
| 4    | Bootstrap config file missing / read failure.                 |
| 5    | Bootstrap config validation failure (yaml / refs / uniqueness).|
| 6    | Database mutation failed mid-apply.                           |

systemd `Type=oneshot` on `botwork-bootstrap.service` picks up the exit
code; non-zero blocks every subsequent broker unit on the boot chain,
exactly like `botwork-db-migrate.service`.

## Container image

`botwork/bootstrap:local`, built from `bootstrap/Dockerfile`.

Production invocation pattern:

```
docker run --rm \
  --network botwork-internal \
  --env-file /var/lib/botwork-db/secret.env \
  -v /etc/botwork/bootstrap.yaml:/etc/botwork/bootstrap.yaml:ro \
  botwork/bootstrap:local
```

The container runs as the broker uid (1100), per the workspace
convention. distroless `base-nossl-debian12:nonroot` runtime, same
posture as db-migrate.

## Test posture

Same rails as `db/`: testcontainers stays in `[dev-dependencies]` and
no test reads `BOTWORK_DATABASE_URL`. The integration test
`tests/bootstrap_smoke.rs` exercises the full apply path:

1. spin a real postgres, run `Migrator::up`,
2. apply a minimal `BootstrapConfig` (one tenant, one workspace, two
   plugins, one binding with a config blob and one without),
3. assert the expected row counts via the resolve-shape JOIN that
   config-broker will run post-cutover,
4. re-apply the same config and assert zero mutations
   (idempotency — boot can restart safely),
5. mutate a plugin image + a binding config, re-apply, assert the
   expected per-table update counts.

Post-#127 there is no bootstrap container any more; the equivalent
end-to-end production-path proof now lives in admin-api's smoke test
(invoked by the `admin-api` job in `.github/workflows/ci.yml`), which
exercises the same `apply()` library function via the `botwork-tools
bootstrap` CLI.
