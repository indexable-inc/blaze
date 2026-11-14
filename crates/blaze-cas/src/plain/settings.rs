//! Which store a root opens: `<root>/store.json`, one typed value. No file is the
//! content-addressed store of packs, as before; `{"form": "plain"}` is the plain store.

use std::path::Path;

use serde::Deserialize;

/// The store's form.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    /// The content-addressed store of packs ([`crate::Store`]).
    #[default]
    Packs,
    /// The plain store ([`super::Plain`]).
    Plain,
}

/// `<root>/store.json`.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub form: Form,
    /// The plain store's byte cap, in MiB: compaction prunes past it. None: no cap.
    #[serde(default)]
    pub cap_mib: Option<u64>,
    /// Sync every commit (a caller that needs a commit to survive a power cut; none today).
    #[serde(default)]
    pub durable: bool,
}

/// The file's name under a root.
pub const FILE: &str = "store.json";

impl Settings {
    /// The settings of `root`: the default when there is no file; a file that does not parse
    /// is an error naming it.
    pub fn read(root: &Path) -> Result<Settings, String> {
        let path = root.join(FILE);
        match std::fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            // alloc: an error
            Err(error) => Err(format!("{}: {error}", path.display())),
            Ok(bytes) => serde_json::from_slice(&bytes)
                // alloc: an error
                .map_err(|error| format!("{}: {error}: it is `{{\"form\": \"packs\" | \"plain\", \"cap_mib\": N, \"durable\": bool}}`", path.display())),
        }
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_mib.map_or(u64::MAX, |mib| mib.saturating_mul(1 << 20))
    }
}
