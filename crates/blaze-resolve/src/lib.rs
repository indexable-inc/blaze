//! cargo's resolver, owned by blaze: the package model, `Cargo.lock`, version selection and
//! feature unification, and the resolve that `cargo metadata` and `--unit-graph` print.
//!
//! Every project cargo resolves must resolve the same here. The code follows cargo 1.99.0
//! (`5f94df4789`, the cargo of Rust 1.99.0), whose names it keeps so that the two can be read
//! side by side; each module says which of cargo's files it follows. cargo is MIT or
//! Apache-2.0.
//!
//! The package model:
//!
//! - [`SourceId`]: where packages come from (a path, a registry with a git or sparse index, a
//!   git repository at a reference and commit, a directory, a local registry), compared by
//!   cargo's canonical URL and written as lock files and package ids write it.
//! - [`PackageId`]: a name, a version and a source. [`PackageIdSpec`]: `-p` and
//!   `cargo metadata`'s ids.
//! - [`Dependency`]: what a manifest or index entry asks for.
//! - [`Summary`]: one version of a package, with its [`FeatureMap`] of [`FeatureValue`]s.
//! - [`Registry`]: what the resolver asks of the sources.
//!
//! [`Resolve`] is a resolved graph, and [`lockfile`] reads and writes it as `Cargo.lock`.

mod dependency;
mod graph;
pub mod lockfile;
pub mod metadata;
mod names;
mod package_id;
mod package_id_spec;
mod persistent;
mod poll;
mod registry;
mod resolve;
pub mod resolver;
mod summary;
mod version;
pub mod workspace_resolve;

use blaze_source::{interning, source_id};
use stdx::edit_distance::cargo as edit_distance;

pub use blaze_source::{
    CRATES_IO_DOMAIN, CRATES_IO_HTTP_INDEX, CRATES_IO_INDEX, CRATES_IO_REGISTRY, CanonicalUrl,
    GitReference, InternedString, Precise, SourceId, SourceIdAsUrl, SourceKind,
};
pub use dependency::{DepKind, Dependency};
pub use edit_distance::{closest, closest_msg, edit_distance};
pub use graph::Graph;
pub use names::{validate_feature_name, validate_package_name, validate_registry_name};
pub use package_id::PackageId;
pub use package_id_spec::PackageIdSpec;
pub use poll::block_on;
pub use registry::{IndexSummary, MemRegistry, QueryKind, Registry, query_pinned};
pub use resolve::{LockfileMetadata, Resolve, ResolveVersion};
pub use summary::{
    FeatureDep, FeatureMap, FeatureValue, MissingDependencyError, Summary, build_feature_map,
};
pub use version::{OptVersionReq, PartialVersion, RustVersion};

pub use cargo_platform::{Cfg, Platform};
