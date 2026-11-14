//! Which namespace a record goes to. A record is public only when every input of its key is
//! public:
//! - its package's origin is public (a registry archive known by its checksum, or the
//!   toolchain's own crates: [`blaze_keyed::kind::namespace_of`]);
//! - every record its key names is public;
//! - its key reads nothing from outside (no environment variable, no file outside the package).
//!
//! The rule is computed from the key's inputs alone, as a fold over them, so it needs nothing
//! of the machine. Everything else goes to the organisation's namespace when the organisation
//! has opted in, and otherwise to [`Namespace::LOCAL`], which never leaves the machine. The
//! store writes one namespace per segment, and a push refuses a segment of any other namespace
//! than the one pushed to ([`crate::trust::Why::Private`]).
//!
//! The core's `Namespace` (blaze-keyed, a 32-byte id) and the store's (blaze-cas, 8 bytes in
//! every segment header) differ. Only "public or not" crosses between them here; the report
//! lists the difference for reconciling.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_cas::plain::Namespace;
use blaze_keyed::kind::{Origin, namespace_of};

/// The inputs of a record's key that decide its namespace.
#[derive(Clone, Copy, Debug)]
pub struct Inputs<'a> {
    pub origin: Origin,
    /// The namespaces of the records its key names.
    pub reads: &'a [Namespace],
    /// Whether its key names a read from outside the package (an environment variable, an
    /// outside file).
    pub outside: bool,
}

/// The namespace of a record with `inputs`, for an organisation `org` (None: no organisation
/// opted in, so what is not public stays local).
pub fn namespace_for(inputs: &Inputs<'_>, org: Option<Namespace>) -> Namespace {
    let origin_public = namespace_of(inputs.origin, blaze_keyed::kind::Namespace(blaze_ids::ContentId([0xff; 32]))) == blaze_keyed::kind::Namespace::PUBLIC;
    if origin_public && !inputs.outside && inputs.reads.iter().all(|it| *it == Namespace::PUBLIC) {
        Namespace::PUBLIC
    } else {
        org.unwrap_or(Namespace::LOCAL)
    }
}
