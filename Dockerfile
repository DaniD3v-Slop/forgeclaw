# The OpenClaw gateway image is node:24-bookworm-slim. Build against the same
# libc so the copied daemon remains runnable in that runtime image.
FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p forgeclaw

FROM ghcr.io/openclaw/openclaw:2026.9.5-browser
USER root
COPY --from=build /build/target/release/forgeclaw /usr/local/bin/forgeclaw
COPY --chown=node:node plugins/forgeclaw /opt/forgeclaw/plugin
USER node
HEALTHCHECK --interval=10s --timeout=2s --start-period=5s --retries=5 \
  CMD curl --fail --silent http://localhost:3080/healthz >/dev/null || exit 1
