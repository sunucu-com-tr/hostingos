# syntax=docker/dockerfile:1.7
#
# HostingOS — multi-stage image.
#
#   docker build -t hostingos --build-arg HOS_BUILD_COMMIT="$(git rev-parse HEAD)" \
#     --build-arg SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" .
#   docker run --env-file .env -p 8080:8080 hostingos            # serve (web + worker)
#   docker run --env-file .env hostingos migrate                  # one-off commands
#
# Stages: cargo-chef planner → dependency cache (`cargo chef cook`) → release
# build of the `hostingos` binary → small Debian runtime (non-root, rustls only,
# no OpenSSL). Askama templates and SQL migrations are compiled into the
# binary; `static/` and `locales/` are copied next to it.
#
# `--target prebuilt` packages the release binaries built by
# .github/workflows/release.yml instead (context: `docker-dist/<arch>/` =
# an unpacked release tarball); that is how the published multi-arch image
# is made, so image and tarball contain the same binary.
#
# The image never updates itself (docs/UPDATES.md): pull the new tag and
# recreate the container; `hostingos migrate` runs before the new version,
# or set HOS_MIGRATE_ON_START=1 (= `serve --migrate`) and the container
# applies pending migrations itself before it opens its port.
#
# Only the paths below are copied into the image (sources, static/, locales/,
# LICENSE, THIRD-PARTY-NOTICES.md); `.dockerignore` keeps `.env*`, `.git`
# and `target/` out of the build context.

# Builder pinned to an exact cargo-chef release on an exact Rust release
# (Debian bookworm, matching the runtime image's glibc). Bump RUST_VERSION and
# the tag together; `rust-toolchain.toml` says `stable`, which would otherwise
# make rustup download whatever "stable" is on the day of the build.
#
# Both base images are also pinned by digest (the multi-arch index digest
# Docker Hub lists for the tag), so a re-pushed tag cannot change what is
# built. The runtime digest freezes Debian's security updates too: bump it
# (and the builder's) when a new bookworm-slim is published —
#   docker buildx imagetools inspect debian:bookworm-slim
# shows the current index digest.
ARG RUST_VERSION=1.100.0
ARG RUST_IMAGE=lukemathwalker/cargo-chef:0.1.78-rust-1.100.0-bookworm@sha256:9573268a2a874efc1c755a0094a50c25988b2c6a0b7a520ddb7fd08d868d1c43
ARG RUNTIME_IMAGE=debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc

# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS chef
ARG RUST_VERSION
WORKDIR /src
# RUSTUP_TOOLCHAIN overrides rust-toolchain.toml: build with the image's
# toolchain instead of fetching the current stable.
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    RUSTUP_TOOLCHAIN=${RUST_VERSION}

# ---------------------------------------------------------------------------
FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------
FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
COPY rust-toolchain.toml ./
# Dependencies only: this layer is reused until Cargo.toml / Cargo.lock change.
# Keep every ARG below this step: a RUN after an ARG is rebuilt whenever the
# ARG's value changes, which would recompile all dependencies on each commit.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json -p hos-web --bin hostingos
# Build metadata shown in /platform (the build context has no .git): commit
# and a reproducible build date (never the wall clock).
ARG HOS_BUILD_COMMIT=unknown
ARG SOURCE_DATE_EPOCH
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates
# hos-pdf compiles two fonts into the binary (include_bytes! of
# static/fonts/…, crates/hos-pdf/src/font.rs).
COPY static/fonts ./static/fonts
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    HOS_BUILD_COMMIT="${HOS_BUILD_COMMIT}" SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH}" \
    cargo build --release --locked -p hos-web --bin hostingos \
 && install -Dm755 target/release/hostingos /out/hostingos

# ---------------------------------------------------------------------------
# Shared runtime base. ca-certificates: rustls verifies against the system
# store. No curl or shell tricks for the health check: the binary probes
# itself (`hostingos healthcheck`).
FROM ${RUNTIME_IMAGE} AS runtime-base
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --system --gid 10001 hostingos \
 && useradd --system --uid 10001 --gid hostingos --home-dir /app --shell /usr/sbin/nologin hostingos
WORKDIR /app
# HOS_INSTALL_TYPE: the update console offers image pulls / the deploy
# webhook, never an in-place binary swap.
ENV HOS_BIND=0.0.0.0:8080 \
    HOS_STATIC_DIR=/app/static \
    HOS_LOCALES_DIR=/app/locales \
    HOS_INSTALL_TYPE=container \
    HOS_LOG_FORMAT=json \
    RUST_LOG=info,sqlx=warn
EXPOSE 8080
# Liveness: GET /healthz · readiness (database reachable and migrated):
# GET /readyz. `hostingos healthcheck` sends a plain HTTP GET /healthz to
# HOS_BIND (0.0.0.0 becomes 127.0.0.1) and exits 0 or 1: no curl, no shell,
# nothing written, so it works as the non-root user on a read-only root
# filesystem. The start period covers the startup work (migrations with
# HOS_MIGRATE_ON_START or in demo mode, the first reset in demo mode).
# Coolify uses this check while its own Healthcheck page is disabled.
HEALTHCHECK --interval=15s --timeout=5s --start-period=60s --retries=4 \
  CMD ["/usr/local/bin/hostingos", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/hostingos"]
CMD ["serve"]

# ---------------------------------------------------------------------------
# Release image from the signed release binaries (CI: docker buildx
# --platform linux/amd64,linux/arm64 --target prebuilt).
FROM runtime-base AS prebuilt
ARG TARGETARCH
COPY --chmod=0755 docker-dist/${TARGETARCH}/hostingos /usr/local/bin/hostingos
COPY docker-dist/${TARGETARCH}/static ./static
COPY docker-dist/${TARGETARCH}/locales ./locales
# Licence of HostingOS and notices of the bundled third-party code (the
# fonts' and noVNC's own licence files are inside static/).
COPY docker-dist/${TARGETARCH}/LICENSE docker-dist/${TARGETARCH}/THIRD-PARTY-NOTICES.md ./
USER 10001:10001

# ---------------------------------------------------------------------------
# Default: built from source (last stage = `docker build .`).
FROM runtime-base AS runtime
COPY --from=builder /out/hostingos /usr/local/bin/hostingos
COPY static ./static
COPY locales ./locales
# Licence of HostingOS and notices of the bundled third-party code (the
# fonts' and noVNC's own licence files are inside static/).
COPY LICENSE THIRD-PARTY-NOTICES.md ./
USER 10001:10001
