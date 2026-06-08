# syntax=docker/dockerfile:1
FROM rust:slim-bookworm AS builder

WORKDIR /app
COPY . .

ARG PURGERY_ALLOW_RELEASE_BLOCKERS=
ENV PURGERY_ALLOW_RELEASE_BLOCKERS=${PURGERY_ALLOW_RELEASE_BLOCKERS}

RUN cargo build --release --workspace

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    openssh-client \
    rsync \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/purgery-client /usr/local/bin/
COPY --from=builder /app/target/release/purgery-server /usr/local/bin/

ENTRYPOINT ["purgery-server"]
