//! The macro side (decision D89 (a)): a workspace member's proc macro is built for
//! [`MACRO_TARGET`] and run in Wasmtime (rule 10), so the crates it is built from are neither
//! the target's nor the host's units but a third side's: the same units, under
//! `wasm32-wasip1`'s cfg, each under a key of its own (rule 21: the side is an input of the
//! names, since the cfg differs), with that target's `core`, `alloc`, `std` and `proc_macro`.
//!
//! A member's proc macro and its library test harness (the same code, units/macro_crate.rs)
//! are both taken here, the harness given the side's `proc_macro` as the proc macro is.
//!
//! Only a MEMBER's proc macro is given its dependencies: a dependency's proc macro stays a
//! crate without them, its macros' records coming from the module store. Until the
//! toolchain is cut for `wasm32-wasip1` each member proc macro is refused by name, as its own
//! crate's refusal ([`RefusedCrate`] holding [`UnitsError::NoMacroToolchain`]): the list is
//! still made and every other crate resolves (rule 20). The host's crates are never taken
//! instead (rule 3).
//!
//! The graph's sysroot is the target side's ([`crate::graph::Sysroot`] is one per graph), so a
//! macro-side crate names its side's `core`, `alloc`, `std` and `proc_macro` by explicit
//! dependencies, which win over the sysroot's in [`crate::graph::CrateGraph::externs`]. Known
//! gap, stated: under these explicit names `alloc` and `proc_macro` are in the extern prelude
//! without an `extern crate` (a superset of Rust's rule), and a `#![no_std]` crate keeps `std`
//! nameable. The injected `#[macro_use] extern crate` and the standard prelude are the side's
//! own: the injection names its crate by the extern prelude's entry (graph.rs `externs`), as
//! Rust's does (taking the target side's gave every glob of a side `core`'s macros a second,
//! different candidate: tempo's libc `assert!`, `Ambiguous(GlobVsOuter)`; test
//! tests/macro_side_prelude.rs). The end state is a sysroot per side in the graph.
//!
//! Sans-IO, like its parent: nothing here reads a file.

#![blaze::sans_io]

use blaze_id::{ContentId, CrateItemKey};
use blaze_manifest::lock::Lock;
use blaze_own::{AVec, Alloc};
use blaze_toolchain::{Role, ToolchainView, UnitKind as ToolchainKind, UnitRow};
use blaze_unit_graph::{StdIs, Text, UnitGraph, UnitId, UnitKind, implicit};
use edition::PackageEdition;

use super::{Origin, PROFILE, UnitCrate, UnitsError, edition_of, edition_text, joined_root, kind_name, numbered, toolchain_kind_name, unit_key};

/// The target a proc macro is built for (rule 10).
pub const MACRO_TARGET: &str = "wasm32-wasip1";

/// The domain of a macro-side crate's key: the target-side key's bytes under it.
const SIDE_DOMAIN: &str = "blaze-scope macro side v1";

/// Where a macro-side toolchain crate's files are: `/macro/toolchain/<unit>/`, `<unit>` its
/// index among [`MACRO_TARGET`]'s units (never the target's `/toolchain/<unit>/`, whose
/// indices are another list's).
const TOOLCHAIN_PREFIX: &str = "/macro/toolchain/";

/// The side's toolchain crates every macro-side crate names, by their index in the list.
#[derive(Clone, Copy)]
struct SideSysroot {
    core: usize,
    alloc: usize,
    std: usize,
    proc_macro: usize,
}

/// A target-side key moved to the macro side.
fn side_key(key: CrateItemKey) -> CrateItemKey {
    CrateItemKey(ContentId::of(SIDE_DOMAIN, &key.0.0))
}

/// A crate of the list that cannot be made, with why (rule 20: its own refusal, the rest of
/// the list goes on). It stays in [`super::UnitCrates::crates`] at `krate` so every index
/// holds; whoever asks for its names is given `why`, naming it, instead.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RefusedCrate<'a> {
    /// Its index in [`super::UnitCrates::crates`].
    pub krate: usize,
    pub why: UnitsError<'a>,
}

/// Adds the macro side to `crates`: for each member proc macro among the kept units
/// (`at_of[unit]` is its crate's index, `usize::MAX` when not kept), its dependency closure
/// again as macro-side crates and [`MACRO_TARGET`]'s toolchain crates, and the proc macro's
/// `deps` pointed at them. Nothing is added when no member proc macro is kept.
/// `build_scripts` counts the macro-side crates with a build script (no record is joined
/// for them: their script runs for the macro side).
///
/// When the toolchain has no [`MACRO_TARGET`] cut (or a cut without one of `core`, `alloc`,
/// `std`, `proc_macro`), nothing is added and EACH member proc macro is pushed onto `refused`
/// as [`UnitsError::NoMacroToolchain`] naming it: the list is still made, and every other
/// crate resolves as before (rule 20). The host's crates are never taken instead (rule 3).
#[expect(clippy::too_many_arguments, reason = "the parent's inputs, passed through from `unit_crates`")]
pub(super) fn add_macro_side<'a>(
    graph: &'a UnitGraph<'_>,
    lock: &'a Lock<'_>,
    members: &[super::Member<'a>],
    view: ToolchainView<'a>,
    at_of: &[usize],
    crates: &mut AVec<'a, UnitCrate<'a>>,
    build_scripts: &mut usize,
    refused: &mut AVec<'a, RefusedCrate<'a>>,
    built_for_macro: &mut AVec<'a, usize>,
    alloc: Alloc<'a>,
) -> Result<(), UnitsError<'a>> {
    let units = &graph.units;
    let mut macros: AVec<'a, usize> = AVec::new_in(alloc);
    for (at, u) in units.iter().enumerate() {
        if at_of[at] != usize::MAX && super::macro_crate::is_macro_crate(units, at) && members.iter().any(|m| m.id() == u.package) {
            macros.push(at);
        }
    }
    if macros.is_empty() {
        return Ok(());
    }
    let Some((side, target_cfg)) = side_toolchain(view, crates, alloc)? else {
        for &at in macros.iter() {
            refused.push(RefusedCrate {
                krate: at_of[at],
                why: UnitsError::NoMacroToolchain { krate: units[at].crate_name.as_str(), target: MACRO_TARGET },
            });
        }
        return Ok(());
    };

    // The members' proc macros' dependency closure, by a work list; a proc macro reached is
    // kept and not followed.
    let mut reached: AVec<'a, bool> = AVec::new_in(alloc);
    reached.resize(units.len(), false);
    let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
    for &at in macros.iter() {
        todo.extend(units[at].deps.iter().map(|d| d.unit.index()));
    }
    while let Some(at) = todo.pop() {
        if reached[at] {
            continue;
        }
        reached[at] = true;
        if units[at].kind != UnitKind::ProcMacro {
            todo.extend(units[at].deps.iter().map(|d| d.unit.index()));
        }
    }
    let mut side_at: AVec<'a, usize> = AVec::new_in(alloc);
    side_at.resize(units.len(), usize::MAX);
    let mut next = crates.len();
    for (at, u) in units.iter().enumerate() {
        if reached[at] && matches!(u.kind, UnitKind::Lib | UnitKind::ProcMacro) {
            side_at[at] = next;
            next += 1;
        }
    }

    for (at, u) in units.iter().enumerate() {
        if side_at[at] == usize::MAX {
            continue;
        }
        let name = u.crate_name.as_str();
        let base = numbered("/package/", u.package.0, alloc);
        let root = joined_root(&base, u.root.as_str(), alloc).ok_or(UnitsError::RootLeavesPackage { krate: name, root: u.root.as_str() })?;
        let implicit = implicit(u.edition, &u.kind);
        let mut deps = AVec::new_in(alloc);
        // A dependency's proc macro stays depless: its records come from the module store.
        if u.kind != UnitKind::ProcMacro {
            for d in u.deps.iter() {
                let to = side_at[d.unit.index()];
                if to == usize::MAX {
                    let dep = &units[d.unit.index()];
                    return Err(UnitsError::DependencyNotKept {
                        krate: name,
                        kind: kind_name(&u.kind),
                        dep: d.name.as_str(),
                        dep_kind: kind_name(&dep.kind),
                        dep_for_host: dep.for_host,
                    });
                }
                deps.push((d.name.as_str(), to));
            }
        }
        add_side_externs(&mut deps, side, implicit.std);
        let mut features = AVec::with_capacity_in(u.features.len(), alloc);
        features.extend(u.features.iter().map(|f| f.as_str()));
        let mut cfg = AVec::with_capacity_in(target_cfg.len() + PROFILE.len() + features.len() + 1, alloc);
        cfg.extend(target_cfg.iter().copied());
        cfg.extend(PROFILE.iter().copied());
        cfg.extend(features.iter().map(|f| ("feature", Some(*f))));
        if u.kind == UnitKind::ProcMacro {
            cfg.push(("proc_macro", None));
        }
        cfg.sort_unstable();
        cfg.dedup();
        *build_scripts += usize::from(u.build_script.is_some());
        crates.push(UnitCrate {
            name,
            origin: Origin::Unit(UnitId(at as u32)),
            key: side_key(unit_key(u, lock.package(u.package).source_text.as_str(), alloc)),
            edition: edition_of(u.edition.syntax),
            implicit,
            root,
            shown_from: base,
            seen: super::seen_of(members, u.package),
            features,
            deps,
            cfg,
            build_script: u.build_script.is_some(),
            script: None,
        });
    }

    // Each member proc macro (and its library test harness) is built for the macro side: its
    // dependencies are the macro side's copies, its cfg the side's target atoms (never the host
    // unit's platform atoms) with its features and its own `test`/`proc_macro` flags, and its
    // index is listed in `built_for_macro`, so the world gives it the macro side (its
    // build-script run, rule 21) though it sits before `macro_from`.
    for &at in macros.iter() {
        let u = &units[at];
        let flag = |name: &str| u.cfg.iter().any(|a| a.name.as_str() == name && a.value.is_none());
        let own = &crates[at_of[at]];
        // The proc macro's own cfg controls which tokens its expansion contains. Keep the
        // workspace profile here, so a dev macro evaluates `#[cfg(debug_assertions)]` as cargo does.
        let mut cfg = AVec::with_capacity_in(target_cfg.len() + PROFILE.len() + own.features.len() + 2, alloc);
        cfg.extend(target_cfg.iter().copied());
        cfg.extend(PROFILE.iter().copied());
        cfg.extend(own.features.iter().map(|f| ("feature", Some(*f))));
        for name in ["proc_macro", "test"] {
            if flag(name) {
                cfg.push((name, None));
            }
        }
        cfg.sort_unstable();
        cfg.dedup();
        crates[at_of[at]].cfg = cfg;
        built_for_macro.push(at_of[at]);
        let mut deps = AVec::with_capacity_in(u.deps.len() + 4, alloc);
        for d in u.deps.iter() {
            let to = side_at[d.unit.index()];
            if to == usize::MAX {
                let dep = &units[d.unit.index()];
                return Err(UnitsError::DependencyNotKept {
                    krate: u.crate_name.as_str(),
                    kind: kind_name(&u.kind),
                    dep: d.name.as_str(),
                    dep_kind: kind_name(&dep.kind),
                    dep_for_host: dep.for_host,
                });
            }
            deps.push((d.name.as_str(), to));
        }
        add_side_externs(&mut deps, side, super::macro_crate::unit_implicit(units, at).std);
        crates[at_of[at]].deps = deps;
    }
    Ok(())
}

/// The side's `core`, `alloc`, `std` (when the crate's `std` is Rust's: a blaze edition's is
/// bstd, which the side has none of) and `proc_macro`, each unless the crate already has a
/// dependency of that name.
fn add_side_externs<'a>(deps: &mut AVec<'a, (&'a str, usize)>, side: SideSysroot, std: StdIs) {
    let std = match std {
        StdIs::Rust => Some(side.std),
        StdIs::Bstd(_) => None,
    };
    for (name, krate) in [("core", Some(side.core)), ("alloc", Some(side.alloc)), ("std", std), ("proc_macro", Some(side.proc_macro))] {
        if let Some(krate) = krate
            && !deps.iter().any(|(it, _)| *it == name)
        {
            deps.push((name, krate));
        }
    }
}

/// [`MACRO_TARGET`]'s toolchain crates: the closure of its `core`, `alloc`, `std` and
/// `proc_macro` over their dependencies, pushed onto `out` under macro-side keys, with the
/// side's cfg base (the target's atoms and the profile's). `None`, with nothing pushed, when
/// the toolchain was not cut for the target or its cut lacks one of the four: the caller
/// refuses each member proc macro by name.
///
/// The same walk as the parent's `toolchain_crates` over another triple's rows; it does not
/// call that function because that one needs bstd and a sysroot of five, which the macro side
/// has no use for.
#[expect(clippy::type_complexity, reason = "the side's crates and its cfg base, made in one walk")]
fn side_toolchain<'a>(
    view: ToolchainView<'a>,
    out: &mut AVec<'a, UnitCrate<'a>>,
    alloc: Alloc<'a>,
) -> Result<Option<(SideSysroot, AVec<'a, (&'a str, Option<&'a str>)>)>, UnitsError<'a>> {
    let (Some(rows), Some(target_cfg)) = (view.units(MACRO_TARGET), view.cfg(MACRO_TARGET)) else { return Ok(None) };
    let mut cfg_base: AVec<'a, (&'a str, Option<&'a str>)> = AVec::new_in(alloc);
    cfg_base.extend(target_cfg);
    cfg_base.extend(PROFILE.iter().copied());
    let mut all: AVec<'a, UnitRow<'a>> = AVec::new_in(alloc);
    all.extend(rows);
    // core, alloc, std, proc_macro: the first target-side library unit of each role.
    let mut found = [None; 4];
    for (at, row) in all.iter().enumerate() {
        if row.kind != ToolchainKind::Lib || row.for_host {
            continue;
        }
        let slot = match view.krate(row.krate).role {
            Role::Core => 0,
            Role::Alloc => 1,
            Role::Std => 2,
            Role::ProcMacro => 3,
            Role::Bstd | Role::Library | Role::Blaze | Role::Registry => continue,
        };
        found[slot].get_or_insert(at);
    }
    let [Some(core), Some(alloc_unit), Some(std), Some(proc_macro)] = found else { return Ok(None) };

    let mut keep: AVec<'a, bool> = AVec::new_in(alloc);
    keep.resize(all.len(), false);
    let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
    todo.extend([core, alloc_unit, std, proc_macro]);
    while let Some(u) = todo.pop() {
        if keep[u] {
            continue;
        }
        keep[u] = true;
        if all[u].kind != ToolchainKind::ProcMacro {
            todo.extend(all[u].deps().map(|(_, d)| d as usize));
        }
    }
    let kept = |row: &UnitRow<'_>| row.kind == ToolchainKind::ProcMacro || (row.kind == ToolchainKind::Lib && !row.for_host);
    let first = out.len();
    let mut at_of: AVec<'a, usize> = AVec::new_in(alloc);
    at_of.resize(all.len(), usize::MAX);
    let mut n = 0;
    for (u, row) in all.iter().enumerate() {
        if keep[u] && kept(row) {
            at_of[u] = first + n;
            n += 1;
        }
    }
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    for (u, row) in all.iter().enumerate() {
        if at_of[u] == usize::MAX {
            continue;
        }
        let krate = view.krate(row.krate);
        let edition = edition_text(krate.edition).ok_or(UnitsError::ToolchainEdition { krate: krate.lib, written: krate.edition })?;
        let proc_macro_kind = row.kind == ToolchainKind::ProcMacro;
        let base = numbered(TOOLCHAIN_PREFIX, u as u32, alloc);
        // A tree crate's pack paths start with its directory; a registry crate's do not.
        let mut root = Text::from_str_in(base.as_str(), alloc);
        if !krate.dir.is_empty() && !row.root.starts_with(krate.dir) {
            root.push_str(krate.dir);
            root.push_str("/");
        }
        root.push_str(row.root);
        let mut deps = AVec::new_in(alloc);
        if !proc_macro_kind {
            for (name, d) in row.deps() {
                let to = at_of[d as usize];
                if to == usize::MAX {
                    let dep = &all[d as usize];
                    return Err(UnitsError::DependencyNotKept {
                        krate: krate.lib,
                        kind: "lib",
                        dep: name,
                        dep_kind: toolchain_kind_name(dep.kind),
                        dep_for_host: dep.for_host,
                    });
                }
                deps.push((name, to));
            }
        }
        let mut features = AVec::new_in(alloc);
        features.extend(row.features());
        let mut cfg = AVec::with_capacity_in(cfg_base.len() + features.len(), alloc);
        cfg.extend(cfg_base.iter().copied());
        cfg.extend(features.iter().map(|f| ("feature", Some(*f))));
        // The target-side key's bytes (the pack and the features), then the side.
        let mut key = AVec::new_in(alloc);
        key.extend_from_slice(&krate.pack);
        for f in features.iter() {
            key.extend_from_slice(f.as_bytes());
            key.push(0);
        }
        out.push(UnitCrate {
            name: krate.lib,
            origin: Origin::Toolchain { unit: u as u32, krate: row.krate },
            key: side_key(CrateItemKey(ContentId::of("blaze toolchain unit", &key))),
            edition,
            implicit: implicit(rust, if proc_macro_kind { &UnitKind::ProcMacro } else { &UnitKind::Lib }),
            root,
            shown_from: base,
            seen: blaze_expand::seen::SeenRoot::Package,
            features,
            deps,
            cfg,
            build_script: false,
            script: None,
        });
    }
    let side = SideSysroot { core: at_of[core], alloc: at_of[alloc_unit], std: at_of[std], proc_macro: at_of[proc_macro] };
    Ok(Some((side, cfg_base)))
}
