//! Native TUN mode: ratatosk owns the TUN interface and runs the in-process
//! netstack (`ratatosk_core::netstack`) directly — no sing-box.
//!
//! The netstack turns tunnelled packets into routed connections through
//! `ProxyClient`. This module does the OS plumbing sing-box used to do: create
//! the TUN, point the default route and DNS at it, keep ratatosk's own sockets
//! off the tunnel, and restore all of it on exit.
//!
//! Loop avoidance: every exit in the list gets a host route via the original
//! gateway, so a server switched to at runtime stays off the tunnel too;
//! `direct`-routed flows egress the
//! physical interface because `ProxyClient` is given its name (`IP_BOUND_IF` on
//! macOS, `SO_BINDTODEVICE` on Linux — see `ratatosk_core::proxy::egress`);
//! LAN/private ranges are routed around the tunnel outright.
//!
//! The netstack, the TUN device creation and the connector are shared; each OS
//! differs only in how it reads the default route and sets routes + DNS
//! (`route`/`networksetup` on macOS, `ip`/`resolv.conf` on Linux).

use anyhow::Result;
use ratatosk_core::{ActiveServer, Config};
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::sync::watch;

/// Entry point used by the process lifecycle (`crate::runtime`).
///
/// `active` is the same handle the TUI switches: the TUN netstack must see a
/// switch too, otherwise the interface keeps using the exit chosen at start-up
/// while the UI claims otherwise. `exit_ips` are the pre-resolved addresses of
/// every exit the user may switch to (see `shared::prepare_exits`), so all of
/// them stay routed off the tunnel; empty means "just the active server".
pub async fn run(
    config: Arc<Config>,
    shutdown: watch::Receiver<bool>,
    active: ActiveServer,
    exit_ips: Vec<Ipv4Addr>,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::run(config, shutdown, active, exit_ips).await
    }
    #[cfg(target_os = "linux")]
    {
        linux::run(config, shutdown, active, exit_ips).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (config, shutdown, active, exit_ips);
        anyhow::bail!("native --tun is supported on macOS and Linux only")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
use shared::*;

/// Resolve every exit before the TUN redirects DNS to FakeDNS, returning the
/// addresses the interface must keep routed off the tunnel (see
/// [`shared::prepare_exits`]). Empty on platforms without TUN mode.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub async fn prepare_exits(
    servers: &mut ratatosk_core::ServerList,
    active: &mut ratatosk_core::ServerConfig,
) -> Vec<Ipv4Addr> {
    shared::prepare_exits(servers, active).await
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub async fn prepare_exits(
    _servers: &mut ratatosk_core::ServerList,
    _active: &mut ratatosk_core::ServerConfig,
) -> Vec<Ipv4Addr> {
    Vec::new()
}

/// OS-agnostic pieces: the netstack, the TUN device, the connector.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod shared {
    use anyhow::{Context, Result};
    use ratatosk_core::netstack::device::channel_device;
    use ratatosk_core::netstack::fakedns::{FakeDns, DEFAULT_POOL};
    use ratatosk_core::netstack::flows::{FlowLog, DEFAULT_CAPACITY};
    use ratatosk_core::netstack::Netstack;
    use ratatosk_core::proxy::direct_dns::DirectResolver;
    use ratatosk_core::proxy::dns_cache::{DnsCache, DnsCacheConfig};
    use ratatosk_core::proxy::egress::Egress;
    use ratatosk_core::proxy::runtime::ProxyClient;
    use ratatosk_core::{ActiveServer, Config};
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use tokio::sync::watch;
    use tracing::{info, warn};

    pub const FAKE_DNS_CAPACITY: u32 = 16_384;
    /// Routed around the tunnel via the original gateway (LAN, link-local,
    /// multicast). The tunnel's own /30 is more specific, so it still wins.
    pub const LOCAL_NETS: [&str; 5] = [
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "169.254.0.0/16",
        "224.0.0.0/4",
    ];

    pub fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    pub fn new_fake_dns() -> Arc<FakeDns> {
        Arc::new(FakeDns::new(DEFAULT_POOL, FAKE_DNS_CAPACITY))
    }

    /// The proxy connector, pinned to the physical interface so every `direct`
    /// dial egresses there instead of looping back into the tunnel. `direct`
    /// domains are resolved against `config.tun.dns` over the same interface,
    /// since the system resolver is redirected to FakeDNS.
    pub fn connector(
        config: &Config,
        phys_iface: &str,
        active: &ActiveServer,
    ) -> Result<ProxyClient> {
        let egress = Egress::new(phys_iface);
        let cache = DnsCache::new(DnsCacheConfig {
            enabled: config.dns.cache.enabled,
            max_entries: config.dns.cache.max_entries,
            default_ttl_secs: config.dns.cache.ttl_secs,
            min_ttl_secs: config.dns.cache.min_ttl_secs,
            max_ttl_secs: config.dns.cache.max_ttl_secs,
        });
        let direct = DirectResolver::new(
            direct_dns_upstream(&config.tun.dns),
            Some(egress.clone()),
            cache,
        );
        Ok(ProxyClient::new(
            // The TUI's handle, not a private copy: a switch must reach the
            // netstack. `tun_native::run` is given that same handle.
            active.clone(),
            config.dns.clone(),
            config.runtime.clone(),
            &config.routing,
            &config.mux,
        )?
        .with_egress(egress)
        .with_direct_resolver(direct))
    }

    /// `config.tun.dns` as a `SocketAddr`, defaulting the port to 53 and falling
    /// back to Google public DNS if it is not a bare IP.
    pub fn direct_dns_upstream(dns: &str) -> std::net::SocketAddr {
        if let Ok(addr) = dns.parse::<std::net::SocketAddr>() {
            return addr;
        }
        if let Ok(ip) = dns.parse::<std::net::IpAddr>() {
            return std::net::SocketAddr::new(ip, 53);
        }
        std::net::SocketAddr::from(([8, 8, 8, 8], 53))
    }

    /// Create the TUN device (brought up, addressed), returning it and its name.
    pub fn build_device(config: &Config) -> Result<(Arc<tun_rs::AsyncDevice>, String)> {
        let (addr, prefix) = parse_cidr(&config.tun.address)?;
        let device = tun_rs::DeviceBuilder::new()
            .mtu(config.tun.mtu as u16)
            .ipv4(addr, prefix, None)
            .enable(true)
            .build_async()
            .context("create TUN device (needs root)")?;
        let name = device.name().context("read TUN name")?;
        info!("TUN {name} up ({addr}/{prefix}, mtu {})", config.tun.mtu);
        Ok((Arc::new(device), name))
    }

    /// Pump packets between the TUN and the netstack through the channel
    /// device, run the netstack until `shutdown`, then stop and release the TUN.
    pub async fn drive(
        netstack: Netstack,
        device: Arc<tun_rs::AsyncDevice>,
        mtu: u16,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let (chan_dev, mut host) = channel_device();
        let reader_dev = device.clone();
        let to_stack = host.to_stack.clone();
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; mtu as usize + 4];
            loop {
                match reader_dev.recv(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if to_stack.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("TUN read ended: {e}");
                        break;
                    }
                }
            }
        });
        let writer_dev = device.clone();
        let writer = tokio::spawn(async move {
            while let Some(pkt) = host.from_stack.recv().await {
                if writer_dev.send(&pkt).await.is_err() {
                    break;
                }
            }
        });

        let result = netstack.run(chan_dev, shutdown).await;
        reader.abort();
        writer.abort();
        drop(device);
        result
    }

    /// Build the netstack from a connector and a FakeDNS.
    pub fn netstack(connector: ProxyClient, dns: Arc<FakeDns>, mtu: u16) -> Netstack {
        Netstack {
            connector,
            dns,
            mtu,
            flows: Arc::new(FlowLog::new(DEFAULT_CAPACITY)),
        }
    }

    /// Parse "172.18.0.1/30" into an address and prefix length.
    pub fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
        let (addr, prefix) = cidr
            .split_once('/')
            .context("tun.address must be CIDR, e.g. 172.18.0.1/30")?;
        Ok((
            addr.parse()
                .with_context(|| format!("bad tun address {addr}"))?,
            prefix
                .parse()
                .with_context(|| format!("bad prefix /{prefix}"))?,
        ))
    }

    /// Resolve the server to its IPv4 addresses (for the host-route exception),
    /// while normal DNS still works. Empty for an IPv6-literal server.
    pub async fn resolve_server(config: &Config) -> Result<Vec<Ipv4Addr>> {
        let host = config.server.address.clone();
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            return Ok(vec![ip]);
        }
        if host.parse::<std::net::Ipv6Addr>().is_ok() {
            return Ok(Vec::new());
        }
        let addrs = tokio::net::lookup_host((host.as_str(), config.server.port)).await?;
        Ok(addrs
            .filter_map(|a| match a.ip() {
                std::net::IpAddr::V4(v4) => Some(v4),
                std::net::IpAddr::V6(_) => None,
            })
            .collect())
    }

    /// Resolve every exit up front, while the system resolver still tells the
    /// truth, and return the addresses to route around the tunnel.
    ///
    /// Once the tunnel is up, DNS points at FakeDNS, so a hostname dialled
    /// later resolves to a fake address and loops back into the tunnel. Each
    /// server's address is therefore replaced by its first IPv4, with the
    /// original host kept as SNI (the certificate still has to match it). That
    /// is what makes switching exits in the TUI work in TUN mode.
    pub async fn prepare_exits(
        servers: &mut ratatosk_core::ServerList,
        active: &mut ratatosk_core::ServerConfig,
    ) -> Vec<Ipv4Addr> {
        let mut pins = Vec::new();
        for entry in &mut servers.entries {
            pin_exit(&mut entry.server, &mut pins).await;
        }
        pin_exit(active, &mut pins).await;
        pins.sort_unstable();
        pins.dedup();
        pins
    }

    async fn pin_exit(server: &mut ratatosk_core::ServerConfig, pins: &mut Vec<Ipv4Addr>) {
        let host = server.address.clone();
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            pins.push(ip);
            return;
        }
        if host.parse::<std::net::Ipv6Addr>().is_ok() {
            warn!("exit {host} is IPv6-only; the netstack is IPv4-only, leaving it as is");
            return;
        }
        let looked_up = tokio::net::lookup_host((host.as_str(), server.port)).await;
        let ips: Vec<Ipv4Addr> = match looked_up {
            Ok(addrs) => addrs
                .filter_map(|a| match a.ip() {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    std::net::IpAddr::V6(_) => None,
                })
                .collect(),
            Err(e) => {
                warn!("cannot resolve exit {host}: {e}; switching to it will not work in TUN mode");
                return;
            }
        };
        let Some(first) = ips.first().copied() else {
            warn!("exit {host} has no IPv4 address; switching to it will not work in TUN mode");
            return;
        };
        pins.extend(ips);
        // The certificate still has to match the name, and so does a WS Host.
        if server.sni.is_none() {
            server.sni = Some(host.clone());
        }
        server.address = first.to_string();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_cidr() {
            assert_eq!(
                parse_cidr("172.18.0.1/30").unwrap(),
                ("172.18.0.1".parse().unwrap(), 30)
            );
            assert!(parse_cidr("172.18.0.1").is_err());
        }

        #[test]
        fn local_nets_cover_rfc1918_and_multicast() {
            assert!(LOCAL_NETS.contains(&"192.168.0.0/16"));
            assert!(LOCAL_NETS.contains(&"224.0.0.0/4"));
        }

        /// A literal address needs no DNS and no SNI rewriting — but it must
        /// still be pinned so a switch to it stays off the tunnel.
        #[tokio::test]
        async fn prepare_exits_pins_literal_addresses() {
            let link =
                "h3://a1b2c3d4-e5f6-7890-abcd-ef1234567890@1.2.3.4:443?sni=example.com#literal";
            let config = ratatosk_core::config::servers::config_from_link(link, 1080).unwrap();
            let mut server = config.server;
            let mut list = ratatosk_core::ServerList::single(server.clone());
            let pins = prepare_exits(&mut list, &mut server).await;
            assert_eq!(pins, vec![Ipv4Addr::new(1, 2, 3, 4)]);
            assert_eq!(server.address, "1.2.3.4", "a literal stays as it is");
            assert_eq!(server.sni.as_deref(), Some("example.com"), "SNI kept");
            assert_eq!(list.entries[0].server.address, "1.2.3.4");
        }

        /// The regression this module was fixed for: the TUN netstack used to
        /// build its own `ActiveServer`, so a switch in the TUI changed the
        /// local inbound only and the interface kept the exit chosen at start-up.
        #[tokio::test]
        async fn the_netstack_sees_a_server_switch() {
            let h3 = ratatosk_core::config::servers::config_from_link(
                "h3://a1b2c3d4-e5f6-7890-abcd-ef1234567890@1.2.3.4:443?sni=example.com#first",
                1080,
            )
            .unwrap();
            let vless = ratatosk_core::config::servers::config_from_link(
                "vless://b1b2c3d4-e5f6-7890-abcd-ef1234567891@5.6.7.8:443?security=tls&sni=second.example#second",
                1080,
            )
            .unwrap();
            let active = ActiveServer::new(h3.server.clone());
            // "lo" is only a name here: nothing is bound until a flow dials.
            let client = connector(&h3, "lo", &active).unwrap();
            assert_eq!(client.server().uuid, h3.server.uuid);
            active.swap(vless.server.clone(), &h3).unwrap();
            assert_eq!(
                client.server().uuid,
                vless.server.uuid,
                "the netstack's client must follow the shared handle"
            );
        }
    }
}

// ---------------------------------------------------------------- macOS ------

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use anyhow::{bail, Context, Result};
    use ratatosk_core::proxy::runtime::ProxyClient;
    use ratatosk_core::Config;
    use std::net::Ipv4Addr;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;
    use tracing::{info, warn};

    pub async fn run(
        config: Arc<Config>,
        shutdown: watch::Receiver<bool>,
        active: ActiveServer,
        exit_ips: Vec<Ipv4Addr>,
    ) -> Result<()> {
        if !is_root() {
            bail!(
                "TUN mode creates a network interface and must run as root (sudo ratatosk --tun ...)"
            );
        }
        let (gateway, phys_iface) = default_route().context("find the default route")?;
        info!("Default route: gateway {gateway} on {phys_iface}");
        // The TUI resolves every exit before the tunnel starts (`prepare_exits`),
        // so switches keep working; a plain run resolves its single server here.
        let server_ips = if exit_ips.is_empty() {
            resolve_server(&config).await?
        } else {
            exit_ips
        };
        if server_ips.is_empty() {
            bail!(
                "could not resolve the server address {}",
                config.server.address
            );
        }

        let fake_dns = new_fake_dns();
        let dns_addr = fake_dns.server_addr();
        let mtu = config.tun.mtu as u16;
        let (device, iface) = build_device(&config)?;

        let egress_iface = phys_iface.clone();
        let guard = Arc::new(std::sync::Mutex::new(NetGuard {
            gateway,
            iface,
            phys_iface,
            server_ips,
            dns_addr,
            bypass: LOCAL_NETS
                .iter()
                .map(|s| s.to_string())
                .chain(config.tun.bypass_cidrs.iter().cloned())
                .collect(),
            bypass_nets: Vec::new(),
            scoped_default: false,
            service: None,
            saved_dns: None,
        }));
        guard.lock().expect("net guard poisoned").install()?;

        // The TUI's handle, so a server switch reaches the netstack, and the
        // same handle is handed to the network watcher for `network_changed`.
        let connector = connector(&config, &egress_iface, &active)?;
        let mut watcher = watch_network(guard.clone(), connector.clone(), shutdown.clone());
        let netstack = netstack(connector, fake_dns, mtu);
        info!(
            "Native TUN up: all traffic via {}",
            config.server.endpoint()
        );
        let result = drive(netstack, device, mtu, shutdown).await;
        // Stop the watcher before the guard is dropped, so the restore below
        // really happens here and not later in a detached task.
        if tokio::time::timeout(Duration::from_secs(5), &mut watcher)
            .await
            .is_err()
        {
            watcher.abort();
        }
        drop(guard);
        info!("Native TUN stopped, routes and DNS restored");
        result
    }

    /// Follow the default route: a new gateway or interface means the tunnel's
    /// own socket, its host pin and the FakeDNS override all point at a network
    /// that is gone. h3 also has to move to the new path — `network_changed`
    /// rebinds it (or drops it so the next flow redials) and retires mux
    /// sessions queued into the dead one.
    fn watch_network(
        guard: Arc<std::sync::Mutex<NetGuard>>,
        connector: ProxyClient,
        mut shutdown: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut lost = false;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shutdown.changed() => break,
                }
                let Ok((gateway, iface)) = default_route() else {
                    // Between a Wi-Fi drop and the next association there is no
                    // default route; remember it so the recovery is noticed
                    // even when the new network looks identical.
                    lost = true;
                    continue;
                };
                let (moved, stale) = {
                    let g = guard.lock().expect("net guard poisoned");
                    (g.gateway != gateway || g.phys_iface != iface, lost)
                };
                if !moved && !stale {
                    continue;
                }
                if moved {
                    info!("network changed: gateway {gateway} on {iface}");
                    guard
                        .lock()
                        .expect("net guard poisoned")
                        .renumber(&gateway, &iface);
                } else {
                    info!("network is back: gateway {gateway} on {iface}");
                }
                connector.network_changed().await;
                lost = false;
            }
        })
    }

    struct NetGuard {
        gateway: String,
        iface: String,
        /// The physical interface (e.g. en0) — whose `networksetup` service
        /// carries the DNS configuration, not the utun.
        phys_iface: String,
        /// The active exit plus every exit the TUI can switch to: all of them
        /// stay off the tunnel, or a switch would dial into the tunnel.
        server_ips: Vec<Ipv4Addr>,
        dns_addr: Ipv4Addr,
        /// Networks to route around the tunnel (LAN, link-local, multicast,
        /// user bypasses), before any of them is actually installed.
        bypass: Vec<String>,
        bypass_nets: Vec<String>,
        /// Whether we added an interface-scoped default for `phys_iface` (see
        /// `install`); removed on teardown.
        scoped_default: bool,
        service: Option<String>,
        saved_dns: Option<Vec<String>>,
    }

    impl NetGuard {
        fn install(&mut self) -> Result<()> {
            for ip in &self.server_ips {
                pin_route(ip, &self.gateway)?;
            }
            route(&["-n", "add", "-net", "0.0.0.0/1", "-interface", &self.iface])?;
            route(&[
                "-n",
                "add",
                "-net",
                "128.0.0.0/1",
                "-interface",
                &self.iface,
            ])?;
            // `direct` sockets egress the physical NIC via IP_BOUND_IF
            // (egress.rs), which does an interface-scoped route lookup. The
            // plain default route is not scoped, so once the tunnel owns 0/1
            // such a socket gets ENETUNREACH. Add a scoped default for the
            // physical interface so the bound lookup resolves via the gateway.
            let _ = route(&[
                "-n",
                "add",
                "-ifscope",
                &self.phys_iface,
                "default",
                &self.gateway,
            ]);
            self.scoped_default = true;
            self.add_bypass_routes();
            match network_service(&self.phys_iface) {
                Some(service) => self.redirect_dns(service),
                None => warn!(
                    "could not find the network service for {}; DNS not redirected",
                    self.phys_iface
                ),
            }
            Ok(())
        }

        /// A new network: the pins, the scoped default and the bypasses all
        /// belong to the old gateway, and the DNS override to the old service.
        /// The two split-default routes point at the utun and survive.
        fn renumber(&mut self, gateway: &str, iface: &str) {
            for ip in &self.server_ips {
                let _ = route(&["-n", "delete", "-host", &ip.to_string()]);
            }
            if self.scoped_default {
                let _ = route(&["-n", "delete", "-ifscope", &self.phys_iface, "default"]);
                self.scoped_default = false;
            }
            for net in &self.bypass_nets {
                let _ = route(&["-n", "delete", "-net", net]);
            }
            self.bypass_nets.clear();

            self.gateway = gateway.to_string();
            self.phys_iface = iface.to_string();

            for ip in &self.server_ips {
                let _ = pin_route(ip, gateway);
            }
            if route(&["-n", "add", "-ifscope", iface, "default", gateway]).is_ok() {
                self.scoped_default = true;
            }
            self.add_bypass_routes();
            match network_service(iface) {
                Some(service) if self.service.as_deref() != Some(service.as_str()) => {
                    self.restore_dns();
                    self.redirect_dns(service);
                }
                Some(_) => {}
                None => warn!(
                    "could not find the network service for {iface}; DNS left on {:?}",
                    self.service
                ),
            }
        }

        fn add_bypass_routes(&mut self) {
            for net in self.bypass.clone() {
                if route(&["-n", "add", "-net", &net, &self.gateway]).is_ok() {
                    self.bypass_nets.push(net);
                }
            }
        }

        fn redirect_dns(&mut self, service: String) {
            let saved = current_dns(&service);
            if set_dns(&service, &[self.dns_addr.to_string()]).is_ok() {
                info!("DNS -> {} (FakeDNS) on \"{service}\"", self.dns_addr);
                self.service = Some(service);
                self.saved_dns = Some(saved);
            } else {
                warn!("could not set the DNS servers of \"{service}\"");
            }
        }

        fn restore_dns(&mut self) {
            if let (Some(service), Some(saved)) = (self.service.take(), self.saved_dns.take()) {
                if saved.is_empty() {
                    let _ = set_dns(&service, &["Empty".to_string()]);
                } else {
                    let _ = set_dns(&service, &saved);
                }
            }
        }

        fn restore(&mut self) {
            for ip in &self.server_ips {
                let _ = route(&["-n", "delete", "-host", &ip.to_string()]);
            }
            let _ = route(&["-n", "delete", "-net", "0.0.0.0/1"]);
            let _ = route(&["-n", "delete", "-net", "128.0.0.0/1"]);
            if self.scoped_default {
                let _ = route(&["-n", "delete", "-ifscope", &self.phys_iface, "default"]);
            }
            for net in &self.bypass_nets {
                let _ = route(&["-n", "delete", "-net", net]);
            }
            self.restore_dns();
        }
    }

    impl Drop for NetGuard {
        fn drop(&mut self) {
            self.restore();
        }
    }

    /// A `/32` for one exit via `gateway`. `add` fails when a pin survived a
    /// crash (macOS does not flush them with the interface), so fall back to
    /// `change` instead of aborting start-up.
    fn pin_route(ip: &Ipv4Addr, gateway: &str) -> Result<()> {
        let ip = ip.to_string();
        match route(&["-n", "add", "-host", &ip, gateway]) {
            Ok(()) => Ok(()),
            Err(_) => route(&["-n", "change", "-host", &ip, gateway]),
        }
    }

    fn route(args: &[&str]) -> Result<()> {
        let out = Command::new("/sbin/route")
            .args(args)
            .output()
            .context("run /sbin/route")?;
        if !out.status.success() {
            bail!(
                "route {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn default_route() -> Result<(String, String)> {
        let out = Command::new("/sbin/route")
            .args(["-n", "get", "default"])
            .output()
            .context("run route get default")?;
        if !out.status.success() {
            bail!("no default route (route get default failed)");
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |key: &str| {
            text.lines()
                .find_map(|l| l.trim().strip_prefix(&format!("{key}: ")))
                .map(|v| v.trim().to_string())
        };
        Ok((
            field("gateway").context("default route has no gateway")?,
            field("interface").context("default route has no interface")?,
        ))
    }

    fn network_service(iface: &str) -> Option<String> {
        let out = Command::new("/usr/sbin/networksetup")
            .arg("-listnetworkserviceorder")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut current: Option<String> = None;
        for line in text.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix('(') {
                if let Some((num, name)) = rest.split_once(')') {
                    if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
                        current = Some(name.trim().to_string());
                        continue;
                    }
                }
            }
            if t.contains(&format!("Device: {iface}")) {
                return current.clone();
            }
        }
        None
    }

    fn current_dns(service: &str) -> Vec<String> {
        let Ok(out) = Command::new("/usr/sbin/networksetup")
            .args(["-getdnsservers", service])
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("aren't any") || text.contains("any DNS Servers") {
            return Vec::new();
        }
        text.lines()
            .map(str::trim)
            .filter(|l| l.parse::<std::net::IpAddr>().is_ok())
            .map(String::from)
            .collect()
    }

    fn set_dns(service: &str, servers: &[String]) -> Result<()> {
        let out = Command::new("/usr/sbin/networksetup")
            .arg("-setdnsservers")
            .arg(service)
            .args(servers)
            .output()
            .context("run networksetup -setdnsservers")?;
        if !out.status.success() {
            bail!(
                "networksetup -setdnsservers {service}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- Linux ------

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::{bail, Context, Result};
    use ratatosk_core::proxy::runtime::ProxyClient;
    use ratatosk_core::Config;
    use std::net::Ipv4Addr;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::watch;
    use tracing::{info, warn};

    /// Where the system resolver reads from. Replaced while the tunnel is up,
    /// restored on teardown.
    const RESOLV_CONF: &str = "/etc/resolv.conf";

    pub async fn run(
        config: Arc<Config>,
        shutdown: watch::Receiver<bool>,
        active: ActiveServer,
        exit_ips: Vec<Ipv4Addr>,
    ) -> Result<()> {
        if !is_root() {
            bail!(
                "TUN mode creates a network interface and must run as root (sudo ratatosk --tun ...)"
            );
        }
        let (gateway, phys_iface) = default_route().context("find the default route")?;
        info!("Default route: gateway {gateway} on {phys_iface}");
        // The TUI resolves every exit before the tunnel starts (`prepare_exits`),
        // so switches keep working; a plain run resolves its single server here.
        let server_ips = if exit_ips.is_empty() {
            resolve_server(&config).await?
        } else {
            exit_ips
        };
        if server_ips.is_empty() {
            bail!(
                "could not resolve the server address {}",
                config.server.address
            );
        }

        let fake_dns = new_fake_dns();
        let dns_addr = fake_dns.server_addr();
        let mtu = config.tun.mtu as u16;
        let (device, iface) = build_device(&config)?;

        let egress_iface = phys_iface.clone();
        let guard = Arc::new(std::sync::Mutex::new(NetGuard {
            gateway,
            phys_iface,
            iface,
            server_ips,
            dns_addr,
            bypass: LOCAL_NETS
                .iter()
                .map(|s| s.to_string())
                .chain(config.tun.bypass_cidrs.iter().cloned())
                .collect(),
            routed_nets: Vec::new(),
            saved_resolv: None,
        }));
        guard.lock().expect("net guard poisoned").install()?;

        // The TUI's handle, so a server switch reaches the netstack, and the
        // same handle is handed to the network watcher for `network_changed`.
        let connector = connector(&config, &egress_iface, &active)?;
        let mut watcher = watch_network(guard.clone(), connector.clone(), shutdown.clone());
        let netstack = netstack(connector, fake_dns, mtu);
        info!(
            "Native TUN up: all traffic via {}",
            config.server.endpoint()
        );
        let result = drive(netstack, device, mtu, shutdown).await;
        if tokio::time::timeout(Duration::from_secs(5), &mut watcher)
            .await
            .is_err()
        {
            watcher.abort();
        }
        drop(guard);
        info!("Native TUN stopped, routes and DNS restored");
        result
    }

    /// Follow the default route: a new gateway or interface invalidates the
    /// exit pins, while h3 has to move to the new path (`network_changed`).
    fn watch_network(
        guard: Arc<std::sync::Mutex<NetGuard>>,
        connector: ProxyClient,
        mut shutdown: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut lost = false;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shutdown.changed() => break,
                }
                let Ok((gateway, iface)) = default_route() else {
                    lost = true;
                    continue;
                };
                let (moved, stale) = {
                    let g = guard.lock().expect("net guard poisoned");
                    (g.gateway != gateway || g.phys_iface != iface, lost)
                };
                if !moved && !stale {
                    continue;
                }
                if moved {
                    info!("network changed: gateway {gateway} on {iface}");
                    guard
                        .lock()
                        .expect("net guard poisoned")
                        .retarget(&gateway, &iface);
                } else {
                    info!("network is back: gateway {gateway} on {iface}");
                }
                connector.network_changed().await;
                lost = false;
            }
        })
    }

    struct NetGuard {
        gateway: String,
        phys_iface: String,
        iface: String,
        /// The active exit plus every exit the TUI can switch to: all of them
        /// stay off the tunnel, or a switch would dial into the tunnel.
        server_ips: Vec<Ipv4Addr>,
        dns_addr: Ipv4Addr,
        /// Networks to route around the tunnel, before any is installed.
        bypass: Vec<String>,
        /// Networks we added via the gateway (deleted on teardown). The two
        /// split-default routes to the TUN go away with the interface.
        routed_nets: Vec<String>,
        /// Previous `/etc/resolv.conf` content, restored on teardown. `None`
        /// means we did not touch it.
        saved_resolv: Option<Vec<u8>>,
    }

    impl NetGuard {
        fn install(&mut self) -> Result<()> {
            // `replace`, not `add`: a pin can already be there (left by a
            // previous run, installed by the operator, or added by a
            // failover hook), and a plain add would fail with EEXIST and
            // abort start-up.
            // Server IPs via the real gateway (transport off the tunnel).
            // Not recorded for teardown: the pin is harmless once the tunnel
            // is gone, but deleting it breaks a concurrent or the next
            // instance — its transport would enter the tunnel it just built.
            for ip in &self.server_ips {
                let net = format!("{ip}/32");
                ip_route(&[
                    "replace",
                    &net,
                    "via",
                    &self.gateway,
                    "dev",
                    &self.phys_iface,
                ])?;
            }
            // Override the default without deleting it: two /1 halves to the TUN.
            ip_route(&["replace", "0.0.0.0/1", "dev", &self.iface])?;
            ip_route(&["replace", "128.0.0.0/1", "dev", &self.iface])?;
            self.routed_nets.push("0.0.0.0/1".into());
            self.routed_nets.push("128.0.0.0/1".into());
            // LAN / link-local / multicast and configured bypasses around it.
            for net in self.bypass.clone() {
                if ip_route(&[
                    "replace",
                    &net,
                    "via",
                    &self.gateway,
                    "dev",
                    &self.phys_iface,
                ])
                .is_ok()
                {
                    self.routed_nets.push(net);
                }
            }
            // Point the resolver at the netstack's FakeDNS.
            match std::fs::read(RESOLV_CONF) {
                Ok(prev) => {
                    if self.write_resolv() {
                        self.saved_resolv = Some(prev);
                    }
                }
                Err(e) => warn!("could not read {RESOLV_CONF} ({e}); DNS not redirected"),
            }
            Ok(())
        }

        /// A new network: the exit pins and bypasses name the old gateway, and
        /// a network manager may have rewritten `/etc/resolv.conf` in the
        /// meantime. The two split-default routes point at the tun and stay.
        fn retarget(&mut self, gateway: &str, iface: &str) {
            self.gateway = gateway.to_string();
            self.phys_iface = iface.to_string();
            for ip in &self.server_ips {
                let net = format!("{ip}/32");
                let _ = ip_route(&["replace", &net, "via", gateway, "dev", iface]);
            }
            for net in &self.bypass {
                let _ = ip_route(&["replace", net, "via", gateway, "dev", iface]);
            }
            if self.saved_resolv.is_some() {
                self.write_resolv();
            }
        }

        fn write_resolv(&self) -> bool {
            match std::fs::write(RESOLV_CONF, format!("nameserver {}\n", self.dns_addr)) {
                Ok(()) => {
                    info!("DNS -> {} (FakeDNS) in {RESOLV_CONF}", self.dns_addr);
                    true
                }
                Err(e) => {
                    warn!("could not write {RESOLV_CONF}; DNS not redirected ({e})");
                    false
                }
            }
        }

        fn restore(&mut self) {
            for net in self.routed_nets.iter().rev() {
                // Default-split routes are dev routes; the rest carry a gateway.
                if net == "0.0.0.0/1" || net == "128.0.0.0/1" {
                    let _ = ip_route(&["del", net, "dev", &self.iface]);
                } else {
                    let _ = ip_route(&["del", net, "via", &self.gateway, "dev", &self.phys_iface]);
                }
            }
            if let Some(prev) = &self.saved_resolv {
                let _ = std::fs::write(RESOLV_CONF, prev);
            }
        }
    }

    impl Drop for NetGuard {
        fn drop(&mut self) {
            self.restore();
        }
    }

    fn ip_route(args: &[&str]) -> Result<()> {
        let mut full = vec!["route"];
        full.extend_from_slice(args);
        let out = Command::new("ip")
            .args(&full)
            .output()
            .context("run ip route")?;
        if !out.status.success() {
            bail!(
                "ip {}: {}",
                full.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// `(gateway, interface)` of the current default route via `ip route`.
    fn default_route() -> Result<(String, String)> {
        let out = Command::new("ip")
            .args(["-4", "route", "show", "default"])
            .output()
            .context("run ip route show default")?;
        if !out.status.success() {
            bail!("no default route (ip route show default failed)");
        }
        // "default via 192.168.1.1 dev eth0 proto dhcp ..."
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next().context("no default route")?;
        let toks: Vec<&str> = line.split_whitespace().collect();
        let after = |key: &str| {
            toks.iter()
                .position(|t| *t == key)
                .and_then(|i| toks.get(i + 1))
                .map(|s| s.to_string())
        };
        Ok((
            after("via").context("default route has no gateway")?,
            after("dev").context("default route has no interface")?,
        ))
    }
}
