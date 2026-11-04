//! What the app keeps across launches: `~/.harness/native.json` holds `{"selected": <session id>}`, the object the main
//! pane showed last. Other keys in the file are kept as they are.
use crate::link::home;
use serde_json::{Value, json};
use std::path::PathBuf;

fn path() -> PathBuf {
    home().join("native.json")
}

fn read() -> Value {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

/// The session selected when the app last ran.
pub fn selected() -> Option<u64> {
    read()["selected"].as_u64()
}

/// Remember `sid` as the selected session (through a temporary file, so a crash leaves the old file or the new one).
pub fn set_selected(sid: Option<u64>) -> Result<(), String> {
    let mut v = read();
    v["selected"] = json!(sid);
    let p = path();
    let tmp = p.with_extension("json.tmp");
    std::fs::create_dir_all(home()).and_then(|()| std::fs::write(&tmp, format!("{v:#}\n"))).and_then(|()| std::fs::rename(&tmp, &p)).map_err(|e| format!("{}: {e}", p.display()))
}
