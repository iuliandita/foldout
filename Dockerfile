# syntax=docker/dockerfile:1
FROM oven/bun:1.4.3@sha256:ec06c3b6cea04192ae6770c434f668ca41d343ad19fa6472216c7b48be39c598 AS frontend
WORKDIR /build
COPY tools/package.json tools/bun.lock ./tools/
RUN bun install --cwd tools --frozen-lockfile
COPY web/package.json web/bun.lock ./web/
RUN ./tools/node_modules/.bin/bun install --cwd web --frozen-lockfile
COPY scripts/bun scripts/generate-api ./scripts/
COPY api ./api
COPY web ./web
RUN ./scripts/generate-api /tmp/schema.d.ts \
    && cmp /tmp/schema.d.ts web/src/lib/api/schema.d.ts \
    && ./scripts/bun run --cwd web check \
    && ./scripts/bun run --cwd web build

FROM rust:1.99.1-alpine3.23@sha256:2a941e16ec6e357ff7112e92336087772584122b6ae4b30eb36cb9adff5b5458 AS backend
ARG TARGETARCH
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apk add --no-cache musl-dev pkgconf
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs && cargo fetch --locked && rm src/main.rs
COPY build.rs ./
COPY migrations ./migrations
COPY --exclude=**/.verify --exclude=**/target src ./src
COPY --from=frontend /build/web/dist ./web/dist
ARG LIBRARY_BUILD_COMMIT=unknown
RUN --mount=type=cache,id=libraryd-rust-${TARGETARCH},target=/build/target,sharing=locked \
    LIBRARY_BUILD_COMMIT=${LIBRARY_BUILD_COMMIT} cargo build --locked --offline --release --features embedded-ui \
    && cp /build/target/release/libraryd /build/libraryd

FROM alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6 AS archive
ARG TARGETARCH
RUN apk add --no-cache ca-certificates xz tar
WORKDIR /archive
# Alpine's 7zip package omits RAR decoding; use the upstream static build.
RUN case "$TARGETARCH" in \
      amd64) archive_arch=x64; archive_sha=dc99eff5008f1ab79bd7084c68513701547a808a89502bf4133683535ab3c695 ;; \
      arm64) archive_arch=arm64; archive_sha=2389ba20e4d8295e8709c20b6263b69bd1ec4972fe38a04ad7a1badbf595b996 ;; \
      *) echo "Unsupported archive architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && wget -O archive.tar.xz "https://github.com/ip7z/7zip/releases/download/26.03/7z2603-linux-${archive_arch}.tar.xz" \
    && printf '%s  archive.tar.xz\n' "$archive_sha" | sha256sum -c - \
    && tar -xJf archive.tar.xz 7zzs License.txt readme.txt \
    && chmod 755 7zzs

FROM rust:1.99.1-trixie@sha256:931c2c0531a8b143e044f0f3914dfe802f15bb3d1955f0b657e9359327ac2ffc AS backend-debian
ARG TARGETARCH
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && printf 'fn main() {}\n' > src/main.rs && cargo fetch --locked && rm src/main.rs
COPY build.rs ./
COPY migrations ./migrations
COPY --exclude=**/.verify --exclude=**/target src ./src
COPY --from=frontend /build/web/dist ./web/dist
ARG LIBRARY_BUILD_COMMIT=unknown
RUN --mount=type=cache,id=libraryd-rust-debian-${TARGETARCH},target=/build/target,sharing=locked \
    LIBRARY_BUILD_COMMIT=${LIBRARY_BUILD_COMMIT} cargo build --locked --offline --release --features embedded-ui \
    && cp /build/target/release/libraryd /build/libraryd

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS debian
COPY --from=archive /archive/7zzs /usr/bin/7z
COPY --from=archive /archive/License.txt /archive/readme.txt /usr/share/licenses/7zip/
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates poppler-utils util-linux libgcc-s1 wget \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 library \
    && useradd --no-log-init --no-create-home --uid 1000 --gid library --shell /usr/sbin/nologin library \
    && mkdir /state \
    && chown 1000:1000 /state \
    && chmod 700 /state \
    && test -x /usr/bin/prlimit \
    && test -x /usr/bin/7z \
    && test -x /usr/bin/pdfinfo \
    && test -x /usr/bin/pdftoppm
COPY --from=backend-debian /build/libraryd /usr/local/bin/libraryd
ENV LIBRARY_STATE_DIR=/state LIBRARY_LISTEN=0.0.0.0:8787 LIBRARY_ORIGIN=http://localhost:8787
USER 1000:1000
WORKDIR /state
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8787/health/ready || exit 1
ENTRYPOINT ["/bin/sh", "-c", "umask 077; exec /usr/local/bin/libraryd \"$@\"", "libraryd"]

FROM alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
COPY --from=archive /archive/7zzs /usr/bin/7z
COPY --from=archive /archive/License.txt /archive/readme.txt /usr/share/licenses/7zip/
RUN apk add --no-cache ca-certificates poppler-utils util-linux-misc libgcc \
    && addgroup -g 1000 library \
    && adduser -D -H -u 1000 -G library library \
    && mkdir /state \
    && chown 1000:1000 /state \
    && chmod 700 /state \
    && test -x /usr/bin/prlimit \
    && test -x /usr/bin/7z \
    && test -x /usr/bin/pdfinfo \
    && test -x /usr/bin/pdftoppm
COPY --from=backend /build/libraryd /usr/local/bin/libraryd
ENV LIBRARY_STATE_DIR=/state LIBRARY_LISTEN=0.0.0.0:8787 LIBRARY_ORIGIN=http://localhost:8787
USER 1000:1000
WORKDIR /state
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8787/health/ready || exit 1
ENTRYPOINT ["/bin/sh", "-c", "umask 077; exec /usr/local/bin/libraryd \"$@\"", "libraryd"]
