//! The app's small preferences, in `$HARNESS_HOME/native.json` (default ~/.harness): one JSON object other parts of the
//! app (and other features) share, so a write reads the file, changes one key and writes it back whole (through a
//! temporary file and a rename, so a reader never sees half of it).
//!
//! Keys: `selected` (the session the main pane showed last), `sidebar_width`, `trashed` (session ids cmd+W'd into the
//! sidebar's Trash) and `hidden` (ids the Trash was emptied of, never shown again).
use serde_json::{Map, Value};
use std::path::PathBuf;

fn path() -> PathBuf {
    crate::link::home().join("native.json")
}

fn read() -> Map<String, Value> {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// The value at `key`, if the file has it.
pub fn get(key: &str) -> Option<Value> {
    read().remove(key)
}

/// Set `key` to `value`, keeping every other key.
pub fn set(key: &str, value: Value) {
    let mut m = read();
    m.insert(key.to_owned(), value);
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let body = match serde_json::to_vec_pretty(&Value::Object(m)) {
        Ok(b) => b,
        Err(e) => return eprintln!("harness-native: {}: {e}", p.display()),
    };
    if let Err(e) = std::fs::create_dir_all(p.parent().unwrap_or(&p)).and_then(|()| std::fs::write(&tmp, body)).and_then(|()| std::fs::rename(&tmp, &p)) {
        eprintln!("harness-native: write {}: {e}", p.display());
    }
}

/// The ids listed at `key` (the Trash's `trashed`, the emptied `hidden`).
pub fn ids(key: &str) -> std::collections::BTreeSet<u64> {
    get(key).and_then(|v| v.as_array().map(|a| a.iter().filter_map(Value::as_u64).collect())).unwrap_or_default()
}
