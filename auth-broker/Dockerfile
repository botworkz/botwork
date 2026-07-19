# Build from the repository root:
#   docker build -t botwork/auth-broker:local \
#     -f containers/auth-broker/Dockerfile .

# Pin builder to bookworm so the binary links against glibc 2.36, matching
# the distroless base-nossl-debian12 runtime. The default rust:1.90-slim
# tag is trixie-based (glibc 2.41) and produces binaries that fail on
# the runtime with `GLIBC_2.39 not found`.
FROM rust:1.90-slim-bookworm AS builder

WORKDIR /build

# Workspace members listed in the root Cargo.toml must all be
# resolvable at `cargo build` time — even if `botwork-auth-broker`
# doesn't depend on them — because cargo walks the whole workspace
# graph during resolution. Each new top-level crate added to
# `[workspace] members` requires a matching COPY line here.
COPY vault/ ./vault/
COPY opaque-handshake/ ./opaque-handshake/
COPY auth-broker/ ./auth-broker/
COPY VERSION ./VERSION
COPY Cargo.toml Cargo.lock ./

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG BOTWORK_GIT_SHA=""
RUN BOTWORK_GIT_SHA="${BOTWORK_GIT_SHA}" cargo build --release --locked --package botwork-auth-broker

# ── Runtime ─────────────────────────────────────────────────────────────────

FROM gcr.io/distroless/base-nossl-debian12:nonroot

ARG GIT_SHA=""
ARG BOTWORK_VERSION=""
LABEL org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.source="https://github.com/botworkz/botwork-extra"
LABEL org.opencontainers.image.version="${BOTWORK_VERSION}"

# Rust's precompiled std links libgcc_s.so.1 as NEEDED (stack unwinder),
# and panic = "abort" doesn't drop it because std itself was built
# upstream with panic = "unwind". base-nossl doesn't ship libgcc_s,
# so copy it from the builder.
COPY --from=builder /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=builder /build/target/release/botwork-auth-broker \
    /usr/local/bin/botwork-auth-broker

EXPOSE 9100
EXPOSE 9101

ENTRYPOINT ["/usr/local/bin/botwork-auth-broker"]
