FROM rust:1.99 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p swarm

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/swarm /usr/local/bin/swarm
CMD ["swarm"]
