//! Where the desktop client keeps its configuration and logs.

use std::path::{Path, PathBuf};

/// The config directory under `home`: `~/.config/ratatosk`, or the legacy
/// `~/.config/vproxy` when only that one exists.
///
/// Keeping an existing vproxy directory avoids splitting a user's server list
/// or config in two during the rename; a fresh install gets the new name.
pub fn config_dir_for(home: &Path) -> PathBuf {
    let config = home.join(".config");
    let new = config.join("ratatosk");
    if new.is_dir() {
        return new;
    }
    let legacy = config.join("vproxy");
    if legacy.is_dir() {
        return legacy;
    }
    new
}

/// `$HOME` shorthand for [`config_dir_for`].
pub fn user_config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| config_dir_for(Path::new(&home)))
}
