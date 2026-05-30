# Build from the repository root:
#   docker build -t botwork/session-broker:local \
#     -f containers/session-broker/Dockerfile .

# Pin builder to bookworm so the binary links against glibc 2.36, matching
# the distroless base-nossl-debian12 runtime. The default rust:1.90-slim
# tag is trixie-based (glibc 2.41) and produces binaries that fail on
# the runtime with `GLIBC_2.39 not found`.
FROM rust:1.90-slim-bookworm AS builder

WORKDIR /build

COPY session-broker/ ./session-broker/
COPY launcher/ ./launcher/
COPY tools/ ./tools/
COPY Cargo.toml ./
COPY Cargo.toml Cargo.lock ./

RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN cargo build --release --package botwork-session-broker

# Runtime
FROM gcr.io/distroless/base-nossl-debian12:nonroot

# Rust's precompiled std links libgcc_s.so.1 as NEEDED (stack unwinder),
# and panic = "abort" doesn't drop it because std itself was built
# upstream with panic = "unwind". base-nossl doesn't ship libgcc_s,
# so copy it from the builder.
COPY --from=builder /lib/x86_64-linux-gnu/libgcc_s.so.1 \
    /lib/x86_64-linux-gnu/libgcc_s.so.1

COPY --from=builder /build/target/release/botwork-session-broker \
    /usr/local/bin/botwork-session-broker

EXPOSE 9001 9002

ENTRYPOINT ["/usr/local/bin/botwork-session-broker"]
