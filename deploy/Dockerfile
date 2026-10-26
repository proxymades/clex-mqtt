FROM node:22.21.1-bookworm-slim AS frontend
WORKDIR /build
COPY package.json package-lock.json ./
COPY frontend/package.json ./frontend/package.json
RUN npm ci
COPY frontend ./frontend
RUN npm run ui:build

FROM rust:1.93.1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY db/migrations ./db/migrations
COPY --from=frontend /build/frontend/dist ./frontend/dist
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 clexmqtt \
    && useradd --system --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin clexmqtt
WORKDIR /app
COPY --from=builder --chmod=0755 /build/target/release/clex-mqtt /usr/local/bin/clex-mqtt
COPY LICENSE THIRD_PARTY_NOTICES.md /usr/share/doc/clex-mqtt/
COPY third-party /usr/share/doc/clex-mqtt/third-party
USER 10001:10001
EXPOSE 1883 8085
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8085/health/ready || exit 1
ENTRYPOINT ["/usr/local/bin/clex-mqtt"]
CMD ["serve"]
