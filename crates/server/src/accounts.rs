//! Accounts: labels mapped to uuids, persisted as a JSON file and swapped into
//! the live allow-list without restarting the server.
//!
//! `users` in `server.toml` stay the static list (untouched at runtime); the
//! file holds the accounts the admin socket can add, disable and remove. The
//! live allow-list is the union of the static uuids and the enabled stored
//! accounts, replaced as a whole on every change: the next handshake sees it,
//! connections already up are left alone.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::Accounts;

/// One stored account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Human label, unique in the file; the handle `accounts` commands use.
    pub label: String,
    /// What the client presents, as the user part of an `h3://` link.
    pub uuid: Uuid,
    /// Unix seconds; `0` for a file written before this field existed.
    #[serde(default)]
    pub created: u64,
    /// Disabled accounts stay in the file but out of the allow-list.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct File {
    #[serde(default)]
    accounts: Vec<Account>,
}

/// The accounts file. Every mutation rewrites the whole file through a
/// temporary and a rename, so a crash leaves the old or the new version,
/// never a torn one — and mode `0600`, because it holds credentials.
pub struct Store {
    path: PathBuf,
    state: Mutex<File>,
}

impl Store {
    /// Open `path`, creating an empty file when it does not exist. A file that
    /// exists but does not parse is an error: refusing to start beats
    /// overwriting the operator's accounts with an empty list.
    pub fn open(path: PathBuf) -> Result<Self> {
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Create it now: a wrong path is then caught at start-up rather
                // than at the first `accounts add`.
                let store = Self {
                    path,
                    state: Mutex::new(File::default()),
                };
                store.persist(&File::default())?;
                return Ok(store);
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Vec<Account> {
        self.state
            .lock()
            .expect("accounts lock poisoned")
            .accounts
            .clone()
    }

    /// Uuids of the accounts that may connect.
    pub fn enabled(&self) -> Vec<Uuid> {
        self.state
            .lock()
            .expect("accounts lock poisoned")
            .accounts
            .iter()
            .filter(|a| !a.disabled)
            .map(|a| a.uuid)
            .collect()
    }

    /// Add an account with a fresh uuid and persist it.
    pub fn add(&self, label: &str) -> Result<Account> {
        let label = label.trim();
        if label.is_empty() {
            bail!("label must not be empty");
        }
        let mut state = self.state.lock().expect("accounts lock poisoned");
        if state.accounts.iter().any(|a| a.label == label) {
            bail!("account {label:?} already exists");
        }
        let account = Account {
            label: label.to_string(),
            uuid: Uuid::new_v4(),
            created: now(),
            disabled: false,
        };
        let mut next = File {
            accounts: state.accounts.clone(),
        };
        next.accounts.push(account.clone());
        self.persist(&next)?;
        *state = next;
        Ok(account)
    }

    /// Remove a stored account. Static `server.toml` users are not here.
    pub fn remove(&self, label: &str) -> Result<Account> {
        let mut state = self.state.lock().expect("accounts lock poisoned");
        let index = state
            .accounts
            .iter()
            .position(|a| a.label == label)
            .with_context(|| format!("no account labeled {label:?}"))?;
        let mut next = File {
            accounts: state.accounts.clone(),
        };
        let removed = next.accounts.remove(index);
        self.persist(&next)?;
        *state = next;
        Ok(removed)
    }

    /// Turn an account on or off without removing its uuid.
    pub fn set_disabled(&self, label: &str, disabled: bool) -> Result<Account> {
        let mut state = self.state.lock().expect("accounts lock poisoned");
        let mut next = File {
            accounts: state.accounts.clone(),
        };
        let account = next
            .accounts
            .iter_mut()
            .find(|a| a.label == label)
            .with_context(|| format!("no account labeled {label:?}"))?;
        account.disabled = disabled;
        let account = account.clone();
        self.persist(&next)?;
        *state = next;
        Ok(account)
    }

    /// Re-read the file (an operator may have edited it by hand).
    pub fn reload(&self) -> Result<()> {
        let text = std::fs::read_to_string(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        let file: File = serde_json::from_str(&text)
            .with_context(|| format!("parse {}", self.path.display()))?;
        *self.state.lock().expect("accounts lock poisoned") = file;
        Ok(())
    }

    fn persist(&self, file: &File) -> Result<()> {
        let mut text = serde_json::to_string_pretty(file).context("encode the accounts file")?;
        text.push('\n');
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        write_private(&tmp, text.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))?;
        // Make the rename itself durable, not just the data.
        if let Some(dir) = self.path.parent() {
            if let Ok(dir) = std::fs::File::open(dir) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
}

/// Write `bytes` to a new file only the owner can read, fsynced.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Short, non-secret tag of an account for the log and the stats.
pub fn tag(uuid: &Uuid) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(uuid.as_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One row of `accounts list`: a static user or a stored account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub label: String,
    pub uuid: Uuid,
    /// The masked tag used in logs and stats.
    pub tag: String,
    /// `server` (from `users` in `server.toml`) or `file`.
    pub source: String,
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
}

/// The static users, the mutable store and the live allow-list they feed.
/// Cheap to clone: the live handle inside is shared with every connection.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

struct Inner {
    /// `users` from `server.toml`; replaced on SIGHUP.
    static_users: RwLock<Vec<Uuid>>,
    store: RwLock<Option<Arc<Store>>>,
    live: Accounts,
}

impl Registry {
    pub fn new(static_users: Vec<Uuid>, store: Option<Arc<Store>>) -> Self {
        let registry = Self {
            inner: Arc::new(Inner {
                static_users: RwLock::new(static_users),
                store: RwLock::new(store),
                live: Arc::new(RwLock::new(Arc::from(Vec::<Uuid>::new()))),
            }),
        };
        registry.refresh();
        registry
    }

    /// The handle connections read. Swapping its contents reaches the next
    /// handshake; connections already accepted keep the list they started with.
    pub fn live(&self) -> Accounts {
        self.inner.live.clone()
    }

    pub fn store(&self) -> Option<Arc<Store>> {
        self.inner
            .store
            .read()
            .expect("accounts lock poisoned")
            .clone()
    }

    /// Attach (or detach) the accounts file and rebuild the allow-list.
    pub fn set_store(&self, store: Option<Arc<Store>>) -> usize {
        *self.inner.store.write().expect("accounts lock poisoned") = store;
        self.refresh()
    }

    /// SIGHUP: new static users from `server.toml`. The accounts file is
    /// re-read too, since an operator may have edited it by hand. On error
    /// nothing changes.
    pub fn reload(&self, static_users: Vec<Uuid>) -> Result<usize> {
        if let Some(store) = self.store() {
            store.reload()?;
        }
        *self
            .inner
            .static_users
            .write()
            .expect("accounts lock poisoned") = static_users;
        Ok(self.refresh())
    }

    /// Rebuild the allow-list; returns its size.
    pub fn refresh(&self) -> usize {
        let static_users = self
            .inner
            .static_users
            .read()
            .expect("accounts lock poisoned");
        let store = self.inner.store.read().expect("accounts lock poisoned");
        let mut uuids: Vec<Uuid> = static_users.clone();
        if let Some(store) = store.as_ref() {
            uuids.extend(store.enabled());
        }
        uuids.sort_unstable();
        uuids.dedup();
        let count = uuids.len();
        *self.inner.live.write().expect("accounts lock poisoned") = Arc::from(uuids);
        count
    }

    /// Every account, static users first.
    pub fn entries(&self) -> Vec<Entry> {
        let static_users = self
            .inner
            .static_users
            .read()
            .expect("accounts lock poisoned")
            .clone();
        let stored = self.store().map(|s| s.list()).unwrap_or_default();
        let mut entries: Vec<Entry> = static_users
            .into_iter()
            .map(|uuid| Entry {
                label: "(server.toml)".to_string(),
                uuid,
                tag: tag(&uuid),
                source: "server".to_string(),
                disabled: false,
                created: None,
            })
            .collect();
        entries.extend(stored.into_iter().map(|a| Entry {
            label: a.label,
            uuid: a.uuid,
            tag: tag(&a.uuid),
            source: "file".to_string(),
            disabled: a.disabled,
            created: Some(a.created),
        }));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh directory per test; the caller cleans it up.
    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ratatosk-accounts-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_persists_and_reloads() {
        let dir = temp();
        let path = dir.join("accounts.json");
        let uuid;
        {
            let store = Store::open(path.clone()).unwrap();
            let account = store.add("phone").unwrap();
            assert_eq!(account.label, "phone");
            assert!(!account.disabled);
            assert!(account.created > 0);
            assert_eq!(store.enabled(), vec![account.uuid]);
            uuid = account.uuid;
        }
        let store = Store::open(path.clone()).unwrap();
        let accounts = store.list();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "phone");
        assert_eq!(accounts[0].uuid, uuid);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_file_is_private() {
        let dir = temp();
        let path = dir.join("accounts.json");
        let store = Store::open(path.clone()).unwrap();
        store.add("phone").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an accounts file holds credentials");
        assert!(!path.with_file_name("accounts.json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_labels_are_rejected() {
        let dir = temp();
        let store = Store::open(dir.join("accounts.json")).unwrap();
        store.add("phone").unwrap();
        let error = store.add(" phone ").unwrap_err().to_string();
        assert!(error.contains("already exists"), "{error}");
        assert!(store.add("  ").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn remove_and_disable() {
        let dir = temp();
        let store = Store::open(dir.join("accounts.json")).unwrap();
        let phone = store.add("phone").unwrap();
        let laptop = store.add("laptop").unwrap();

        store.set_disabled("phone", true).unwrap();
        assert_eq!(store.enabled(), vec![laptop.uuid]);
        assert_eq!(store.list().len(), 2, "disabled accounts stay in the file");

        store.set_disabled("phone", false).unwrap();
        assert_eq!(store.enabled().len(), 2);

        assert_eq!(store.remove("phone").unwrap().uuid, phone.uuid);
        assert!(store.remove("phone").is_err());
        assert_eq!(store.list().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_broken_file_is_refused_not_reset() {
        let dir = temp();
        let path = dir.join("accounts.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Store::open(path.clone()).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not json",
            "the operator's file must not be overwritten"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_is_created() {
        let dir = temp();
        let path = dir.join("accounts.json");
        assert!(!path.exists());
        let store = Store::open(path.clone()).unwrap();
        assert!(path.exists());
        assert!(store.list().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn registry_unions_static_and_enabled_accounts() {
        let dir = temp();
        let store = Arc::new(Store::open(dir.join("accounts.json")).unwrap());
        let static_user = Uuid::from_u128(0xa11ce);
        store.add("phone").unwrap();
        store.add("old").unwrap();
        store.set_disabled("old", true).unwrap();

        let registry = Registry::new(vec![static_user], Some(store));
        let live = registry.live().read().unwrap().clone();
        assert_eq!(live.len(), 2, "one static plus one enabled file account");
        assert!(live.contains(&static_user));

        let entries = registry.entries();
        assert_eq!(entries.len(), 3, "the disabled account is still listed");
        assert_eq!(entries[0].source, "server");
        assert!(entries.iter().any(|e| e.label == "old" && e.disabled));

        // Dropping the store leaves the static user only.
        assert_eq!(registry.set_store(None), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reload_picks_up_manual_edits_and_keeps_state_on_errors() {
        let dir = temp();
        let path = dir.join("accounts.json");
        let store = Arc::new(Store::open(path.clone()).unwrap());
        let registry = Registry::new(Vec::new(), Some(store));
        let added = registry
            .store()
            .unwrap()
            .add("phone")
            .map(|a| a.uuid)
            .unwrap();
        assert_eq!(registry.refresh(), 1);

        std::fs::write(&path, "broken").unwrap();
        assert!(registry.reload(Vec::new()).is_err());
        let live = registry.live().read().unwrap().clone();
        assert_eq!(&*live, &[added], "a broken file keeps the current accounts");

        std::fs::write(&path, r#"{"accounts":[]}"#).unwrap();
        assert_eq!(registry.reload(vec![Uuid::from_u128(7)]).unwrap(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_tag_is_short_and_stable() {
        let uuid = Uuid::from_u128(0xa11ce);
        assert_eq!(tag(&uuid).len(), 8);
        assert_eq!(tag(&uuid), tag(&Uuid::from_u128(0xa11ce)));
        assert_ne!(tag(&uuid), tag(&Uuid::from_u128(0xb0b)));
    }
}
