//! HTML documentation written from rust-analyzer's HIR, the way `cargo doc` and rustdoc write
//! it, with no rustdoc.
//!
//! Three passes over a loaded database, each parallel over the crates: [`places`] walks every
//! module tree as rustdoc does and gives each item its page; [`collect`] builds the [`model`] of
//! each crate (declarations, docs with intra-doc links resolved, members, impls); [`html`]
//! writes the pages, source pages, sidebars and search index from the model alone.

#![feature(allocator_api)]

mod attrs;
mod cfg;
mod collect;
pub mod doctest;
mod highlight;
pub mod html;
mod links;
mod lint;
mod markdown;
pub mod model;
mod places;
mod sig;
mod source;
#[cfg(test)]
mod tests;

use base_db::Db;
use blaze_hash::HashSet;
use hir::Crate;
use std::alloc::Global;

pub use crate::places::Documented;

/// Each crate's dependencies only its tests use (a workspace member's dev-dependencies), which
/// rust-analyzer's crate graph gives the crate itself and `cargo doc` neither documents nor
/// links: `(crate, dependency)`.
pub type TestOnly = HashSet<(Crate, Crate), Global>;

/// A crate the user asked to document.
#[derive(Debug, Clone, Copy)]
pub struct Asked {
    pub krate: Crate,
    /// Document items that are not public too: `--document-private-items`, or a binary.
    pub private: bool,
}

/// The crates `cargo doc` documents for `asked`: those, and unless `no_deps` every library
/// they depend on, directly or not, but none of the sysroot's and none only tests use. Two
/// crates of one name keep the one asked for, else as `one_per_name` says, and a crate only
/// one left out depends on is left out too. A crate with no name is refused: its pages are
/// filed under its name.
pub fn crates_to_document(
    db: &Db,
    asked: &[Asked],
    no_deps: bool,
    test_only: &TestOnly,
) -> Result<Vec<Documented>, base_db::UnnamedCrate> {
    let mut out: Vec<Documented> = vec![];
    let mut names = HashSet::new_in(Global);
    let mut seen = HashSet::new_in(Global);
    for it in asked {
        let name = crate_name(db, it.krate)?;
        if names.insert(name.clone()) {
            seen.insert(it.krate);
            out.push(Documented { krate: it.krate, name, private: it.private, warn: true });
        }
    }
    if no_deps {
        return Ok(out);
    }
    // Each crate reached, and whether a path to it leads through no proc macro: cargo builds
    // a proc macro and everything it depends on for the host, the rest for the target.
    let mut reached: Vec<(Crate, bool)> = vec![];
    let mut walked = HashSet::new_in(Global);
    let mut stack: Vec<(Crate, bool)> = asked.iter().map(|it| (it.krate, false)).collect();
    while let Some((krate, host)) = stack.pop() {
        for dep in krate.dependencies(db) {
            let dep = dep.krate;
            let host = host || is_proc_macro(db, dep);
            if seen.contains(&dep)
                || is_sysroot(db, dep)
                || test_only.contains(&(krate, dep))
                || !walked.insert((dep, host))
            {
                continue;
            }
            match reached.iter_mut().find(|(it, _)| *it == dep) {
                Some(it) => it.1 |= !host,
                None => reached.push((dep, !host)),
            }
            stack.push((dep, host));
        }
    }
    let reached = reached
        .into_iter()
        .map(|(krate, target)| {
            Ok(Reached {
                name: crate_name(db, krate)?,
                krate,
                version: krate.version(db).unwrap_or_default(),
                target,
            })
        })
        .collect::<Result<Vec<_>, base_db::UnnamedCrate>>()?
        .into_iter()
        .filter(|it| !names.contains(&it.name))
        .collect();
    let kept = one_per_name(reached);
    let keep: HashSet<Crate, Global> = kept.iter().map(|it| it.krate).collect();
    let roots: Vec<Crate> = asked.iter().map(|it| it.krate).collect();
    let linked = still_reached(&roots, &keep, |krate| {
        krate
            .dependencies(db)
            .into_iter()
            .map(|dep| dep.krate)
            .filter(|dep| !test_only.contains(&(krate, *dep)))
            .collect()
    });
    out.extend(kept.into_iter().filter(|it| linked.contains(&it.krate)).map(|it| Documented {
        warn: it.krate.origin(db).is_local(),
        krate: it.krate,
        name: it.name,
        private: false,
    }));
    Ok(out)
}

fn is_proc_macro(db: &Db, krate: Crate) -> bool {
    base_db::Crate::from(krate).data(db).target_kind.is_proc_macro()
}

/// A dependency `cargo doc` reached.
#[derive(Debug)]
struct Reached<K> {
    name: String,
    krate: K,
    version: String,
    /// Reached by a path through no proc macro, so built for the target.
    target: bool,
}

/// The dependencies cargo documents of those it reached, in the order reached. Two sharing a
/// name share a directory too, so cargo keeps at most one (`remove_duplicate_doc`): of several,
/// none built only for the host, as it does with no `--target`; then the newest version.
fn one_per_name<K>(reached: Vec<Reached<K>>) -> Vec<Reached<K>> {
    let mut by_name: Vec<Vec<Reached<K>>> = vec![];
    for it in reached {
        match by_name.iter_mut().find(|same| same[0].name == it.name) {
            Some(same) => same.push(it),
            None => by_name.push(vec![it]),
        }
    }
    by_name
        .into_iter()
        .filter_map(|mut same| {
            if same.len() > 1 {
                same.retain(|it| it.target);
            }
            same.into_iter().max_by(|a, b| version_key(&a.version).cmp(&version_key(&b.version)))
        })
        .collect()
}

/// Of `kept`, those `roots` still reach through `kept` alone. cargo takes the units it leaves
/// out of the graph with their edges, and with them what only they led to
/// (`remove_duplicate_doc` in `src/cargo/ops/cargo_compile/mod.rs`, cargo 749d85d9fc).
fn still_reached<K: Copy + Eq + std::hash::Hash>(
    roots: &[K],
    kept: &HashSet<K, Global>,
    deps: impl Fn(K) -> Vec<K>,
) -> HashSet<K, Global> {
    let mut out = HashSet::new_in(Global);
    let mut stack = roots.to_vec();
    while let Some(it) = stack.pop() {
        for dep in deps(it) {
            if kept.contains(&dep) && out.insert(dep) {
                stack.push(dep);
            }
        }
    }
    out
}

/// A crate's version as semver orders it, near enough: the numbers, then a release above
/// any of its pre-releases.
fn version_key(version: &str) -> (Vec<u64>, bool, String) {
    let version = version.split('+').next().unwrap_or_default();
    let (release, pre) = version.split_once('-').unwrap_or((version, ""));
    let numbers = release.split('.').map(|it| it.parse().unwrap_or(0)).collect();
    (numbers, pre.is_empty(), pre.to_owned())
}

/// The name code uses for a crate: its library name, `-` as `_`.
/// The name `krate`'s pages are filed under: its Rust name ([`base_db::Crate::rust_name`]).
fn crate_name(db: &Db, krate: Crate) -> Result<String, base_db::UnnamedCrate> {
    krate.base().rust_name(db).map(|it| it.as_str().to_owned()) // alloc: a documented crate's name, once
}

fn is_sysroot(db: &Db, krate: Crate) -> bool {
    matches!(krate.origin(db), base_db::CrateOrigin::Lang(_))
}

/// Sets `cfg(doc)` on the crates documented, as rustdoc does for the crate it documents.
pub fn set_cfg_doc(db: &mut Db, crates: &[Documented]) {
    db.batch(base_db::Trigger::named("cfg(doc) set on the crates documented"), |db| {
        for documented in crates {
            let krate: base_db::Crate = documented.krate.into();
            let mut cfg = krate.cfg_options(db).clone();
            cfg.insert_atom(intern::Symbol::intern("doc"));
            krate.set_cfg_options(db, cfg);
        }
    });
}

/// The documentation of `crates`, ready to write.
pub fn model(db: &Db, crates: Vec<Documented>, test_only: TestOnly) -> model::Model {
    let places = places::place(db, crates, test_only);
    collect::build(db, &places)
}
