//! Thin OS host driver over the `#![no_std]` sans-IO [`blaze_shell_config`] crate:
//! resolves `$XDG_CONFIG_HOME/blaze-shell/config.toml` (or `~/.config/blaze-shell/config.toml`),
//! writes the starter config on first launch, and hot-reloads on `(mtime, len)` change.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::UNIX_EPOCH;

use blaze_own::Alloc;
pub use blaze_shell_config::{
    CompletionSpecOverride, NamedDesc, STARTER_CONFIG, SymbolRule, UserConfig,
    symbol_cluster_count,
};

/// Resolve the user config directory (`$XDG_CONFIG_HOME/blaze-shell` or `<home>/.config/blaze-shell`).
pub fn config_dir(home: &str) -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg).join("blaze-shell");
    }
    PathBuf::from(home).join(".config").join("blaze-shell")
}

/// Resolve the user config file path (`<config_dir>/config.toml`).
pub fn config_path(home: &str) -> PathBuf {
    config_dir(home).join("config.toml")
}

/// Ensure `~/.config/blaze-shell/config.toml` exists on first run (never overwrites existing config).
pub fn ensure_default_config(home: &str) -> PathBuf {
    let path = config_path(home);
    if !path.exists()
        && let Some(parent) = path.parent()
    {
        let _ = fs::create_dir_all(parent);
        let _ = fs::write(&path, STARTER_CONFIG);
    }
    path
}

/// Load `~/.config/blaze-shell/config.toml` into `Alloc::global()`, cached thread-locally by
/// `(path, mtime, len)` so edits hot-reload immediately with zero lock contention.
pub fn load_cached(home: &str) -> Rc<UserConfig<'static>> {
    struct CacheEntry {
        mtime_ns: u128,
        len: u64,
        cfg: Rc<UserConfig<'static>>,
    }
    thread_local! {
        static CACHE: RefCell<HashMap<PathBuf, CacheEntry>> = RefCell::new(HashMap::new());
    }

    let path = config_path(home);
    let (mtime_ns, len) = match fs::metadata(&path).ok() {
        Some(m) => {
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            (mt, m.len())
        }
        None => (0, 0),
    };

    if let Some(hit) = CACHE.with(|c| {
        c.borrow()
            .get(&path)
            .filter(|e| e.mtime_ns == mtime_ns && e.len == len)
            .map(|e| Rc::clone(&e.cfg))
    }) {
        return hit;
    }

    let alloc = Alloc::global();
    let parsed = fs::read_to_string(&path)
        .map(|s| UserConfig::parse_in(&s, alloc))
        .unwrap_or_else(|_| UserConfig::empty_in(alloc));
    let rc = Rc::new(parsed);
    CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() > 8 {
            map.clear();
        }
        map.insert(path, CacheEntry { mtime_ns, len, cfg: Rc::clone(&rc) });
    });
    rc
}
