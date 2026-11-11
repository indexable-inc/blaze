//! The host of blaze's extensions (docs/v2/plan/design/extensions.md).
//!
//! A workspace names its extensions in its manifest ([`manifest`]). For each, [`prepare`]
//! finds its source ([`source`]), holds it to the compile-time checks ([`check`]), keys its
//! build ([`key`]), has the daemon compile it natively ([`build::Toolchain`]) and maps the
//! code into this process ([`load`]). [`run::check`] then runs every loaded extension over a
//! crate: its facts to a fixpoint kept by code identity ([`facts`]), its lints over each
//! body, every call a job bounded by fuel, a stack depth and a capped arena, whose panic or
//! stop fails that job alone ([`job`]).
//!
//! Unsafe code is in [`load`] alone: the one place a compiled extension's address becomes a
//! function.

#![feature(allocator_api)]

pub mod build;
pub mod check;
pub mod facts;
mod front;
pub mod job;
pub mod key;
pub mod load;
pub mod manifest;
pub mod run;
pub mod source;
#[cfg(test)]
mod tests;

use std::{fmt, path::Path};

pub use front::Unidentified;

/// One extension of a workspace, ready to run.
pub struct Prepared {
    pub declared: manifest::Declared,
    pub loaded: load::Loaded,
}

/// Why a workspace's extensions are not ready. Nothing runs with some of them missing: the
/// workspace asked for each.
#[derive(Debug)]
pub enum PrepareError {
    Manifest(manifest::ManifestError),
    Find(source::FindError),
    /// The compile-time checks refused it: every refusal, in the source's order.
    Refused(Vec<check::Refusal>),
    Build(build::BuildError),
    Load(load::LoadError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrepareError::Manifest(it) => it.fmt(f),
            PrepareError::Find(it) => it.fmt(f),
            PrepareError::Refused(refusals) => {
                for (at, refusal) in refusals.iter().enumerate() {
                    if at > 0 {
                        f.write_str("\n")?;
                    }
                    refusal.fmt(f)?;
                }
                Ok(())
            }
            PrepareError::Build(it) => it.fmt(f),
            PrepareError::Load(it) => it.fmt(f),
        }
    }
}

impl std::error::Error for PrepareError {}

/// The extensions the workspace at `workspace` names in its root manifest (`manifest`, its
/// text; `shown`, how an error names it), each checked, built by `toolchain` and loaded, in
/// the order of their names. An extension is never built before its checks pass, and never
/// loaded from a build under another key.
pub fn prepare(
    workspace: &Path,
    manifest: &str,
    shown: &str,
    identity: &key::Identity,
    // dyn: called once per extension per build of it
    toolchain: &dyn build::Toolchain,
) -> Result<Vec<Prepared>, PrepareError> {
    let declared = manifest::declared(manifest, shown).map_err(PrepareError::Manifest)?;
    let mut prepared = Vec::with_capacity(declared.len()); // alloc: the workspace's extensions, a few
    for declared in declared {
        let sources = source::find(workspace, &declared).map_err(PrepareError::Find)?;
        let mut refusals = check::check(&sources);
        refusals.extend(check::grants(&declared));
        if !refusals.is_empty() {
            return Err(PrepareError::Refused(refusals));
        }
        let key = key::build_key(identity, &sources, &declared.grants);
        let request = build::Request { key, sources: &sources, entry: build::ENTRY };
        let image = toolchain.build(&request).map_err(PrepareError::Build)?;
        if image.key != key {
            return Err(PrepareError::Build(build::BuildError {
                extension: declared.name.clone(), // clone: error path
                why: format!(
                    "the build gave an image under the key {}, not the one asked, {key}",
                    image.key
                ),
            }));
        }
        let loaded = load::load(image, &declared.name).map_err(PrepareError::Load)?;
        prepared.push(Prepared { declared, loaded });
    }
    Ok(prepared)
}
