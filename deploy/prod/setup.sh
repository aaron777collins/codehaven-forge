#!/usr/bin/env bash
# Checks the host and prepares what the instance needs: directories, permissions and the database
# credentials. Idempotent, and it never prints a secret. Run it as root (sudo) once, then up.sh.
#   sudo bash deploy/prod/setup.sh
# Everything comes from the env file (copy .env.example to .env first).
set -euo pipefail
CH_SCRIPT=setup
. "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"
ch_env_load
ch_need CODEHAVEN_PROJECT CODEHAVEN_DATA_DIR CODEHAVEN_SECRETS_DIR CODEHAVEN_BACKUP_DIR CODEHAVEN_PROXY
dir="$(ch_checkout)"
uid=10001   # the unprivileged user inside the server image

# --- what this host needs, each with a plain error --------------------------------------------
need_cmd() { command -v "$1" >/dev/null 2>&1 || ch_die "'$1' is not installed. $2"; }
need_cmd docker "Install Docker Engine: https://docs.docker.com/engine/install/"
need_cmd git "Install git."
need_cmd openssl "Install openssl."
need_cmd curl "Install curl."
need_cmd python3 "Install python3."
need_cmd ip "Install iproute2 (the 'ip' command)."
docker info >/dev/null 2>&1 || ch_die "cannot talk to docker. Is it running, and are you root or in the docker group?"
cv=$(docker compose version --short 2>/dev/null | sed 's/^v//') || ch_die "Docker Compose v2 is not installed (the 'docker compose' command)."
[ "$(printf '%s\n2.24.0\n' "$cv" | sort -V | head -1)" = 2.24.0 ] || ch_die "Docker Compose $cv is too old. Version 2.24 or newer is needed."
docker buildx version >/dev/null 2>&1 || ch_die "docker buildx is not installed. Install the docker-buildx plugin."
mem_kb=$(awk '/^MemTotal:/{print $2}' /proc/meminfo)
[ "${mem_kb:-0}" -ge 3800000 ] || ch_die "this host has less than 4 GB of memory. Building needs it."
mkdir -p "$CODEHAVEN_DATA_DIR"
free_kb=$(df -Pk "$CODEHAVEN_DATA_DIR" | awk 'NR==2{print $4}')
[ "${free_kb:-0}" -ge 20000000 ] || ch_die "less than 20 GB free under $CODEHAVEN_DATA_DIR. Free some space or point CODEHAVEN_DATA_DIR elsewhere."
[ "$(id -u)" = 0 ] || ch_die "run this as root (sudo): it creates directories owned by the server's user."
case "$(cd "$CODEHAVEN_BACKUP_DIR" 2>/dev/null && pwd || echo "$CODEHAVEN_BACKUP_DIR")/" in
  "$dir"/*) ch_die "CODEHAVEN_BACKUP_DIR is inside the checkout. Put backups somewhere else." ;;
esac

bash "$dir/deploy/dev/diskcheck.sh"

# --- directories ------------------------------------------------------------------------------
mkdir -p "$CODEHAVEN_DATA_DIR"/{postgres,git,blobs,logs,secrets,dind} "$CODEHAVEN_SECRETS_DIR"
# Directories the server writes belong to its unprivileged user. Postgres and dind own theirs.
chown "$uid:$uid" "$CODEHAVEN_DATA_DIR"/{git,blobs,logs}
chown "$uid:$uid" "$CODEHAVEN_DATA_DIR/secrets" && chmod 700 "$CODEHAVEN_DATA_DIR/secrets"

# --- compose secrets: readable by root and the server's group only ----------------------------
chown root:"$uid" "$CODEHAVEN_SECRETS_DIR" && chmod 750 "$CODEHAVEN_SECRETS_DIR"
if [ ! -s "$CODEHAVEN_SECRETS_DIR/pg_password" ]; then
  umask 077
  openssl rand -hex 24 > "$CODEHAVEN_SECRETS_DIR/pg_password"
  printf 'postgres://codehaven:%s@postgres:5432/codehaven\n' "$(cat "$CODEHAVEN_SECRETS_DIR/pg_password")" > "$CODEHAVEN_SECRETS_DIR/database_url"
  echo "setup: generated database credentials (they stay in $CODEHAVEN_SECRETS_DIR and are never printed)"
fi
chown root:"$uid" "$CODEHAVEN_SECRETS_DIR/pg_password" "$CODEHAVEN_SECRETS_DIR/database_url"
chmod 440 "$CODEHAVEN_SECRETS_DIR/pg_password" "$CODEHAVEN_SECRETS_DIR/database_url"

# --- the proxy choice --------------------------------------------------------------------------
case "$CODEHAVEN_PROXY" in
  none) ;;
  network)
    ch_need CODEHAVEN_PROXY_NETWORK CODEHAVEN_PROXY_ALIAS
    docker network inspect "$CODEHAVEN_PROXY_NETWORK" >/dev/null 2>&1 || ch_die "the docker network '$CODEHAVEN_PROXY_NETWORK' (CODEHAVEN_PROXY_NETWORK) does not exist."
    # An alias in use by a container of another project would split the traffic.
    for c in $(docker network inspect "$CODEHAVEN_PROXY_NETWORK" --format '{{range .Containers}}{{.Name}} {{end}}'); do
      proj=$(docker inspect "$c" --format '{{index .Config.Labels "com.docker.compose.project"}}' 2>/dev/null || true)
      [ "$proj" != "$CODEHAVEN_PROJECT" ] || continue
      if docker inspect "$c" --format "{{with index .NetworkSettings.Networks \"$CODEHAVEN_PROXY_NETWORK\"}}{{range .Aliases}}{{println .}}{{end}}{{end}}" 2>/dev/null | grep -qx "$CODEHAVEN_PROXY_ALIAS"; then
        ch_die "another container already has the alias '$CODEHAVEN_PROXY_ALIAS' on '$CODEHAVEN_PROXY_NETWORK'."
      fi
    done
    ;;
  caddy) ch_need CODEHAVEN_SITE_HOST CODEHAVEN_EDGE_SUBNET ;;
  *) ch_die "CODEHAVEN_PROXY must be none, network or caddy" ;;
esac
echo "setup: ok"
