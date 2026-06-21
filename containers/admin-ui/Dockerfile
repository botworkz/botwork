# Build from the repository root:
#   docker build -t botwork/admin-ui:local \
#     -f containers/admin-ui/Dockerfile .
#
# Two-stage build, two unusual wrinkles compared to the other broker
# Dockerfiles:
#
# 1. The builder stage installs `trunk` + the `wasm32-unknown-unknown`
#    target before running cargo. `botwork-admin-ui-wasm` only links
#    against wasm; `botwork-admin-ui-server` embeds its compiled
#    output via `include_dir!`.
# 2. We `cd admin-ui/wasm && trunk build --release` to populate the
#    `dist/` directory that the server crate inlines, then build the
#    server binary as usual.
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
FROM rust:1.90-slim-bookworm AS builder

WORKDIR /build

# Workspace members copied wholesale so cargo's workspace resolver can
# see every member's manifest. We don't strictly need launcher/ or
# tools/ for the admin-ui-server build, but copying selectively here
# diverges from the pattern the other broker Dockerfiles establish —
# keep the layer identical so the build behaviour is uniform.
COPY admin-api/ ./admin-api/
COPY admin-core/ ./admin-core/
COPY admin-ui/ ./admin-ui/
COPY bootstrap/ ./bootstrap/
COPY config-broker/ ./config-broker/
COPY control-plane/ ./control-plane/
COPY db/ ./db/
COPY session-broker/ ./session-broker/
COPY launcher/ ./launcher/
COPY tools/ ./tools/
COPY Cargo.toml Cargo.lock ./

# trunk + wasm32 target. Pin trunk to the same version gander uses
# so cross-project upgrades are mechanical. `cargo install --locked`
# uses the trunk crate's Cargo.lock for reproducibility.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown \
    && cargo install trunk --version 0.21.5 --locked

# Build the WASM bundle. trunk writes into admin-ui/wasm/dist/, which
# is the path `include_dir!` resolves at the next step.
RUN cd admin-ui/wasm && trunk build --release

# Build the server. It embeds admin-ui/wasm/dist/ as a side effect of
# compilation (include_dir! is a proc macro).
RUN cargo build --release \
    --package botwork-admin-ui-server \
    --bin botwork-admin-ui-server

# Runtime
FROM gcr.io/distroless/base-nossl-debian12:nonroot

ARG GIT_SHA=""
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork"

# Rust's precompiled std links libgcc_s.so.1 as NEEDED (stack unwinder),
# and panic = "abort" doesn't drop it because std itself was built
# upstream with panic = "unwind". base-nossl doesn't ship libgcc_s,
# so copy it from the builder. Identical to the other broker
# Dockerfiles.
COPY --from=builder /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=builder /build/target/release/botwork-admin-ui-server \
    /usr/local/bin/botwork-admin-ui-server

EXPOSE 9500

ENTRYPOINT ["/usr/local/bin/botwork-admin-ui-server"]
