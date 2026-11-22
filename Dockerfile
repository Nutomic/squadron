# syntax=docker/dockerfile:1
# #11: runtime image must be able to run the product end to end:
# swarm binary, sprite CLI (drives lanes), tokei (artifact stats),
# Chromium (screenshot queue) and the dashboard static assets.

FROM rust:1.99 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p swarm
# tokei is not packaged for debian; build it once in the rust image.
RUN cargo install tokei --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl chromium fonts-liberation fonts-noto-core \
        tini \
    && rm -rf /var/lib/apt/lists/*
# sprite CLI: official installer (statically linked Go binary).
# SPRITE_INSTALL_BIN_DIR lands it in /usr/local/bin (on PATH; the installer
# otherwise defaults to ~/.local/bin, which is not on PATH here).
# Build-time verification: `sprite -v` — note there is no `sprite version`
# subcommand; --version is a flag.
# Runtime auth is provisioned at boot from the SPRITE_TOKEN Fly secret (see CMD
# below + fly.toml); never bake a token into the image.
RUN curl -fsSL https://sprites.dev/install.sh | SPRITE_INSTALL_BIN_DIR=/usr/local/bin sh \
    && sprite -v

COPY --from=builder /app/target/release/swarm /usr/local/bin/swarm
COPY --from=builder /usr/local/cargo/bin/tokei /usr/local/bin/tokei
COPY crates/swarm/static /app/static

ENV PORT=3000 \
    DATABASE_URL=sqlite:/data/swarm.db?mode=rwc \
    DATA_DIR=/data \
    SWARM_STATIC_DIR=/app/static \
    CHROMIUM_BIN=/usr/bin/chromium \
    SPRITE_CLI=/usr/local/bin/sprite
VOLUME /data
EXPOSE 3000
ENTRYPOINT ["/usr/bin/tini", "--"]
# Boot step (audit #5): the swarm orchestrator shells out to `sprite`, so auth
# is provisioned at boot from the SPRITE_TOKEN Fly secret — never hardcoded
# here. With the secret set, `sprite auth setup` + a real `sprite list` call
# prove the token works before the orchestrator starts. Without the secret,
# the machine still boots and serves the dashboard (runs fail per-lane with a
# clear error until the secret is set — setting it restarts the machine).
CMD ["/bin/sh", "-ec", "\
  if [ -z \"${SPRITE_TOKEN:-}\" ]; then \
    echo 'WARN: SPRITE_TOKEN is not set — dashboard will serve, but lane runs cannot spawn sprites. Run: fly secrets set SPRITE_TOKEN=<org-slug/org-id/token-id/token-value> (generate via the sprites.dev console)'; \
  else \
    sprite auth setup --token \"$SPRITE_TOKEN\" || { echo 'FATAL: sprite auth setup rejected the SPRITE_TOKEN secret'; exit 1; }; \
    sprite list >/dev/null || { echo 'FATAL: sprite list failed after auth setup — token may be invalid or org unreachable'; exit 1; }; \
    echo 'sprite auth OK'; \
  fi; \
  exec /usr/local/bin/swarm"]
