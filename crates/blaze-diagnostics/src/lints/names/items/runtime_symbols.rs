//! `invalid_runtime_symbol_definitions` (deny) and `suspicious_runtime_symbol_definitions`: a
//! function or static of this crate whose symbol is one `core` or `std` declares with
//! `#[rustc_canonical_symbol]` (`memcpy`, `strlen`, `malloc`, ..), with another signature. It is
//! invalid when the ABI, the C variadics or the number of parameters differ, or when it returns
//! nothing where the runtime's returns a value; else suspicious.
//!
//! Follows `compiler/rustc_lint/src/runtime_symbols.rs`, `compiler/rustc_passes/src/
//! canonical_symbols.rs`, `compiler/rustc_symbol_mangling/src/lib.rs` (`symbol_name_from_attrs`)
//! and `compiler/rustc_lint/src/lints.rs` (`RedefiningRuntimeSymbolsDiag`) (MIT or Apache-2.0).

use blaze_hash::{HashMap, HashSet};
use blaze_items::{Fact, ItemId};
use hir::{ModuleDef, view::HasSourceView as _};
use hir_def::{
    AdtId, AttrDefId, CallableDefId, FunctionId, GenericDefId, ItemContainerId, Lookup as _,
    StaticId, attrs::AttrFlags, hir::generics::GenericParams, lang_item::lang_items,
    signatures::FunctionSignature, unstable_features::UnstableFeatures,
};
use hir_ty::db::HirDatabase;
use hir_ty::next_solver::{
    Binder, DbInterner, FnSig, ParamEnv, Region, RegionExt as _, Ty, TyKind, TypingMode,
    infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
};
use intern::{Symbol, sym};
use rustc_type_ir::{
    Interner as _,
    inherent::{GenericArgs as _, IntoKind as _, Ty as _},
};
use syntax::view::{self as ast, AstNode, HasAttrs};

use super::walk::{Container, Item};
use crate::{
    cx::{Cx, item_range_view, signature_range_view},
    lower::{Found, Phase},
    model::Diag,
};

/// The runtime symbols this crate's items could redefine: each symbol a crate of its graph
/// declares canonical, with the foreign function that declares it. Keyed by interned symbol:
/// one hash of a pointer per lookup.
pub(super) struct Canonical {
    symbols: HashMap<Symbol, FunctionId>,
}

impl Canonical {
    /// rustc's `all_canonical_symbols`, read only for the symbols `items` define, so that a
    /// crate defining none reads no other crate.
    pub(super) fn of<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>]) -> Canonical {
        let mut wanted = HashSet::default();
        for item in items {
            if let Some(candidate) = candidate(cx, item) {
                wanted.insert(candidate.symbol);
            }
        }
        let mut symbols = HashMap::default();
        if wanted.is_empty() {
            return Canonical { symbols };
        }
        let db = cx.db;
        for krate in cx.krate.base().transitive_deps(db).iter().copied() {
            if !UnstableFeatures::query(db, krate).is_enabled(&sym::rustc_attrs) {
                continue;
            }
            for module in hir::Crate::from(krate).modules_iter(db) {
                for decl in module.declarations_iter(db) {
                    let ModuleDef::Function(f) = decl else { continue };
                    let Ok(id) = FunctionId::try_from(f) else { continue };
                    if !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_)) {
                        continue;
                    }
                    let symbol = foreign_symbol(cx, AttrDefId::FunctionId(id), &FunctionSignature::of(db, id).name);
                    if !wanted.contains(&symbol) || !is_canonical(cx, f) {
                        continue;
                    }
                    symbols.entry(symbol).or_insert(id);
                }
            }
        }
        Canonical { symbols }
    }
}

/// Whether a foreign function is declared `#[rustc_canonical_symbol]`.
fn is_canonical(cx: &Cx<'_, '_>, f: hir::Function) -> bool {
    let Some(source) = f.source_view(cx.db) else { return false };
    source.value.attrs().any(|attr| attr.simple_name().is_some_and(|name| name == "rustc_canonical_symbol"))
}

/// The symbol of a foreign item: its `#[link_name]`, else its name.
fn foreign_symbol(cx: &Cx<'_, '_>, owner: AttrDefId, name: &hir::Name) -> Symbol {
    match AttrFlags::link_name(cx.db, owner) {
        Some(it) => it,
        None => *name.symbol(),
    }
}

/// rustc's `symbol_name_from_attrs` of a function or a static defined here: its
/// `#[export_name]`, else its name when it is `#[no_mangle]`; else it is mangled, and can be no
/// runtime symbol.
fn defined_symbol(cx: &Cx<'_, '_>, owner: AttrDefId, name: &hir::Name) -> Option<Symbol> {
    if let Some(it) = AttrFlags::export_name(cx.db, owner) {
        return Some(it);
    }
    AttrFlags::query(cx.db, owner).contains(AttrFlags::NO_MANGLE).then(|| *name.symbol())
}

/// What of an item is checked: a function or a static, with its symbol.
struct Candidate {
    symbol: Symbol,
    def: Def,
}

#[derive(Clone, Copy)]
enum Def {
    Fn(hir::Function, FunctionId),
    Static(hir::Static, StaticId),
}

/// The function or static of `item` whose symbol rustc checks, if it is one: a function that is
/// not generic or a static, defined here with a symbol of its own, or declared in an `extern`
/// block (a static only with a `#[linkage]`, as `std`'s `weak!` declares them). A function's
/// symbol, from its attribute flags, is read before its signature: most have none of their own.
fn candidate<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Candidate> {
    let db = cx.db;
    let foreign = match item.container {
        Container::Module => false,
        Container::Extern(_) => true,
        Container::Trait(_) | Container::Impl(_) => return None,
    };
    match item.node.value {
        ast::Item::Fn(f) => {
            let def = cx.vsema.to_def2(item.node.with_value(f))?;
            let id = FunctionId::try_from(def).ok()?;
            let name = &FunctionSignature::of(db, id).name;
            let symbol = if foreign {
                foreign_symbol(cx, AttrDefId::FunctionId(id), name)
            } else {
                let symbol = defined_symbol(cx, AttrDefId::FunctionId(id), name)?;
                if !GenericParams::of(db, GenericDefId::FunctionId(id)).is_empty() {
                    return None;
                }
                let sig = db.callable_item_signature(CallableDefId::FunctionId(id)).skip_binder();
                if !sig.bound_vars().is_empty() {
                    return None;
                }
                symbol
            };
            Some(Candidate { symbol, def: Def::Fn(def, id) })
        }
        ast::Item::Static(s) => {
            let def = cx.vsema.to_def2(item.node.with_value(s))?;
            let id: StaticId = def.into();
            let owner = AttrDefId::StaticId(id);
            let name = def.name(db);
            let symbol = if foreign {
                if !AttrFlags::query(db, owner).contains(AttrFlags::HAS_LINKAGE) {
                    return None;
                }
                foreign_symbol(cx, owner, &name)
            } else {
                defined_symbol(cx, owner, &name)?
            };
            Some(Candidate { symbol, def: Def::Static(def, id) })
        }
        _ => None,
    }
}

/// The lints of `item` at rustc's `check_item`: of itself when it is a function or a static, of
/// its foreign items (`foreign`, the items after it) when it is an `extern` block, emitted at the
/// block.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, foreign: &[Item<'db>], canonical: &Canonical) -> Vec<Found> {
    if canonical.symbols.is_empty() {
        return Vec::new(); // alloc: empty, no allocation
    }
    // alloc: empty, no allocation, unless a definition fires a lint
    let mut found = Vec::new();
    match &item.node.value {
        ast::Item::Fn(_) | ast::Item::Static(_) if matches!(item.container, Container::Module) => {
            check(cx, item, item, canonical, &mut found);
        }
        ast::Item::ExternBlock(_) => {
            for it in foreign.iter().take_while(|it| matches!(it.container, Container::Extern(_))) {
                check(cx, it, item, canonical, &mut found);
            }
        }
        _ => {}
    }
    found
}

/// rustc's `check_fn` and `check_static` of `item`, linted at `at`.
fn check<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, at: &Item<'db>, canonical: &Canonical, found: &mut Vec<Found>) {
    let Some(candidate) = candidate(cx, item) else { return };
    let Some(&expected_id) = canonical.symbols.get(&candidate.symbol) else { return };
    let db = cx.db;
    let owner = match candidate.def {
        Def::Fn(_, id) => GenericDefId::FunctionId(id),
        Def::Static(_, id) => GenericDefId::StaticId(id),
    };
    let types = Types::new(cx, owner);
    let interner = types.interner;
    let sig_of = |id: FunctionId| {
        let sig = db.callable_item_signature(CallableDefId::FunctionId(id)).skip_binder();
        types.sig(interner.instantiate_bound_regions_with_erased(sig))
    };
    let expected = sig_of(expected_id);
    let (printer, user, span) = match candidate.def {
        Def::Fn(def, id) => {
            let ast::Item::Fn(f) = item.node.value else { return };
            (def.ret_type(db), Ok(sig_of(id)), item.node.with_value(signature_range_view(f)))
        }
        Def::Static(def, id) => {
            let ty = types.normalize(db.type_for_static(id).skip_binder());
            let option = lang_items(db, cx.krate.into()).Option;
            let ty = match ty.kind() {
                TyKind::Adt(adt, args) if option.is_some_and(|it| adt.def_id() == AdtId::EnumId(it)) => args.type_at(0),
                _ => ty,
            };
            let user = match ty.kind() {
                TyKind::FnPtr(sig_tys, header) => {
                    Ok(types.sig(interner.instantiate_bound_regions_with_erased(sig_tys.with(header))))
                }
                _ => Err(ty),
            };
            (def.ty(db), user, item.node.map(|it| item_range_view(it.syntax())))
        }
    };
    let fn_ptr = |sig| Ty::new_fn_ptr(interner, Binder::dummy(sig));
    let (invalid, found_ty) = match user {
        Ok(user) if user == expected => return,
        Ok(user) => {
            let returns = !expected.output().is_unit() && !expected.output().is_never();
            let invalid = expected.abi() != user.abi()
                || expected.c_variadic() != user.c_variadic()
                || expected.inputs().len() != user.inputs().len()
                || (returns && user.output().is_unit());
            (invalid, fn_ptr(user))
        }
        Err(ty) => (true, ty),
    };
    let print = |ty| cx.ty(&printer.derived_from_next_solver(ty));
    let symbol = &candidate.symbol;
    let (lint, what) = match invalid {
        true => ("invalid_runtime_symbol_definitions", "invalid"),
        false => ("suspicious_runtime_symbol_definitions", "suspicious"),
    };
    // alloc: a lint that fires
    let message = format!("{what} definition of the runtime `{symbol}` symbol used by the standard library");
    let mut diag = Diag::lint(lint, message, cx.span(span));
    // alloc: a lint that fires
    diag.note(format!(
        "expected `{}` (for the current target)\nfound    `{}`",
        print(fn_ptr(expected)),
        print(found_ty)
    ));
    // alloc: a lint that fires
    diag.help(format!(
        "either fix the signature or remove any attributes like `#[unsafe(no_mangle)]`, \
         `#[unsafe(export_name = \"{symbol}\")]`, or `#[link_name = \"{symbol}\"]`"
    ));
    if !invalid {
        diag.help("allow this lint if the signature is compatible");
    }
    let (id, name) = match candidate.def {
        // alloc: the name in a fact of a lint that fires
        Def::Fn(_, id) => (ItemId::Function(id), FunctionSignature::of(db, id).name.as_str().to_owned()),
        Def::Static(def, id) => (ItemId::Static(id), def.name(db).as_str().to_owned()), // alloc: as above
    };
    // alloc: a lint that fires
    diag.fact(Fact::Signature(cx.key(ItemId::Function(expected_id))), format!("the signature of the runtime `{symbol}`"));
    diag.fact(Fact::Signature(cx.key(id)), format!("the signature of `{name}`")); // alloc: a lint that fires
    let at = at.node.map(|it| ast::ptr(it.syntax()));
    found.push(Found::lint(diag, Phase::Late, at));
}

/// The types of one item in its param env, normalized with regions erased as rustc's
/// `normalize_erasing_regions`.
struct Types<'db> {
    interner: DbInterner<'db>,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
}

impl<'db> Types<'db> {
    fn new(cx: &Cx<'_, 'db>, owner: GenericDefId) -> Self {
        let interner = DbInterner::new_with(cx.db, cx.krate.base());
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        Types { interner, infcx, env: cx.db.trait_environment(owner) }
    }

    fn erased(&self, ty: Ty<'db>) -> Ty<'db> {
        let interner = self.interner;
        rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
    }

    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        let ty = self.erased(ty);
        match self.infcx.at(&ObligationCause::dummy(), self.env).deeply_normalize(ty) {
            Ok(it) => self.erased(it),
            Err(_) => ty,
        }
    }

    fn sig(&self, sig: FnSig<'db>) -> FnSig<'db> {
        let tys = sig.inputs_and_output.iter().map(|ty| self.normalize(ty));
        FnSig { inputs_and_output: self.interner.mk_type_list_from_iter(tys), ..sig }
    }
}
