# ratatosk on OpenWrt — transparent whole-LAN gateway

Turn an OpenWrt router into an anti-censorship gateway: every device on its LAN
egresses through the exit server, with DNS resolved inside the tunnel (no leak).
One process, no sidecar — the TUN device, the TCP/IP netstack, FakeDNS and the
routing rules all live in `ratatosk`.

```
LAN device ─▶ tun ─▶ ratatosk: netstack + FakeDNS + routing ─▶ h3/QUIC ─▶ exit
                  (DNS answered as fake IPs, mapped back to names)
```

Fail-closed: the firewall only forwards LAN traffic into the tunnel, never to
the WAN, so a dead tunnel means no LAN internet — not a silent fallback to the
ISP.

Two exits are supported. The first link in `/etc/ratatosk/servers.txt` is the
primary (put `h3://` there: it has the lowest handshake latency), the second is
used automatically while the primary stops answering, and the primary is taken
back once it recovers.

## Requirements

- **arch: aarch64 or x86_64.** The crypto core (aws-lc-rs) has no MIPS/ARMv7
  build, so most cheap routers are out. Tested on a GL.iNet GL-AXT1800
  (IPQ6000, `aarch64_cortex-a53`, OpenWrt 25.12).
- ~25 MB free on `/overlay`, ≥128 MB RAM, and the default OpenWrt package
  feeds reachable (the installer pulls `kmod-tun`, `curl`, `ca-bundle`).
- One or two connection links for accounts on the exit server: h3/QUIC
  (`h3://<uuid>@<host>:443?sni=<host>#label`) and/or VLESS REALITY
  (`vless://<uuid>@<host>:443?security=reality&...#label`).

## Install

On the router (or over ssh):

```sh
sh install.sh 'h3://<uuid>@<host>:443?sni=<host>#stand' \
              'vless://<uuid>@<host>:443?security=reality&...#backup'
```

The second link is optional. The script installs the deps and the binary (from
the GitHub release, checksum verified), writes the links, pins the exit hosts
in `/etc/hosts`, sets the kernel knobs, installs the two services (tunnel +
watchdog), rewires the firewall and dnsmasq, then verifies the tunnel by asking
`1.1.1.1` through it. Re-running is safe (idempotent) and reorders the links to
the arguments given.

Env overrides: `RATATOSK_VERSION` (default `0.20.9`), `RATATOSK_REPO`,
`SERVER_IP_1` / `SERVER_IP_2` (skip the DNS lookup for that link).

Check with your phone or laptop on the Wi-Fi/LAN:

```sh
curl -s https://ipinfo.io/json      # should show the exit server's IP/city
```

## What it configures

| Path | Purpose |
|------|---------|
| `/usr/bin/ratatosk` | the client binary (static aarch64/x86_64 musl) |
| `/etc/ratatosk/config.toml` | routing, DNS policy and TUN knobs (server comes from the link) |
| `/etc/ratatosk/servers.txt` | the links, **first = primary exit** (mode 600) |
| `/etc/ratatosk/active.txt` | the exit in use (the watchdog rewrites it) |
| `/etc/ratatosk/exit-ips` | every exit host address, kept off the tunnel |
| `/usr/bin/ratatosk-pins` | `/32 via <wan gateway>` for each of them, re-applied on WAN ifup |
| `/etc/init.d/ratatosk` | procd service: `-c config.toml -f active.txt --tun` |
| `/usr/bin/ratatosk-failover` | probes the tunnel, switches the exit link |
| `/etc/init.d/ratatosk-failover` | procd service that keeps the watchdog alive |
| `/etc/sysctl.d/99-ratatosk.conf` | UDP socket buffers (4 MB), `ip_forward`, no IPv6 forward |
| `/etc/config/firewall` | a `proxy` zone on the tun (masq + MSS clamp), `lan → proxy` only, IPv6-leak block |
| `/etc/config/dhcp` | dnsmasq: `noresolv`, upstream `198.18.0.2`, rebind protection off |
| `/etc/config/network` | `peerdns=0` on `wan`/`wan6`: the ISP resolvers never come back |
| `/etc/hotplug.d/iface/98-ratatosk-pins` | re-pins the exit hosts when the WAN comes up |
| `/etc/hotplug.d/net/99-proxy-fw` | re-points the fw4 `proxy` zone at the tun that appeared |
| `/etc/hosts` | pins `<exit-host> → <ip>` (see note) |

**Why every exit host is pinned, not just the active one:** the failover probes
the primary with a bare handshake while the router is running on the fallback.
Without a `/32` route to the primary through the real gateway, that probe would
enter the active tunnel and have to leave the exit server first — an extra,
fragile hop that made the switch-back useless in testing. The pins are also
re-applied on every WAN ifup, because a new lease brings a new gateway.

**Why the `/etc/hosts` pin matters:** without it, ratatosk's own DNS lookup of
the exit host is pulled into the tunnel it is still building → deadlock. With
the pin it connects by IP; the kernel route the binary installs for the exit
keeps that connection off the tunnel (no loop).

**Why dnsmasq points at `198.18.0.2`:** that is the netstack's FakeDNS address
(`DEFAULT_POOL + 2`). dnsmasq caches, LAN clients keep the fake address, and
the netstack maps it back to the name and resolves it over the proxy. Answers
live in `198.18.0.0/15`, which dnsmasq's rebind protection would drop, hence it
is disabled — and `peerdns=0` keeps the ISP resolvers out of the mix.
ratatosk also writes `198.18.0.2` into `/etc/resolv.conf`, but on OpenWrt that
file is regenerated by netifd, so it is treated as a hint, not as the setup.

**Why masquerade on the `proxy` zone:** LAN packets get their source rewritten
to the tun address, so the netstack handles them exactly like the router's own
traffic, and the replies come back through conntrack to the client.

**Why `config.toml` and the link separately:** `ratatosk` accepts a TOML
together with a link, the file holding everything but the server. The routing
policy, the DNS blocks and the TUN settings stay static while the failover
swaps the link underneath.

**Why DNS is blocked at the exit besides dnsmasq:** split-tunnelling matches
*names*. A client that resolves on its own (DoH, plain DNS to 8.8.8.8) ends up
connecting to a real address, the flow carries no name, and a `.ru` host then
leaves through the exit. The config blocks port 53/853 and the well-known DoH
resolvers for LAN flows, so clients come back to dnsmasq and get a fake address
the netstack maps back to a name. `--split-ru` is the same policy expressed as
`[routing] direct_suffix`.

## Operate

```sh
/etc/init.d/ratatosk status
/etc/init.d/ratatosk stop          # cut the LAN off (fail-closed)
/etc/init.d/ratatosk start
logread | grep ratatosk-failover   # exit switches, probe failures
logread | grep ratatosk            # tunnel logs
ratatosk --ping -f /etc/ratatosk/servers.txt   # handshake latency per link
```

To change the order by hand: edit `/etc/ratatosk/servers.txt` (first line is
the primary) and `/etc/init.d/ratatosk restart`, or just re-run `install.sh`
with the links in the wanted order.

## Wi-Fi

Join LAN clients over Wi-Fi so they get routed too. On a fresh flash both radios
are disabled; enable with (replace name/password/country):

```sh
for i in 0 1; do
  uci set wireless.default_radio$i.ssid='MyNet'
  uci set wireless.default_radio$i.encryption='sae-mixed'   # WPA2/WPA3
  uci set wireless.default_radio$i.key='a-strong-password'
  uci set wireless.default_radio$i.disabled='0'
  uci set wireless.radio$i.country='DE'
done
uci commit wireless && wifi reload
```

## Notes & limits

- **One exit per process.** ratatosk holds a single server per instance, so a
  switch is a service restart (a second of LAN downtime). The watchdog does it
  after three failed probes, and retries the primary every ~100 s while on a
  fallback (a handshake over the pinned `/32`, so the retry never depends on
  the tunnel it is trying to replace).
- **IPv6 is not tunnelled** (the netstack is IPv4-only): an fw4 rule rejects
  LAN→WAN IPv6 so nothing leaks around the tunnel.
- **Migrating from the sing-box setup:** the installer stops and removes the
  `sing-box` package, the `vproxy*` init scripts and `/etc/vproxy`. The old
  chain (ratatosk SOCKS + sing-box TUN) is what `git log` before v0.20.3 shows.
- This is an install script, not yet a real feed package. The productization
  step is an `.apk` built with the OpenWrt SDK: procd init, uci config and a
  package manifest instead of a shell script that edits `/etc/config/*`.
