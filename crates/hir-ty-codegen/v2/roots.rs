//! What a unit's build starts from ([`HirFrontend::roots`]): the instances rustc's collector
//! starts from for the crate type, each compiled with everything it reaches.
//!
//! - An executable: the C `main` that runs the program's `main` through `lang_start`
//!   ([`Kind::Start`]), unless `#![no_main]`; and the program's kept symbols ([`kept`]).
//! - A test harness (`--test`, the crate has `cfg(test)`): the same with the harness's `main`
//!   ([`harness`]), and each `#[test]` and `#[bench]` function, listed as libtest lists them.
//!   A test target with `harness = false` has no `cfg(test)` and is an executable.
//! - A `cdylib`: the program's kept symbols and the allocator shim.
//! - A library: what another crate names of it and the crate's own kept symbols; what a program
//!   reaches past those it compiles itself, so a library's roots are only what is worth having
//!   in the store before.
//! - A proc-macro crate, a shared library with the standard library in it: its macros, then a
//!   `cdylib`'s roots, with the list of the macros among its kept symbols ([`proc_macros`]).
//!
//! rustc's `#[start]` and `#[unix_sigpipe]` are gone from the pinned nightly (rust#134299,
//! `-Zon-broken-pipe`), whose front end refuses them; `lang_start` is passed `sigpipe::DEFAULT`
//! as rustc passes it without the flag.

pub(super) mod harness;
mod kept;
mod outside;
pub mod proc_macros;
#[cfg(test)]
mod tests;

use base_db::{Crate, Db};
use blaze_codegen::{CodegenError, InstanceKey};
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use hir_def::{
    AdtId, AssocItemId, FunctionId, GenericDefId, HasModule, ItemContainerId, Lookup, ModuleDefId,
    ModuleId, attrs::AttrFlags, hir::generics::GenericParams, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use hir_ty::db::HirDatabase;
use intern::sym;

use super::{HirFrontend, HirInstance, HirStatic, Kind, frontend::root_in};
use crate::{compile_time_only, home::Homes};

/// What a unit makes, as its roots are chosen.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RootKind {
    /// A program: a `bin`, an example, a build script, a test with `harness = false`.
    Executable,
    /// A test: libtest's harness over the crate's tests, built with `cfg(test)`.
    Test,
    /// An `rlib`.
    Library,
    /// A `cdylib`.
    Shared,
    /// A proc-macro crate.
    ProcMacro,
}

/// One test of a harness, as libtest lists it.
#[derive(PartialEq, Eq, Debug)]
pub struct RootTest {
    /// The name libtest shows and `--exact` picks: its path from the crate's root.
    pub name: String,
    pub function: FunctionId,
    /// Its function's place in [`Roots::roots`], and so in the build's roots.
    pub root: usize,
    /// `#[ignore]`, with a message or not.
    pub ignore: bool,
    /// `#[should_panic]`, with the message its `expected` gives.
    pub should_panic: Option<Option<String>>,
    /// A `#[bench]`, called with a `Bencher`.
    pub bench: bool,
    /// It reaches the outside world through what neither its code hashes nor the trace of its
    /// run say (processes, the network, assembly): it always runs.
    pub outside: bool,
}

/// The roots of a unit: each instance once, then each static a linker or a runtime finds by
/// its name though no code names it.
#[derive(PartialEq, Eq, Debug, Default)]
pub struct Roots {
    pub roots: Vec<HirInstance>,
    /// `#[used]` statics, and those exported (`#[no_mangle]`, `#[export_name]`).
    pub statics: Vec<HirStatic>,
    /// A test harness's tests, sorted by name, as rustc's harness lists them.
    pub tests: Vec<RootTest>,
}

/// The roots as they are found: each instance once, at the place it was first found.
#[derive(Default)]
struct Listed {
    roots: Vec<HirInstance>,
    seen: HashSet<HirInstance>,
    statics: Vec<HirStatic>,
    seen_statics: HashSet<HirStatic>,
}

impl Listed {
    /// The place of `instance` among the roots.
    fn instance(&mut self, instance: HirInstance) -> usize {
        // clone: once a root, the set's key beside the list's
        if self.seen.insert(instance.clone()) {
            self.roots.push(instance);
            return self.roots.len() - 1;
        }
        self.roots.iter().position(|it| *it == instance).expect("a root seen is listed")
    }

    fn function(&mut self, db: &Db, homes: &mut Homes, func: FunctionId) -> usize {
        self.instance(root_in(db, homes, func))
    }

    fn static_(&mut self, value: HirStatic) {
        if self.seen_statics.insert(value) {
            self.statics.push(value);
        }
    }
}

impl HirFrontend<'_> {
    /// The roots of the unit that compiles `krate` as `kind` makes it. An input rustc takes and
    /// blaze can't is an error naming it.
    pub fn roots(&self, krate: Crate, kind: RootKind) -> Result<Roots, CodegenError> {
        self.with(|db, homes| {
            let mut listed = Listed::default();
            // alloc: once a unit, the tests cargo is given (empty but for a harness)
            let mut tests = Vec::new();
            match kind {
                RootKind::Executable => self.program(db, homes, krate, true, &mut listed)?,
                RootKind::Test if harness::tested(db, krate) => {
                    tests = harness::roots(self, db, homes, krate, &mut listed)?;
                    self.program(db, homes, krate, false, &mut listed)?;
                }
                RootKind::Test => self.program(db, homes, krate, true, &mut listed)?,
                RootKind::Shared => self.program(db, homes, krate, false, &mut listed)?,
                RootKind::Library => library(db, homes, krate, &mut listed),
                RootKind::ProcMacro => {
                    proc_macros::roots(db, homes, krate, &mut listed)?;
                    self.program(db, homes, krate, false, &mut listed)?;
                }
            }
            if !tests.is_empty() {
                let outside = outside::of(self, db, homes, &listed.roots, &tests)
                    .map_err(|what| refused(db, krate, what))?;
                for (test, outside) in tests.iter_mut().zip(outside) {
                    test.outside = outside;
                }
            }
            Ok(Roots { roots: listed.roots, statics: listed.statics, tests })
        })
    }

    /// Every function of `krate` compiled as a root of its own ([`compiled`]), public or
    /// not, in modules and impls, in the def map's order: with the crate's roots, what reaches
    /// every instance an edit of one of its bodies can change. For a harness that counts what
    /// an edit compiles again (`crates/hir-firewall`); a build starts from [`Self::roots`].
    pub fn every_function(&self, krate: Crate) -> Vec<HirInstance> {
        self.with(|db, homes| {
            let mut listed = Listed::default();
            for (_, data) in crate_def_map(db, krate).modules(db) {
                let scope = data.scope(db);
                for def in scope.declarations() {
                    if let ModuleDefId::FunctionId(func) = def
                        && compiled(db, func)
                    {
                        listed.function(db, homes, func);
                    }
                }
                for imp in scope.impls() {
                    for &(_, item) in imp.impl_items(db).items.iter() {
                        if let AssocItemId::FunctionId(func) = item
                            && compiled(db, func)
                        {
                            listed.function(db, homes, func);
                        }
                    }
                }
            }
            listed.roots
        })
    }

    /// The roots of a program of `krate` (an executable, a test harness, a `cdylib`): with
    /// `entry`, the C `main` of its `main`; the allocator shim; and what every crate of it
    /// keeps by name, as rustc's `linked_symbols` (C-level exports, `#[used]` statics, the
    /// standard library's symbols linked by name).
    fn program(
        &self,
        db: &Db,
        homes: &mut Homes,
        krate: Crate,
        entry: bool,
        listed: &mut Listed,
    ) -> Result<(), CodegenError> {
        if entry && let Some(main) = entry_fn(db, krate).map_err(|what| refused(db, krate, what))? {
            listed.instance(HirInstance(Kind::Start(main)));
        }
        let shim = super::allocator::shim(db, krate, |func| root_in(db, homes, func))
            // alloc: the error's message
            .map_err(|what| refused(db, krate, format!("its allocator shim: {what}")))?;
        for instance in shim {
            listed.instance(instance);
        }
        // Sorted by the crates' ids, not in the set's order, which its hasher decides.
        // alloc: once a program, its crates
        let mut linked: Vec<Crate> = crate::linked_crates(db, [krate]).into_iter().collect();
        linked.sort_unstable_by_key(|it| blaze_query::db::AsId::as_id(it));
        for linked in linked {
            add_kept(db, homes, linked, listed);
        }
        Ok(())
    }
}

/// What `krate` keeps by name, as roots.
fn add_kept(db: &Db, homes: &mut Homes, krate: Crate, listed: &mut Listed) {
    let kept = kept::kept(db, krate);
    for &func in &kept.functions {
        listed.function(db, homes, func);
    }
    for &value in &kept.statics {
        listed.static_(HirStatic(value));
    }
}

/// rustc's `entry_fn` of an executable: `None` with `#![no_main]`; else the function
/// `#[rustc_main]` marks, else what the crate's root names `main` in the value namespace,
/// which may be imported. Another `main` is rustc's E0601.
fn entry_fn(db: &Db, krate: Crate) -> Result<Option<FunctionId>, &'static str> {
    let def_map = crate_def_map(db, krate);
    let root = def_map.crate_root(db);
    if AttrFlags::has_word(db, root.into(), "no_main") {
        return Ok(None);
    }
    for (_, data) in def_map.modules(db) {
        for def in data.scope(db).declarations() {
            if let ModuleDefId::FunctionId(func) = def
                && AttrFlags::query(db, func.into()).contains(AttrFlags::RUSTC_MAIN)
            {
                return Ok(Some(func));
            }
        }
    }
    let main = def_map.module(db, root).scope(db).get(&Name::new_symbol_root(sym::main));
    match main.values.map(|it| it.def) {
        Some(ModuleDefId::FunctionId(func)) if !generic(db, func) => Ok(Some(func)),
        Some(ModuleDefId::FunctionId(_)) => Err("its `main` is generic (rustc's E0131)"),
        Some(_) => Err("its `main` is not a function (rustc's E0601)"),
        None => Err("it has no `main` function and no `#![no_main]` (rustc's E0601)"),
    }
}

/// The roots of an `rlib`: the functions another crate names, through `pub` names from the
/// root (re-exports too), with the methods of the impls of what it names; and the crate's own
/// kept symbols. Each not generic: a generic is compiled by the crate that instantiates it.
/// What rustc's reachable set has past these (a private function a public generic calls) is
/// compiled by the program that reaches it.
fn library(db: &Db, homes: &mut Homes, krate: Crate, listed: &mut Listed) {
    let def_map = crate_def_map(db, krate);
    let local = |module: ModuleId| module.krate(db) == krate;
    let mut adts = HashSet::default();
    let mut traits = HashSet::default();
    let mut seen = HashSet::default();
    // alloc: once a library, the modules it names
    let mut stack = vec![def_map.crate_root(db)];
    seen.insert(def_map.crate_root(db));
    while let Some(module) = stack.pop() {
        let scope = module.def_map(db).module(db, module).scope(db);
        for (_, item) in scope.types() {
            if !item.vis.is_public() {
                continue;
            }
            match item.def {
                ModuleDefId::ModuleId(it) if local(it) && seen.insert(it) => stack.push(it),
                ModuleDefId::AdtId(it) if local(it.module(db)) => _ = adts.insert(it),
                ModuleDefId::TraitId(it) if local(it.module(db)) => _ = traits.insert(it),
                _ => {}
            }
        }
        for (_, item) in scope.values() {
            if let ModuleDefId::FunctionId(func) = item.def
                && item.vis.is_public()
                && local(func.module(db))
                && compiled(db, func)
            {
                listed.function(db, homes, func);
            }
        }
    }
    // Impls are named by no path: each of the crate's, of a type and a trait another crate
    // can name.
    for (_, data) in def_map.modules(db) {
        for imp in data.scope(db).impls() {
            if GenericParams::of(db, imp.into()).len_type_or_consts() != 0 {
                continue;
            }
            let adt = db.impl_self_ty(imp).skip_binder().as_adt().map(|it| it.0);
            let named = |adt: AdtId| !local(adt.module(db)) || adts.contains(&adt);
            let trait_ = db.impl_trait(imp).map(|it| it.skip_binder().def_id.0);
            let reached = match trait_ {
                Some(it) => adt.is_none_or(named) && (!local(it.module(db)) || traits.contains(&it)),
                None => adt.is_some_and(|it| local(it.module(db)) && adts.contains(&it)),
            };
            if !reached {
                continue;
            }
            for &(_, item) in imp.impl_items(db).items.iter() {
                let AssocItemId::FunctionId(func) = item else { continue };
                let public = trait_.is_some() || item.assoc_visibility(db).is_public();
                if public && compiled(db, func) {
                    listed.function(db, homes, func);
                }
            }
        }
    }
    add_kept(db, homes, krate, listed);
}

/// Whether `func` is generic over a type or a constant, its own or its impl's or trait's.
pub(super) fn generic(db: &Db, func: FunctionId) -> bool {
    let own = |def: GenericDefId| GenericParams::of(db, def).len_type_or_consts() != 0;
    own(func.into())
        || match func.lookup(db).container {
            ItemContainerId::ImplId(imp) => own(imp.into()),
            ItemContainerId::TraitId(_) => true,
            ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => false,
        }
}

/// Whether `func` is compiled as a root of its own: not generic, with a body (no foreign
/// declaration, no intrinsic) the program runs (not `#[rustc_comptime]`).
pub(super) fn compiled(db: &Db, func: FunctionId) -> bool {
    !generic(db, func)
        && !matches!(func.lookup(db).container, ItemContainerId::ExternBlockId(_))
        && !AttrFlags::query(db, func.into()).contains(AttrFlags::RUSTC_INTRINSIC)
        && FunctionSignature::of(db, func).has_body()
        && !compile_time_only(db, func)
}

/// The error of the roots of `krate`, saying `what`.
pub(super) fn refused(db: &Db, krate: Crate, what: impl std::fmt::Display) -> CodegenError {
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item: ItemKey(ContentId::default()),
        // alloc: the error's message
        what: format!("the roots of crate `{}`: {what}", crate::fncache::crate_name(db, krate)),
    }
}
