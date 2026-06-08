FROM rust:bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --workspace --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-client rsync \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/purgery-client /usr/local/bin/
COPY --from=builder /app/target/release/purgery-server /usr/local/bin/
ENTRYPOINT ["purgery-client"]
