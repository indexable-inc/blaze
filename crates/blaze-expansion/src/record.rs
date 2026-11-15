//! What this crate's records are about. Kinds, keys, record heads, the outside form, proofs and
//! the store are the core's (`blaze_keyed`).

use std::{fmt, sync::Arc};

/// A content digest: BLAKE3 under a domain that names what was hashed.
pub use blaze_ids::ContentId as Digest;

/// The digest of an answer that found nothing (a file, a variable or a listing that is not
/// there): absence is an answer like any other, so a file that appears is a change.
pub const NOTHING: Digest = Digest([0; 32]);

/// What a record is about, for every error and report (AGENTS.md rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum What {
    ProcMacro,
    DeclarativeMacro,
    BuildScript,
    /// A proc-macro crate's compiled module ([`crate::modules`]).
    ExpanderModule,
}

/// The item a record is about: its kind and its name as a person reads it (`serde_derive::
/// Serialize`, `vec!`, `ring v0.17.8`). The name is shared: errors carry it by reference count.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Subject {
    pub what: What,
    pub name: Arc<str>,
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.what {
            What::ProcMacro => write!(f, "the proc macro `{}`", self.name),
            What::DeclarativeMacro => write!(f, "the macro `{}!`", self.name),
            What::BuildScript => write!(f, "the build script of `{}`", self.name),
            What::ExpanderModule => write!(f, "the wasm module of the proc-macro crate `{}`", self.name),
        }
    }
}
