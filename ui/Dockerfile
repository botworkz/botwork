# Build from the repository root:
#   docker build -t botwork/ui:local \
#     -f ui/Dockerfile .
#
# Two unusual wrinkles vs the other broker Dockerfiles:
#
#  1. The `build-source` stage installs `trunk` + the
#     `wasm32-unknown-unknown` target before running cargo.
#     `botwork-ui-wasm` only links against wasm;
#     `botwork-ui-server` embeds the compiled output via
#     `include_dir!`.
#  2. `(cd ui/wasm && trunk build --release)` populates the
#     `dist/` directory that include_dir! resolves at compile time,
#     before the server cargo build.
#
# The `BINARY_SOURCE=prebuilt` CI path takes a server binary the
# runner already built (with trunk having run on the host first —
# see ui's `ci-tools` stage and `.github/workflows/_crate.yml`),
# so the runtime stage is byte-identical regardless of which source
# path produced the binary. See session-broker/Dockerfile for the
# full five-stage rationale.
#
# Pin builder to bookworm so the binary links against glibc 2.36,
# matching the distroless base-nossl-debian12 runtime. The default
# rust:1.90-slim tag is trixie-based (glibc 2.41) and produces
# binaries that fail on the runtime with `GLIBC_2.39 not found`.
# Symmetric with the other broker Dockerfiles.

# hadolint global ignore=DL3008
# DL3008 (pin apt-get versions): the upstream rust image and the
# trunk crate-pin together already constrain the surface area; pinning
# every transitive .deb on top adds churn without a real win.

ARG BINARY_SOURCE=source
ARG TAILWINDCSS_VERSION=3.4.17

FROM rust:1.90-slim-bookworm AS rust-toolchain

# Toolchain image with trunk and the wasm32 target on top of plain
# rust-toolchain. The trunk crate-pin matches ui's CI invocation
# so cross-project upgrades are mechanical. `cargo install --locked`
# uses the trunk crate's Cargo.lock for reproducibility.
FROM rust-toolchain AS trunk-toolchain
ARG TAILWINDCSS_VERSION
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown \
    && cargo install trunk --version 0.21.5 --locked \
    && curl -fsSL \
        "https://github.com/tailwindlabs/tailwindcss/releases/download/v${TAILWINDCSS_VERSION}/tailwindcss-linux-x64" \
        -o /usr/local/bin/tailwindcss \
    && chmod +x /usr/local/bin/tailwindcss \
    && tailwindcss --help >/dev/null

FROM trunk-toolchain AS build-source
WORKDIR /build
# Workspace members copied wholesale so cargo's workspace resolver can
# see every member's manifest. We don't strictly need launcher/ or
# tools/ for the ui-server build, but copying selectively here
# diverges from the pattern the other broker Dockerfiles establish —
# keep the layer identical so the build behaviour is uniform.
COPY api/ ./api/
COPY api-core/ ./api-core/
COPY ui/ ./ui/
COPY bootstrap/ ./bootstrap/
COPY cli/ ./cli/
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

# Build the WASM bundle. trunk writes into ui/wasm/dist/, which
# is the path `include_dir!` resolves at the server cargo build below.
# Then build the server binary; it embeds ui/wasm/dist/ as a side
# effect of compilation (include_dir! is a proc macro).
RUN --mount=type=cache,id=botwork-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=botwork-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=botwork-target,target=/build/target,sharing=locked \
    --mount=type=cache,id=botwork-trunk,target=/build/ui/wasm/target,sharing=locked \
    (cd ui/wasm && trunk build --release) \
    && cargo build --release --locked \
        --package botwork-ui-server \
        --bin botwork-ui-server \
    && install -D /build/target/release/botwork-ui-server /out/botwork-ui-server

FROM scratch AS build-prebuilt
COPY .ci-prebuilt/ui/botwork-ui-server /out/botwork-ui-server

FROM build-${BINARY_SOURCE} AS build

# CI uses this image to run the same `(trunk build && cargo build)` pair
# on the host (so target/ lands on the host for Swatinem/rust-cache to
# pick up). Per-crate apt deps for the test surface land here.
FROM trunk-toolchain AS ci-tools
RUN rustup component add rustfmt clippy

# Runtime
FROM gcr.io/distroless/base-nossl-debian12:nonroot

ARG GIT_SHA=""
ARG BOTWORK_VERSION=""
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork"
LABEL org.opencontainers.image.version="${BOTWORK_VERSION}"

# Rust's precompiled std links libgcc_s.so.1 as NEEDED (stack unwinder),
# and panic = "abort" doesn't drop it because std itself was built
# upstream with panic = "unwind". base-nossl doesn't ship libgcc_s,
# so copy it from the toolchain. Identical to the other broker
# Dockerfiles.
COPY --from=rust-toolchain /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=build /out/botwork-ui-server \
    /usr/local/bin/botwork-ui-server

EXPOSE 9500

ENTRYPOINT ["/usr/local/bin/botwork-ui-server"]
