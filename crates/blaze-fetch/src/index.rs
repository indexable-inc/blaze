//! A registry's sparse index, read by blaze itself (D123): what resolving a project that has no
//! lock file asks of a registry, on the crate's rules (sans-IO, typed errors, the allocator
//! handed in, no third-party crate).
//!
//! ```text
//! asks (name, kind) --Index--> Read (the cache record) --> one batch of Http (conditional)
//!                            --> Write (a new record) --> each ask's files, read in place
//! ```
//!
//! - [`line`]: one index file (cargo's format: a JSON object a line, one per published
//!   version) parsed into [`record`]'s form, with typed errors naming the line and the field.
//! - [`record`]: the form, which is also the cache: one file's versions sorted, its strings in
//!   one buffer, its validators (`ETag` or `Last-Modified`) in its header; written once per
//!   change of the file, read in place by [`record::IndexView`].
//! - [`request`]: the pure rules of the protocol: a name's index path, the conditional request,
//!   what an answer's status means, `config.json` and its `dl` template, and where the cache
//!   record of a file lives under the directory grant.
//! - [`machine`]: the [`machine::Index`] state machine: a batch of asks in, the cache read, one
//!   batch of conditional requests, each tried once (D172), and each ask's files out.
//!
//! Why a machine of its own and not new states of [`crate::Machine`]: that machine's unit is a
//! lock entry with a pin (a SHA-256 or a commit) checked before anything is kept, its requests
//! carry fixed headers, and its answers carry no headers. An index file has no pin (the
//! registry's answer is the truth, as cargo takes it), needs a header made per request (the
//! validator) and reads two of the answer's headers. The two share [`crate::Limits`] (one try
//! each, a deadline, a byte limit) and [`crate::NetFailure`], and a driver runs both over one HTTP client.

pub mod line;
pub mod machine;
pub mod record;
pub mod request;

pub use line::{Bad, Field, IndexLimits, IndexRefused, parse_in};
pub use machine::{FileAsk, FileEnd, Index, IndexAsk, IndexEffect, IndexPlant, IndexReply, IndexWhy, QueryKind, Refused, Update};
pub use record::{DepKind, DepRow, FeatureRow, IndexView, Status, Validator, VersionRow, ViewRefused};
pub use request::{AnswerKind, ConfigRefused, NameRefused, RegistryConfig, RegistryKey};
