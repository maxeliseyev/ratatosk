//! Desktop benchmark for the core relay path: SOCKS5 inbound -> VLESS -> a
//! local mock server that echoes bytes back. Measures idle CPU, CPU under
//! load, memory (RSS) and throughput, so energy/performance changes have a
//! number to compare against.
//!
//! Build in release for meaningful numbers:
//!
//! ```sh
//! cargo run --release --example bench -- --mbytes 100 --idle-secs 10
//! ```
//!
//! Output is one JSON object on stdout (plus a couple of log lines on
//! stderr). The measured CPU and RSS are for the whole benchmark process,
//! which includes the mock server and the client: idle numbers are exact,
//! load numbers are a fair but slightly pessimistic upper bound on the
//! core's own cost (the echo server does symmetric work).

use anyhow::{anyhow, bail, Result};
use ratatosk_core::protocol::vless;
use ratatosk_core::proxy::runtime::serve_with_shutdown;
use ratatosk_core::{
    ActiveServer, Config, DnsCacheConfig, DnsConfig, InboundConfig, MuxConfig, RoutingConfig,
    RuntimeConfig, ServerConfig,
};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

const ECHO_PORT: u16 = 7000;
const BUF: usize = 256 * 1024;

#[derive(Default)]
struct Args {
    mbytes: usize,
    idle_secs: u64,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        mbytes: 100,
        idle_secs: 10,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mbytes" => {
                args.mbytes = it
                    .next()
                    .ok_or_else(|| anyhow!("--mbytes needs a number"))?
                    .parse()?;
            }
            "--idle-secs" => {
                args.idle_secs = it
                    .next()
                    .ok_or_else(|| anyhow!("--idle-secs needs a number"))?
                    .parse()?;
            }
            "--help" | "-h" => {
                println!("usage: bench [--mbytes N] [--idle-secs N]");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?} (try --help)"),
        }
    }
    if args.mbytes == 0 {
        bail!("--mbytes must be positive");
    }
    Ok(args)
}

/// Process CPU seconds so far (user + system), from `getrusage`.
fn cpu_seconds() -> f64 {
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0.0;
        }
        let to_secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 * 1e-6;
        to_secs(usage.ru_utime) + to_secs(usage.ru_stime)
    }
}

/// Resident set size in bytes.
#[cfg(target_os = "linux")]
fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    pages * page_size
}

/// Resident set size in bytes.
#[cfg(target_os = "macos")]
// `mach_task_self` is deprecated in libc in favour of the `mach2` crate; a
// benchmark example is not worth the extra dependency.
#[allow(deprecated)]
fn rss_bytes() -> u64 {
    unsafe {
        let mut info: libc::mach_task_basic_info = std::mem::zeroed();
        let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
        let kr = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            &mut info as *mut libc::mach_task_basic_info as libc::task_info_t,
            &mut count,
        );
        if kr == libc::KERN_SUCCESS {
            info.resident_size as u64
        } else {
            0
        }
    }
}

/// Skip a VLESS request header the way a server would; the mock ignores the
/// UUID and target, it only needs to get past the header to the payload.
async fn read_vless_request_header<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<()> {
    if s.read_u8().await? != vless::VERSION {
        bail!("unexpected VLESS version");
    }
    let mut uuid = [0u8; 16];
    s.read_exact(&mut uuid).await?;
    let addons_len = s.read_u8().await? as usize;
    let mut addons = vec![0u8; addons_len];
    s.read_exact(&mut addons).await?;
    let command = s.read_u8().await?;
    if command == vless::Command::Mux as u8 {
        return Ok(());
    }
    let _port = s.read_u16().await?;
    match s.read_u8().await? {
        1 => {
            let mut octets = [0u8; 4];
            s.read_exact(&mut octets).await?;
        }
        2 => {
            let len = s.read_u8().await? as usize;
            let mut domain = vec![0u8; len];
            s.read_exact(&mut domain).await?;
        }
        3 => {
            let mut octets = [0u8; 16];
            s.read_exact(&mut octets).await?;
        }
        other => bail!("unexpected VLESS address type {other}"),
    }
    Ok(())
}

/// A VLESS session that echoes everything back, so the client can both push
/// and pull at line rate.
async fn echo_session(mut stream: TcpStream) -> Result<()> {
    read_vless_request_header(&mut stream).await?;
    stream.write_all(&[vless::VERSION, 0]).await?;
    stream.flush().await?;
    let mut buf = vec![0u8; BUF];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
        stream.flush().await?;
    }
    Ok(())
}

/// A loopback VLESS server that echoes payloads. Runs until aborted.
async fn spawn_echo_server() -> Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let addr = listener.local_addr()?;
    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        if let Err(e) = echo_session(stream).await {
                            eprintln!("echo session: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    eprintln!("echo server accept: {e}");
                    break;
                }
            }
        }
    });
    Ok((addr, task))
}

async fn socks5_connect(stream: &mut TcpStream, host: &str, port: u16) -> Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut auth_reply = [0u8; 2];
    stream.read_exact(&mut auth_reply).await?;
    if auth_reply != [5, 0] {
        bail!("unexpected SOCKS5 auth reply: {auth_reply:?}");
    }
    stream.write_all(&[5, 1, 0, 3, host.len() as u8]).await?;
    stream.write_all(host.as_bytes()).await?;
    stream.write_all(&port.to_be_bytes()).await?;
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await?;
    if reply[1] != 0 {
        bail!("SOCKS5 connect failed with reply code {}", reply[1]);
    }
    Ok(())
}

/// Bind a listener, remember its port, then release it: a port that is free
/// right now for the proxy to bind.
async fn reserve_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    Ok(listener.local_addr()?.port())
}

async fn wait_for_listener(addr: SocketAddr) -> Result<()> {
    for _ in 0..100 {
        if TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(anyhow!("listener did not start on {addr}"))
}

fn proxy_config(proxy_port: u16, server_addr: SocketAddr) -> Config {
    Config {
        server: ServerConfig {
            address: server_addr.ip().to_string(),
            port: server_addr.port(),
            uuid: Uuid::nil(),
            use_tls: false,
            security: "none".to_string(),
            encryption: "none".to_string(),
            sni: None,
            fingerprint: None,
            reality_public_key: None,
            reality_short_id: None,
            flow: None,
            spider_x: None,
            transport: "tcp".to_string(),
            ws_path: None,
            ws_host: None,
            alpn: None,
            xhttp_mode: None,
            xhttp_extra: None,
            remark: None,
        },
        inbound: InboundConfig {
            address: Ipv4Addr::LOCALHOST.to_string(),
            port: proxy_port,
            protocol: "socks5".to_string(),
        },
        dns: DnsConfig {
            mode: "remote".to_string(),
            strategy: "prefer_ipv4".to_string(),
            rules: vec![],
            cache: DnsCacheConfig::default(),
        },
        runtime: RuntimeConfig {
            connect_timeout_secs: 5,
            read_timeout_secs: 300,
            write_timeout_secs: 60,
            idle_timeout_secs: 600,
            max_connections: 1024,
            max_parallel_handshakes: 3,
            xray_path: "xray".to_string(),
            reality_engine: "auto".to_string(),
        },
        tun: Default::default(),
        routing: RoutingConfig::default(),
        mux: MuxConfig::default(),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = parse_args()?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::io::stderr)
        .init();

    let (server_addr, server_task) = spawn_echo_server().await?;
    let proxy_port = reserve_port().await?;
    let proxy_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, proxy_port));
    let config = Arc::new(proxy_config(proxy_port, server_addr));
    let active = ActiveServer::new(config.server.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let proxy_task: JoinHandle<Result<()>> = {
        let (config, active) = (config.clone(), active);
        tokio::spawn(async move { serve_with_shutdown(config, active, shutdown_rx).await })
    };
    wait_for_listener(proxy_addr).await?;

    // ── idle phase ────────────────────────────────────────────────────────
    let cpu0 = cpu_seconds();
    tokio::time::sleep(Duration::from_secs(args.idle_secs)).await;
    let idle_cpu_secs = cpu_seconds() - cpu0;
    let idle_cpu_pct = idle_cpu_secs / args.idle_secs as f64 * 100.0;
    let rss_idle_mb = rss_bytes() as f64 / 1_048_576.0;

    // ── load phase: one SOCKS5 session, full-duplex against the echo ──────
    let target = args.mbytes * 1024 * 1024;
    let mut client = TcpStream::connect(proxy_addr).await?;
    socks5_connect(&mut client, "bench.example", ECHO_PORT).await?;
    let (mut rd, mut wr) = client.into_split();

    let sent = Arc::new(AtomicUsize::new(0));
    let sent_counter = sent.clone();
    let writer = tokio::spawn(async move {
        let buf = vec![0xABu8; BUF];
        while wr.write_all(&buf).await.is_ok() {
            sent_counter.fetch_add(buf.len(), Ordering::Relaxed);
        }
    });

    let cpu0 = cpu_seconds();
    let wall0 = Instant::now();
    let mut received = 0usize;
    let mut buf = vec![0u8; BUF];
    while received < target {
        match rd.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => received += n,
            Err(e) => {
                eprintln!("load read: {e}");
                break;
            }
        }
    }
    let wall = wall0.elapsed();
    let load_cpu_secs = cpu_seconds() - cpu0;
    writer.abort();

    let rss_load_mb = rss_bytes() as f64 / 1_048_576.0;
    let wall_secs = wall.as_secs_f64();
    let sent_bytes = sent.load(Ordering::Relaxed);
    let total_mbit = (received + sent_bytes) as f64 * 8.0 / 1e6;
    let down_mbps = received as f64 * 8.0 / 1e6 / wall_secs;
    let up_mbps = sent_bytes as f64 * 8.0 / 1e6 / wall_secs;
    let load_cpu_pct = load_cpu_secs / wall_secs * 100.0;
    let cpu_per_mbit = if total_mbit > 0.0 {
        load_cpu_secs / total_mbit
    } else {
        f64::NAN
    };

    // ── report ────────────────────────────────────────────────────────────
    let report = serde_json::json!({
        "platform": std::env::consts::OS,
        "idle": {
            "secs": args.idle_secs,
            "cpu_secs": idle_cpu_secs,
            "cpu_pct": idle_cpu_pct,
        },
        "load": {
            "target_mbytes": args.mbytes,
            "received_bytes": received,
            "sent_bytes": sent_bytes,
            "wall_secs": wall_secs,
            "cpu_secs": load_cpu_secs,
            "cpu_pct": load_cpu_pct,
            "down_mbps": down_mbps,
            "up_mbps": up_mbps,
            "cpu_per_mbit": cpu_per_mbit,
        },
        "rss_idle_mb": rss_idle_mb,
        "rss_load_mb": rss_load_mb,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);

    // ── teardown ──────────────────────────────────────────────────────────
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), proxy_task).await;
    server_task.abort();
    Ok(())
}
