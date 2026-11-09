//! The app's small preferences, in `$HARNESS_HOME/native.json` (default ~/.harness): one JSON object other parts of the
//! app (and other features) share, so a write reads the file, changes one key and writes it back whole (through a
//! temporary file and a rename, so a reader never sees half of it).
//!
//! Keys: `selected` (the session the main pane showed last), `sidebar_width`, `web_width` (the browser pane's, as last
//! dragged), `closed` (objects cmd+W'd, by key: `s14`,
//! `r12`; the older `archived` and `trashed` session ids are read once in its place), `pinned` (objects kept in the
//! tree), `new_kind` (the kind last started from the File > New… picker, highlighted when it opens next) and `chat_kind`
//! (the agent kind a chat was last started with, what cmd+N starts), and `drafts` (each chat's unsent words and pasted
//! pictures by session, `{"s14": {"text", "images"}}`, src/app/chat.rs).
use serde_json::{Map, Value};
use std::path::PathBuf;

fn path() -> PathBuf {
    crate::link::home().join("native.json")
}

fn read() -> Map<String, Value> {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// The value at `key`, if the file has it.
pub fn get(key: &str) -> Option<Value> {
    read().remove(key)
}

/// Set `key` to `value`, keeping every other key.
pub fn set(key: &str, value: Value) {
    let mut m = read();
    m.insert(key.to_owned(), value);
    write(m);
}

/// Write the whole object (through a temporary file and a rename).
fn write(m: Map<String, Value>) {
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let body = match serde_json::to_vec_pretty(&Value::Object(m)) {
        Ok(b) => b,
        Err(e) => return eprintln!("harness-native: {}: {e}", p.display()),
    };
    if let Err(e) = std::fs::create_dir_all(p.parent().unwrap_or(&p))
        .and_then(|()| std::fs::write(&tmp, body))
        .and_then(|()| std::fs::rename(&tmp, &p))
    {
        eprintln!("harness-native: write {}: {e}", p.display());
    }
}

/// The ids listed at `key` (the Archive's `archived`), or None when the file has no such key.
pub fn ids(key: &str) -> Option<std::collections::BTreeSet<u64>> {
    get(key).and_then(|v| v.as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()))
}

/// The strings listed at `key` (`closed`, `pinned`), or None when the file has no such key.
pub fn keys(key: &str) -> Option<std::collections::BTreeSet<String>> {
    get(key).and_then(|v| {
        v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
    })
}

/// Drop `keys` from the file (when it has any of them), keeping every other key.
pub fn remove(keys: &[&str]) {
    let m = read();
    if keys.iter().any(|k| m.contains_key(*k)) {
        let mut m = m;
        for k in keys {
            m.remove(*k);
        }
        write(m);
    }
}
