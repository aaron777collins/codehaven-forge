# The guard for anything that starts, fills or deletes a test stack. Source it after env.sh.
#
# A test stack is "throwaway" only when BOTH its compose project name and its image tag say so,
# and only when it is never the instance people use. The live project and the live tag cannot
# pass these checks: there is no flag that turns them off.

CH_THROWAWAY_PREFIXES='test|e2e|shots|tmp'

ch_valid_project() { [[ "$1" =~ ^($CH_THROWAWAY_PREFIXES)-[a-z0-9][a-z0-9-]*$ ]]; }
ch_valid_tag()     { [[ "$1" =~ ^($CH_THROWAWAY_PREFIXES)-[A-Za-z0-9_.-]+$ ]]; }

# ch_require_throwaway <project> <tag>
ch_require_throwaway() {
  local project="$1" tag="$2"
  ch_valid_project "$project" || ch_die "project '$project' is not a throwaway name. Use a name like test-<word> or e2e-<word>."
  ch_valid_tag "$tag" || ch_die "tag '$tag' is not a throwaway tag. Use a tag like test-<word> or e2e-<word>."
  [ "$project" != "${CODEHAVEN_PROJECT:-}" ] || ch_die "project '$project' is the instance people use."
  [ "$tag" != "${CODEHAVEN_TAG:-}" ] || ch_die "tag '$tag' is the tag the instance people use runs."
}

# ch_project_only_throwaway <project>: stop when the project already has a container that was
# not started as a throwaway one. A leftover throwaway project may be reused and cleaned.
ch_project_only_throwaway() {
  local project="$1" id label
  for id in $(docker ps -aq --filter "label=com.docker.compose.project=$project"); do
    label=$(docker inspect --format '{{index .Config.Labels "org.codehaven.throwaway"}}' "$id" 2>/dev/null || true)
    [ "$label" = true ] || ch_die "project '$project' already holds a container that is not a throwaway one. Not touching it."
  done
}

# ch_tag_in_use <tag>: succeeds when a running container uses a codehaven image with this tag
# and is not a throwaway one.
ch_tag_in_use() {
  local tag="$1" id img label
  for id in $(docker ps -q); do
    img=$(docker inspect --format '{{.Config.Image}}' "$id" 2>/dev/null || true)
    case "$img" in codehaven-server:"$tag"|codehaven-runner:"$tag"|codehaven-e2e:"$tag") ;; *) continue;; esac
    label=$(docker inspect --format '{{index .Config.Labels "org.codehaven.throwaway"}}' "$id" 2>/dev/null || true)
    [ "$label" = true ] || return 0
  done
  return 1
}
