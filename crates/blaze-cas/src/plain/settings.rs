//! How a root's plain store is kept: `<root>/store.json`, one typed value. No file is every
//! default. The packs' store is gone, so an old file naming `"form"` is refused naming it.

use std::alloc::Allocator;
use std::{alloc::Global, path::Path};

use blaze_data::{Cursor, FromValue, ShapeError};
use blaze_keyed::kind::Namespace;

/// `<root>/store.json`: every field optional (the default), unknown fields refused.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Settings {
    /// The plain store's byte cap, in MiB: compaction prunes past it. None: no cap.
    pub cap_mib: Option<u64>,
    /// Sync every commit (a caller that needs a commit to survive a power cut; none today).
    pub durable: bool,
    /// The organisation whose namespace ([`Namespace::named`]) this machine reads, after its
    /// own and before the public one. None: no organisation's.
    pub org: Option<String>,
    /// This machine is a trusted writer (CI with the namespace's credentials): its puts may name
    /// the public namespace and the organisation's. Off (every developer's machine): everything
    /// it writes is [`Namespace::LOCAL`] and never leaves it.
    pub trusted_writer: bool,
}

impl<A: core::alloc::Allocator + Clone> FromValue<A> for Settings {
    fn from_value(c: &Cursor<'_, '_, A>) -> Result<Settings, ShapeError<A>> {
        c.deny_unknown(&["cap_mib", "durable", "org", "trusted_writer"])?;
        let org = match c.field_opt("org")? {
            Some(org) if !org.is_null() => Some(org.text()?.to_owned()),
            _ => None,
        };
        Ok(Settings {
            cap_mib: c.opt("cap_mib")?,
            durable: c.opt("durable")?.unwrap_or(false),
            org,
            trusted_writer: c.opt("trusted_writer")?.unwrap_or(false),
        })
    }
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
            Ok(bytes) => blaze_json::read_in(&bytes, blaze_json::Limits::DEFAULT, Global)
                .map_err(|error| error.to_string())
                .and_then(|value| Cursor::root(&value, Global).get::<Settings>().map_err(|error| error.to_string()))
                .map_err(|error| {
                    format!(
                        "{}: {error}: it is `{{\"cap_mib\": N, \"durable\": bool, \"org\": \"name\", \"trusted_writer\": bool}}`",
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
    /// In `alloc`, the caller's.
    pub fn reads<A: Allocator>(&self, alloc: A) -> Vec<Namespace, A> {
        let mut out = Vec::with_capacity_in(3, alloc);
        out.push(Namespace::LOCAL);
        out.extend(self.org_namespace());
        out.push(Namespace::PUBLIC);
        out
    }

    /// The namespaces a put may name: [`Namespace::LOCAL`] alone unless this machine is a
    /// trusted writer, which adds the organisation's and the public one (both absent: the
    /// default, everything local).
    /// In `alloc`, the caller's.
    pub fn writes<A: Allocator>(&self, alloc: A) -> Vec<Namespace, A> {
        let mut out = Vec::with_capacity_in(3, alloc);
        out.push(Namespace::LOCAL);
        if self.trusted_writer {
            out.extend(self.org_namespace());
            out.push(Namespace::PUBLIC);
        }
        out
    }
}
