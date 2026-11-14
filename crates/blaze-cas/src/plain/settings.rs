//! Which store a root opens: `<root>/store.json`, one typed value. No file is the
//! content-addressed store of packs, as before; `{"form": "plain"}` is the plain store.

use std::{alloc::Global, path::Path};

use blaze_keyed::kind::Namespace;
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
#[derive(Deserialize, Clone, PartialEq, Eq, Debug, Default)]
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
    /// The organisation whose namespace ([`Namespace::named`]) this machine reads, after its
    /// own and before the public one. None: no organisation's.
    #[serde(default)]
    pub org: Option<String>,
    /// This machine is a trusted writer (CI with the namespace's credentials): its puts may name
    /// the public namespace and the organisation's. Off (every developer's machine): everything
    /// it writes is [`Namespace::LOCAL`] and never leaves it.
    #[serde(default)]
    pub trusted_writer: bool,
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
            Err(error) => Err(format!("{}: {error}", path.display())),
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "{}: {error}: it is `{{\"form\": \"packs\" | \"plain\", \"cap_mib\": N, \"durable\": bool, \"org\": \"name\", \"trusted_writer\": bool}}`",
                    path.display()
                )
            }),
        }
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_mib.map_or(u64::MAX, |mib| mib.saturating_mul(1 << 20))
    }

    /// The organisation's namespace, when one is set.
    pub fn org_namespace(&self) -> Option<Namespace> {
        self.org.as_deref().map(Namespace::named)
    }

    /// The namespaces a lookup asks, in order: this machine's own, the organisation's, the
    /// public one. No other is ever searched.
    pub fn reads(&self) -> Vec<Namespace> {
        let mut out = Vec::with_capacity_in(3, Global);
        out.push(Namespace::LOCAL);
        out.extend(self.org_namespace());
        out.push(Namespace::PUBLIC);
        out
    }

    /// The namespaces a put may name: [`Namespace::LOCAL`] alone unless this machine is a
    /// trusted writer, which adds the organisation's and the public one (both absent: the
    /// default, everything local).
    pub fn writes(&self) -> Vec<Namespace> {
        let mut out = Vec::with_capacity_in(3, Global);
        out.push(Namespace::LOCAL);
        if self.trusted_writer {
            out.extend(self.org_namespace());
            out.push(Namespace::PUBLIC);
        }
        out
    }
}
