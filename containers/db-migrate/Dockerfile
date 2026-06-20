# Build from the repository root:
#   docker build -t botwork/db-migrate:local \
#     -f containers/db-migrate/Dockerfile .

# Pin builder to bookworm so the binary links against glibc 2.36, matching
# the distroless base-nossl-debian12 runtime. The default rust:1.90-slim
# tag is trixie-based (glibc 2.41) and produces binaries that fail on
# the runtime with `GLIBC_2.39 not found`.
#
# Symmetric with containers/config-broker/Dockerfile and the other broker
# images so the build behaviour is uniform across the workspace.
FROM rust:1.90-slim-bookworm AS builder

WORKDIR /build

# Workspace members copied wholesale: cargo workspace builds need every
# member's manifest to resolve the lockfile, even when we only build one
# package. Matches the pattern in the other broker Dockerfiles.
COPY config-broker/ ./config-broker/
COPY control-plane/ ./control-plane/
COPY db/ ./db/
COPY session-broker/ ./session-broker/
COPY launcher/ ./launcher/
COPY tools/ ./tools/
COPY Cargo.toml Cargo.lock ./

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Only the production binary is shipped. The operator CLI (sea-orm-migration
# `up`/`down`/`status`/etc.) is not built into this image — see
# db/migration/src/main.rs for the rationale. RFE 97 lists CLI exposure as
# a follow-up once there are real migrations whose state is worth inspecting.
RUN cargo build --release --package botwork-migration --bin botwork-migration

# Runtime
FROM gcr.io/distroless/base-nossl-debian12:nonroot

ARG GIT_SHA=""
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork"

# Rust's precompiled std links libgcc_s.so.1 as NEEDED (stack unwinder),
# and panic = "abort" doesn't drop it because std itself was built
# upstream with panic = "unwind". base-nossl doesn't ship libgcc_s,
# so copy it from the builder. Identical to the other broker Dockerfiles.
COPY --from=builder /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=builder /build/target/release/botwork-migration \
    /usr/local/bin/botwork-migration

# No EXPOSE line: this image is a oneshot, not a server. Production
# invocation pattern is `docker run --rm --network botwork-internal \
# --env-file /var/lib/botwork-db/secret.env botwork/db-migrate:local`.
# It connects, runs Migrator::up, exits.

ENTRYPOINT ["/usr/local/bin/botwork-migration"]
