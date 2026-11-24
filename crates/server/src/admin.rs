//! Local Unix-socket admin surface: live stats and account commands.
//!
//! The protocol is one JSON request per line and one JSON reply per line, so
//! it can be driven by hand (`socat - UNIX-CONNECT:/run/ratatosk-server.sock`).
//! A client that sends nothing within [`HELLO_TIMEOUT`] is treated as a stats
//! watcher and gets the newline-delimited [`Snapshot`] stream this socket has
//! always pushed, so `--stats`/`--tui` from an older build keep working.
//!
//! Local only, and the socket is mode `0600`: it hands out uuids, which are
//! credentials.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::accounts::{self, Registry};
use crate::stats::{AccountSnapshot, Snapshot, Stats};

/// Where the server serves, and the clients look, unless overridden.
pub const DEFAULT_SOCKET: &str = "/run/ratatosk-server.sock";

/// A client that sends no request within this long is a stats watcher. Short
/// enough to stay invisible to `--stats`, long enough for a slow local client
/// to get its request line in.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// Everything the admin socket can touch.
#[derive(Clone)]
pub struct Admin {
    pub stats: Arc<Stats>,
    pub registry: Registry,
    /// Host put into the `h3://` links `accounts` commands print; without it
    /// only the uuid is shown.
    pub public_host: Option<String>,
    /// Port put into those links, normally the listening port.
    pub port: u16,
}

impl Admin {
    fn link(&self, label: &str, uuid: &Uuid) -> Option<String> {
        let host = self.public_host.as_deref()?;
        Some(format!(
            "h3://{uuid}@{host}:{}?sni={host}#{label}",
            self.port
        ))
    }

    fn store(&self) -> Result<Arc<accounts::Store>> {
        self.registry.store().context(
            "this server has no accounts file: set `accounts = \"/path/accounts.json\"` in server.toml",
        )
    }
}

/// One line of the request protocol. `cmd` is the discriminator, e.g.
/// `{"cmd":"accounts_add","label":"phone"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Stream snapshots as they are produced (what `--tui` runs on).
    StatsWatch,
    /// One snapshot right now (what `--stats` runs on).
    StatsSnapshot,
    /// Zero the cumulative counters; `label` narrows it to one account.
    StatsReset {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    AccountsList,
    AccountsAdd {
        label: String,
    },
    AccountsRemove {
        label: String,
    },
    AccountsSetDisabled {
        label: String,
        disabled: bool,
    },
}

/// The reply to one request. `data` is whatever the command returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Reply {
    fn data(data: impl Serialize) -> Self {
        match serde_json::to_value(data) {
            Ok(data) => Self {
                ok: true,
                error: None,
                data: Some(data),
            },
            Err(e) => Self::error(format!("encode reply: {e}")),
        }
    }

    fn error(error: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            data: None,
        }
    }
}

/// One row of `accounts list`: a static user or a stored account, with its
/// live counters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRow {
    pub label: String,
    pub uuid: Uuid,
    pub tag: String,
    /// `server` (from `users` in `server.toml`) or `file`.
    pub source: String,
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
    #[serde(default)]
    pub open: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub idle_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// What `accounts add` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddedAccount {
    pub label: String,
    pub uuid: Uuid,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// Serve the admin socket on `path` until the process ends. A socket left by
/// a previous run is replaced.
pub async fn serve(path: &Path, admin: Admin) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind admin socket {}", path.display()))?;
    // It can add and list accounts, so only the owner may connect.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", path.display()))?;
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let admin = admin.clone();
        tokio::spawn(async move { session(socket, admin).await });
    }
}

async fn session(socket: UnixStream, admin: Admin) {
    let (read, write) = socket.into_split();
    let mut lines = BufReader::new(read).lines();
    match tokio::time::timeout(HELLO_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => request_loop(lines, write, admin, line).await,
        // Closed, or garbage instead of a line: nothing to answer.
        Ok(Ok(None)) | Ok(Err(_)) => {}
        // Nobody sent a request: the pre-command protocol, still used by
        // `--stats`/`--tui` from older builds.
        Err(_) => snapshots(write, admin.stats).await,
    }
}

/// Push one snapshot per second until the peer goes away.
async fn snapshots(mut write: OwnedWriteHalf, stats: Arc<Stats>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let Ok(mut line) = serde_json::to_string(&stats.snapshot()) else {
            continue;
        };
        line.push('\n');
        if write.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn request_loop(
    mut lines: Lines<BufReader<OwnedReadHalf>>,
    mut write: OwnedWriteHalf,
    admin: Admin,
    first: String,
) {
    let mut line = first;
    loop {
        match serde_json::from_str::<Request>(&line) {
            Ok(Request::StatsWatch) => {
                snapshots(write, admin.stats.clone()).await;
                return;
            }
            Ok(request) => {
                let reply = match handle(request, &admin) {
                    Ok(data) => Reply::data(data),
                    Err(e) => Reply::error(format!("{e:#}")),
                };
                let Ok(mut text) = serde_json::to_string(&reply) else {
                    return;
                };
                text.push('\n');
                if write.write_all(text.as_bytes()).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let mut text = serde_json::to_string(&Reply::error(format!("bad request: {e}")))
                    .unwrap_or_else(|_| r#"{"ok":false,"error":"bad request"}"#.to_string());
                text.push('\n');
                if write.write_all(text.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
        match lines.next_line().await {
            Ok(Some(next)) => line = next,
            _ => return,
        }
    }
}

fn handle(request: Request, admin: &Admin) -> Result<Value> {
    match request {
        // Handled by `request_loop`, which switches the connection over.
        Request::StatsWatch => unreachable!("stats.watch is handled before handle()"),
        Request::StatsSnapshot => Ok(serde_json::to_value(admin.stats.snapshot())?),
        Request::StatsReset { label } => {
            match label {
                None => admin.stats.reset_totals(),
                Some(label) => {
                    let account = find_stored(admin, &label)?;
                    admin.stats.reset_account(&accounts::tag(&account.uuid));
                }
            }
            Ok(Value::Null)
        }
        Request::AccountsList => {
            let snapshot = admin.stats.snapshot();
            let by_tag: HashMap<&str, &AccountSnapshot> = snapshot
                .accounts
                .iter()
                .map(|a| (a.tag.as_str(), a))
                .collect();
            let rows: Vec<AccountRow> = admin
                .registry
                .entries()
                .into_iter()
                .map(|entry| {
                    let usage = by_tag.get(entry.tag.as_str());
                    AccountRow {
                        // The static list has no label to name a link after;
                        // its row shows the uuid instead.
                        link: (entry.source == "file")
                            .then(|| admin.link(&entry.label, &entry.uuid))
                            .flatten(),
                        label: entry.label,
                        uuid: entry.uuid,
                        tag: entry.tag,
                        source: entry.source,
                        disabled: entry.disabled,
                        created: entry.created,
                        open: usage.map_or(0, |u| u.open),
                        total: usage.map_or(0, |u| u.total),
                        idle_secs: usage.map_or(0, |u| u.idle_secs),
                    }
                })
                .collect();
            Ok(serde_json::to_value(rows)?)
        }
        Request::AccountsAdd { label } => {
            let account = admin.store()?.add(&label)?;
            admin.registry.refresh();
            Ok(serde_json::to_value(AddedAccount {
                link: admin.link(&account.label, &account.uuid),
                label: account.label,
                uuid: account.uuid,
                tag: accounts::tag(&account.uuid),
            })?)
        }
        Request::AccountsRemove { label } => {
            admin.store()?.remove(&label)?;
            admin.registry.refresh();
            Ok(Value::Null)
        }
        Request::AccountsSetDisabled { label, disabled } => {
            admin.store()?.set_disabled(&label, disabled)?;
            admin.registry.refresh();
            Ok(Value::Null)
        }
    }
}

/// A stored account by label; static `server.toml` users have none.
fn find_stored(admin: &Admin, label: &str) -> Result<accounts::Account> {
    admin
        .store()?
        .list()
        .into_iter()
        .find(|a| a.label == label)
        .with_context(|| format!("no account labeled {label:?}"))
}

// ---------------------------------------------------------------- client ----

/// Connect and hand `request` over, returning its `data`.
async fn call<T: DeserializeOwned>(path: &Path, request: &Request) -> Result<T> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect {} (is the server running?)", path.display()))?;
    let (read, mut write) = stream.into_split();
    let mut text = serde_json::to_string(request).context("encode request")?;
    text.push('\n');
    write
        .write_all(text.as_bytes())
        .await
        .context("send admin request")?;

    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .context("admin socket closed without a reply")?;
    let reply: Reply = serde_json::from_str(&line).context("parse admin reply")?;
    if !reply.ok {
        bail!(
            "{}",
            reply.error.unwrap_or_else(|| "admin request failed".into())
        );
    }
    serde_json::from_value(reply.data.unwrap_or(Value::Null)).context("parse admin data")
}

/// One snapshot right now (`--stats`).
pub async fn fetch(path: &Path) -> Result<Snapshot> {
    call(path, &Request::StatsSnapshot).await
}

/// The snapshot stream (`--tui`). The returned lines are the snapshots, one
/// JSON object per line.
pub async fn watch(path: &Path) -> Result<Lines<BufReader<OwnedReadHalf>>> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect {} (is the server running?)", path.display()))?;
    let (read, mut write) = stream.into_split();
    let mut text = serde_json::to_string(&Request::StatsWatch).context("encode request")?;
    text.push('\n');
    write
        .write_all(text.as_bytes())
        .await
        .context("send admin request")?;
    Ok(BufReader::new(read).lines())
}

/// Every account with its live counters.
pub async fn list(path: &Path) -> Result<Vec<AccountRow>> {
    call(path, &Request::AccountsList).await
}

/// Add an account with a fresh uuid.
pub async fn add(path: &Path, label: &str) -> Result<AddedAccount> {
    call(
        path,
        &Request::AccountsAdd {
            label: label.to_string(),
        },
    )
    .await
}

/// Remove a stored account. Live sessions keep running until they end.
pub async fn remove(path: &Path, label: &str) -> Result<()> {
    call(
        path,
        &Request::AccountsRemove {
            label: label.to_string(),
        },
    )
    .await
}

/// Take an account in or out of the allow-list without removing it.
pub async fn set_disabled(path: &Path, label: &str, disabled: bool) -> Result<()> {
    call(
        path,
        &Request::AccountsSetDisabled {
            label: label.to_string(),
            disabled,
        },
    )
    .await
}

/// Zero the cumulative counters, for every account or just one.
pub async fn reset(path: &Path, label: Option<&str>) -> Result<()> {
    call(
        path,
        &Request::StatsReset {
            label: label.map(str::to_string),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin(host: Option<&str>, port: u16) -> Admin {
        Admin {
            stats: Arc::new(Stats::new()),
            registry: Registry::new(Vec::new(), None),
            public_host: host.map(str::to_string),
            port,
        }
    }

    #[test]
    fn links_use_the_public_endpoint() {
        let uuid = Uuid::from_u128(1);
        let with_host = admin(Some("example.com"), 443);
        assert_eq!(
            with_host.link("phone", &uuid).as_deref(),
            Some(format!("h3://{uuid}@example.com:443?sni=example.com#phone").as_str())
        );
        assert_eq!(
            with_host
                .link("(server.toml)", &uuid)
                .as_deref()
                .map(|l| l.contains("server.toml")),
            Some(true)
        );
        assert_eq!(admin(None, 443).link("phone", &uuid), None);
    }

    #[test]
    fn a_reply_is_ok_or_an_error() {
        let ok = Reply::data(42);
        assert!(ok.ok);
        assert_eq!(ok.data, Some(serde_json::json!(42)));
        let err = Reply::error("nope");
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("nope"));
    }
}
