//! The read path of the shared cache (docs/v2/plan/design/remote-read.md): a command that
//! misses records locally reads them from an origin (a folder today, a static object store by
//! HTTP next) by byte range, checks every byte against a chain of digests that starts at a
//! signed head, and computes nothing the origin holds.
//!
//! - [`form`]: the objects (head, manifest, summary, index pages) and their checked decoders.
//! - [`machine`]: the sans-IO [`Reader`] that decides what to ask and checks what comes back.
//! - [`origin`]: what an origin is ([`Origin`]) and the loop that drives a reader against one.
//! - [`publish`]: a namespace's segments sealed into those objects.

pub mod form;
pub mod machine;
pub mod origin;
#[cfg(feature = "sign")]
pub mod publish;

#[cfg(all(test, feature = "sign"))]
mod tests;

pub use form::{Ed25519Keys, Head, HeadCheck, Object, Refused, SignatureWhy};
pub use machine::{Answer, Ask, BatchError, Limits, OriginError, Poll, Reader, Report, Unfetched};
pub use origin::{Origin, fetch};
#[cfg(feature = "sign")]
pub use publish::{PublishError, Sealed, publish, publish_bound};

/// The store's namespace, which every head names: re-exported for a caller holding no store.
pub use blaze_cas::plain::Namespace;
