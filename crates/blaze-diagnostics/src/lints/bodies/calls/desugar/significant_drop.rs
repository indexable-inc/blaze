//! The parts of a value whose destructors are significant, as rustc's edition-2024 lints of
//! drop order find them: every type that dropping the value may run a `Drop` impl of, but those
//! std marks `#[rustc_insignificant_dtor]` (`Vec`, `Box`, `Rc`, `HashMap`, ..) of insignificant
//! parts, and those of the ecosystem rustc knows to drop trivially (`Waker`, `syn`'s, `bytes`'
//! `Bytes`, ..). A part is the type being opened up when a significant destructor is found in
//! it: a tuple holding a `Drop` type, not the `Drop` type.
//!
//! Follows rustc's `compiler/rustc_middle/src/ty/significant_drop_order.rs`
//! (`extract_component_with_significant_dtor`, `extract_component_raw`,
//! `true_significant_drop_ty`, `ty_dtor_span`), `compiler/rustc_ty_utils/src/needs_drop.rs`
//! (`list_significant_drop_tys`, `NeedsDropTypes` when `exhaustive`, `drop_tys_helper` when
//! `only_significant`, `adt_consider_insignificant_dtor`) and
//! `compiler/rustc_middle/src/ty/util.rs` (`needs_drop_components`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use hir::{InFile, db::HirDatabase};
use hir_def::{
    AdtId, HasModule as _, Lookup, VariantId, nameres::crate_def_map, src::HasSource as _,
};
use hir_expand::attrs::{AstPathExt as _, expand_cfg_attr_view};
use hir_ty::{
    consteval::try_const_usize,
    layout::enum_variant_ids,
    next_solver::{AdtDef, GenericArgs, Ty, TyKind, infer::traits::ObligationCause},
};
use rustc_type_ir::{
    InferTy, Interner as _,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind as _},
};
use smallvec::{SmallVec, smallvec};
use syntax::view::{self as ast, AstNode, HasAttrs};

use crate::{
    cx::def_range_view,
    lints::bodies::{BodyCx, types::util::Solver},
    model::Span,
};

/// What an ADT's own destructor is: rustc's `DtorType`, or none.
#[derive(Clone, Copy, PartialEq)]
enum Dtor {
    None,
    /// `#[rustc_insignificant_dtor]`: its drop matters only through its type arguments.
    Insignificant,
    Significant,
}

/// The significant destructors of a body's types, each ADT's read once.
pub(super) struct SignificantDrops<'s, 'db> {
    db: &'db Db,
    solver: &'s Solver<'db>,
    /// The crate's `#![recursion_limit]` (rustc's `tcx.recursion_limit()`).
    limit: usize,
    dtors: HashMap<AdtId, Dtor>,
    /// The types whose parts went deeper than the limit: rustc's `NeedsDropOverflow`, an
    /// error ("overflow while checking whether `{ty}` requires drop").
    pub(super) overflowed: Vec<Ty<'db>>,
}

impl<'s, 'db> SignificantDrops<'s, 'db> {
    pub(super) fn new(b: &BodyCx<'_, '_, 'db>, solver: &'s Solver<'db>) -> SignificantDrops<'s, 'db> {
        let db = b.db();
        let limit = crate_def_map(db, b.cx.krate.into()).recursion_limit() as usize;
        // alloc: empty, pushed to only on the overflow error
        SignificantDrops { db, solver, limit, dtors: HashMap::default(), overflowed: Vec::new() }
    }

    /// rustc's `extract_component_with_significant_dtor`: the parts of a value of `ty` with a
    /// significant destructor, each once, in rustc's order.
    pub(super) fn components(&mut self, ty: Ty<'db>) -> SmallVec<[Ty<'db>; 4]> {
        let mut seen = HashSet::default();
        let mut tys = self.components_raw(ty, &mut seen);
        let mut unique = HashSet::default();
        tys.retain(|it| unique.insert(*it));
        tys
    }

    /// rustc's `extract_component_raw`.
    fn components_raw(&mut self, ty: Ty<'db>, seen: &mut HashSet<Ty<'db>>) -> SmallVec<[Ty<'db>; 4]> {
        let ty = self.normalize(ty);
        let mut out = SmallVec::new();
        for ty in self.list(ty) {
            match self.true_significant_drop_ty(ty) {
                Some(delegated) => {
                    for ty in delegated {
                        if seen.insert(ty) {
                            out.extend(self.components_raw(ty, seen));
                        }
                    }
                }
                None => {
                    if seen.insert(ty) {
                        out.push(ty);
                    }
                }
            }
        }
        out
    }

    /// rustc's `list_significant_drop_tys`: `NeedsDropTypes` asked for every part, those of
    /// an ADT through `adt_consider_insignificant_dtor`. A part is the type popped when one of
    /// its components always needs drop, or the component itself when it is an alias, an
    /// array of unknown length or a parameter that is all there is to it.
    fn list(&mut self, query: Ty<'db>) -> SmallVec<[Ty<'db>; 4]> {
        let mut out = SmallVec::new();
        let mut seen = HashSet::default();
        seen.insert(query);
        let mut unchecked: SmallVec<[(Ty<'db>, usize); 8]> = SmallVec::new();
        unchecked.push((query, 0));
        'next: while let Some((ty, level)) = unchecked.pop() {
            if level > self.limit {
                self.overflowed.push(query);
                out.push(ty);
                continue;
            }
            let Some(components) = self.needs_drop_components(ty) else {
                out.push(ty);
                continue;
            };
            let mut queue = |component: Ty<'db>| {
                if seen.insert(component) {
                    unchecked.push((component, level + 1));
                }
            };
            for component in components {
                match component.kind() {
                    TyKind::Coroutine(def, args) => {
                        for upvar in args.as_coroutine().upvar_tys().iter() {
                            queue(upvar);
                        }
                        queue(args.as_coroutine().resume_ty());
                        let interner = self.solver.interner();
                        let witness = interner.coroutine_hidden_types(def).instantiate(interner, args).skip_norm_wip();
                        for field in witness.skip_binder().types.iter() {
                            queue(field);
                        }
                    }
                    TyKind::CoroutineWitness(..) => {}
                    TyKind::UnsafeBinder(bound) => queue(bound.skip_binder()),
                    _ if self.solver.is_copy(component) => {}
                    TyKind::Closure(_, args) => {
                        for upvar in args.as_closure().upvar_tys().iter() {
                            queue(upvar);
                        }
                    }
                    TyKind::CoroutineClosure(_, args) => {
                        for upvar in args.as_coroutine_closure().upvar_tys().iter() {
                            queue(upvar);
                        }
                    }
                    TyKind::Adt(def, args) => {
                        let Some(tys) = self.adt_components(def, args) else {
                            out.push(ty);
                            continue 'next;
                        };
                        for required in tys {
                            queue(self.normalize(required));
                        }
                    }
                    TyKind::Alias(..) | TyKind::Array(..) | TyKind::Placeholder(_) | TyKind::Param(_) => {
                        if ty == component {
                            out.push(component);
                            continue 'next;
                        }
                        queue(component);
                    }
                    TyKind::Foreign(_) | TyKind::Dynamic(..) => {
                        out.push(ty);
                        continue 'next;
                    }
                    // `needs_drop_components` gives none of the others.
                    _ => {}
                }
            }
        }
        out
    }

    /// rustc's `needs_drop_components`: the types `ty` needs drop through, none when it
    /// always needs drop.
    fn needs_drop_components(&self, ty: Ty<'db>) -> Option<SmallVec<[Ty<'db>; 2]>> {
        match ty.kind() {
            TyKind::Infer(InferTy::FreshIntTy(_) | InferTy::FreshFloatTy(_))
            | TyKind::Bool
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::Char
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::Str
            | TyKind::Foreign(_) => Some(SmallVec::new()),
            TyKind::Dynamic(..) | TyKind::Error(_) => None,
            TyKind::Pat(ty, _) | TyKind::Slice(ty) => self.needs_drop_components(ty),
            TyKind::Array(element, len) => match self.needs_drop_components(element) {
                Some(components) if components.is_empty() => Some(components),
                components => match try_const_usize(self.db, len) {
                    Some(0) => Some(SmallVec::new()),
                    Some(_) => components,
                    // alloc: inline, a `SmallVec` of one allocates nothing
                    None => Some(smallvec![ty]),
                },
            },
            TyKind::Tuple(fields) => {
                let mut components = SmallVec::new();
                for field in fields.iter() {
                    components.extend(self.needs_drop_components(field)?);
                }
                Some(components)
            }
            TyKind::Adt(..)
            | TyKind::Alias(..)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Closure(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Coroutine(..)
            | TyKind::CoroutineWitness(..)
            // alloc: inline, a `SmallVec` of one allocates nothing
            | TyKind::UnsafeBinder(_) => Some(smallvec![ty]),
        }
    }

    /// `drop_tys_helper`'s `adt_components` when only significant destructors count: what a
    /// value of the ADT needs drop through, none when its own destructor is significant.
    fn adt_components(&mut self, def: AdtDef, args: GenericArgs<'db>) -> Option<SmallVec<[Ty<'db>; 8]>> {
        if def.is_manually_drop() {
            return Some(SmallVec::new());
        }
        let adt = def.def_id();
        match self.dtor(adt) {
            Dtor::Significant => return None,
            Dtor::Insignificant => return Some(SmallVec::from_iter(args.types())),
            Dtor::None => {}
        }
        let db = self.db;
        let interner = self.solver.interner();
        let mut fields = SmallVec::new();
        let mut of_variant = |variant: VariantId| {
            for field in db.field_types(variant).values() {
                fields.push(field.ty().instantiate(interner, args).skip_norm_wip());
            }
        };
        match adt {
            AdtId::UnionId(_) => return Some(SmallVec::new()),
            AdtId::StructId(it) => of_variant(VariantId::StructId(it)),
            AdtId::EnumId(it) => {
                for &variant in enum_variant_ids(db, it).iter() {
                    of_variant(VariantId::EnumVariantId(variant));
                }
            }
        }
        Some(fields)
    }

    /// rustc's `adt_consider_insignificant_dtor`.
    fn dtor(&mut self, adt: AdtId) -> Dtor {
        let db = self.db;
        *self.dtors.entry(adt).or_insert_with(|| {
            if insignificant_dtor(db, adt) {
                Dtor::Insignificant
            } else if hir_ty::drop::destructor(db, adt).is_some() {
                Dtor::Significant
            } else {
                Dtor::None
            }
        })
    }

    /// rustc's `true_significant_drop_ty`: the parts a type of the ecosystem known to drop
    /// trivially delegates its drop to (none for most), by its path where it is defined.
    fn true_significant_drop_ty(&self, ty: Ty<'db>) -> Option<SmallVec<[Ty<'db>; 2]>> {
        let TyKind::Adt(def, args) = ty.kind() else { return None };
        let path = def_path(self.db, def.def_id())?;
        let arg = |index: usize| args.iter().nth(index).and_then(|it| it.ty());
        match path[..] {
            ["syn" | "proc_macro2", ..]
            | ["core" | "std", "task", "LocalWaker" | "Waker"]
            | ["core" | "std", "task", "wake", "LocalWaker" | "Waker"]
            | ["tracing", "instrument", "Instrumented"]
            | ["bytes", "Bytes"] => Some(SmallVec::new()),
            // alloc: inline, a `SmallVec` of one allocates nothing
            ["hashbrown", "raw", "RawTable" | "RawIntoIter"] => arg(0).map(|it| smallvec![it]),
            // alloc: inline, a `SmallVec` of one allocates nothing
            ["hashbrown", "raw", "RawDrain"] => arg(1).map(|it| smallvec![it]),
            _ => None,
        }
    }

    /// rustc's `try_normalize_erasing_regions` in the typing environment of the lint.
    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        let cause = ObligationCause::dummy();
        self.solver.infcx.at(&cause, self.solver.env).deeply_normalize(ty).unwrap_or(ty)
    }
}

/// The crate's name and the names of the modules down to the ADT and its own, when each is
/// a name in the type namespace (rustc's `DefPathData::TypeNs`): none for an ADT in a block.
/// Only those of the crates `true_significant_drop_ty` knows are read.
fn def_path(db: &Db, adt: AdtId) -> Option<SmallVec<[&'static str; 6]>> {
    let adt = hir::Adt::from(adt);
    let module = adt.module(db);
    let krate = module.krate(db).display_name(db)?.crate_name().as_str();
    if !matches!(krate, "syn" | "proc_macro2" | "core" | "std" | "tracing" | "bytes" | "hashbrown") {
        return None;
    }
    let mut path: SmallVec<[&'static str; 6]> = SmallVec::new();
    path.push(adt.name(db).symbol().as_str());
    for module in module.path_to_root(db) {
        if module.is_crate_root(db) {
            break;
        }
        if module.nearest_non_block_module(db) != module {
            return None;
        }
        path.push(module.name(db)?.symbol().as_str());
    }
    path.push(krate);
    path.reverse();
    Some(path)
}

/// Whether the ADT is `#[rustc_insignificant_dtor]`, its `cfg_attr`s expanded.
fn insignificant_dtor(db: &Db, adt: AdtId) -> bool {
    let attrs = match adt {
        AdtId::StructId(it) => it.lookup(db).source_view(db).value.attrs(),
        AdtId::EnumId(it) => it.lookup(db).source_view(db).value.attrs(),
        AdtId::UnionId(it) => it.lookup(db).source_view(db).value.attrs(),
    };
    let cfg = adt.krate(db).cfg_options(db);
    expand_cfg_attr_view(attrs, || cfg, |meta, _| match meta {
        ast::Meta::PathMeta(meta) if meta.path().is1("rustc_insignificant_dtor") => ControlFlow::Break(()),
        _ => ControlFlow::Continue(()),
    })
    .is_some()
}

/// rustc's `ty_dtor_span`: of an ADT, its `Drop` impl's header, or without one where the ADT
/// is defined. The other types a part can be (a tuple, an array, a trait object, an alias, a
/// parameter) have none; closures, coroutines and fn items are opened up or need no drop, so
/// are never parts.
pub(super) fn dtor_span(b: &BodyCx<'_, '_, '_>, ty: Ty<'_>) -> Option<Span> {
    let TyKind::Adt(def, _) = ty.kind() else { return None };
    let db = b.db();
    let adt = def.def_id();
    let node: InFile<ast::Node<'_>> = match hir_ty::drop::destructor(db, adt) {
        Some(impl_) => impl_.lookup(db).source_view(db).map(|it| it.syntax()),
        None => match adt {
            AdtId::StructId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
            AdtId::EnumId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
            AdtId::UnionId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
        },
    };
    Some(b.cx.span(node.map(def_range_view)))
}
