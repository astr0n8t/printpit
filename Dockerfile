# syntax=docker/dockerfile:1

# ── build: static musl binary, cross-compiled for the target platform ───
# The build stage always runs on the build machine's architecture; cargo-zigbuild (zig as the
# C cross-compiler for ring and SQLite) produces amd64 or arm64 binaries without emulation.
# zig comes from the official `ziglang` PyPI wheel (a zip holding a static binary), pinned by
# SHA-256, so no extra package manager is needed.
FROM --platform=$BUILDPLATFORM rust:1.97-alpine AS build
ARG BUILDARCH
ARG TARGETARCH
ARG ZIG_WHEELS=https://files.pythonhosted.org/packages
RUN set -eu; \
    case "$BUILDARCH" in \
      amd64) whl=db/81/f9f5c7fd68d80b8edff2a641a1c95ac4125356387d298374227040b8e1e8/ziglang-0.15.2-py3-none-manylinux_2_12_x86_64.manylinux2010_x86_64.musllinux_1_1_x86_64.whl; \
             sum=806316453d1ede7174ed3fefb8e5348154629089c3bb5fe812dfd0e172fac130 ;; \
      arm64) whl=53/7d/8c277208250ffa72f12a10f52dfc1d45850f08244093065b40c5f4628260/ziglang-0.15.2-py3-none-manylinux_2_17_aarch64.manylinux2014_aarch64.musllinux_1_1_aarch64.whl; \
             sum=edc0aa60ec964a4cf462d40f68d7de242ddf37fd9a80f2afaee6397059463230 ;; \
      *) echo "unsupported BUILDARCH: $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    wget -qO /tmp/zig.whl "$ZIG_WHEELS/$whl"; \
    echo "$sum  /tmp/zig.whl" | sha256sum -c -; \
    mkdir -p /opt/zig && unzip -q /tmp/zig.whl -d /opt/zig && rm /tmp/zig.whl; \
    chmod +x /opt/zig/ziglang/zig && ln -s /opt/zig/ziglang/zig /usr/local/bin/zig; \
    zig version; \
    cargo install --locked cargo-zigbuild@0.24.0
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl ;; \
      arm64) echo aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target \
 && rustup target add "$(cat /rust-target)"
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry-$TARGETARCH \
    --mount=type=cache,target=/src/target,id=target-$TARGETARCH \
    cargo zigbuild --release --locked --target "$(cat /rust-target)" \
 && cp "target/$(cat /rust-target)/release/printpit" /printpit \
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
