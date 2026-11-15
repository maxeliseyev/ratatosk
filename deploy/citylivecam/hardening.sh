#!/usr/bin/env bash
# Host-level hardening for the h3 stand. Idempotent, needs root:
#
#   sudo bash hardening.sh
#
# What it does, and why, is in README.md ("Защита от сканов и флуда").
# Nothing here closes port 22: the SSH rate limit is added before the plain
# allow rule is removed, so the box cannot be locked out by a half-applied run.
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
H="$DIR/hardening"

say() { printf '\n== %s\n' "$*"; }

say "sysctl: QUIC buffers, backlogs, conntrack table"
install -m 0644 "$H/sysctl.conf" /etc/sysctl.d/99-ratatosk.conf
# nf_conntrack_max lives in the netfilter module; if nothing loaded it yet,
# the rest still applies.
sysctl -p /etc/sysctl.d/99-ratatosk.conf 2>/dev/null || sysctl --system >/dev/null
sysctl -n net.core.rmem_max net.netfilter.nf_conntrack_max 2>/dev/null || true

say "swap: 1 GiB file when the box has none and <= 2 GiB RAM"
if swapon --show | grep -q .; then
    echo "already on: $(swapon --show --noheadings | awk '{print $1}' | tr '\n' ' ')"
else
    mem_kb=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
    if [ "$mem_kb" -lt 2097152 ]; then
        if [ ! -f /swapfile ]; then
            fallocate -l 1G /swapfile 2>/dev/null || dd if=/dev/zero of=/swapfile bs=1M count=1024 status=none
            chmod 600 /swapfile
        fi
        mkswap /swapfile >/dev/null
        swapon /swapfile
        grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >>/etc/fstab
        echo "swap on /swapfile (1 GiB, swappiness 10)"
    else
        echo "RAM >= 2 GiB, no swap needed"
    fi
fi

say "docker: log rotation (10m x3) and live-restore"
if [ -f /etc/docker/daemon.json ]; then
    echo "/etc/docker/daemon.json already exists, not touching it; check its log-opts by hand"
else
    install -D -m 0644 "$H/docker-daemon.json" /etc/docker/daemon.json
    systemctl reload docker
    echo "installed /etc/docker/daemon.json; containers get it on their next recreate"
fi

say "ufw: default deny incoming, stand ports, ssh rate limited"
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
for rule in "80/tcp" "443/tcp" "443/udp"; do
    ufw allow "$rule" >/dev/null
done
if ! ufw status | grep -q 'LIMIT'; then
    # Add the limit rule first, then drop the plain allow: port 22 stays
    # open while the two rules change places.
    if ufw status | grep -q 'OpenSSH'; then
        ufw limit OpenSSH >/dev/null
        ufw delete allow OpenSSH >/dev/null
    elif ufw status | grep -qE '^22/tcp'; then
        ufw limit 22/tcp >/dev/null
        ufw delete allow 22/tcp >/dev/null
    fi
    echo "ssh limit added (6 new connections per 30 s from one address)"
fi
ufw --force enable >/dev/null
ufw status | sed -n '1,12p'

say "fail2ban: sshd and caddy-scan jails"
install -m 0644 "$H/fail2ban/filter.d/caddy-scan.conf" /etc/fail2ban/filter.d/caddy-scan.conf
install -m 0644 "$H/fail2ban/action.d/nftables-forward.conf" /etc/fail2ban/action.d/nftables-forward.conf
install -m 0644 "$H/fail2ban/jail.local" /etc/fail2ban/jail.d/ratatosk.local
LOG="/var/lib/docker/volumes/citylivecam_caddy_data/_data/access.log"
if [ ! -f "$LOG" ]; then
    # The stand has not run yet: keep the jail configured but off.
    tmp=$(mktemp)
    awk '/^\[caddy-scan\]/ { s = 1 } s && /^enabled =/ { print "enabled = false"; next } { print }' \
        /etc/fail2ban/jail.d/ratatosk.local >"$tmp"
    install -m 0644 "$tmp" /etc/fail2ban/jail.d/ratatosk.local
    rm -f "$tmp"
    echo "$LOG is missing: caddy-scan is configured but disabled; re-run after the stand is up"
fi
fail2ban-client -t
systemctl restart fail2ban
sleep 2
fail2ban-client status
echo "log matches so far: $(fail2ban-regex "$LOG" /etc/fail2ban/filter.d/caddy-scan.conf 2>/dev/null | grep -m1 'Lines' || echo 'no log yet')"

say "checks that must stay as they are"
sshd -T | grep -E '^(passwordauthentication|permitrootlogin|pubkeyauthentication)' || true
echo "done"
