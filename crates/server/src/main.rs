//! `ratatosk-server -c server.toml [-v]`: run the h3 transport server.
//! `ratatosk-server --stats [--admin <socket>]`: print a live stats snapshot.
//! `ratatosk-server accounts …`: manage accounts on a running server.

use anyhow::{bail, Context, Result};
use ratatosk_server::{
    admin, AccountRow, Registry, Server, Settings, Snapshot, DEFAULT_ADMIN_SOCKET,
};
use std::path::PathBuf;

mod tui;

const USAGE: &str = "\
usage: ratatosk-server -c server.toml [-v]
       ratatosk-server --stats [--admin <socket>]
       ratatosk-server --tui [--admin <socket>]
       ratatosk-server accounts list [--admin <socket>]
       ratatosk-server accounts add <label> [--admin <socket>]
       ratatosk-server accounts rm <label> [--admin <socket>]
       ratatosk-server accounts disable <label> [--admin <socket>]
       ratatosk-server accounts enable <label> [--admin <socket>]
       ratatosk-server accounts reset [<label>] [--admin <socket>]";

#[tokio::main]
async fn main() -> Result<()> {
    // `--stats | head` should end quietly: Rust ignores SIGPIPE, so the write
    // to the closed pipe would panic on `println!` instead.
    #[cfg(unix)]
    // SAFETY: installing the default disposition for SIGPIPE has no
    // preconditions and runs before any threads or handlers exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("accounts")) {
        return accounts_command(&args[1..]).await;
    }

    let mut config: Option<PathBuf> = None;
    let mut admin: Option<PathBuf> = None;
    let mut stats = false;
    let mut tui = false;
    let mut verbose = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => config = args.next().map(PathBuf::from),
            "--stats" => stats = true,
            "--tui" => tui = true,
            "--admin" => admin = args.next().map(PathBuf::from),
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }

    if stats || tui {
        let path = admin.unwrap_or_else(|| PathBuf::from(DEFAULT_ADMIN_SOCKET));
        if tui {
            return tui::run(&path).await;
        }
        let snap = ratatosk_server::fetch_stats(&path).await?;
        print_stats(&snap);
        return Ok(());
    }

    let Some(config) = config else {
        bail!("{USAGE}");
    };
    tracing_subscriber::fmt()
        .with_max_level(if verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .with_target(false)
        .init();

    let settings = Settings::load(&config)?;
    let server = Server::from_settings(&settings)?;
    let registry = server.registry();
    tokio::select! {
        result = server.run() => result,
        _ = reload_on_sighup(registry, config.clone()) => Ok(()),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// `accounts list|add|rm|disable|enable|reset`.
async fn accounts_command(rest: &[String]) -> Result<()> {
    let mut admin = PathBuf::from(DEFAULT_ADMIN_SOCKET);
    let mut words: Vec<&str> = Vec::new();
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--admin" => {
                admin = args.next().context("--admin needs a socket path")?.into();
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other if other.starts_with('-') => bail!("unknown argument {other:?}\n{USAGE}"),
            other => words.push(other),
        }
    }

    let (action, positional) = words.split_first().context(USAGE)?;
    match (*action, positional) {
        ("list", []) => print_accounts(admin::list(&admin).await?),
        ("add", [label]) => {
            let added = admin::add(&admin, label).await?;
            println!("added {label}");
            println!("  uuid  {}", added.uuid);
            println!("  tag   {}", added.tag);
            if let Some(link) = added.link {
                println!("  link  {link}");
            }
            println!("(new connections only; live sessions keep running)");
        }
        ("rm", [label]) => {
            admin::remove(&admin, label).await?;
            println!("removed {label} (live sessions keep running until they end)");
        }
        ("disable", [label]) => {
            admin::set_disabled(&admin, label, true).await?;
            println!("disabled {label} (live sessions keep running until they end)");
        }
        ("enable", [label]) => {
            admin::set_disabled(&admin, label, false).await?;
            println!("enabled {label}");
        }
        ("reset", []) => {
            admin::reset(&admin, None).await?;
            println!("reset all cumulative counters");
        }
        ("reset", [label]) => {
            admin::reset(&admin, Some(label)).await?;
            println!("reset counters of {label}");
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn print_accounts(rows: Vec<AccountRow>) {
    if rows.is_empty() {
        println!("no accounts");
        return;
    }
    println!(
        "{:<13}  {:<8}  {:<6}  {:<5}  {:>4}  {:>6}  link / uuid",
        "label", "tag", "source", "state", "open", "total"
    );
    for row in rows {
        let state = if row.disabled { "off" } else { "on" };
        let detail = row.link.unwrap_or_else(|| row.uuid.to_string());
        println!(
            "{:<13}  {:<8}  {:<6}  {:<5}  {:>4}  {:>6}  {}",
            row.label, row.tag, row.source, state, row.open, row.total, detail
        );
    }
}

/// Reload the static users and the accounts file on every SIGHUP, so accounts
/// can also be changed by editing the files, with `kill -HUP` instead of a
/// restart that drops every live connection. A broken config or file is kept
/// out — the current list stays. Never returns.
async fn reload_on_sighup(registry: Registry, config: PathBuf) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hup = match signal(SignalKind::hangup()) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("SIGHUP unavailable, hot-reload off: {e}");
            return std::future::pending().await;
        }
    };
    while hup.recv().await.is_some() {
        match Settings::load(&config).and_then(|s| registry.reload(s.users)) {
            Ok(n) => tracing::info!("reloaded {n} accounts on SIGHUP"),
            Err(e) => tracing::warn!("SIGHUP reload failed, keeping current accounts: {e:#}"),
        }
    }
    std::future::pending().await
}

fn print_stats(s: &Snapshot) {
    println!("uptime     {}", duration(s.uptime_secs));
    println!("conns      {} open · {} total", s.conns_open, s.conns_total);
    println!(
        "streams    {} open · {} total",
        s.streams_open, s.streams_total
    );
    println!("udp        {} open · {} total", s.udp_open, s.udp_total);
    println!(
        "traffic    up {} · down {}",
        bytes(s.bytes_up),
        bytes(s.bytes_down)
    );
    println!("site       {} HTTP/3 requests", s.site_requests);
    if !s.accounts.is_empty() {
        println!("\naccount   open  total  idle");
        for a in &s.accounts {
            println!(
                "{:<8}  {:>4}  {:>5}  {}",
                a.tag,
                a.open,
                a.total,
                duration(a.idle_secs)
            );
        }
    }
}

fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
