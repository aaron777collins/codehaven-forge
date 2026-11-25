#!/bin/sh
# Entry point of the dind service. Jobs run in containers inside this daemon, and a job is code
# anyone with CI rights wrote, so it must reach the internet and nothing else on this host.
#
# Without this, a job's packets leave through the `runners` network and the host answers on
# every address it has: the private bridge addresses, and its own public IPv4/IPv6 addresses
# (HOST_PUBLIC_IPS, from deploy/prod/host-ips.sh), where ssh, postgres, redis and other services
# listen. The rules go here, in the daemon's own network namespace, so the host's firewall is
# never touched:
#
#   FORWARD (DOCKER-USER)  a job container may not open a connection to a private range, nor to
#                          one of the host's public addresses. Exception: tcp/443, the public
#                          https site Caddy serves to the whole internet exactly as it is to a
#                          job; a job may reach that, and nothing else on those addresses.
#   INPUT                  a job container may not talk to the daemon's own address
#   OUTPUT                 nothing in here, host-network jobs included, may open a connection
#                          to a private range or a host public address except tcp/443 (the
#                          runner's connection and its replies are established, so they are let
#                          through first)
#
# The host's public addresses are not local to dind, so they need no INPUT rule: a job's packets
# to them are forwarded (DOCKER-USER) and the daemon's own are outgoing (OUTPUT).
#
# A throwaway stack (deploy/compose.shots-dogfood.yml) may set GUARD_ALLOW="<ip>:<port> ..." so its
# jobs can reach the throwaway server they belong to, which has no public https name. Nothing in
# the production compose file passes it, and each entry must be a plain address and a port.
#
# Jobs cannot undo this: container.options has no --privileged, --network or --cap-add.
# The rules are checked again every 30 seconds in case dockerd rebuilds its chains.
set -eu

PRIVATE="10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 100.64.0.0/10"
# The host's public addresses, IPv4 and IPv6, space separated (deploy/prod/host-ips.sh).
PUBLIC="${HOST_PUBLIC_IPS:-}"

ensure() { # ensure <iptables|ip6tables> <chain> <args...>: add the rule at the top once
  bin="$1"; shift
  chain="$1"; shift
  "$bin" -C "$chain" "$@" 2>/dev/null || "$bin" -I "$chain" 1 "$@"
}

apply() {
  for net in $PRIVATE; do
    ensure iptables DOCKER-USER -o eth0 -d "$net" -j DROP
    ensure iptables OUTPUT -d "$net" -m conntrack --ctstate NEW -j DROP
  done
  ensure iptables INPUT -i docker0 -j DROP
  ensure iptables INPUT -i br+ -j DROP
  for ip in $PUBLIC; do
    # The drop goes in first and its exception second: ensure() inserts at position 1, so the
    # tcp/443 rule ends up above the drop.
    case "$ip" in
      *:*)
        # IPv6. ip6tables may be missing, and dockerd only creates a DOCKER-USER chain for it
        # once an IPv6 network exists; either way there is nothing to do here, and a failure on
        # this path must not stop the guard loop.
        command -v ip6tables >/dev/null 2>&1 || continue
        ip6tables -n -L DOCKER-USER >/dev/null 2>&1 || continue
        ensure ip6tables DOCKER-USER -d "$ip" -j DROP || true
        ensure ip6tables DOCKER-USER -d "$ip" -p tcp --dport 443 -j RETURN || true
        ensure ip6tables OUTPUT -d "$ip" -m conntrack --ctstate NEW -j DROP || true
        ensure ip6tables OUTPUT -d "$ip" -p tcp --dport 443 -j ACCEPT || true
        ;;
      *)
        ensure iptables DOCKER-USER -d "$ip" -j DROP
        ensure iptables DOCKER-USER -d "$ip" -p tcp --dport 443 -j RETURN
        ensure iptables OUTPUT -d "$ip" -m conntrack --ctstate NEW -j DROP
        ensure iptables OUTPUT -d "$ip" -p tcp --dport 443 -j ACCEPT
        ;;
    esac
  done
  # Last, so these end up above the drops.
  for hp in ${GUARD_ALLOW:-}; do
    case "$hp" in
      *[!0-9.:]*|*:*:*|:*|*:|"") continue ;;
    esac
    ensure iptables DOCKER-USER -d "${hp%:*}" -p tcp --dport "${hp##*:}" -j RETURN
  done
}

(
  i=0
  while [ "$i" -lt 180 ]; do
    iptables -n -L DOCKER-USER >/dev/null 2>&1 && break
    i=$((i + 1)); sleep 1
  done
  apply
  while sleep 30; do apply || true; done
) &

exec dockerd-entrypoint.sh "$@"
