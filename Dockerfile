# syntax=docker/dockerfile:1
FROM oven/bun:1.4.2@sha256:9114c058aeae42162ee16dd5084b95fe9473970bb6bcb5b232ab1630f0546895 AS frontend
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

FROM rust:1.98.1-alpine3.23@sha256:737ba17e6a2ffe14475b59861cd69f3d7152c29c75140bdbf6750befcfda7e6c AS backend
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
RUN --mount=type=cache,id=libraryd-rust-${TARGETARCH},target=/build/target,sharing=locked \
    cargo build --locked --offline --release --features embedded-ui \
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
