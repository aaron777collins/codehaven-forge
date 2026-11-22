# Runtime images. The binaries are built in the builder container (deploy/dev/release.sh), which
# shares the cargo and sccache volumes and the 3 build slots, then copied in from ./dist.
# Build with: docker buildx build --builder codehaven --load --target server -t codehaven-server:dev -f deploy/images/runtime.Dockerfile dist
# Targets: server, e2e, runner. See ARCHITECTURE.md section 15.5.
FROM debian:trixie-slim AS base
LABEL org.codehaven=1
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 10001 --create-home --shell /usr/sbin/nologin codehaven
ENV GIT_CONFIG_NOSYSTEM=1
ENTRYPOINT ["/usr/bin/tini", "--"]

FROM base AS server
COPY --chmod=0755 codehaven /usr/local/bin/codehaven
USER 10001
EXPOSE 3200 2222
CMD ["codehaven", "serve", "--migrate"]

# The end-to-end harness. It also holds the server binary so steps can run `codehaven admin ...`
# against the same database, which is what an operator does with `docker compose exec`.
FROM base AS e2e
RUN apt-get update && apt-get install -y --no-install-recommends openssh-client curl \
 && rm -rf /var/lib/apt/lists/*
COPY --chmod=0755 codehaven ch-e2e /usr/local/bin/
USER 10001
CMD ["ch-e2e"]

# The CI runner. It never gets the host's Docker socket: DOCKER_HOST points at the separate dind
# daemon (deploy/compose.yml). /work is shared with that daemon, because job containers bind
# their job directory at the same path.
FROM base AS runner
COPY --chmod=0755 haven-runner /usr/local/bin/haven-runner
RUN mkdir -p /work && chown 10001:10001 /work
WORKDIR /home/codehaven
USER 10001
CMD ["haven-runner", "run"]
