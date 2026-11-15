# ratatosk on OpenWrt — transparent whole-LAN router

Turn an OpenWrt router into a transparent anti-censorship gateway: every device
on its LAN egresses through the h3/QUIC transport to the exit server, with DNS
resolved inside the tunnel (no leak). Fail-closed — if the tunnel is down the
LAN has no internet.

```
LAN device ─▶ tun ─▶ sing-box ─▶ SOCKS ─▶ ratatosk ─▶ h3/QUIC ─▶ exit server ─▶ internet
            (system) (DNS hijack → DoH over the proxy)   (Chrome-shaped QUIC)
```

`ratatosk` speaks our transport and exposes a local SOCKS5 inbound; `sing-box` is
the TUN + routing + DNS layer. sing-box resolves names with its own DoH **over
the proxy** because raw UDP DNS through SOCKS is unreliable. The firewall only
forwards LAN traffic into the tunnel, never to WAN, so a dead tunnel means no
internet — not a silent fallback to the ISP.

## Requirements

- **arch: aarch64 or x86_64.** The crypto core (aws-lc-rs) has no MIPS/ARMv7
  build, so most cheap routers are out. Tested on a GL.iNet GL-AXT1800
  (IPQ6000, `aarch64_cortex-a53`, OpenWrt 25.12).
- ~20 MB free on `/overlay`, ≥128 MB RAM, and the default OpenWrt package feeds
  reachable (the installer pulls `sing-box`, `kmod-tun`, `curl`). On small
  routers the `system` TUN stack is used; it has a much smaller footprint than
  gVisor and does not OOM under load.
- A connection link for an account on the exit server, either the h3/QUIC
  transport (`h3://<uuid>@<host>:443?sni=<host>#label`) or VLESS REALITY
  (`vless://<uuid>@<host>:443?security=reality&...#label`).

## Install

On the router (or over ssh):

```sh
sh install.sh 'h3://<uuid>@<host>:443?sni=<host>#label'      # or a vless:// REALITY link
```

It installs the deps, fetches the `ratatosk` binary for this arch from the GitHub
release, writes the link, pins the exit host in `/etc/hosts`, generates the
sing-box config, enables both services (`ratatosk` then `sing-box`, as root), and
starts them. Re-running is safe (idempotent). Verify:

```sh
curl -s https://ipinfo.io/json      # should show the exit server's IP/city
```

Env overrides: `RATATOSK_VERSION`, `SERVER_IP` (skip the DNS lookup), `SOCKS_PORT`
(default 1080), `REPO` (GitHub `owner/repo` of the release).

## What it configures

| Path | Purpose |
|------|---------|
| `/usr/bin/ratatosk` | the client binary (static aarch64/x86_64 musl) |
| `/etc/ratatosk/link` | the `h3://` connection link (mode 600) |
| `/etc/init.d/ratatosk` | procd service, SOCKS5 on `127.0.0.1:1080` |
| `/etc/sing-box/config.json` | tun + DNS-hijack + routing (system stack) |
| `/etc/config/sing-box` | uci: `user root`, `enabled 1` |
| `/etc/config/firewall` | a `proxy` zone (tun0, masq, MSS clamp), `lan → proxy` only, and a WAN DNS-leak block |
| `/etc/config/dhcp` | dnsmasq with `noresolv` + a public upstream, so LAN DNS goes through the tunnel |
| `/etc/hotplug.d/net/99-proxy-fw` | reloads fw4 when tun0 appears (boot order) |
| `/etc/hosts` | pins `<exit-host> -> <ip>` (see note) |

**Why the `/etc/hosts` pin matters:** without it, ratatosk's own DNS lookup of the
exit host is pulled into the tunnel it is still building → deadlock. The pin
lets ratatosk connect by IP, and the sing-box route rule `<ip>/32 → direct` keeps
its QUIC off the tunnel (no loop).

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

## Operate

```sh
/etc/init.d/sing-box stop      # pause the tunnel (LAN loses internet: fail-closed)
/etc/init.d/sing-box start
logread | grep -i ratatosk        # ratatosk logs
cat /tmp/sb.log                 # sing-box logs
```

## Notes & limits

- **Fail-closed**: the installer removes the stock `lan → wan` forwarding and
  blocks router-originated DNS to WAN. If `sing-box` is down, LAN clients get
  no internet and no DNS; they never fall back to the ISP path. At boot there
  is a brief window with no LAN internet until `sing-box` creates `tun0`.
- UDP DNS over the proxy does not work; dnsmasq sends queries to a public
  resolver, sing-box routes that output into `tun0` and hijacks it, and the
  actual lookup goes over DoH (TCP) through the proxy.
- This is an install script, not yet a real feed package (`.apk`/`.ipk` built
  with the OpenWrt SDK). That — a procd init + uci + a `sing-box` dependency in
  a package manifest — is the remaining productization step.
