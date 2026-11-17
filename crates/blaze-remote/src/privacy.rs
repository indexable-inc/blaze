//! Which namespace a record goes to. On a machine that is not a trusted writer (every
//! developer's machine), always [`Namespace::LOCAL`]: a fully local, private build names no
//! shared namespace ([`blaze_keyed::kind::namespace_of`]). On a trusted writer (the owner's CI,
//! later its remote workers), a record is public only when every input of its key is public:
//! - its package's origin is public (a registry archive known by its checksum, or the
//!   toolchain's own crates);
//! - every record its key names is public;
//! - its key reads nothing from outside (no environment variable, no file outside the package).
//!
//! Everything else on a trusted writer goes to the organisation's namespace when one is set,
//! and otherwise to [`Namespace::LOCAL`], which never leaves the machine. The rule is computed
//! from the key's inputs and the store's settings (`org`, `trusted_writer`) alone, as a fold
//! over the inputs. The store writes one namespace per segment, and a push refuses a segment of
//! any other namespace than the one pushed to ([`crate::trust::Why::Private`]).

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
/// opted in, so what is not public stays local) on a machine that is a trusted writer or not
/// (the store's `Settings::trusted_writer`).
pub fn namespace_for(inputs: &Inputs<'_>, org: Option<Namespace>, trusted_writer: bool) -> Namespace {
    let private = org.unwrap_or(Namespace::LOCAL);
    let by_origin = namespace_of(inputs.origin, private, trusted_writer);
    if by_origin == Namespace::PUBLIC && !inputs.outside && inputs.reads.iter().all(|it| *it == Namespace::PUBLIC) {
        Namespace::PUBLIC
    } else if trusted_writer {
        private
    } else {
        Namespace::LOCAL
    }
}
