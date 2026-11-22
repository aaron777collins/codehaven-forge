#!/bin/sh
# The host's public addresses: every global-scope IPv4 and IPv6 address that is neither private
# (10/8, 172.16/12, 192.168/16, 169.254/16, 100.64/10, 127/8, ::1, fc00::/7, fe80::/10) nor on one
# of docker's own bridge interfaces (docker*, br-*, veth*), plus the default-route source
# addresses. One line, space separated, or a JSON list with `--json`.
#
# deploy/compose.sh exports this as HOST_PUBLIC_IPS, and deploy/dind/guard.sh drops
# job traffic to every one of these addresses except tcp/443, so a CI job cannot reach ssh,
# postgres, redis or anything else the host happens to listen on at its public address.
set -eu

# Candidates: the global addresses of the host's own interfaces...
cands=$(ip -o addr show scope global | awk '
  $2 ~ /^(docker|br-|veth)/ { next }
  {
    for (i = 1; i < NF; i++)
      if ($i == "inet" || $i == "inet6") { split($(i + 1), a, "/"); print a[1] }
  }')
# ...and the address the host would leave on, which need not sit on an interface kept above.
v4=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([^ ]*\).*/\1/p' || true)
v6=$(ip -6 route get 2606:4700:4700::1111 2>/dev/null | sed -n 's/.* src \([^ ]*\).*/\1/p' || true)

# The privacy filter, once, for every candidate.
ips=$(printf '%s\n' $cands $v4 $v6 | awk '
  function priv(ip) {
    return ip ~ /^10\./ || ip ~ /^127\./ || ip ~ /^192\.168\./ || ip ~ /^169\.254\./ ||
           ip ~ /^172\.(1[6-9]|2[0-9]|3[01])\./ ||
           ip ~ /^100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\./ ||
           ip == "::1" || ip ~ /^[fF][cCdD]/ || ip ~ /^[fF][eE][89aAbB]/
  }
  NF && !priv($0) { print }' | sort -u | tr '\n' ' ')
if [ "${1:-}" = --json ]; then
  # A JSON list for CODEHAVEN__WEBHOOKS__BLOCKED_IPS, so the server's webhooks refuse these too.
  out=""
  for ip in $ips; do out="${out:+$out,}\"$ip\""; done
  echo "[$out]"
else
  echo "$ips"
fi
