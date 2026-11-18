#!/bin/sh
# ratatosk on OpenWrt: a transparent, whole-LAN gateway in a single process.
#
# Every device on the router's LAN egresses through the tunnel; DNS is resolved
# inside it (no leak). The TUN device, the netstack, FakeDNS and the routing
# rules all live in `ratatosk` itself — no sing-box, no sidecar.
#
#   LAN device -> tun -> ratatosk (netstack + FakeDNS + routing) -> h3/QUIC -> exit
#
# Fail-closed: LAN traffic is only forwarded into the tunnel, so when the
# tunnel is down the LAN has no internet rather than a silent fallback to the
# ISP. The first link is the primary exit; the second one (optional) is used
# automatically when the primary stops answering, and the primary is retried
# afterwards.
#
# Usage, on the router (or over ssh):
#   sh install.sh 'h3://<uuid>@<host>:443?sni=<host>#stand' ['vless://...@<host>:443?...']
# Re-running is safe (idempotent): it reconfigures in place, keeping the link
# order from the arguments.
#
# Env overrides: RATATOSK_VERSION, RATATOSK_REPO, SERVER_IP_<n> (skip the DNS
# lookup for link n).
set -eu

VERSION="${RATATOSK_VERSION:-0.20.9}"
REPO="${RATATOSK_REPO:-maxeliseyev/ratatosk}"
# The netstack answers DNS here; see FakeDns::server_addr in the source.
FAKE_DNS="198.18.0.2"
DIR=/etc/ratatosk
PROG=/usr/bin/ratatosk

say() { echo ">> $*"; }
die() { echo "error: $*" >&2; exit 1; }

# --- links ------------------------------------------------------------------
# Keep the existing list when called without arguments (reconfigure in place).
if [ "$#" -gt 0 ]; then
	LINKS="$*"
else
	[ -f "$DIR/servers.txt" ] || die "usage: sh install.sh '<primary link>' ['<fallback link>']"
	LINKS="$(grep -v '^[[:space:]]*#' "$DIR/servers.txt" | tr '\n' ' ')"
fi
[ -n "$LINKS" ] || die "no links given"

# --- arch guard -------------------------------------------------------------
case "$(uname -m)" in
	aarch64) ASSET=arm64 ;;
	x86_64) ASSET=x86_64 ;;
	*) die "unsupported arch '$(uname -m)': the aws-lc-rs crypto core builds only on aarch64/x86_64" ;;
esac

# --- dependencies -----------------------------------------------------------
# kmod-tun for /dev/net/tun, curl for the health probe, ca-bundle for the TLS
# handshake with the exit server.
say "installing dependencies"
if command -v apk >/dev/null 2>&1; then
	apk update >/dev/null 2>&1 || true
	apk add kmod-tun curl ca-bundle >/dev/null
else
	opkg update >/dev/null 2>&1 || true
	opkg install kmod-tun curl ca-bundle >/dev/null
fi
modprobe tun 2>/dev/null || true
[ -e /dev/net/tun ] || die "no /dev/net/tun after loading kmod-tun"

# --- retire the sing-box based install --------------------------------------
# The old chain was ratatosk (SOCKS) + sing-box (TUN). Both services and the
# package go away; ratatosk owns the TUN now.
for svc in sing-box vproxy vproxy-reality; do
	if [ -x "/etc/init.d/$svc" ]; then
		say "retiring $svc"
		/etc/init.d/$svc stop 2>/dev/null || true
		/etc/init.d/$svc disable 2>/dev/null || true
	fi
done
rm -f /etc/init.d/vproxy /etc/init.d/vproxy-reality
if apk info -e sing-box >/dev/null 2>&1; then
	apk del sing-box >/dev/null 2>&1 || true
fi
if opkg list-installed 2>/dev/null | grep -q '^sing-box '; then
	opkg remove sing-box >/dev/null 2>&1 || true
fi
rm -rf /etc/sing-box /etc/config/sing-box

# The rename-era layout: links and SOCKS services under /etc/vproxy.
rm -rf /etc/vproxy

# --- ratatosk binary --------------------------------------------------------
if ! "$PROG" --version 2>/dev/null | grep -q "$VERSION"; then
	PKG="ratatosk-linux-${ASSET}-musl"
	URL="https://github.com/$REPO/releases/download/v${VERSION}/$PKG.tar.gz"
	say "downloading $URL"
	curl -fsSL -o "/tmp/$PKG.tar.gz" "$URL" || die "cannot download $URL"
	# Refuse to install an unverified binary when the release carries a sum.
	if curl -fsSL -o "/tmp/$PKG.sum" "$URL.sha256" 2>/dev/null; then
		expected="$(awk '{print $1}' "/tmp/$PKG.sum")"
		actual="$(sha256sum "/tmp/$PKG.tar.gz" | awk '{print $1}')"
		[ "$expected" = "$actual" ] || die "sha256 mismatch for $PKG.tar.gz"
	fi
	tar xzf "/tmp/$PKG.tar.gz" -C /tmp
	cp "/tmp/$PKG/ratatosk" "$PROG.new"
	chmod 0755 "$PROG.new"
	mv "$PROG.new" "$PROG"
	rm -rf "/tmp/$PKG.tar.gz" "/tmp/$PKG.sum" "/tmp/$PKG"
fi
say "ratatosk: $("$PROG" --version)"

# --- links + pinned exit hosts ---------------------------------------------
mkdir -p "$DIR"
chmod 700 "$DIR"
printf '%s\n' $LINKS > "$DIR/servers.txt"
head -n 1 "$DIR/servers.txt" > "$DIR/active.txt"
chmod 600 "$DIR/servers.txt" "$DIR/active.txt"

# ratatosk's own lookup of the exit host must not go into the tunnel it is
# still building, so pin every exit host to its address.
: > "$DIR/exit-ips"
n=0
for link in $LINKS; do
	n=$((n + 1))
	host="$(printf '%s' "$link" | sed -E 's#^[a-z0-9]+://[^@]*@([^:/?#]+).*#\1#')"
	[ -n "$host" ] && [ "$host" != "$link" ] || die "cannot parse the host out of link $n"
	eval "pinned=\${SERVER_IP_$n:-}"
	# On a re-run the pins from last time are the authority: dnsmasq already
	# points at FakeDNS, so a lookup now would return a 198.18.x fake address.
	if [ -z "$pinned" ]; then
		pinned="$(awk -v h="$host" '$2 == h {print $1; exit}' /etc/hosts)"
	fi
	if [ -z "$pinned" ]; then
		case "$host" in
		# A bare address needs no lookup.
		[0-9]*.[0-9]*.[0-9]*.[0-9]*) pinned="$host" ;;
		*) pinned="$(nslookup "$host" 2>/dev/null |
			awk '/[Aa]ddress/{print $NF}' | grep -Eo '^[0-9]+(\.[0-9]+){3}$' | head -n 1)" ;;
		esac
	fi
	[ -n "$pinned" ] || die "cannot resolve $host — re-run with SERVER_IP_$n=<ip>"
	case "$pinned" in
	198.18.*) die "$host resolved to $pinned, the netstack's fake range: pass SERVER_IP_$n=<real ip>" ;;
	esac
	# A named exit host is pinned so the tunnel's own lookup never needs DNS.
	if [ "$pinned" != "$host" ]; then
		sed -i "/[[:space:]]$host\$/d" /etc/hosts 2>/dev/null || true
		echo "$pinned $host" >> /etc/hosts
		say "pinned $host -> $pinned"
	fi
	echo "$pinned" >> "$DIR/exit-ips"
done

# Keep every exit host off the tunnel even when it is not the active one: the
# failover probes the primary by handshake, and through the tunnel that probe
# would have to leave the exit server first. Re-applied on every WAN ifup.
cat > /usr/bin/ratatosk-pins <<'PINS'
#!/bin/sh
# A /32 route to each exit host via the real gateway.
[ -s /etc/ratatosk/exit-ips ] || exit 0
# shellcheck disable=SC2046 # the fields are read as $1/$2 on purpose
set -- $(ip route 2>/dev/null | awk '$1 == "default" {
	for (i = 2; i <= NF; i++) if ($i == "via") gw = $(i + 1);
	for (i = 2; i <= NF; i++) if ($i == "dev") dev = $(i + 1);
	if (dev !~ /^tun/) { print gw, dev; exit }
}')
gw="$1"
dev="$2"
[ -n "$gw" ] && [ -n "$dev" ] || exit 0
while read -r ip; do
	[ -n "$ip" ] && ip route replace "$ip/32" via "$gw" dev "$dev"
done < /etc/ratatosk/exit-ips
PINS
chmod 0755 /usr/bin/ratatosk-pins

cat > /etc/hotplug.d/iface/98-ratatosk-pins <<'HOTPLUG'
#!/bin/sh
[ "$ACTION" = ifup ] || exit 0
case "$INTERFACE" in wan*) /usr/bin/ratatosk-pins & ;; esac
HOTPLUG
chmod 0755 /etc/hotplug.d/iface/98-ratatosk-pins

# --- kernel knobs -----------------------------------------------------------
# quinn's UDP socket needs room for the bandwidth-delay product: the OpenWrt
# default of 208 KiB caps a single stream at ~5 Mbit/s per 30 ms of RTT.
cat > /etc/sysctl.d/99-ratatosk.conf <<'SYSCTL'
net.core.rmem_max = 4194304
net.core.wmem_max = 4194304
net.core.rmem_default = 262144
net.core.wmem_default = 262144
net.ipv4.ip_forward = 1
net.ipv6.conf.all.forwarding = 0
SYSCTL
sysctl -p /etc/sysctl.d/99-ratatosk.conf >/dev/null 2>&1 || true

# --- static config: routing, DNS policy, TUN knobs --------------------------
# The link supplies the server (see active.txt); everything here stays put.
# Blocking DNS on the way out is what keeps split-tunnelling honest: a client
# that resolves names itself gets real addresses, and then a `.ru` host has no
# name left to match the domain rule against — it would go through the exit.
# Forcing it back to dnsmasq means every LAN flow carries a fake address the
# netstack maps back to a name.
cat > "$DIR/config.toml" <<'CONFIG'
# Written by packaging/openwrt/install.sh: the server comes from the link in
# active.txt, this file owns routing, DNS policy and the TUN.
[inbound]
address = "127.0.0.1"
port = 1080

[routing]
# Split-tunnel: the Russian zone goes out directly, everything else through
# the exit. Same as --split-ru, kept here so one file describes the policy.
direct_suffix = ["ru", "su", "рф"]

# Plain DNS and DoT from the LAN: only dnsmasq may resolve (it forwards to the
# netstack's FakeDNS, which is answered before any routing happens).
[[routing.rules]]
outbound = "block"
port = [53, 853]

# Known DNS-over-HTTPS resolvers, so a browser cannot sidestep dnsmasq and
# lose its names. Clients fall back to the system resolver within seconds.
[[routing.rules]]
outbound = "block"
port = [443]
ip_cidr = [
  "1.1.1.1/32", "1.0.0.1/32",          # Cloudflare
  "8.8.8.8/32", "8.8.4.4/32",          # Google
  "9.9.9.9/32", "149.112.112.112/32",  # Quad9
  "94.140.14.14/32", "94.140.15.15/32",# AdGuard
  "208.67.222.222/32", "208.67.220.220/32", # OpenDNS
  "76.76.2.0/32", "76.76.10.0/32",     # Control D
  "185.228.168.9/32", "185.228.169.11/32", # CleanBrowsing
  "45.90.28.0/24", "45.90.30.0/24",    # NextDNS
  "86.54.11.100/32", "86.54.11.200/32",# DNS0
  "146.112.41.2/32", "146.112.41.3/32",# Cisco
  "156.154.70.2/32", "156.154.71.2/32",# Neustar
  "64.6.64.6/32",                       # Verisign
]

# The same resolvers by name, for clients that resolved the hostname through
# dnsmasq first (then the flow carries the name).
[[routing.rules]]
outbound = "block"
domain = [
  "dns.google", "cloudflare-dns.com", "mozilla.cloudflare-dns.com",
  "dns.quad9.net", "dns.adguard-dns.com", "doh.opendns.com",
  "doh.cleanbrowsing.org", "dns.nextdns.io", "dns.dns.sb",
  "common.dot.dns.yandex.net", "doh.pub", "dns.alidns.com",
]

[tun]
enabled = true
# Upstream for `direct`-routed names (the netstack asks this one itself; the
# block rules above apply to LAN flows only).
dns = "8.8.8.8"
CONFIG
chmod 600 "$DIR/config.toml"
# What the watchdog probes through the tunnel; must not be blocked above.
[ -f "$DIR/probe-url" ] || echo "https://example.com/" > "$DIR/probe-url"
chmod 600 "$DIR/probe-url"

# --- ratatosk service (native TUN, whole system) ----------------------------
cat > /etc/init.d/ratatosk <<'INIT'
#!/bin/sh /etc/rc.common
# ratatosk: native TUN for the whole router; LAN traffic is forwarded into it
# by the firewall. `active.txt` holds the exit in use — the watchdog switches
# it between the links of `servers.txt`.
USE_PROCD=1
START=95
STOP=10
PROG=/usr/bin/ratatosk

start_service() {
	procd_open_instance
	procd_set_param command "$PROG" -c /etc/ratatosk/config.toml \
		-f /etc/ratatosk/active.txt --tun
	procd_set_param respawn 3600 5 10
	procd_set_param stdout 1
	procd_set_param stderr 1
	procd_set_param env HOME=/root
	procd_close_instance
}
INIT
chmod 0755 /etc/init.d/ratatosk

# --- exit watchdog ----------------------------------------------------------
# ratatosk holds one exit per process, so switching links means restarting it.
# Every 20 s the tunnel is probed through its own device; three failures in a
# row move to the next link in servers.txt. While running on a fallback, the
# primary is retried (a bare handshake) and taken back when it answers again.
cat > /usr/bin/ratatosk-failover <<'FAILOVER'
#!/bin/sh
# Switch the exit in /etc/ratatosk/active.txt when the tunnel stops working.
set -u
DIR=/etc/ratatosk
PROG=/usr/bin/ratatosk
# Anything reachable only through the tunnel will do; a DoH resolver would be
# a bad choice, it is blocked by config.toml on purpose.
PROBE="$(cat "$DIR/probe-url" 2>/dev/null || echo https://example.com/)"
FAILS=0
TICKS=0

# BusyBox has no `ip -br`: read the device names out of `ip link show`.
tun_dev() {
	ip link show 2>/dev/null | awk '/^[0-9]+: tun/{sub(":", "", $2); print $2; exit}'
}

probe() {
	dev="$(tun_dev)"
	[ -n "$dev" ] || return 1
	curl -fsS -m 8 -o /dev/null --interface "$dev" "$PROBE"
}

# Restart on the link at position $1 (1-based); no-op when already active.
switch_to() {
	link="$(sed -n "$1p" "$DIR/servers.txt")"
	[ -n "$link" ] || return 1
	[ "$link" = "$(cat "$DIR/active.txt")" ] && return 0
	echo "ratatosk-failover: switching to link $1 ($(printf '%s' "$link" | sed -E 's#^([a-z0-9]+://)[^@]*@([^:/?#]+).*#\1***@\2#'))"
	printf '%s\n' "$link" > "$DIR/active.txt"
	chmod 600 "$DIR/active.txt"
	/etc/init.d/ratatosk restart
	sleep 5
}

while :; do
	sleep 20
	TICKS=$((TICKS + 1))
	# A tunnel restart removes the exit-host pin of the link it was running
	# (ratatosk deletes the routes it installed), so put the pins back before
	# probing: the switch-back probe must not enter the tunnel.
	/usr/bin/ratatosk-pins
	count=$(grep -vc '^[[:space:]]*#' "$DIR/servers.txt")
	[ "$count" -gt 1 ] || continue
	active=$(grep -n -Fx -f "$DIR/active.txt" "$DIR/servers.txt" | head -n 1 | cut -d: -f1)
	[ -n "$active" ] || active=1

	if probe; then
		FAILS=0
		# Back on a fallback and the primary answers: prefer it again.
		if [ "$active" -gt 1 ] && [ $((TICKS % 5)) -eq 0 ]; then
			if "$PROG" --probe -f "$DIR/servers.txt" >/dev/null 2>&1; then
				switch_to 1
				TICKS=0
			else
				echo "ratatosk-failover: primary still down, staying on link $active"
			fi
		fi
		continue
	fi

	FAILS=$((FAILS + 1))
	echo "ratatosk-failover: probe failed ($FAILS/3) on link $active"
	if [ "$FAILS" -ge 3 ]; then
		next=$((active % count + 1))
		switch_to "$next"
		FAILS=0
		TICKS=0
	fi
done
FAILOVER
chmod 0755 /usr/bin/ratatosk-failover

cat > /etc/init.d/ratatosk-failover <<'INIT'
#!/bin/sh /etc/rc.common
# Keeps the tunnel on a working exit (see /usr/bin/ratatosk-failover).
USE_PROCD=1
START=96
STOP=10

start_service() {
	procd_open_instance
	procd_set_param command /usr/bin/ratatosk-failover
	procd_set_param respawn 3600 5 10
	procd_set_param stdout 1
	procd_set_param stderr 1
	procd_close_instance
}
INIT
chmod 0755 /etc/init.d/ratatosk-failover

# --- firewall ---------------------------------------------------------------
# A `proxy` zone that owns the TUN (masqueraded, so LAN flows look like the
# router's own to the netstack), the only forwarding the LAN gets, and a
# reload-on-appearance hook for boot order.
cat > /etc/hotplug.d/net/99-proxy-fw <<'HOTPLUG'
#!/bin/sh
# fw4 caches device names: point the proxy zone at the tun that just appeared
# (the netstack picks the first free tun device, which need not be tun0).
[ "$ACTION" = add ] || exit 0
case "$DEVICE" in tun*) ;; *) exit 0 ;; esac
if [ "$(uci -q get firewall.proxy.device)" != "$DEVICE" ]; then
	uci set firewall.proxy.device="$DEVICE"
	uci commit firewall
fi
/etc/init.d/firewall reload >/dev/null 2>&1 &
HOTPLUG
chmod 0755 /etc/hotplug.d/net/99-proxy-fw

firewall_zone() {
	tun="$1"
	uci -q delete firewall.proxy || true
	uci set firewall.proxy=zone
	uci set firewall.proxy.name='proxy'
	uci set firewall.proxy.input='ACCEPT'
	uci set firewall.proxy.output='ACCEPT'
	uci set firewall.proxy.forward='ACCEPT'
	uci set firewall.proxy.masq='1'
	uci set firewall.proxy.mtu_fix='1'
	uci set firewall.proxy.device="$tun"
	# The LAN never talks to the WAN directly: fail-closed. Only lan -> proxy
	# survives, so a dead tunnel means no LAN internet at all.
	i=0
	while uci -q get "firewall.@forwarding[$i]" >/dev/null 2>&1; do
		if [ "$(uci -q get "firewall.@forwarding[$i].src")" = lan ]; then
			uci -q delete "firewall.@forwarding[$i]"
		else
			i=$((i + 1))
		fi
	done
	uci -q delete firewall.lan_proxy || true
	uci set firewall.lan_proxy=forwarding
	uci set firewall.lan_proxy.src='lan'
	uci set firewall.lan_proxy.dest='proxy'
	# The old chain blocked all router-originated DNS to the WAN (a leak
	# guard for the sing-box setup). The netstack needs that path for the
	# resolver behind `direct`-routed domains, so the rule goes away.
	uci -q delete firewall.block_dns_leak || true
	# Exit hosts stay reachable from the LAN even though the LAN may not use
	# the WAN directly: their /32 routes make the kernel send those packets
	# out the WAN, where the fail-closed policy would drop them.
	uci -q delete firewall.exits_direct || true
	uci set firewall.exits_direct=rule
	uci set firewall.exits_direct.name='Allow-LAN-to-exits'
	uci set firewall.exits_direct.src='lan'
	uci set firewall.exits_direct.dest='wan'
	uci set firewall.exits_direct.dest_ip="$(tr '\n' ' ' < "$DIR/exit-ips" 2>/dev/null)"
	uci set firewall.exits_direct.target='ACCEPT'
	# No IPv6 escape hatch: the netstack is IPv4-only.
	uci -q delete firewall.ipv6_leak || true
	uci set firewall.ipv6_leak=rule
	uci set firewall.ipv6_leak.name='Block-IPv6-leak'
	uci set firewall.ipv6_leak.family='ipv6'
	uci set firewall.ipv6_leak.src='lan'
	uci set firewall.ipv6_leak.dest='wan'
	uci set firewall.ipv6_leak.target='REJECT'
	uci commit firewall
	/etc/init.d/firewall reload
}

# --- DNS --------------------------------------------------------------------
# dnsmasq forwards to the netstack's FakeDNS, so names are resolved inside the
# tunnel and the answers are fake addresses the netstack maps back to names.
# Rebind protection would drop those 198.18/15 answers, hence it is off.
dns_setup() {
	uci set dhcp.@dnsmasq[0].noresolv='1'
	uci set dhcp.@dnsmasq[0].rebind_protection='0'
	while uci -q delete dhcp.@dnsmasq[0].server >/dev/null 2>&1; do :; done
	uci add_list dhcp.@dnsmasq[0].server="$FAKE_DNS"
	# The ISP resolvers must not sneak back in through DHCP.
	uci set network.wan.peerdns='0' 2>/dev/null || true
	uci set network.wan6.peerdns='0' 2>/dev/null || true
	uci commit dhcp
	uci commit network
}

# --- bring it up ------------------------------------------------------------
say "starting ratatosk"
/etc/init.d/ratatosk enable
/etc/init.d/ratatosk restart

tun=""
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
	tun="$(ip link show 2>/dev/null | awk '/^[0-9]+: tun/{sub(":", "", $2); print $2; exit}')"
	[ -n "$tun" ] && break
	sleep 1
done
[ -n "$tun" ] || die "ratatosk did not create a tun device; see logread"
say "tun device: $tun"

say "pinning the exit hosts off the tunnel"
/usr/bin/ratatosk-pins

dns_setup
firewall_zone "$tun"
/etc/init.d/dnsmasq restart
# With a single exit there is nothing to fail over to: the LAN simply loses
# its internet when that exit is down (the firewall only forwards into the
# tunnel), so the watchdog stays off.
links_count=$(grep -vc '^[[:space:]]*#' "$DIR/servers.txt")
if [ "$links_count" -gt 1 ]; then
	/etc/init.d/ratatosk-failover enable
	# An already running watchdog keeps the old probe and link list in memory,
	# so stop it first: `start` alone is a no-op on a live service.
	/etc/init.d/ratatosk-failover stop 2>/dev/null || true
	/etc/init.d/ratatosk-failover start 2>/dev/null || /etc/init.d/ratatosk-failover restart || true
else
	/etc/init.d/ratatosk-failover stop 2>/dev/null || true
	/etc/init.d/ratatosk-failover disable 2>/dev/null || true
	say "single exit: no failover, a dead tunnel leaves the LAN without internet"
fi

# --- verify -----------------------------------------------------------------
say "verifying the tunnel"
probe_url="$(cat "$DIR/probe-url")"
code="$(curl -fsS -m 15 -o /dev/null -w '%{http_code}' --interface "$tun" "$probe_url" 2>/dev/null || true)"
[ -n "$code" ] && [ "$code" != "000" ] || die "the tunnel does not answer; check logread | grep ratatosk"
say "tunnel answers $probe_url with $code"
cat <<MSG

Done. Every LAN device now egresses through the exit in /etc/ratatosk/active.txt.
  /etc/init.d/ratatosk stop       # cut the LAN off (fail-closed)
  /etc/init.d/ratatosk start
  ratatosk-failover's log: logread | grep ratatosk-failover
  links in use: cat /etc/ratatosk/servers.txt (first = primary)
MSG
