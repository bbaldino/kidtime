# kidtime-server image. Build from the repo root:
#   docker build -t kidtime-server .

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p server \
    && cp target/release/kidtime-server /kidtime-server \
    && mkdir /data && chown 65532:65532 /data

# Distroless: glibc and CA certificates only, no shell, runs as uid 65532
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /kidtime-server /usr/local/bin/kidtime-server
# Owned by the nonroot user, so a fresh volume mounted here starts out writable
COPY --from=build --chown=65532:65532 /data /data
ENV KIDTIME_LISTEN=0.0.0.0:8470 \
    KIDTIME_DB=/data/kidtime.db
VOLUME /data
EXPOSE 8470
ENTRYPOINT ["/usr/local/bin/kidtime-server"]
