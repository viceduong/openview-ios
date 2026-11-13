# syntax=docker/dockerfile:1
#
# OpenView relay (ovrelay) container image.
#
#   docker build -f packaging/docker/Dockerfile -t openview-relay .
#   docker run -d --name ovrelay -p 4569:4569 --read-only --cap-drop ALL \
#     --security-opt no-new-privileges openview-relay
#
# Build context: the repository root. The result is one static (musl)
# binary on a distroless base: no shell, no package manager, runs as
# uid 65532. See docs/INSTALL.md, "Run a relay on Linux/Docker".

# Keep in step with `rust-version` in the root Cargo.toml.
ARG RUST_VERSION=1.92

FROM rust:${RUST_VERSION}-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
# Only what cargo needs. rust-toolchain.toml is deliberately left out: it
# pins "stable" plus Windows / iOS targets, which would make rustup inside
# the image download a second toolchain instead of using the image's.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The release profile already strips symbols. musl links statically.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ovrelay \
    && cp target/release/ovrelay /ovrelay

FROM gcr.io/distroless/static-debian12:nonroot
ARG VERSION=0.1.0
LABEL org.opencontainers.image.title="ovrelay" \
      org.opencontainers.image.description="OpenView relay server: pairs a viewer and a host by session id and pipes their end-to-end encrypted traffic" \
      org.opencontainers.image.source="https://github.com/viceduong/openview" \
      org.opencontainers.image.url="https://github.com/viceduong/openview" \
      org.opencontainers.image.documentation="https://github.com/viceduong/openview/blob/main/docs/INSTALL.md" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}"
COPY --from=builder /ovrelay /usr/local/bin/ovrelay
COPY LICENSE /usr/share/doc/ovrelay/LICENSE
# `--bind` reads OVRELAY_BIND, and so does `--healthcheck`: change the port
# here (not with `--bind` on the command line) so both stay in step.
ENV OVRELAY_BIND=0.0.0.0:4569
EXPOSE 4569/tcp
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/ovrelay", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/ovrelay"]
