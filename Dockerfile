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
RUN curl -fsSL https://sprites.dev/install.sh | bash && sprite version

COPY --from=builder /app/target/release/swarm /usr/local/bin/swarm
COPY --from=builder /usr/local/cargo/bin/tokei /usr/local/bin/tokei
COPY crates/swarm/static /app/static

ENV PORT=3000 \
    DATABASE_URL=sqlite:/data/swarm.db?mode=rwc \
    DATA_DIR=/data \
    SWARM_STATIC_DIR=/app/static \
    CHROMIUM_BIN=/usr/bin/chromium
VOLUME /data
EXPOSE 3000
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/swarm"]
