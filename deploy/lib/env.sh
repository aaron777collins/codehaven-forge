# Shared by every deploy script. Source it, do not run it.
#
#   . "$(dirname "${BASH_SOURCE[0]}")/../lib/env.sh"      # from deploy/<dir>/script.sh
#   ch_env_load                                           # find, parse and export the env file
#   ch_need CODEHAVEN_PROJECT CODEHAVEN_EXTERNAL_URL      # stop with a plain message if unset
#
# The env file is found as CH_ENV_FILE, else <checkout>/.env, else <checkout>/../codehaven.env.
# It is parsed, never sourced, so a value can hold no command. A variable that is already set
# in the environment wins over the file, which is how a throwaway run overrides one value.

ch_die() { printf '%s: %s\n' "${CH_SCRIPT:-deploy}" "$*" >&2; exit 1; }

# The checkout this library belongs to (the directory that holds deploy/).
ch_checkout() { (cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd); }

ch_env_file() {
  local root cands=() f
  root="$(ch_checkout)"
  if [ -n "${CH_ENV_FILE:-}" ]; then
    [ -f "$CH_ENV_FILE" ] || ch_die "CH_ENV_FILE=$CH_ENV_FILE does not exist"
    printf '%s\n' "$CH_ENV_FILE"; return 0
  fi
  for f in "$root/.env" "$root/../codehaven.env"; do [ -f "$f" ] && cands+=("$f"); done
  case ${#cands[@]} in
    0) ch_die "no env file. Copy $root/.env.example to $root/.env and fill it in, or set CH_ENV_FILE." ;;
    1) printf '%s\n' "${cands[0]}" ;;
    *) ch_die "two env files found (${cands[0]} and ${cands[1]}). Keep one, or set CH_ENV_FILE to pick." ;;
  esac
}

# ch_parse_file <file>: parse KEY=VALUE lines. Blank lines and # comments are skipped. A value
# may be wrapped in one pair of single or double quotes. Nothing is evaluated, and a variable
# that is already set is left alone.
ch_parse_file() {
  local file="$1" line key val
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    case "$line" in ''|'#'*|[[:space:]]'#'*) continue;; esac
    [[ "$line" =~ ^[[:space:]]*([A-Z][A-Z0-9_]*)=(.*)$ ]] || ch_die "$file: cannot read this line (expected KEY=value): ${line:0:40}"
    key="${BASH_REMATCH[1]}"; val="${BASH_REMATCH[2]}"
    if [[ "$val" =~ ^\'(.*)\'$ ]]; then val="${BASH_REMATCH[1]}"
    elif [[ "$val" =~ ^\"(.*)\"$ ]]; then val="${BASH_REMATCH[1]}"; fi
    [ -n "${!key+x}" ] || export "$key=$val"
  done < "$file"
}

ch_env_load() {
  local file
  file="$(ch_env_file)" || exit 1
  ch_parse_file "$file"
  CH_ENV_LOADED="$file"; export CH_ENV_LOADED
}

# For the machine you type `make` on. Where to build: CH_HOST (an ssh name) and CH_REMOTE_ROOT
# (a directory on that host), from the environment or from deploy/dev/local.env in the main
# checkout. There is no default host: with neither set this stops instead of guessing.
ch_remote_load() {
  local root main f
  root="$(ch_checkout)"
  main="$(cd "$root" && git rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's,/\.git$,,' || true)"
  [ -n "$main" ] || main="$root"
  for f in "$root/deploy/dev/local.env" "$main/deploy/dev/local.env"; do
    if [ -f "$f" ]; then ch_parse_file "$f"; break; fi
  done
  [ -n "${CH_HOST:-}" ] || ch_die "set CH_HOST to the ssh name of the build host (in the environment or deploy/dev/local.env). Leave it unset to build on this machine."
  [ -n "${CH_REMOTE_ROOT:-}" ] || ch_die "set CH_REMOTE_ROOT to the directory on $CH_HOST that holds the checkouts (in the environment or deploy/dev/local.env)."
}

# ch_stream_source <stream>: the directory on this machine that a stream builds from.
ch_stream_source() {
  local stream="$1" here
  here="$(ch_checkout)"
  [[ "$stream" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || ch_die "stream '$stream' is not a plain name"
  if [ "$stream" = foundation ] || [ "$stream" = integration ] || [ "$stream" = src ]; then printf '%s\n' "$here"
  elif [ "$(basename "$here")" = "$stream" ] && [ "$(basename "$(dirname "$here")")" = .worktrees ]; then printf '%s\n' "$here"
  elif [ -d "$here/.worktrees/$stream" ]; then printf '%s\n' "$here/.worktrees/$stream"
  else ch_die "no worktree at $here/.worktrees/$stream"; fi
}

# ch_need KEY...: every one must be set and not empty.
ch_need() {
  local k
  for k in "$@"; do
    [ -n "${!k:-}" ] || ch_die "$k is not set. Set it in ${CH_ENV_LOADED:-your env file} (see .env.example)."
  done
}

# ch_need_set KEY...: must be set, may be empty (for example HOST_PUBLIC_IPS).
ch_need_set() {
  local k
  for k in "$@"; do
    [ -n "${!k+x}" ] || ch_die "$k is not set. Set it in ${CH_ENV_LOADED:-your env file} (see .env.example); an empty value is allowed."
  done
}
