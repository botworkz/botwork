# Build from the repository root:
#   docker build -t botwork/db-migrate:local \
#     -f db/migration/Dockerfile .
#
# Image is named "db-migrate" (the oneshot's role) but the crate is
# `botwork-migration` under `db/migration/`. Five-stage build supporting
# `BINARY_SOURCE=source` (default) and `BINARY_SOURCE=prebuilt` (CI).
# See session-broker/Dockerfile for the full rationale.

ARG BINARY_SOURCE=source

FROM rust:1.90-slim-bookworm AS rust-toolchain

FROM rust-toolchain AS build-source
WORKDIR /build
COPY api/ ./api/
COPY api-core/ ./api-core/
COPY ui/ ./ui/
COPY bootstrap/ ./bootstrap/
COPY botwork-cli/ ./botwork-cli/
COPY config-broker/ ./config-broker/
COPY control-plane/ ./control-plane/
COPY db/ ./db/
COPY launcher/ ./launcher/
COPY opaque-handshake/ ./opaque-handshake/
COPY session-broker/ ./session-broker/
COPY tools/ ./tools/
COPY version/ ./version/
COPY VERSION ./VERSION
COPY Cargo.toml Cargo.lock ./

ARG BOTWORK_GIT_SHA=""
ENV BOTWORK_GIT_SHA=${BOTWORK_GIT_SHA}

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Only the production binary is shipped. The operator CLI (sea-orm-migration
# `up`/`down`/`status`/etc.) is not built into this image — see
# db/migration/src/main.rs for the rationale. RFE 97 lists CLI exposure as
# a follow-up once there are real migrations whose state is worth inspecting.
RUN --mount=type=cache,id=botwork-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=botwork-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=botwork-target,target=/build/target,sharing=locked \
    cargo build --release --locked --package botwork-migration --bin botwork-migration \
    && install -D /build/target/release/botwork-migration /out/botwork-migration

FROM scratch AS build-prebuilt
# Prebuilt path: the CI runner stages the binary under the *crate* path so
# that the .ci-prebuilt/ layout matches the workspace layout exactly.
COPY .ci-prebuilt/db/migration/botwork-migration /out/botwork-migration

FROM build-${BINARY_SOURCE} AS build

FROM rust-toolchain AS ci-tools
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy

# Runtime
FROM gcr.io/distroless/base-nossl-debian12:nonroot

ARG GIT_SHA=""
ARG BOTWORK_VERSION=""
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork"
LABEL org.opencontainers.image.version="${BOTWORK_VERSION}"

COPY --from=rust-toolchain /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=build /out/botwork-migration \
    /usr/local/bin/botwork-migration

# No EXPOSE line: this image is a oneshot, not a server. Production
# invocation pattern is `docker run --rm --network botwork-internal \
# --env-file /var/lib/botwork-db/secret.env botwork/db-migrate:local`.
# It connects, runs Migrator::up, exits.

ENTRYPOINT ["/usr/local/bin/botwork-migration"]
