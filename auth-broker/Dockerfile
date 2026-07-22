# Build from the repository root:
#   docker build -t botwork/auth-broker:local \
#     -f auth-broker/Dockerfile .
#
# Five-stage build to support two build-source contracts on one
# Dockerfile (see also session-broker, config-broker, control-plane,
# api, db/migration, ui -- same shape across the workspace):
#
#   BINARY_SOURCE=source   (default; local `docker buildx build ...`,
#                           and any bare `docker build`) -- cargo build
#                           runs inside the image via `build-source`.
#                           Cache mounts on registry / git / target let
#                           rebuilds on the same docker host reuse
#                           cargo work across all crate images.
#
#   BINARY_SOURCE=prebuilt (CI; .github/workflows/_crate.yml) -- a binary
#                           that the per-crate runner just `cargo build
#                           --release`-ed on the host (with the host's
#                           `Swatinem/rust-cache@v2` warm) is staged at
#                           .ci-prebuilt/auth-broker/, and
#                           `build-prebuilt` consumes it. No cargo
#                           runs in this branch.
#
# `ci-tools` is the toolchain image CI uses to run cargo on the host
# against a mounted workspace -- `rust-toolchain` plus rustfmt/clippy
# plus any per-crate apt deps that the test surface needs. Adding a
# test-time dependency is a one-place change here.
#
# Pin builder to bookworm so the binary links against glibc 2.36,
# matching the distroless base-nossl-debian12 runtime. The default
# rust:1.90-slim tag is trixie-based (glibc 2.41) and produces
# binaries that fail on the runtime with `GLIBC_2.39 not found`.

ARG BINARY_SOURCE=source

FROM rust:1.97-slim-bookworm AS rust-toolchain

# ── build-source ─────────────────────────────────────────────────────────
# Cargo build from the workspace tree. Cache mounts are shared across
# every botwork-image build on the same docker host via the `botwork-`
# cache id prefix.
FROM rust-toolchain AS build-source
WORKDIR /build
COPY api/ ./api/
COPY api-core/ ./api-core/
COPY auth-broker/ ./auth-broker/
COPY bootstrap/ ./bootstrap/
COPY cli/ ./cli/
COPY config-broker/ ./config-broker/
COPY control-plane/ ./control-plane/
COPY db/ ./db/
COPY launcher/ ./launcher/
COPY opaque-handshake/ ./opaque-handshake/
COPY session-broker/ ./session-broker/
COPY ctl/ ./ctl/
COPY test-support/ ./test-support/
COPY ui/ ./ui/
COPY vault/ ./vault/
COPY version/ ./version/
COPY VERSION ./VERSION
COPY Cargo.toml Cargo.lock ./

ARG BOTWORK_GIT_SHA=""
ENV BOTWORK_GIT_SHA=${BOTWORK_GIT_SHA}

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN --mount=type=cache,id=botwork-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=botwork-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=botwork-target,target=/build/target,sharing=locked \
    cargo build --release --locked --package botwork-auth-broker \
    && install -D /build/target/release/botwork-auth-broker /out/botwork-auth-broker

# ── build-prebuilt ───────────────────────────────────────────────────────
# Receive a binary the CI runner built on the host. The path under
# .ci-prebuilt/ is the *crate* name (not the service / image name) so
# the convention is uniform across the Dockerfiles.
FROM scratch AS build-prebuilt
COPY .ci-prebuilt/auth-broker/botwork-auth-broker /out/botwork-auth-broker

FROM build-${BINARY_SOURCE} AS build

# ── ci-tools ─────────────────────────────────────────────────────────────
# Toolchain image for CI's host-side cargo invocations. No extra apt
# deps for auth-broker today; future test-time deps (e.g. a CLI the
# integration test shells out to) land here, not in the workflow yaml.
FROM rust-toolchain AS ci-tools
# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy

# ── Runtime ──────────────────────────────────────────────────────────────
FROM gcr.io/distroless/base-nossl-debian12:nonroot

ARG GIT_SHA=""
ARG BOTWORK_VERSION=""
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork"
LABEL org.opencontainers.image.version="${BOTWORK_VERSION}"

# Rust's precompiled std links libgcc_s.so.1 as NEEDED (stack unwinder),
# and panic = "abort" doesn't drop it because std itself was built
# upstream with panic = "unwind". base-nossl doesn't ship libgcc_s,
# so copy it from the toolchain image. Identical to the other broker
# Dockerfiles.
COPY --from=rust-toolchain /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=build /out/botwork-auth-broker \
    /usr/local/bin/botwork-auth-broker

EXPOSE 9100
EXPOSE 9101

ENTRYPOINT ["/usr/local/bin/botwork-auth-broker"]
