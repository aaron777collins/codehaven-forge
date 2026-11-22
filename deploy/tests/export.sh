#!/usr/bin/env bash
# deploy/tests/export.sh: section E, deploy/export-public.sh and nothing else. Every check runs
# against a scratch copy of this checkout under a mktemp dir; the real checkout is only read, once,
# to make that copy, and nothing is pushed anywhere.
#
# Needs python3 and git, and no Docker daemon: section B needs the docker CLI with compose and
# nothing else, and this file needs neither. The checks that need a leak scanner print skip when
# gitleaks is not on PATH and CH_GITLEAKS_IMAGE is not set (with docker); every other check runs
# without one because the script under test refuses before it gets there.
#
# One check prints one line, ok or FAIL, and the last line is a count. The exit code is 1 when
# anything failed. Output of the script under test is kept in a variable and only grepped, never
# printed, so a finding never echoes what it matched.
#
# The planted values below are joined from parts, and so is the author's address: this file is part
# of every snapshot the script under test makes, so anything it held in one piece would be planted
# into every export and would fail the happy path.
set -uo pipefail

pass=0 fail=0 skipd=0
ok()   { printf 'ok    %s\n' "$*"; pass=$((pass + 1)); }
bad()  { printf 'FAIL  %s\n' "$*"; fail=$((fail + 1)); }
skip() { printf 'skip  %s\n' "$*"; skipd=$((skipd + 1)); }
check(){ local name="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi; }

command -v python3 >/dev/null 2>&1 || { echo "export.sh: python3 is needed" >&2; exit 2; }
command -v git >/dev/null 2>&1 || { echo "export.sh: git is needed" >&2; exit 2; }

# Can the script under test reach a scanner? gitleaks on PATH, or a digest-pinned image plus docker.
scanner=yes
if ! command -v gitleaks >/dev/null 2>&1; then
  if [ -z "${CH_GITLEAKS_IMAGE:-}" ] || ! command -v docker >/dev/null 2>&1; then scanner=no; fi
fi

# --- the scratch tree -----------------------------------------------------------------------------

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
mkdir "$T/tree"
tar -C "$ROOT" --exclude=./.git --exclude=./target --exclude=./.worktrees --exclude=./dist -cf - . \
  | tar -C "$T/tree" -xf -
git -C "$T/tree" init -q
git -C "$T/tree" config user.name Test
git -C "$T/tree" config user.email test@example.com
git -C "$T/tree" add -A
git -c core.hooksPath=/dev/null -C "$T/tree" commit -q -m scratch
if [ ! -f "$T/tree/deploy/export-public.sh" ] || ! git -C "$T/tree" rev-parse --verify HEAD >/dev/null 2>&1; then
  echo "export.sh: the scratch tree could not be built or committed (check git config: signing, hooks)" >&2
  exit 1
fi
ok "the scratch tree was built and committed"

# fresh_copy: COPY becomes a new copy of the committed scratch tree, with its own git and one
# commit, so the script under test always runs in a copy of its own directory.
N=0
fresh_copy() {
  N=$((N + 1))
  COPY="$T/copy$N"
  mkdir "$COPY"
  git -C "$T/tree" archive HEAD | tar -x -C "$COPY"
  git -C "$COPY" init -q
  git -C "$COPY" config user.name Test
  git -C "$COPY" config user.email test@example.com
  git -C "$COPY" add -A
  git -c core.hooksPath=/dev/null -C "$COPY" commit -q -m scratch
}

# plant_commit <file> <text>: append one line to the copy and commit it.
plant_commit() {
  printf '%s\n' "$2" >>"$COPY/$1"
  git -C "$COPY" add -A
  git -c core.hooksPath=/dev/null -C "$COPY" commit -q -m plant
}

# run_export <out-dir> [args...]: run the copy's export-public.sh with the patterns file. The
# output lands in OUT and the code in RC; OUT is only ever grepped, never printed.
run_export() {
  local o="$1"; shift
  OUT="$(CH_OWNER_PATTERNS_FILE="$PAT_FILE" bash "$COPY/deploy/export-public.sh" "$o" "$@" 2>&1)"
  RC=$?
}

# want_refusal <name> <text>: the last run_export failed and its message holds text.
want_refusal() {
  local name="$1" text="$2"
  if [ "$RC" -ne 0 ] && printf '%s\n' "$OUT" | grep -qF -- "$text"; then ok "$name"; else bad "$name"; fi
}

# out_has_no <name> <dir> <text>: nothing under dir holds text. A dir that is not there passes,
# which is what a refusal should leave.
out_has_no() {
  local name="$1" dir="$2" text="$3"
  if [ -d "$dir" ] && grep -rIlF -- "$text" "$dir" >/dev/null 2>&1; then bad "$name"; else ok "$name"; fi
}

# --- section E: export-public.sh -------------------------------------------------------------------

printf 'export.sh: checkout %s, scanner %s\n' "$ROOT" "$scanner"

# The parts. Joined at use, never written whole (see the header).
ANAME='Jane Doe'
MAILDOM='gmail.com'                # a real mail domain, only ever a domain, never a full address
AMAIL_L='jane.doe@'
AMAIL="${AMAIL_L}${MAILDOM}"       # the author, a real person
MAIL_L='someone@'
W_A='de'; W_B='v3'; OWNER_W="${W_A}${W_B}"                       # a word from the server list
IP_A='45.33.'; IP_B='32.156'; PUB_IP="${IP_A}${IP_B}"           # a public address
PAT_A='zzq-private-host.example.'; PAT_B='org'; PATLINE="${PAT_A}${PAT_B}" # the one private pattern
CTOK_P='chma_'; CTOK_B='A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0U1v'  # 43 chars
GTOK_W='to'; GTOK_W="${GTOK_W}ken"
GTOK_P='ghp_'; GTOK_B='R4nd0mT0kenValu3ForTest1ng0nly12345x'    # 36 chars
AUTHOR=(--author-name "$ANAME" --author-email "$AMAIL")

PAT_FILE="$T/patterns.txt"
printf '%s\n' "$PATLINE" >"$PAT_FILE"

# 1. No patterns file named: refused, and the message names the variable.
fresh_copy
OUT="$(env -u CH_OWNER_PATTERNS_FILE bash "$COPY/deploy/export-public.sh" "$T/o1" "${AUTHOR[@]}" 2>&1)"; RC=$?
want_refusal "no CH_OWNER_PATTERNS_FILE is refused" CH_OWNER_PATTERNS_FILE

# 2. A dirty tree: the snapshot is of HEAD, so a new uncommitted file is refused.
fresh_copy
touch "$COPY/a-new-file"
run_export "$T/o2" "${AUTHOR[@]}"
want_refusal "a dirty tree is refused" "working tree has changes"

# 3. An out dir that already exists.
fresh_copy
mkdir "$T/o3"
run_export "$T/o3" "${AUTHOR[@]}"
want_refusal "an out dir that already exists is refused" "already exists"

# 4. An out dir inside the copy itself.
fresh_copy
run_export "$COPY/inside-out" "${AUTHOR[@]}"
want_refusal "an out dir inside the repository is refused" "inside this repository"
[ ! -e "$COPY/inside-out" ] && ok "the inside out dir was never created" || bad "the inside out dir was created"

# 5. No author at all: no flags, and no git config anywhere (global and system are /dev/null, the
# repo's own values are unset, HOME points at an empty dir).
fresh_copy
git -C "$COPY" config --unset user.name || true
git -C "$COPY" config --unset user.email || true
mkdir -p "$T/nohome"
OUT="$(GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null HOME="$T/nohome" \
  CH_OWNER_PATTERNS_FILE="$PAT_FILE" bash "$COPY/deploy/export-public.sh" "$T/o5" 2>&1)"; RC=$?
want_refusal "a missing author is refused" author

# 6. Placeholder authors: a placeholder name, an example or invalid domain, a "you" local part, a
# product name with noreply, and a value that is no address at all. Each refuses and leaves nothing.
fresh_copy
p_mail_1="$AMAIL"
p_mail_2='jane@example.com'
p_mail_3='jane@mail.invalid'
p_mail_4="you@${MAILDOM}"
p_mail_5='noreply@users.noreply.invalid'
p_mail_6='notanemail'
i=0
for pair in "Your Name|$p_mail_1" "Jane Doe|$p_mail_2" "Jane Doe|$p_mail_3" \
            "Jane Doe|$p_mail_4" "Codehaven|$p_mail_5" "Jane Doe|$p_mail_6"; do
  i=$((i + 1))
  n="${pair%%|*}"; m="${pair#*|}"
  o="$T/o6-$i"
  OUT="$(CH_OWNER_PATTERNS_FILE="$PAT_FILE" bash "$COPY/deploy/export-public.sh" "$o" \
    --author-name "$n" --author-email "$m" 2>&1)"; RC=$?
  want_refusal "a placeholder author is refused ($i of 6)" placeholder
  [ ! -e "$o" ] && ok "no output dir was left behind ($i of 6)" || bad "an output dir was left behind ($i of 6)"
done

# 7. A server word from the blocked list, in a file, committed.
fresh_copy
plant_commit planted.txt "ssh $OWNER_W"
run_export "$T/o7" "${AUTHOR[@]}"
want_refusal "a server word in a file is refused" "owner-specific"
out_has_no "the refused output holds no server word" "$T/o7" "$OWNER_W"

# 8. A line from the patterns file, in a file, committed.
fresh_copy
plant_commit planted.txt "ssh $PATLINE"
run_export "$T/o8" "${AUTHOR[@]}"
want_refusal "a private pattern in a file is refused" "owner-specific"
out_has_no "the refused output holds no pattern text" "$T/o8" "$PATLINE"

# 9. A public address outside the documentation ranges.
fresh_copy
plant_commit planted.txt "host = $PUB_IP"
run_export "$T/o9" "${AUTHOR[@]}"
want_refusal "a public ip in a file is refused" "owner-specific"

# 10. An email address on a real domain, in a file.
fresh_copy
plant_commit planted.txt "contact = ${MAIL_L}${MAILDOM}"
run_export "$T/o10" "${AUTHOR[@]}"
want_refusal "an email on a real domain in a file is refused" "owner-specific"

# 11. A Codehaven token shape: the prefix plus 43 mixed chars.
fresh_copy
plant_commit planted.txt "${CTOK_P}${CTOK_B}"
run_export "$T/o11" "${AUTHOR[@]}"
want_refusal "a Codehaven token shape is refused" "Codehaven token shape"

# 12. A value only a scanner catches. Scanner rules vary, so a scanner that stays quiet is a skip,
# not a failure.
if [ "$scanner" = yes ]; then
  fresh_copy
  plant_commit planted.txt "$GTOK_W = ${GTOK_P}${GTOK_B}"
  run_export "$T/o12" "${AUTHOR[@]}"
  if [ "$RC" -ne 0 ]; then
    ok "a scanner-caught value is refused"
  else
    skip "a scanner-caught value (this scanner did not flag the planted line)"
  fi
else
  skip "a scanner-caught value (no gitleaks and no CH_GITLEAKS_IMAGE)"
fi

# 13. The happy path, on a clean copy, with the author given as flags.
if [ "$scanner" = yes ]; then
  fresh_copy
  before="$(git -C "$COPY" rev-parse HEAD)"
  run_export "$T/o13" "${AUTHOR[@]}"
  [ "$RC" -eq 0 ] && ok "a clean copy exports with exit 0" || bad "a clean copy exports with exit 0"
  want_abs="$(cd "$T" && pwd)/o13"
  printf '%s\n' "$OUT" | grep -qFx -- "$want_abs" && ok "it prints the out dir" || bad "it prints the out dir"
  check "the out dir is a git checkout" git -C "$want_abs" rev-parse --git-dir
  [ "$(git -C "$want_abs" rev-list --count HEAD)" = 1 ] \
    && ok "the snapshot has exactly one commit" || bad "the snapshot does not have exactly one commit"
  [ -z "$(git -C "$want_abs" remote)" ] && ok "the snapshot has no remote" || bad "the snapshot has a remote"
  [ "$(git -C "$want_abs" log -1 --format='%an <%ae>')" = "$ANAME <$AMAIL>" ] \
    && ok "the commit author is the one asked for" || bad "the commit author is not the one asked for"
  [ "$(git -C "$want_abs" log -1 --format='%cn <%ce>')" = "$ANAME <$AMAIL>" ] \
    && ok "the committer is the one asked for" || bad "the committer is not the one asked for"
  [ ! -e "$want_abs/target" ] && [ ! -e "$want_abs/.worktrees" ] \
    && ok "the snapshot carries no target or .worktrees" || bad "the snapshot carries target or .worktrees"
  check "the copy is still a git checkout" git -C "$COPY" rev-parse --git-dir
  [ "$(git -C "$COPY" rev-parse HEAD)" = "$before" ] \
    && ok "the copy's own history is unchanged" || bad "the copy's own history moved"
  [ -z "$(git -C "$COPY" remote)" ] && ok "the copy gained no remote" || bad "the copy gained a remote"
  [ -z "$(git -C "$COPY" status --porcelain)" ] \
    && ok "the copy's working tree is still clean" || bad "the copy's working tree was touched"
else
  skip "the happy path (no gitleaks and no CH_GITLEAKS_IMAGE)"
fi

# 14. The author from git config instead of flags.
if [ "$scanner" = yes ]; then
  fresh_copy
  git -C "$COPY" config user.name "$ANAME"
  git -C "$COPY" config user.email "$AMAIL"
  run_export "$T/o14"
  [ "$RC" -eq 0 ] && ok "an export with the author from git config succeeds" \
    || bad "an export with the author from git config succeeds"
  [ "$(git -C "$T/o14" log -1 --format='%an <%ae>')" = "$ANAME <$AMAIL>" ] \
    && ok "the commit author came from git config" || bad "the commit author did not come from git config"
else
  skip "the author from git config (no gitleaks and no CH_GITLEAKS_IMAGE)"
fi

# --- the count --------------------------------------------------------------------------------------

printf 'export.sh: %d passed, %d failed, %d skipped\n' "$pass" "$fail" "$skipd"
[ "$fail" -eq 0 ]
