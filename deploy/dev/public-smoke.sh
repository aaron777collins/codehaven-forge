#!/usr/bin/env bash
# Smoke test for a public instance. Run it from the host, or from anywhere that can reach it.
#   deploy/dev/public-smoke.sh <base-url> [--mcp on|off|auto]
# The URL is required; there is no default. Checks: TLS is valid, /login carries every security
# header, the signed-out sweep gives the same answers through the proxy as on the server itself, a
# signed-out git ls-remote fails with 401, and an identity header signs nobody in. Then the
# connector surface (--mcp, default auto: on when the discovery document answers):
#   on   the discovery documents are JSON, /mcp answers 401 with a resource_metadata challenge,
#        and there is no client registration route in any spelling
#   off  every connector path answers exactly like a path that does not exist
# Only GET, HEAD and signed-out POST requests that are refused, no state changes.
# Against the server itself: deploy/dev/public-smoke.sh http://127.0.0.1:3200
set -uo pipefail
base="${1:-}"
[ -n "$base" ] || { echo "usage: public-smoke.sh <base-url> [--mcp on|off|auto]" >&2; exit 2; }
mcp=auto
[ "${2:-}" != --mcp ] || mcp="${3:-auto}"
fail=0
ok()   { printf 'ok    %s\n' "$*"; }
bad()  { printf 'FAIL  %s\n' "$*"; fail=1; }

code()    { curl -s -o /dev/null -m 15 -w '%{http_code}' "$@"; }
headers() { curl -s -D - -o /dev/null -m 15 "$@" | tr -d '\r'; }

# 1. TLS and reachability. Without -k, curl fails here on a bad certificate.
if [[ "$base" == https://* ]]; then
  c=$(code "$base/healthz")
  [ "$c" = 200 ] && ok "TLS is valid and /healthz is 200" || bad "https /healthz returned $c (TLS or routing problem)"
else
  [ "$(code "$base/healthz")" = 200 ] && ok "/healthz is 200 (plain http self-test)" || bad "/healthz is not 200"
fi
[ "$(curl -s -m 15 "$base/healthz")" = ok ] && ok "/healthz says ok and nothing else" || bad "/healthz body is not exactly ok"

# 2. /login: 200, every security header, nothing that offers sign-up.
h=$(headers "$base/login")
echo "$h" | head -1 | grep -q ' 200' && ok "/login is 200" || bad "/login is not 200"
for name in content-security-policy x-frame-options x-content-type-options referrer-policy cross-origin-opener-policy cross-origin-resource-policy permissions-policy x-robots-tag; do
  echo "$h" | grep -qi "^$name:" && ok "header $name" || bad "missing header $name"
done
if [[ "$base" == https://* ]]; then
  echo "$h" | grep -qi '^strict-transport-security: max-age=31536000' && ok "header strict-transport-security" || bad "missing HSTS"
fi
echo "$h" | grep -qi '^server:' && bad "a Server header is present" || ok "no Server header"
body=$(curl -s -m 15 "$base/login")
echo "$body" | grep -qiE 'sign.?up|register|create an account' && bad "/login offers sign-up" || ok "/login offers no sign-up"

# 3. The signed-out sweep (ROADMAP.md section 6 step 1).
expect() { # path expected-status [header-regex]
  local path="$1" want="$2" re="${3:-}" got
  got=$(code "$base$path")
  if [ "$got" != "$want" ]; then bad "$path returned $got, expected $want"; return; fi
  if [ -n "$re" ] && ! headers "$base$path" | grep -qiE "$re"; then bad "$path lacks header matching $re"; return; fi
  ok "$path -> $want"
}
for p in / /explore /root /nobody/nothing /signup /register /metrics /readyz; do expect "$p" 303 '^location: /login'; done
for p in /api/v3/user /api/v3/meta /api/v3/version /api/v3/repos/x/y; do expect "$p" 401; done
expect '/x/y.git/info/refs?service=git-upload-pack' 401 '^www-authenticate: basic'
expect /v2/ 401 '^www-authenticate: bearer'
expect /api/runner/v1/jobs/request 401
# Registering a runner needs a token: no body, no token, no entry.
c=$(curl -s -o /dev/null -m 15 -w '%{http_code}' -X POST -H 'content-type: application/json' -d '{}' "$base/api/runner/v1/register")
case "$c" in 400|401|403|404|422) ok "POST /api/runner/v1/register without a token -> $c";; *) bad "register without a token returned $c";; esac

# 4. No sign-up route for any method.
for m in POST PUT; do
  c=$(curl -s -o /dev/null -m 15 -w '%{http_code}' -X "$m" "$base/signup"); [ "$c" = 303 ] && ok "$m /signup -> 303" || bad "$m /signup returned $c"
done

# 5. An identity header from an auth proxy signs nobody in.
c=$(code -H 'Remote-User: root' -H 'Remote-Groups: admins' "$base/api/v3/user"); [ "$c" = 401 ] && ok "Remote-User is ignored" || bad "Remote-User gave $c"

# 6. A signed-out git ls-remote fails with 401 and never prompts.
# From an empty directory: a damaged .git in the directory this runs from must not change the answer.
out=$(cd "$(mktemp -d)" && GIT_TERMINAL_PROMPT=0 GIT_ASKPASS=/bin/false git ls-remote "$base/x/y.git" 2>&1); rc=$?
if [ $rc -ne 0 ] && echo "$out" | grep -qiE '401|authentication|could not read Username|terminal prompts disabled'; then ok "git ls-remote signed out is refused"; else bad "git ls-remote signed out: rc=$rc $out"; fi

# 7. Responses for paths that exist and paths that do not look alike.
a=$(headers "$base/api/v3/user" | grep -ciE '^(content-type|www-authenticate)'); b=$(headers "$base/api/v3/zzz-not-real" | grep -ciE '^(content-type|www-authenticate)')
[ "$a" = "$b" ] && ok "existing and missing API paths answer alike" || bad "API paths differ in headers"

# 8. The connector surface.
if [ "$mcp" = auto ]; then
  [ "$(code "$base/.well-known/oauth-protected-resource/mcp")" = 200 ] && mcp=on || mcp=off
fi
sig() { # status plus the headers that tell one answer from another, for a path and method
  local m="$1" path="$2"
  # The sign-in redirect carries the path asked for in its next= value, so that value is not compared.
  curl -s -D - -o /dev/null -m 15 -X "$m" "$base$path" | tr -d '\r' | awk 'NR==1{print $2} tolower($0) ~ /^(location|www-authenticate):/ {print tolower($0)}' | sed -E 's/next=[^& ]*/next=_/'
}
if [ "$mcp" = on ]; then
  for p in /.well-known/oauth-protected-resource/mcp /.well-known/oauth-protected-resource /.well-known/oauth-authorization-server; do
    h=$(headers "$base$p")
    echo "$h" | head -1 | grep -q ' 200' && echo "$h" | grep -qi '^content-type: *application/json' && ok "$p is JSON" || bad "$p is not 200 JSON"
  done
  h=$(curl -s -D - -o /dev/null -m 15 -X POST -H 'content-type: application/json' -d '{}' "$base/mcp" | tr -d '\r')
  echo "$h" | head -1 | grep -q ' 401' && ok "POST /mcp signed out -> 401" || bad "POST /mcp signed out is not 401"
  echo "$h" | grep -qi '^www-authenticate: bearer .*resource_metadata=' && ok "401 carries a resource_metadata challenge" || bad "401 has no resource_metadata challenge"
  for p in /oauth/register /register /oauth/registration /.well-known/openid-configuration; do
    [ "$(sig POST "$p")" = "$(sig POST /zzz-not-real)" ] && ok "$p does not exist" || bad "$p answers differently from a missing path"
  done
elif [ "$mcp" = off ]; then
  for p in /mcp /oauth/authorize /oauth/token /oauth/revoke /oauth/consent /.well-known/oauth-protected-resource /.well-known/oauth-protected-resource/mcp /.well-known/oauth-authorization-server; do
    for m in GET POST; do
      [ "$(sig $m "$p")" = "$(sig $m /zzz-not-real)" ] && ok "$m $p answers like a missing path" || bad "$m $p answers differently from a missing path"
    done
  done
else
  bad "--mcp must be on, off or auto"
fi

if [ $fail -eq 0 ]; then echo "public smoke: all checks passed"; else echo "public smoke: FAILED"; fi
exit $fail
