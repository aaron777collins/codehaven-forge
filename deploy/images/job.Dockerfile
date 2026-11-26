# codehaven-job:trixie, the image jobs run in unless a workflow or the runner's labels say
# otherwise (ARCHITECTURE.md section 9.5). Debian trixie with what CI scripts usually expect:
# git, a C toolchain, curl, Node, Python, Rust (rustup with stable, clippy and rustfmt, as on
# GitHub's Ubuntu image), the usual archive tools, and ICU, which the .NET SDK that setup-dotnet
# installs will not start without (GitHub's image has it). No Docker CLI: jobs cannot reach a daemon.
#
# Jobs run as root in it, like a `container:` job on GitHub. There is also a `runner` user
# (uid 10001, passwordless sudo) for workflows that ask for `--user runner`.
#
# Build and load it into the runner's Docker daemon with deploy/prod/ci-up.sh.
FROM debian:trixie-slim
LABEL org.codehaven=1 org.codehaven.image=job

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash ca-certificates curl wget git openssh-client gnupg \
      build-essential pkg-config libssl-dev cmake \
      python3 python3-pip python3-venv \
      nodejs npm \
      jq unzip zip xz-utils zstd rsync procps sudo less file tzdata \
      libicu76 \
 && rm -rf /var/lib/apt/lists/*

# Rust through rustup, so `rustup toolchain install`, `cargo` and the usual setup actions work
# (ch-dvg). It lives under /usr/local and is writable by everyone, so the `runner` user can add
# toolchains too.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh \
 && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal --default-toolchain stable --component clippy,rustfmt \
 && rm /tmp/rustup-init.sh \
 && chmod -R a+rwX /usr/local/rustup /usr/local/cargo \
 && rustup --version && cargo --version

# Actions pick prebuilt tools by the distribution they find: the toolkit reads /etc/lsb-release first,
# and setup-python, setup-java and the others look for their `ubuntu-24.04` builds. The runner's labels
# say `ubuntu-latest` and `ubuntu-24.04`, so the image says the same there (and `ImageOS` as GitHub's
# image sets it). The userland is Debian trixie, whose glibc is newer than Ubuntu 24.04's, so those
# builds run (ch-ctk.9). /etc/os-release says the same, because ruby/setup-ruby reads its ID and VERSION_ID to decide it
# is on a GitHub-hosted image and may download a prebuilt Ruby (ch-ctk.14); PRETTY_NAME still names the real userland.
RUN printf 'DISTRIB_ID=Ubuntu\nDISTRIB_RELEASE=24.04\nDISTRIB_CODENAME=noble\nDISTRIB_DESCRIPTION="Ubuntu 24.04 (Debian trixie userland)"\n' > /etc/lsb-release \
 && rm -f /etc/os-release \
 && printf 'PRETTY_NAME="Ubuntu 24.04 LTS (Debian trixie userland)"\nNAME="Ubuntu"\nVERSION_ID="24.04"\nVERSION="24.04 LTS (Noble Numbat)"\nVERSION_CODENAME=noble\nID=ubuntu\nID_LIKE=debian\nUBUNTU_CODENAME=noble\n' > /etc/os-release
ENV ImageOS=ubuntu24 ImageVersion=codehaven

# The workspace belongs to the runner's user, not root. git refuses to work in a directory
# owned by someone else unless it is told that is fine.
RUN git config --system --add safe.directory '*' \
 && useradd --uid 10001 --create-home --shell /bin/bash runner \
 && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner \
 && chmod 0440 /etc/sudoers.d/runner

CMD ["bash"]
