# Build and test image for every Codehaven stream. Built once per toolchain as
# codehaven-builder:<toolchain>. deploy/dev/builder-image.sh builds it when it is missing.
ARG RUST_VERSION=1.100.0
FROM rust:${RUST_VERSION}-trixie

ARG NEXTEST_VERSION=0.9.152
ARG SCCACHE_VERSION=0.10.0

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git openssh-client openssh-server postgresql-client curl ca-certificates \
      pkg-config libssl-dev cmake clang lld xz-utils \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy

# Prebuilt binaries where they exist, so the image builds in minutes. The right build is picked
# by CPU, so the image builds on x86-64 and on arm64 alike.
RUN set -eu; \
    case "$(uname -m)" in \
      x86_64)  sccache_arch=x86_64-unknown-linux-musl;  nextest_os=linux ;; \
      aarch64) sccache_arch=aarch64-unknown-linux-musl; nextest_os=linux-arm ;; \
      *) echo "builder.Dockerfile: unsupported CPU $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${sccache_arch}.tar.gz" \
      | tar -xz --strip-components=1 -C /usr/local/bin "sccache-v${SCCACHE_VERSION}-${sccache_arch}/sccache"; \
    curl -fsSL "https://get.nexte.st/${NEXTEST_VERSION}/${nextest_os}" | tar -xz -C /usr/local/cargo/bin

# One-off compiles. They are slow, which is why this image is built once.
RUN CARGO_TARGET_DIR=/tmp/tools cargo install --locked --no-default-features \
      --features rustls,postgres --version =0.8.6 sqlx-cli \
 && CARGO_TARGET_DIR=/tmp/tools cargo install --locked cargo-insta \
 && rm -rf /tmp/tools /usr/local/cargo/registry

ENV CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/target \
    CARGO_INCREMENTAL=0 \
    RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/sccache \
    SCCACHE_CACHE_SIZE=20G \
    RUST_BACKTRACE=1 \
    GIT_CONFIG_GLOBAL=/etc/codehaven-gitconfig

RUN printf '[user]\n\tname = Codehaven Builder\n\temail = builder@codehaven.invalid\n[init]\n\tdefaultBranch = main\n[safe]\n\tdirectory = *\n' > /etc/codehaven-gitconfig

WORKDIR /work
