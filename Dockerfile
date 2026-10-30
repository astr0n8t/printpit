# syntax=docker/dockerfile:1

# ── build: static musl binary ───────────────────────────────────────
FROM rust:1.97-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/printpit /printpit \
 && mkdir -p /out/data

# ── runtime: distroless static (CA certs, tzdata, nonroot user) ─────
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /printpit /printpit
COPY --from=build --chown=65532:65532 /out/data /data
ENV PRINTPIT_BIND=0.0.0.0:8080 \
    PRINTPIT_DATA_DIR=/data
USER 65532:65532
EXPOSE 8080
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/printpit", "healthcheck"]
ENTRYPOINT ["/printpit"]
