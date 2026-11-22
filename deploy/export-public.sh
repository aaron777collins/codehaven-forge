#!/usr/bin/env bash
# Makes a clean, single-commit snapshot of the current commit in a new directory, ready to be
# published. It NEVER pushes, creates no repository on any service and adds no remote.
#
#   CH_OWNER_PATTERNS_FILE=/path/to/patterns.txt bash deploy/export-public.sh <out-dir> \
#       [--author-name "Your Name" --author-email you@example.org]
#
# Why: the history of this repository was written on one deployment and still holds its host
# details. The tree is kept clean by deploy/check-portable.py, but history cannot be edited, so what
# gets published is a fresh repository with one commit.
#
# The author and committer of that commit are whoever runs this: the two --author options, else
# `git config user.name` and `user.email` as git resolves them for this checkout. There is no
# default. It refuses when either is missing, or is a placeholder (an example or .invalid domain,
# "you", "Your Name" and the like), so nobody is credited by accident.
#
# It refuses when:
#   - the working tree has changes (the snapshot is of HEAD, so commit or stash first)
#   - <out-dir> already exists, or is inside this repository
#   - there is no usable author (above)
#   - CH_OWNER_PATTERNS_FILE is not set. That file is yours and stays private: one fixed text per
#     line, the details of YOUR server (its domain, host names, addresses, ssh alias, server paths,
#     runner name), matched in any case. Your own name and email do not belong in it
#   - the snapshot trips deploy/check-portable.py, one of your patterns, a Codehaven token pattern or a
#     secret scanner. The scanner is `gitleaks` on PATH, else the docker image in CH_GITLEAKS_IMAGE
#     (pin it by digest); having neither is a failure, not a skip
# Findings name a file and a line, never the text found.
set -euo pipefail
CH_SCRIPT=export-public
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "export-public: $*" >&2; exit 1; }
# placeholder_author <name> <email>: succeeds when the pair is not a real person's.
placeholder_author() {
  local n e local_part domain
  n="$(printf %s "$1" | tr A-Z a-z)"; e="$(printf %s "$2" | tr A-Z a-z)"
  [[ "$e" =~ ^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$ ]] || return 0
  local_part="${e%@*}"; domain="${e#*@}"
  case "$domain" in
    example.com|example.org|example.net|*.example.com|*.example.org|*.example.net|*.example|example|*.invalid|invalid|*.test|test|localhost|*.localhost|*.local|*.localdomain|localdomain) return 0;;
  esac
  case "$local_part" in you|your|yourname|your.name|name|user|username|email|me|admin|root|unknown|nobody|noreply|no-reply|placeholder|test) return 0;; esac
  case "$n" in "your name"|yourname|name|user|username|unknown|nobody|root|admin|you|placeholder|test|codehaven) return 0;; esac
  return 1
}
out=""; aname=""; amail=""
while [ $# -gt 0 ]; do
  case "$1" in
    --author-name) [ $# -ge 2 ] || die "--author-name needs a value"; aname="$2"; shift 2;;
    --author-email) [ $# -ge 2 ] || die "--author-email needs a value"; amail="$2"; shift 2;;
    -*) die "unknown option $1";;
    *) [ -z "$out" ] || die "one output directory only"; out="$1"; shift;;
  esac
done
[ -n "$out" ] || die "usage: export-public.sh <out-dir> [--author-name NAME --author-email EMAIL]"
[ -n "${CH_OWNER_PATTERNS_FILE:-}" ] || die "set CH_OWNER_PATTERNS_FILE to a private file of strings that must never be published (one per line)"
[ -f "$CH_OWNER_PATTERNS_FILE" ] || die "CH_OWNER_PATTERNS_FILE does not exist"
[ -s "$CH_OWNER_PATTERNS_FILE" ] || die "CH_OWNER_PATTERNS_FILE is empty. List at least the personal email address and addresses to keep out."
git -C "$dir" rev-parse --git-dir >/dev/null 2>&1 || die "run this from a git checkout"
[ -n "$aname" ] || aname="$(git -C "$dir" config user.name 2>/dev/null || true)"
[ -n "$amail" ] || amail="$(git -C "$dir" config user.email 2>/dev/null || true)"
[ -n "$aname" ] && [ -n "$amail" ] || die "no author. Pass --author-name and --author-email, or set git config user.name and user.email. There is no default."
placeholder_author "$aname" "$amail" && die "the author is a placeholder. Use the name and email of the person who publishes this."
[ -z "$(git -C "$dir" status --porcelain)" ] || die "the working tree has changes. Commit or stash them: the snapshot is of HEAD."
[ ! -e "$out" ] || die "$out already exists"
parent="$(cd "$(dirname "$out")" 2>/dev/null && pwd)" || die "the parent of $out does not exist"
abs="$parent/$(basename "$out")"
case "$abs/" in "$dir"/*) die "the output directory is inside this repository" ;; esac

mkdir "$abs"
metad=""
cleanup_on_fail() { rc=$?; [ -z "$metad" ] || rm -rf "$metad"; [ "$rc" = 0 ] || { rm -rf "$abs"; echo "export-public: refused, nothing was left behind" >&2; }; exit "$rc"; }
trap cleanup_on_fail EXIT

git -C "$dir" archive --format=tar HEAD | tar -x -C "$abs"

# 1. The portability guard, on the snapshot itself.
python3 "$dir/deploy/check-portable.py" --root "$abs" --patterns "$CH_OWNER_PATTERNS_FILE" || die "the snapshot holds owner-specific text (see above)"

# 2. Codehaven token shapes: a prefix and 40 or more random characters is a real token, not a demo value.
if grep -rEIl '\bch[a-z]{1,2}_[A-Za-z0-9]{40,}\b' "$abs" >/dev/null 2>&1; then
  grep -rEIn '\bch[a-z]{1,2}_[A-Za-z0-9]{40,}\b' "$abs" | cut -d: -f1,2 | sed "s#^$abs/##; s#\$#: looks like a Codehaven token#" >&2
  die "a Codehaven token shape is in the snapshot"
fi

# 3. A secret scanner. Not having one is a failure.
if command -v gitleaks >/dev/null 2>&1; then
  # gitleaks 8.19 and later scan a plain directory with `dir`; older releases use `detect --no-git`.
  if gitleaks dir --help >/dev/null 2>&1; then
    gitleaks dir "$abs" --no-banner --redact --exit-code 1 >&2 || die "gitleaks found something in the snapshot"
  else
    gitleaks detect --no-git --source "$abs" --no-banner --redact --exit-code 1 >&2 || die "gitleaks found something in the snapshot"
  fi
elif [ -n "${CH_GITLEAKS_IMAGE:-}" ] && command -v docker >/dev/null 2>&1; then
  case "$CH_GITLEAKS_IMAGE" in *@sha256:*) ;; *) die "CH_GITLEAKS_IMAGE must be pinned by digest (name@sha256:...)" ;; esac
  docker run --rm --network none -v "$abs:/repo:ro" "$CH_GITLEAKS_IMAGE" dir /repo --no-banner --redact --exit-code 1 >&2 || die "gitleaks found something in the snapshot"
else
  die "no secret scanner. Install gitleaks, or set CH_GITLEAKS_IMAGE to a digest-pinned gitleaks image and have docker."
fi

# 4. One commit, authored and committed by the person who ran this, no hooks, no remote.
msg="Codehaven snapshot"
(
  cd "$abs"
  git init -q -b main
  git config core.hooksPath /dev/null
  git config user.name "$aname"
  git config user.email "$amail"
  git add -A
  GIT_AUTHOR_NAME="$aname" GIT_AUTHOR_EMAIL="$amail" GIT_COMMITTER_NAME="$aname" GIT_COMMITTER_EMAIL="$amail" \
    git commit -q --no-verify -m "$msg"
)

# 5. Check what was written. The message is scanned like any file. The author is the person's own
# name and email, so the email rule is off for it, but a server word in either is still refused.
metad="$(mktemp -d)"
git -C "$abs" log --format='%an%n%ae%n%cn%n%ce%n%B' > "$metad/commit-metadata.txt"
python3 "$dir/deploy/check-portable.py" --root "$metad" --allow-emails --patterns "$CH_OWNER_PATTERNS_FILE" || die "the commit metadata holds server-specific text"
rm -rf "$metad"; metad=""
[ "$(git -C "$abs" rev-list --count HEAD)" = 1 ] || die "the snapshot does not have exactly one commit"
[ -z "$(git -C "$abs" remote)" ] || die "the snapshot has a remote"
[ "$(git -C "$abs" log -1 --format='%an <%ae>')" = "$aname <$amail>" ] || die "the author is not the one asked for"
[ "$(git -C "$abs" log -1 --format='%cn <%ce>')" = "$aname <$amail>" ] || die "the committer is not the one asked for"
trap - EXIT
echo "$abs"
echo "export-public: one commit, no remote, nothing pushed. Review it, then publish it yourself." >&2
