//! Const stability: rustc's `lookup_const_stability` (`rustc_passes/src/stability.rs`),
//! `is_fn_or_trait_safe_to_expose_on_stable` and `rustc_allow_const_fn_unstable`
//! (`check_consts/mod.rs`), and `Span::allows_unstable` of a call.
//!
//! The attributes (`#[rustc_const_stable]`, `#[rustc_const_unstable]`,
//! `#[rustc_const_stable_indirect]`, `#[rustc_intrinsic_const_stable_indirect]`) count only in a
//! crate with `#![feature(staged_api)]`, as rustc's; a crate without it has a const stability
//! only when everything of it is unstable (`-Zforce-unstable-if-unmarked`, a sysroot crate's).

use std::{num::NonZero, ops::ControlFlow};

use base_db::{Crate, CrateOrigin, Db};
use cfg::CfgOptions;
use hir_def::{
    AttrDefId, FunctionId, HasModule, ImplId, ItemContainerId, Lookup, TraitId,
    attrs::stability::{Stability, StabilityLevel, stability},
    expr_store::ExpressionStore,
    signatures::FunctionSignature,
    src::HasSource,
    unstable_features::UnstableFeatures,
};
use hir_expand::{MacroCallId, MacroDefKind, attrs::expand_cfg_attr_view};
use hir_ty::{InferBodyId, effects::trait_is_const};
use intern::Symbol;
use syntax::{
    NodeOrToken, SyntaxKind,
    view::{self, AstToken, HasAttrs, TokenTreeChildren},
};

use super::MirSpan;

/// What rustc checks the const stability of: a function called, or the trait of a trait's
/// method called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Item {
    Fn(FunctionId),
    Trait(TraitId),
}

impl Item {
    pub(super) fn krate(self, db: &Db) -> Crate {
        match self {
            Item::Fn(it) => it.module(db).krate(db),
            Item::Trait(it) => it.module(db).krate(db),
        }
    }
}

/// rustc's `ConstStability`.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
pub(super) struct ConstStability {
    pub(super) level: Level,
    /// `#[rustc_const_stable_indirect]`, or a `const fn` unmarked and unstable, which rustc
    /// holds to recursive const stability.
    pub(super) const_stable_indirect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
pub(super) enum Level {
    Stable,
    Unstable { feature: Symbol, implied_by: Option<Symbol>, issue: Option<NonZero<u32>> },
}

impl ConstStability {
    fn is_unstable(&self) -> bool {
        matches!(self.level, Level::Unstable { .. })
    }

    /// A regular `#[unstable]` taken as const stability.
    fn of_regular(regular: &Stability, const_stable_indirect: bool) -> ConstStability {
        let level = match &regular.level {
            StabilityLevel::Unstable { issue, implied_by, .. } => Level::Unstable {
                // clone: a `Symbol`, a count bumped
                feature: regular.feature.clone(),
                // clone: a `Symbol`, a count bumped
                implied_by: implied_by.clone(),
                issue: *issue,
            },
            StabilityLevel::Stable { .. } => Level::Stable,
        };
        ConstStability { level, const_stable_indirect }
    }
}

/// rustc's `lookup_const_stability` of `item`.
pub(super) fn const_stability(db: &Db, item: Item) -> Option<&ConstStability> {
    match item {
        Item::Fn(func) => fn_const(db, func).stability.as_ref(),
        Item::Trait(trait_) => trait_const(db, trait_).as_ref(),
    }
}

/// `is_fn_or_trait_safe_to_expose_on_stable`, in a body of `caller`.
pub(super) fn safe_to_expose(db: &Db, item: Item, caller: Crate) -> bool {
    // A default body in a `const trait` is const-stable when the trait is.
    if let Item::Fn(func) = item
        && let ItemContainerId::TraitId(trait_) = func.lookup(db).container
        && trait_is_const(db, trait_)
    {
        return safe_to_expose(db, Item::Trait(trait_), caller);
    }
    match const_stability(db, item) {
        // A `staged_api` crate holds its unmarked functions to recursive const stability; of
        // another crate's that is not known.
        None => item.krate(db) == caller && UnstableFeatures::query(db, caller).staged_api,
        Some(stability) => {
            matches!(stability.level, Level::Stable)
                || stability.const_stable_indirect
                || matches!(item, Item::Fn(func) if intrinsic_const_stable_indirect(db, func))
        }
    }
}

/// `#[rustc_intrinsic_const_stable_indirect]` on `func`.
pub(super) fn intrinsic_const_stable_indirect(db: &Db, func: FunctionId) -> bool {
    fn_const(db, func).intrinsic_const_stable_indirect
}

/// Whether `func` is written `#[rustc_const_unstable]`, nothing inherited.
pub(super) fn written_const_unstable(db: &Db, func: FunctionId) -> bool {
    fn_const(db, func).written_unstable
}

/// `rustc_allow_const_fn_unstable(func, gate)`: `#[rustc_allow_const_fn_unstable(.., gate,
/// ..)]` on `func`.
pub(super) fn allows_const_fn_unstable(db: &Db, func: FunctionId, gate: &Symbol) -> bool {
    let source = func.lookup(db).source_view(db);
    let cfg = func.module(db).krate(db).cfg_options(db);
    lists(source.value.attrs(), cfg, "rustc_allow_const_fn_unstable", gate)
}

/// Whether `krate`'s items are all unstable to rustc (`-Zforce-unstable-if-unmarked`): a
/// sysroot crate's, as `hir_def`'s stability has it.
pub(super) fn forces_unstable(db: &Db, krate: Crate) -> bool {
    matches!(krate.data(db).origin, CrateOrigin::Lang(_))
}

/// `Span::allows_unstable(feature)` of the call at `at` in `owner`'s body (an expression, or the
/// pattern whose comparison it is): the macro whose expansion made it is written
/// `#[allow_internal_unstable(.., feature, ..)]`. rustc reads the expansion off the hygiene of
/// the call's span; here it is that of the call's first token, or the macro whose expansion the
/// call is in. A call in a macro's argument is allowed when either macro allows it, where rustc
/// asks only the one whose tokens it is.
pub(super) fn allows_unstable(db: &Db, owner: InferBodyId<'_>, at: MirSpan, feature: &Symbol) -> bool {
    let source_map = ExpressionStore::with_source_map(db, owner.expression_store_owner(db)).1;
    let (file_id, range) = match at {
        MirSpan::ExprId(expr) => match source_map.expr_syntax(expr) {
            Ok(it) => (it.file_id, it.value.text_range()),
            Err(_) => return false,
        },
        MirSpan::PatId(pat) => match source_map.pat_syntax(pat) {
            Ok(it) => (it.file_id, it.value.text_range()),
            Err(_) => return false,
        },
        _ => return false,
    };
    let Some(file) = file_id.macro_file() else { return false };
    let ctxt = file_id.span_map(db).span_for_range(range).ctx;
    ctxt.outer_expn(db).is_some_and(|call| macro_allows(db, call.into(), feature))
        || macro_allows(db, file, feature)
}

fn macro_allows(db: &Db, call: MacroCallId, feature: &Symbol) -> bool {
    use hir_expand::Lookup as _;
    let def = &call.lookup(db).def;
    let id = match def.kind {
        MacroDefKind::Declarative(id, _)
        | MacroDefKind::BuiltIn(id, _)
        | MacroDefKind::BuiltInAttr(id, _)
        | MacroDefKind::BuiltInDerive(id, _)
        | MacroDefKind::BuiltInEager(id, _)
        | MacroDefKind::UnimplementedBuiltIn(id) => id,
        MacroDefKind::ProcMacro(..) => return false,
    };
    let cfg = def.krate.cfg_options(db);
    lists(id.to_view(db).attrs(), cfg, "allow_internal_unstable", feature)
}

/// Whether an attribute `name(..)` of `attrs` lists `feature`.
fn lists<'t>(
    attrs: impl Iterator<Item = view::Attr<'t>>,
    cfg: &CfgOptions,
    name: &str,
    feature: &Symbol,
) -> bool {
    let found = expand_cfg_attr_view(
        attrs,
        || cfg,
        |meta, _| {
            if meta.simple_name() == Some(name)
                && let view::Meta::TokenTreeMeta(list) = meta
                && let Some(tt) = list.token_tree()
                && TokenTreeChildren::new(tt).any(|it| {
                    matches!(it, NodeOrToken::Token(it) if it.text() == feature.as_str())
                })
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
    );
    found.is_some()
}

/// A function's const stability and the attributes that go with it.
#[derive(Debug, Clone, PartialEq, Eq, Default, blaze_query::Value)]
struct FnConst {
    stability: Option<ConstStability>,
    intrinsic_const_stable_indirect: bool,
    written_unstable: bool,
}

#[blaze_query::tracked(returns(ref))]
fn fn_const(db: &Db, func: FunctionId) -> FnConst {
    let krate = func.module(db).krate(db);
    let is_const = FunctionSignature::of(db, func).is_const();
    let regular = || stability(db, AttrDefId::FunctionId(func).into()).filter(|it| it.is_unstable());
    let read = || {
        let source = func.lookup(db).source_view(db);
        read(source.value.attrs(), krate.cfg_options(db))
    };
    if !UnstableFeatures::query(db, krate).staged_api {
        // Only a crate whose items are all unstable has a stability here: a `const fn` takes
        // it as its const stability.
        let Some(regular) = regular().filter(|_| is_const) else { return FnConst::default() };
        let own = read();
        return FnConst {
            stability: Some(ConstStability::of_regular(regular, own.const_stable_indirect)),
            intrinsic_const_stable_indirect: false,
            written_unstable: false,
        };
    }
    let own = read();
    let written_unstable = own.stability.as_ref().is_some_and(ConstStability::is_unstable);
    let stability = own
        .stability
        // A `const fn` unmarked takes an `#[unstable]` as its const stability.
        .or_else(|| {
            let regular = regular().filter(|_| is_const)?;
            Some(ConstStability::of_regular(regular, true))
        })
        // An `impl` or trait's item takes its parent's const-unstable.
        .or_else(|| {
            match func.lookup(db).container {
                // clone: once per function asked, its parent's stability
                ItemContainerId::TraitId(it) => trait_const(db, it).clone(),
                ItemContainerId::ImplId(it) => impl_const(db, it),
                ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => None,
            }
            .filter(ConstStability::is_unstable)
        });
    FnConst {
        stability,
        intrinsic_const_stable_indirect: own.intrinsic_const_stable_indirect,
        written_unstable,
    }
}

#[blaze_query::tracked(returns(ref))]
fn trait_const(db: &Db, trait_: TraitId) -> Option<ConstStability> {
    let krate = trait_.module(db).krate(db);
    if !UnstableFeatures::query(db, krate).staged_api {
        return None;
    }
    let source = trait_.lookup(db).source_view(db);
    read(source.value.attrs(), krate.cfg_options(db)).stability
}

fn impl_const(db: &Db, impl_: ImplId) -> Option<ConstStability> {
    let krate = impl_.module(db).krate(db);
    if !UnstableFeatures::query(db, krate).staged_api {
        return None;
    }
    let source = impl_.lookup(db).source_view(db);
    read(source.value.attrs(), krate.cfg_options(db)).stability
}

/// What a definition's own const stability attributes say.
#[derive(Default)]
struct Own {
    stability: Option<ConstStability>,
    intrinsic_const_stable_indirect: bool,
    const_stable_indirect: bool,
}

/// The const stability attributes of `attrs`, the first well-formed `#[rustc_const_stable]` or
/// `#[rustc_const_unstable]` counting, with `#[rustc_const_stable_indirect]`
/// (`ConstStability::from_partial`).
fn read<'t>(attrs: impl Iterator<Item = view::Attr<'t>>, cfg: &CfgOptions) -> Own {
    let mut own = Own::default();
    expand_cfg_attr_view(
        attrs,
        || cfg,
        |meta, _| {
            let unstable = match meta.simple_name() {
                Some("rustc_const_stable_indirect") => {
                    own.const_stable_indirect = true;
                    return ControlFlow::Continue(());
                }
                Some("rustc_intrinsic_const_stable_indirect") => {
                    own.intrinsic_const_stable_indirect = true;
                    return ControlFlow::Continue(());
                }
                Some("rustc_const_stable") => false,
                Some("rustc_const_unstable") => true,
                _ => return ControlFlow::Continue(()),
            };
            if own.stability.is_none()
                && let view::Meta::TokenTreeMeta(list) = meta
                && let Some(tt) = list.token_tree()
            {
                own.stability = parse(tt, unstable);
            }
            ControlFlow::<()>::Continue(())
        },
    );
    if let Some(stability) = &mut own.stability {
        stability.const_stable_indirect = own.const_stable_indirect;
    }
    own
}

/// `#[rustc_const_stable(feature = "..", since = "..")]` or `#[rustc_const_unstable(feature =
/// "..", issue = "..", implied_by = "..")]`: `None` without a feature, or an unstable one
/// without an issue, which rustc rejects.
fn parse(tt: view::TokenTree<'_>, unstable: bool) -> Option<ConstStability> {
    let (mut feature, mut issue, mut implied_by) = (None, None, None);
    let mut key = None;
    for child in TokenTreeChildren::new(tt) {
        let NodeOrToken::Token(token) = child else { continue };
        match token.kind() {
            kind if kind.is_any_identifier() => key = Some(token.text()),
            SyntaxKind::STRING => {
                let value = view::String::cast(token)?.value().ok()?;
                let slot = match key.take() {
                    Some("feature") => &mut feature,
                    Some("issue") => &mut issue,
                    Some("implied_by") => &mut implied_by,
                    _ => continue,
                };
                *slot = Some(Symbol::intern(&value));
            }
            _ => {}
        }
    }
    let feature = feature?;
    let level = match unstable {
        false => Level::Stable,
        true => Level::Unstable {
            issue: match issue?.as_str() {
                "none" => None,
                issue => Some(issue.parse().ok()?),
            },
            feature,
            implied_by,
        },
    };
    Some(ConstStability { level, const_stable_indirect: false })
}
