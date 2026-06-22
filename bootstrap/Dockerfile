# Build from the repository root:
#   docker build -t botwork/bootstrap:local \
#     -f bootstrap/Dockerfile .
#
# Five-stage build supporting `BINARY_SOURCE=source` (default) and
# `BINARY_SOURCE=prebuilt` (CI). See session-broker/Dockerfile for the
# full rationale.

ARG BINARY_SOURCE=source

FROM rust:1.90-slim-bookworm AS rust-toolchain

FROM rust-toolchain AS build-source
WORKDIR /build
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

# hadolint ignore=DL3008
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Production-only binary. Bootstrap has no operator CLI — the binary
# does exactly one thing: read bootstrap.yaml, upsert into postgres,
# exit. See bootstrap/src/main.rs for the exit-code contract.
RUN --mount=type=cache,id=botwork-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=botwork-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=botwork-target,target=/build/target,sharing=locked \
    cargo build --release --locked --package botwork-bootstrap --bin botwork-bootstrap \
    && install -D /build/target/release/botwork-bootstrap /out/botwork-bootstrap

FROM scratch AS build-prebuilt
COPY .ci-prebuilt/bootstrap/botwork-bootstrap /out/botwork-bootstrap

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
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork"

COPY --from=rust-toolchain /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=build /out/botwork-bootstrap \
    /usr/local/bin/botwork-bootstrap

# No EXPOSE line: this image is a oneshot, not a server. Production
# invocation pattern is `docker run --rm --network botwork-internal \
# --env-file /var/lib/botwork-db/secret.env \
# -v /etc/botwork/bootstrap.yaml:/etc/botwork/bootstrap.yaml:ro \
# botwork/bootstrap:local`. It connects, upserts, exits.

ENTRYPOINT ["/usr/local/bin/botwork-bootstrap"]
