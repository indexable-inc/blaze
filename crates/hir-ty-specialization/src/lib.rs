//! rustc's `min_specialization` checks (`rustc_hir_analysis::impl_wf_check::min_specialization`)
//! and blaze's sound subset of specialization, `#![blaze::specialization]`.
//!
//! A specializing impl must be *always applicable* (Niko Matsakis, "Maximally minimal
//! specialization: always applicable impls", 2018): whenever the impl it specializes applies,
//! whether this one applies cannot depend on a lifetime, which codegen erases. So it may not
//! name `'static` where its parent is generic, may not repeat a parameter (`(T, T)` asks two
//! lifetimes to be equal), and may only add bounds the parent lacks on traits that are always
//! applicable themselves: `#[rustc_specialization_trait]` or `#[blaze::specialization_trait]`,
//! whose own impls are checked against the trait as if it were the parent.
//!
//! rustc runs these checks in a crate with `#![feature(min_specialization)]` and lets a
//! specializing impl add a bound on a `#[rustc_unsafe_specialization_marker]` trait, which is
//! unsound (`Copy`, `FusedIterator`). blaze's subset refuses those and allows only `default fn`
//! to be specialized; docs/v2/plan/design/specialization.md has the design and the
//! prior art. Codegen picks the specialized impl through `hir_ty::specialized_ancestor`
//! and the solver's preference for a specializing candidate, as rustc does; nothing here
//! changes that choice, it only refuses impls outside the rule.

#![forbid(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

use std::fmt;

use base_db::Db;
use blaze_mem::frame::Frame;
use hir_def::{
    AssocItemId, HasModule as _, ImplId, TraitId,
    attrs::AttrFlags,
    signatures::{
        ConstFlags, ConstSignature, ImplFlags, ImplSignature, TraitFlags, TraitSignature,
        TypeAliasFlags, TypeAliasSignature,
    },
};
use hir_ty::{
    GenericPredicates, Span, SpecializationMode,
    db::HirDatabase as _,
    display::{DisplayTarget, HirDisplay as _},
    next_solver::{
        Clause, ClauseKind, DbInterner, GenericArg, GenericArgKind, GenericArgs, Region, TraitRef,
        Ty, TyKind, TypingMode,
        infer::{
            DbInternerInferExt as _,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    },
    specialization_mode, specialized_ancestor,
};
use rustc_type_ir::{
    AliasTyKind, ClausePolarity, ConstKind, EarlyBinder, OutlivesClause, RegionKind, TraitClause,
    TypeSuperVisitable, TypeVisitable, TypeVisitableExt as _, TypeVisitor, Upcast as _,
    inherent::{GenericArgs as _, IntoKind as _},
};

#[cfg(test)]
mod tests;

type FVec<'f, T> = Vec<T, &'f Frame>;

/// What a specializing impl is checked against: the impl it specializes, or for an impl of an
/// always-applicable trait with no such impl, the trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Parent {
    Impl(ImplId),
    Trait(TraitId),
}

/// Which rules an impl is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rules {
    /// rustc's `check_min_specialization`, in a `#![feature(min_specialization)]` crate.
    Rustc,
    /// blaze's sound subset: a `#![blaze::specialization]` crate, or an impl of a
    /// `#[blaze::specialization_trait]` trait in any crate.
    Blaze,
}

/// The rule a refused impl breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A specializing impl with no item of its own (rustc's `EmptySpecialization`).
    EmptySpecialization,
    /// The parent's generic parameters, equated with this impl's header, name `'static`.
    StaticLifetime,
    /// A parameter of this impl appears twice among the parent's parameters.
    RepeatedParameter,
    /// The parent's generic parameters are not determined by this impl's header.
    UnresolvedParentArgs,
    /// A bound the parent lacks, on a trait that is not always applicable.
    SpecializesOnTrait,
    /// A bound the parent lacks, on what an associated type is.
    SpecializesOnProjection,
    /// A bound the parent lacks, of another kind (an outlives bound, ..).
    SpecializesOnPredicate,
    /// blaze: a bound the parent lacks, on a `#[rustc_unsafe_specialization_marker]` trait,
    /// which rustc allows and which is unsound.
    UnsafeMarker,
    /// blaze: `default type` or `default const`; only `default fn` may be specialized.
    DefaultItem,
    /// blaze: a `default impl`.
    DefaultImpl,
    /// An impl of a `#[rustc_specialization_trait]` trait in a crate with neither feature.
    UnstableAlwaysApplicableImpl,
    /// E0520: an item overrides one an ancestor impl does not mark `default` (rustc's
    /// `check_specialization_validity`), checked in every mode.
    NotMarkedDefault,
}

impl Rule {
    /// The rule's name, as the error shows it.
    pub fn name(self) -> &'static str {
        match self {
            Rule::EmptySpecialization => "a specializing impl specializes an item",
            Rule::StaticLifetime => "always applicable: no `'static` the parent is generic over",
            Rule::RepeatedParameter => "always applicable: no parameter repeated",
            Rule::UnresolvedParentArgs => "always applicable: the header determines the parent",
            Rule::SpecializesOnTrait
            | Rule::SpecializesOnProjection
            | Rule::SpecializesOnPredicate => {
                "always applicable: a bound the parent lacks is on an always-applicable trait"
            }
            Rule::UnsafeMarker => {
                "blaze specialization: no `#[rustc_unsafe_specialization_marker]` bound"
            }
            Rule::DefaultItem => "blaze specialization: only `default fn` is specialized",
            Rule::DefaultImpl => "blaze specialization: no `default impl`",
            Rule::UnstableAlwaysApplicableImpl => "`min_specialization` gates the trait's impls",
            Rule::NotMarkedDefault => "only a `default` item is specialized",
        }
    }

    /// rustc's error code for the rule, `""` where rustc gives none.
    pub fn code(self) -> &'static str {
        match self {
            Rule::NotMarkedDefault => "E0520",
            _ => "",
        }
    }

    /// Whether the rule is blaze's own, outside rustc: only `#![blaze::specialization]` and
    /// `#[blaze::specialization_trait]` code meets it.
    pub fn is_blaze_extension(self) -> bool {
        matches!(self, Rule::UnsafeMarker | Rule::DefaultItem | Rule::DefaultImpl)
    }
}

/// An impl refused by a specialization rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpecializationError {
    /// The refused impl.
    pub impl_: ImplId,
    /// What it was checked against, when the rule is about that.
    pub parent: Option<Parent>,
    /// The rules that refused it; rustc's for a rustc check that runs in every mode (E0520).
    pub rules: Rules,
    pub rule: Rule,
    /// The item the error is at, when it is about one (E0520's overriding item).
    pub item: Option<AssocItemId>,
    /// The error, naming the impl, its parent and the rule; a blaze-only rule says so.
    pub message: String,
    /// rustc's note, if it gives one.
    pub note: Option<String>,
}

/// Which rules `impl_id` is held to, if any: blaze's when its crate says
/// `#![blaze::specialization]` or its trait is a `#[blaze::specialization_trait]`, rustc's in a
/// `min_specialization` crate.
pub fn rules_of(db: &Db, impl_id: ImplId) -> Option<Rules> {
    let trait_ = db.impl_trait(impl_id)?.skip_binder().def_id.0;
    let blaze_trait =
        TraitSignature::of(db, trait_).flags.contains(TraitFlags::BLAZE_SPECIALIZATION_TRAIT);
    match specialization_mode(db, impl_id.krate(db)) {
        SpecializationMode::Blaze => Some(Rules::Blaze),
        _ if blaze_trait => Some(Rules::Blaze),
        SpecializationMode::Rustc => Some(Rules::Rustc),
        SpecializationMode::Unchecked | SpecializationMode::Off => None,
    }
}

/// The specialization errors of `impl_id`, a trait impl; none for an inherent impl.
pub fn check_impl(db: &Db, impl_id: ImplId) -> Vec<SpecializationError> {
    let _span =
        tracing::info_span!(target: "blaze", "specialization_check", impl_ = ?impl_id).entered();
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let mut found = Vec::new_in(frame);
        Check { db, frame, impl_id, found: &mut found }.run();
        // alloc: the caller's result; empty, so no block, unless an impl is refused
        found.drain(..).collect()
    })
}

struct Check<'a, 'f, 'db> {
    db: &'db Db,
    frame: &'f Frame,
    impl_id: ImplId,
    found: &'a mut FVec<'f, SpecializationError>,
}

impl<'f, 'db> Check<'_, 'f, 'db> {
    fn run(&mut self) {
        let db = self.db;
        let Some(trait_ref) = db.impl_trait(self.impl_id) else { return };
        let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
        if trait_ref.references_error() {
            return;
        }
        let trait_ = trait_ref.def_id.0;
        let trait_flags = TraitSignature::of(db, trait_).flags;
        let always_applicable = trait_flags.contains(TraitFlags::SPECIALIZATION_ALWAYS_APPLICABLE);
        let mode = specialization_mode(db, self.impl_id.krate(db));
        // rustc's `check_specialization_validity` runs for every trait impl, whatever the
        // features: an impl that specializes nothing has no ancestor to check.
        self.not_marked_default();
        let Some(rules) = rules_of(db, self.impl_id) else {
            // rustc's `enforce_trait_manually_implementable`.
            if mode == SpecializationMode::Off && always_applicable {
                self.push(
                    None,
                    Rules::Rustc,
                    Rule::UnstableAlwaysApplicableImpl,
                    format_args!("implementing `rustc_specialization_trait` traits is unstable"),
                );
            }
            return;
        };
        if rules == Rules::Blaze && mode == SpecializationMode::Blaze {
            self.default_items();
        }
        // rustc's `parent_specialization_node`: overlapping `#[marker]` impls are not
        // specializations, and an impl of a trait that is not always applicable that
        // specializes no impl is no specialization either.
        if AttrFlags::query(db, hir_def::AttrDefId::TraitId(trait_)).contains(AttrFlags::MARKER) {
            return;
        }
        let parent = match specialized_ancestor(db, self.impl_id, |_| true) {
            Some(parent) => Parent::Impl(parent),
            None if always_applicable => Parent::Trait(trait_),
            None => return,
        };
        self.always_applicable(rules, parent, trait_ref);
    }

    /// blaze's subset specializes only functions: no `default impl`, `default type` or
    /// `default const`.
    fn default_items(&mut self) {
        let db = self.db;
        if ImplSignature::of(db, self.impl_id).flags.contains(ImplFlags::DEFAULT) {
            self.push(
                None,
                Rules::Blaze,
                Rule::DefaultImpl,
                format_args!("a `default impl` is outside blaze's sound specialization"),
            );
        }
        for (name, item) in &self.impl_id.impl_items(db).items {
            let what = match *item {
                AssocItemId::TypeAliasId(it)
                    if TypeAliasSignature::of(db, it)
                        .flags
                        .contains(TypeAliasFlags::IS_DEFAULT) =>
                {
                    "type"
                }
                AssocItemId::ConstId(it)
                    if ConstSignature::of(db, it).flags.contains(ConstFlags::IS_DEFAULT) =>
                {
                    "const"
                }
                _ => continue,
            };
            let name = name.display(db, self.impl_id.krate(db).data(db).edition);
            self.push(
                None,
                Rules::Blaze,
                Rule::DefaultItem,
                format_args!(
                    "`default {what} {name}` is outside blaze's sound specialization, which \
                     specializes only `default fn`"
                ),
            );
        }
    }

    /// rustc's `check_always_applicable`.
    fn always_applicable(&mut self, rules: Rules, parent: Parent, trait_ref: TraitRef<'db>) {
        let db = self.db;
        let interner = DbInterner::new_with(db, self.impl_id.krate(db));
        let Some(parent_args) = self.parent_args(rules, parent, trait_ref) else { return };

        // `check_has_items`
        if let Parent::Impl(_) = parent
            && self.impl_id.impl_items(db).items.is_empty()
        {
            self.push(
                Some(parent),
                rules,
                Rule::EmptySpecialization,
                format_args!("specialization impl does not specialize any associated items"),
            );
        }

        // The parameters of the parent its own where clauses do not determine.
        let unconstrained = match parent {
            Parent::Trait(_) => {
                let mut all = Vec::new_in(self.frame);
                all.extend(parent_args.iter());
                all
            }
            Parent::Impl(parent) => self.unconstrained_parent_args(interner, parent, parent_args),
        };

        // `check_static_lifetimes`
        if unconstrained.iter().any(|arg| names_static(*arg)) {
            self.push(
                Some(parent),
                rules,
                Rule::StaticLifetime,
                format_args!("cannot specialize on `'static` lifetime"),
            );
        }

        // `check_duplicate_params`
        let mut params = Vec::new_in(self.frame);
        for arg in &unconstrained {
            parameters_for(*arg, true, &mut params);
        }
        params.sort_unstable();
        if let Some(window) = params.windows(2).find(|it| it[0] == it[1]) {
            let identity = GenericArgs::identity_for_item(interner, self.impl_id.into());
            let target = self.target();
            match identity.as_slice().get(window[0] as usize) {
                Some(param) => self.push(
                    Some(parent),
                    rules,
                    Rule::RepeatedParameter,
                    format_args!(
                        "specializing impl repeats parameter `{}`",
                        param.display(db, target)
                    ),
                ),
                None => self.push(
                    Some(parent),
                    rules,
                    Rule::RepeatedParameter,
                    format_args!("specializing impl repeats parameter #{}", window[0]),
                ),
            }
        }

        self.predicates(interner, rules, parent, parent_args, trait_ref);
    }

    /// rustc's `get_impl_args`: the parent's generic arguments in terms of this impl's
    /// parameters, from equating the trait refs and proving the parent's where clauses.
    fn parent_args(
        &mut self,
        rules: Rules,
        parent: Parent,
        trait_ref: TraitRef<'db>,
    ) -> Option<GenericArgs<'db>> {
        let Parent::Impl(parent_impl) = parent else { return Some(trait_ref.args) };
        let db = self.db;
        let interner = DbInterner::new_with(db, self.impl_id.krate(db));
        let param_env = db.trait_environment(self.impl_id.into());
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let mut ocx = ObligationCtxt::new(&infcx);
        let cause = ObligationCause::dummy();
        let fresh = infcx.fresh_args_for_item(Span::Dummy, parent_impl.into());
        let parent_trait_ref =
            db.impl_trait(parent_impl)?.instantiate(interner, fresh).skip_norm_wip();
        let equated = ocx.eq(&cause, param_env, trait_ref, parent_trait_ref).is_ok();
        if equated {
            for clause in GenericPredicates::query_all(db, parent_impl.into())
                .iter_instantiated(interner, fresh.as_slice())
            {
                ocx.register_obligation(Obligation::new(
                    interner,
                    cause,
                    param_env,
                    clause.skip_norm_wip(),
                ));
            }
        }
        let proven = equated && ocx.evaluate_obligations_error_on_ambiguity().is_empty();
        let resolved = infcx.deeply_resolve_via_unification_table(fresh);
        // A region of the parent no equality determines is left a variable. rustc's lexical
        // region resolution picks one; blaze refuses it under its own rules, and under rustc's
        // the variable is neither a parameter nor `'static`, so only a bound naming it can
        // fail to match.
        let unresolved = resolved.has_non_region_infer()
            || (rules == Rules::Blaze && resolved.has_infer_regions());
        if !proven || unresolved {
            self.push(
                Some(parent),
                rules,
                Rule::UnresolvedParentArgs,
                format_args!("could not resolve generic parameters on overridden impl"),
            );
            return None;
        }
        Some(resolved)
    }

    /// rustc's `unconstrained_parent_impl_args`: `args` of `parent` but those its where
    /// clauses' projections constrain (`S` of `impl<S, I: IntoIterator<Item = S>>`).
    fn unconstrained_parent_args(
        &self,
        interner: DbInterner<'db>,
        parent: ImplId,
        args: GenericArgs<'db>,
    ) -> FVec<'f, GenericArg<'db>> {
        let (db, frame) = (self.db, self.frame);
        let parent_trait_ref =
            db.impl_trait(parent).map(|it| it.instantiate_identity().skip_norm_wip());
        let mut unconstrained = Vec::new_in(frame);
        let mut constrained = Vec::new_in(frame);
        let mut params = Vec::new_in(frame);
        for clause in GenericPredicates::query_explicit(db, parent.into()).iter_identity() {
            let ClauseKind::Projection(projection) = clause.skip_norm_wip().kind().skip_binder()
            else {
                continue;
            };
            if Some(projection.projection_term.trait_ref(interner)) == parent_trait_ref {
                continue;
            }
            parameters_for(projection.projection_term, true, &mut unconstrained);
            params.clear();
            parameters_for(projection.term, false, &mut params);
            constrained.extend(params.iter().filter(|it| !unconstrained.contains(it)));
            parameters_for(projection.term, true, &mut unconstrained);
        }
        let mut out = Vec::new_in(frame);
        out.extend(
            args.iter()
                .enumerate()
                .filter(|&(index, _)| !constrained.contains(&(index as u32)))
                .map(|(_, arg)| arg),
        );
        out
    }

    /// rustc's `check_predicates`: each where clause of this impl is global, the parent's, a
    /// well-formedness requirement of its header, or on an always-applicable trait.
    fn predicates(
        &mut self,
        interner: DbInterner<'db>,
        rules: Rules,
        parent: Parent,
        parent_args: GenericArgs<'db>,
        trait_ref: TraitRef<'db>,
    ) {
        let db = self.db;
        let mut own = Vec::new_in(self.frame);
        own.extend(rustc_type_ir::elaborate::elaborate(
            interner,
            GenericPredicates::query_explicit(db, self.impl_id.into())
                .iter_identity()
                .map(|it| it.skip_norm_wip()),
        ));
        let mut assumed = Vec::new_in(self.frame);
        if let Parent::Impl(parent) = parent {
            assumed.extend(rustc_type_ir::elaborate::elaborate(
                interner,
                GenericPredicates::query_explicit(db, parent.into())
                    .iter_instantiated(interner, parent_args.as_slice())
                    .map(|it| it.skip_norm_wip()),
            ));
        }
        // What the header's types need to be well-formed.
        let sized = hir_def::lang_item::lang_items(db, self.impl_id.krate(db)).Sized;
        let mut wf = Vec::new_in(self.frame);
        for arg in trait_ref.args.iter() {
            if let GenericArgKind::Type(ty) = arg.kind() {
                wf_clauses(db, interner, sized, ty, &mut wf, 0);
            }
        }
        wf.retain(|it: &Clause<'db>| !it.has_escaping_bound_vars());
        assumed.extend(rustc_type_ir::elaborate::elaborate(interner, wf.iter().copied()));
        // An always-applicable trait's supertraits hold where it does, as its impls assume
        // nothing.
        let mut always = Vec::new_in(self.frame);
        always.extend(
            own.iter().copied().filter(|it| trait_kind(db, *it) == Some(Kind::AlwaysApplicable)),
        );
        assumed.extend(rustc_type_ir::elaborate::elaborate(interner, always.iter().copied()));

        for &clause in &own {
            if assumed.contains(&clause) || clause.is_global() {
                continue;
            }
            let target = self.target();
            match clause.kind().skip_binder() {
                ClauseKind::Trait(predicate) => match trait_kind(db, clause) {
                    Some(Kind::UnsafeMarker) if rules == Rules::Rustc => (),
                    Some(Kind::UnsafeMarker) => self.push(
                        Some(parent),
                        rules,
                        Rule::UnsafeMarker,
                        format_args!(
                            "cannot specialize on trait `{}`: a \
                             `#[rustc_unsafe_specialization_marker]` trait is not always \
                             applicable",
                            predicate.trait_ref.display(db, target)
                        ),
                    ),
                    _ => self.push(
                        Some(parent),
                        rules,
                        Rule::SpecializesOnTrait,
                        format_args!(
                            "cannot specialize on trait `{}`",
                            predicate.trait_ref.display(db, target)
                        ),
                    ),
                },
                ClauseKind::Projection(projection) => self.push(
                    Some(parent),
                    rules,
                    Rule::SpecializesOnProjection,
                    format_args!(
                        "cannot specialize on associated type `{} == {}`",
                        projection
                            .projection_term
                            .to_term(interner, rustc_type_ir::IsRigid::No)
                            .display(db, target),
                        projection.term.display(db, target)
                    ),
                ),
                // rustc allows every `ConstArgHasType`, which std's `const N: usize`
                // specializing impls need.
                ClauseKind::ConstArgHasType(..) => (),
                _ => self.push(
                    Some(parent),
                    rules,
                    Rule::SpecializesOnPredicate,
                    format_args!(
                        "cannot specialize on predicate `{}`",
                        ClauseText(db, target, clause)
                    ),
                ),
            }
        }
    }

    fn target(&self) -> DisplayTarget {
        DisplayTarget::from_crate(self.db, self.impl_id.krate(self.db))
    }

    /// `impl Trait<..> for Self`, as the error names it.
    fn impl_text(&self, impl_id: ImplId) -> String {
        let db = self.db;
        let target = self.target();
        let self_ty = db.impl_self_ty(impl_id).instantiate_identity().skip_norm_wip();
        match db.impl_trait(impl_id) {
            Some(trait_ref) => format!(
                "impl {} for {}",
                trait_ref.instantiate_identity().skip_norm_wip().display(db, target),
                self_ty.display(db, target)
            ),
            None => format!("impl {}", self_ty.display(db, target)),
        }
    }

    fn push(
        &mut self,
        parent: Option<Parent>,
        rules: Rules,
        rule: Rule,
        message: fmt::Arguments<'_>,
    ) {
        let db = self.db;
        let this = self.impl_text(self.impl_id);
        let against = match parent {
            Some(Parent::Impl(parent)) => {
                format!(", which specializes `{}`", self.impl_text(parent))
            }
            Some(Parent::Trait(trait_)) => format!(
                ", checked against the always-applicable trait `{}`",
                TraitSignature::of(db, trait_)
                    .name
                    .display(db, self.impl_id.krate(db).data(db).edition)
            ),
            None => String::new(),
        };
        let extension = if rule.is_blaze_extension() { "blaze extension, " } else { "" };
        // alloc: the error's own text, once per refused rule
        let message = format!("{message} (in `{this}`{against}; {extension}rule: {})", rule.name());
        self.found.push(SpecializationError {
            impl_: self.impl_id,
            parent,
            rules,
            rule,
            item: None,
            message,
            note: None,
        });
    }

    /// rustc's `check_specialization_validity` (E0520) for each item of the impl: walking up
    /// the impls it specializes, the first that has the item must mark it `default` (or be a
    /// `default impl`); one that leaves it out passes it on only if it is a `default impl`.
    fn not_marked_default(&mut self) {
        let db = self.db;
        let items = &self.impl_id.impl_items(db).items;
        if items.is_empty() {
            return;
        }
        // The impl's ancestors, nearest first: rustc's `ancestors(..).skip(1)` but the trait.
        // Each is an impl the one before specializes, so the walk ends at an impl that
        // specializes none; one already seen (a cycle only an error makes) ends it too.
        let mut ancestors = Vec::new_in(self.frame);
        let mut at = self.impl_id;
        while let Some(parent) = specialized_ancestor(db, at, |_| true) {
            if parent == self.impl_id || ancestors.contains(&parent) {
                break;
            }
            ancestors.push(parent);
            at = parent;
        }
        if ancestors.is_empty() {
            return;
        }
        for &(name, item) in items {
            let found = ancestors.iter().find_map(|&ancestor| {
                let impl_default =
                    ImplSignature::of(db, ancestor).flags.contains(ImplFlags::DEFAULT);
                let theirs = ancestor.impl_items(db).items.iter().find(|(other, it)| {
                    *other == name && std::mem::discriminant(it) == std::mem::discriminant(&item)
                });
                match theirs {
                    Some(&(_, it)) if !impl_default && !item_is_default(db, it) => {
                        Some(Err(ancestor))
                    }
                    Some(_) => Some(Ok(())),
                    None if impl_default => None,
                    None => Some(Err(ancestor)),
                }
            });
            let Some(Err(ancestor)) = found else { continue };
            let edition = self.impl_id.krate(db).data(db).edition;
            let name = name.display(db, edition);
            let this = self.impl_text(self.impl_id);
            let parent = self.impl_text(ancestor);
            // alloc: the error's own text, once per refused item
            let message = format!(
                "`{name}` specializes an item from a parent `impl`, but that item is not marked \
                 `default` (in `{this}`, whose parent `{parent}` has `{name}` final; rule: {})",
                Rule::NotMarkedDefault.name()
            );
            let note = match ancestor.krate(db) == self.impl_id.krate(db) {
                true => {
                    format!("to specialize, `{name}` in the parent `impl` must be marked `default`")
                }
                false => format!(
                    "parent implementation is in crate `{}`",
                    ancestor
                        .krate(db)
                        .extra_data(db)
                        .display_name
                        .as_ref()
                        .map_or_else(|| "?".to_owned(), |it| it.to_string())
                ),
            };
            self.found.push(SpecializationError {
                impl_: self.impl_id,
                parent: Some(Parent::Impl(ancestor)),
                rules: Rules::Rustc,
                rule: Rule::NotMarkedDefault,
                item: Some(item),
                message,
                note: Some(note),
            });
        }
    }
}

/// Whether an impl's item is written `default`.
fn item_is_default(db: &Db, item: AssocItemId) -> bool {
    match item {
        AssocItemId::FunctionId(it) => {
            hir_def::signatures::FunctionSignature::of(db, it).is_default()
        }
        AssocItemId::ConstId(it) => {
            ConstSignature::of(db, it).flags.contains(ConstFlags::IS_DEFAULT)
        }
        AssocItemId::TypeAliasId(it) => {
            TypeAliasSignature::of(db, it).flags.contains(TypeAliasFlags::IS_DEFAULT)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    AlwaysApplicable,
    UnsafeMarker,
}

/// rustc's `trait_specialization_kind` of a trait clause.
fn trait_kind(db: &Db, clause: Clause<'_>) -> Option<Kind> {
    let ClauseKind::Trait(predicate) = clause.kind().skip_binder() else { return None };
    let flags = TraitSignature::of(db, predicate.trait_ref.def_id.0).flags;
    if flags.contains(TraitFlags::SPECIALIZATION_ALWAYS_APPLICABLE) {
        Some(Kind::AlwaysApplicable)
    } else if flags.contains(TraitFlags::SPECIALIZATION_UNSAFE_MARKER) {
        Some(Kind::UnsafeMarker)
    } else {
        None
    }
}

/// Whether `arg` names `'static` outside a binder (rustc's `any_free_region_meets(..,
/// is_static)`).
fn names_static(arg: GenericArg<'_>) -> bool {
    struct Static;
    impl<'db> TypeVisitor<DbInterner<'db>> for Static {
        type Result = std::ops::ControlFlow<()>;
        fn visit_region(&mut self, region: Region<'db>) -> Self::Result {
            match region.kind() {
                RegionKind::ReStatic => std::ops::ControlFlow::Break(()),
                _ => std::ops::ControlFlow::Continue(()),
            }
        }
    }
    arg.visit_with(&mut Static).is_break()
}

/// The indices of the parameters in `value`, each as often as it appears: rustc's
/// `parameters_for`. Those in an associated type or an unevaluated constant only with
/// `include_nonconstraining`.
fn parameters_for<'db>(
    value: impl TypeVisitable<DbInterner<'db>>,
    include_nonconstraining: bool,
    out: &mut FVec<'_, u32>,
) {
    struct Collect<'a, 'f> {
        include_nonconstraining: bool,
        out: &'a mut FVec<'f, u32>,
    }
    impl<'db> TypeVisitor<DbInterner<'db>> for Collect<'_, '_> {
        type Result = ();
        fn visit_ty(&mut self, ty: Ty<'db>) {
            match ty.kind() {
                TyKind::Alias(_, alias)
                    if !self.include_nonconstraining
                        && matches!(
                            alias.kind,
                            AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }
                        ) =>
                {
                    return;
                }
                TyKind::Param(param) => self.out.push(param.index),
                _ => (),
            }
            ty.super_visit_with(self)
        }
        fn visit_region(&mut self, region: Region<'db>) {
            if let RegionKind::ReEarlyParam(param) = region.kind() {
                self.out.push(param.index);
            }
        }
        fn visit_const(&mut self, konst: hir_ty::next_solver::Const<'db>) {
            match konst.kind() {
                ConstKind::Alias(..) if !self.include_nonconstraining => return,
                ConstKind::Param(param) => self.out.push(param.index),
                _ => (),
            }
            konst.super_visit_with(self)
        }
    }
    value.visit_with(&mut Collect { include_nonconstraining, out });
}

/// What `ty` needs to be well-formed, as clauses (rustc's `wf::obligations` but the
/// `WellFormed` ones, which no where clause is): an ADT's where clauses and inferred outlives
/// bounds, `T: 'a` of `&'a T`, `T: Sized` of `[T]`, `[T; N]` and a tuple's leading fields, the
/// trait of an associated type.
fn wf_clauses<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    sized: Option<TraitId>,
    ty: Ty<'db>,
    out: &mut FVec<'_, Clause<'db>>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let sized_clause = |ty: Ty<'db>| -> Option<Clause<'db>> {
        let trait_ref = TraitRef::new(interner, sized?.into(), [ty]);
        Some(TraitClause { trait_ref, polarity: ClausePolarity::Positive }.upcast(interner))
    };
    let args = |args: GenericArgs<'db>, out: &mut FVec<'_, Clause<'db>>| {
        for arg in args.iter() {
            if let GenericArgKind::Type(ty) = arg.kind() {
                wf_clauses(db, interner, sized, ty, out, depth + 1);
            }
        }
    };
    match ty.kind() {
        TyKind::Ref(region, inner, _) => {
            out.push(OutlivesClause(inner, region).upcast(interner));
            wf_clauses(db, interner, sized, inner, out, depth + 1);
        }
        TyKind::Adt(def, adt_args) => {
            let adt = def.def_id();
            out.extend(
                GenericPredicates::query_explicit(db, adt.into())
                    .iter_instantiated(interner, adt_args.as_slice())
                    .map(|it| it.skip_norm_wip()),
            );
            for OutlivesClause(arg, region) in hir_ty_wfcheck::Inferred::default().of(db, adt) {
                let OutlivesClause(arg, region) =
                    EarlyBinder::bind(interner, OutlivesClause(arg, region))
                        .instantiate(interner, adt_args)
                        .skip_norm_wip();
                match arg.kind() {
                    GenericArgKind::Type(ty) => {
                        out.push(OutlivesClause(ty, region).upcast(interner))
                    }
                    GenericArgKind::Lifetime(longer) => {
                        out.push(OutlivesClause(longer, region).upcast(interner))
                    }
                    GenericArgKind::Const(_) => (),
                }
            }
            args(adt_args, out);
        }
        TyKind::Tuple(tys) => {
            let len = tys.len();
            for (index, it) in tys.iter().enumerate() {
                if index + 1 < len {
                    out.extend(sized_clause(it));
                }
                wf_clauses(db, interner, sized, it, out, depth + 1);
            }
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) => {
            out.extend(sized_clause(inner));
            wf_clauses(db, interner, sized, inner, out, depth + 1);
        }
        TyKind::RawPtr(inner, _) => wf_clauses(db, interner, sized, inner, out, depth + 1),
        TyKind::FnPtr(sig, _) => {
            for it in sig.skip_binder().inputs_and_output.iter() {
                wf_clauses(db, interner, sized, it, out, depth + 1);
            }
        }
        TyKind::Alias(_, alias) => {
            if matches!(alias.kind, AliasTyKind::Projection { .. }) {
                out.push(
                    TraitClause {
                        trait_ref: alias.trait_ref(interner),
                        polarity: ClausePolarity::Positive,
                    }
                    .upcast(interner),
                );
            }
            args(alias.args, out);
        }
        _ => (),
    }
}

/// A clause as an error shows it.
struct ClauseText<'a, 'db>(&'a Db, DisplayTarget, Clause<'db>);

impl fmt::Display for ClauseText<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ClauseText(db, target, clause) = *self;
        match clause.kind().skip_binder() {
            ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {
                write!(f, "{}: {}", ty.display(db, target), region.display(db, target))
            }
            ClauseKind::RegionOutlives(OutlivesClause(longer, region)) => {
                write!(f, "{}: {}", longer.display(db, target), region.display(db, target))
            }
            // `Debug` names the clause's items only through the attached database.
            _ => blaze_query::db::attach(db, || write!(f, "{clause:?}")),
        }
    }
}
