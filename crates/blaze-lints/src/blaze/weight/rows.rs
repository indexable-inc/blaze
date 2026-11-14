//! What the workspace does with each item, as use rows folded per crate: the facts the lints of
//! [`super`] read about an item every crate of the workspace may use (how many places read a
//! field, construct or match a variant, call or name a function, which constants each argument
//! of a function gets, which impls a trait has).
//!
//! The design of docs/v2/plan/design/end-state/check-test.md's "whole-crate lints as per-item
//! use rows with folds": [`body_uses`] is one body's rows, a tracked query of its own so that an
//! edit of one body reruns one body's walk; [`crate_uses`] folds a crate's bodies and items; a
//! lint reads an item's facts as the sum of the folds of the crates that can name it
//! ([`readers`]): the crates of every configuration of the item's own crate (its lib, its tests)
//! and the workspace crates that depend on any of them.
//!
//! An item is known by where it is written ([`Subject`]): the file and the item's id in that
//! file's AST map, the same in every configuration of a crate, where the definition ids of a
//! crate's lib and of its tests differ. An item a macro made has no subject and no facts.
//!
//! The rule: a fact is a count of places, summed over crates, so the sum does not depend on the
//! order crates are read in; an argument's constant is a join (unseen, one constant, varies)
//! that does not either. A body is counted in one role ([`Role`]): production code, test code,
//! or an impl of a serde trait (the place a value reaches disk or the wire).

use std::alloc::Global;

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use either::Either;
use hir::HasAttrs as _;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FieldId, FunctionId, HasModule as _,
    ImplId, ItemContainerId, Lookup as _, ModuleDefId, TraitId, VariantId,
    expr_store::Body,
    hir::{BinaryOp, CmpOp, Expr, ExprId, Literal, Pat, UnaryOp},
    nameres::crate_def_map,
    resolver::ValueNs,
    signatures::VariantFields,
};
use hir_expand::{AstId, HirFileId};
use hir_ty::{InferenceResult, method_resolution::CandidateId, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;

use crate::{blaze::facts as type_facts, check};

/// An item, a field or a variant, by where it is written: its file, its id in the file's AST
/// map and, for a field, its place in its struct or variant plus one (zero for the rest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Subject {
    file: HirFileId,
    ast: span::ErasedFileAstId,
    part: u32,
}

impl Subject {
    /// The subject of the item at `id`, when a file, not a macro, writes it.
    fn at<N>(id: AstId<N>, part: u32) -> Option<Subject> {
        id.file_id.file_id()?;
        Some(Subject { file: id.file_id, ast: id.value.erase(), part })
    }

    pub(crate) fn of_fn(db: &Db, f: FunctionId) -> Option<Subject> {
        Subject::at(f.lookup(db).id, 0)
    }

    pub(crate) fn of_trait(db: &Db, t: TraitId) -> Option<Subject> {
        Subject::at(t.lookup(db).id, 0)
    }

    pub(crate) fn of_adt(db: &Db, adt: AdtId) -> Option<Subject> {
        match adt {
            AdtId::StructId(it) => Subject::at(it.lookup(db).id, 0),
            AdtId::UnionId(it) => Subject::at(it.lookup(db).id, 0),
            AdtId::EnumId(it) => Subject::at(it.lookup(db).id, 0),
        }
    }

    pub(crate) fn of_variant(db: &Db, variant: VariantId) -> Option<Subject> {
        match variant {
            VariantId::EnumVariantId(it) => Subject::at(it.lookup(db).id, 0),
            VariantId::StructId(it) => Subject::of_adt(db, AdtId::StructId(it)),
            VariantId::UnionId(it) => Subject::of_adt(db, AdtId::UnionId(it)),
        }
    }

    /// The field at place `index` of `variant`.
    pub(crate) fn of_field(db: &Db, variant: VariantId, index: u32) -> Option<Subject> {
        let parent = Subject::of_variant(db, variant)?;
        Some(Subject { part: index + 1, ..parent })
    }

    fn of_field_id(db: &Db, field: FieldId) -> Option<Subject> {
        Subject::of_field(db, field.parent, field.local_id.into_raw().into_u32())
    }

    fn of_impl(db: &Db, imp: ImplId) -> Option<Subject> {
        Subject::at(imp.lookup(db).id, 0)
    }
}

/// The role of a body, which the lints weigh its uses by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    /// Production code.
    Plain = 0,
    /// A test, a bench, an example, or code under `#[cfg(test)]`.
    Test = 1,
    /// An impl of a serde trait: what a value is written to and read from disk or the wire by.
    Stored = 2,
}

const ROLES: usize = 3;

/// What a place does with a subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Way {
    /// A field read: `x.f`, a pattern binding it, a compound assignment to it.
    Read = 0,
    /// A field written: a struct literal, `x.f = ..`.
    Write = 1,
    /// A variant or struct constructed.
    Construct = 2,
    /// A variant named by a pattern.
    Match = 3,
    /// A function called.
    Call = 4,
    /// A function named as a value (a function pointer, a callback) rather than called.
    Named = 5,
    /// An enum's values compared with `==`, `!=` or cast with `as`: its variants are told
    /// apart without a pattern.
    Compare = 6,
    /// A trait bound of an ADT's type parameter: a strategy chosen by type (rule 78).
    Bound = 7,
}

const WAYS: usize = 8;

/// A constant argument: a literal, or a `const`, unit variant or unit struct by where it is
/// written.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Constant {
    Literal(Literal),
    Item(Subject),
}

/// What one parameter of a function gets over every call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Arg {
    #[default]
    Unseen,
    Same(Constant),
    Varies,
}

impl Arg {
    fn join(&mut self, seen: Option<&Constant>) {
        *self = match (&*self, seen) {
            (Arg::Unseen, Some(it)) => Arg::Same(it.clone()), // clone: a constant, a literal's few bytes
            (Arg::Same(had), Some(it)) if had == it => return,
            _ => Arg::Varies,
        };
    }

    fn merge(&mut self, other: &Arg) {
        match other {
            Arg::Unseen => {}
            Arg::Same(it) => self.join(Some(it)),
            Arg::Varies => *self = Arg::Varies,
        }
    }
}

/// The facts of one subject in a crate, or summed over crates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The places of each role that use the subject each way.
    ways: [[u32; WAYS]; ROLES],
    /// For a function, what each parameter (after `self`) gets over its calls.
    pub(crate) args: Vec<Arg>,
    /// For a trait, the impls of it, each once.
    pub(crate) impls: Vec<Subject>,
    /// For a trait, whether an impl stands for more than one type: a blanket impl, an impl for
    /// a type of another crate (an extension trait), or a simulation's impl (rule 64's sim
    /// counts beside the real one).
    pub(crate) open: bool,
}

impl Facts {
    pub(crate) fn get(&self, role: Role, way: Way) -> u32 {
        self.ways[role as usize][way as usize]
    }

    /// The places of every role that use the subject `way`.
    pub(crate) fn all(&self, way: Way) -> u32 {
        self.ways.iter().map(|it| it[way as usize]).sum()
    }

    fn add(&mut self, role: Role, way: Way) {
        self.ways[role as usize][way as usize] += 1;
    }

    fn merge(&mut self, other: &Facts) {
        for (mine, theirs) in self.ways.iter_mut().zip(&other.ways) {
            for (a, b) in mine.iter_mut().zip(theirs) {
                *a += b;
            }
        }
        if self.args.len() < other.args.len() {
            self.args.resize(other.args.len(), Arg::Unseen);
        }
        for (mine, theirs) in self.args.iter_mut().zip(&other.args) {
            mine.merge(theirs);
        }
        for it in &other.impls {
            if !self.impls.contains(it) {
                self.impls.push(*it);
            }
        }
        self.open |= other.open;
    }
}

/// The facts of every subject a crate, or a body, uses.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fold {
    of: HashMap<Subject, Facts, Global>,
}

impl Fold {
    fn new() -> Fold {
        Fold { of: HashMap::new_in(Global) }
    }

    fn at(&mut self, subject: Subject) -> &mut Facts {
        self.of.entry(subject).or_default()
    }

    fn note(&mut self, subject: Option<Subject>, role: Role, way: Way) {
        if let Some(subject) = subject {
            self.at(subject).add(role, way);
        }
    }

    fn merge(&mut self, other: &Fold) {
        for (subject, facts) in &other.of {
            self.at(*subject).merge(facts);
        }
    }
}

/// The facts of `subject` over every crate that can name it from `home`, the crate linted.
pub(crate) fn facts(db: &Db, home: hir::Crate, subject: Subject) -> Facts {
    let mut out = Facts::default();
    for &krate in readers(db, home.base()) {
        if let Some(it) = crate_uses(db, krate).of.get(&subject) {
            out.merge(it);
        }
    }
    out
}

/// The workspace crates that can name an item of `home`: every configuration of `home` (the
/// local crates of its root file) and every local crate that depends on one of them.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn readers(db: &Db, home: base_db::Crate) -> Box<[base_db::Crate]> {
    let home = hir::Crate::from(home);
    let root = home.root_file(db);
    // alloc: the query's value and its seen set, once per crate
    let mut seen: HashSet<base_db::Crate, Global> = HashSet::new_in(Global);
    let mut out = Vec::new();
    for krate in hir::Crate::all(db) {
        if !krate.origin(db).is_local() || krate.root_file(db) != root {
            continue;
        }
        for reader in std::iter::once(krate).chain(krate.transitive_reverse_dependencies(db)) {
            if reader.origin(db).is_local() && seen.insert(reader.base()) {
                out.push(reader.base());
            }
        }
    }
    out.into_boxed_slice()
}

/// A crate's fold: the rows of each of its bodies (those a derive made included, which the
/// lint plan leaves out), its impls of traits and its ADTs' bounds.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_uses(db: &Db, krate: base_db::Crate) -> Fold {
    let mut fold = Fold::new();
    // alloc: the crate's bodies and modules, once per crate
    let mut defs = Vec::new();
    let mut scopes = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        check::bodies_of(db, module.scope(db), &mut defs);
        scopes.push(module.scope(db));
    }
    let mut next = 0;
    while let Some(&def) = defs.get(next) {
        next += 1;
        for (_, block) in Body::of(db, def).blocks(db) {
            for (_, module) in block.modules(db) {
                check::bodies_of(db, module.scope(db), &mut defs);
                scopes.push(module.scope(db));
            }
        }
        fold.merge(body_uses(db, def));
    }
    for scope in scopes {
        for imp in scope.impls() {
            impl_rows(db, imp, &mut fold);
        }
        for def in scope.declarations() {
            if let ModuleDefId::AdtId(adt) = def {
                bound_rows(db, adt, &mut fold);
            }
        }
    }
    fold
}

/// The row of an impl of a trait: the impl under the trait, and whether it is open.
fn impl_rows(db: &Db, imp: ImplId, fold: &mut Fold) {
    let hir_impl = hir::Impl::from(imp);
    let Some(trait_) = hir_impl.trait_(db) else { return };
    let Some(subject) = Subject::of_trait(db, trait_.into()) else { return };
    let Some(at) = Subject::of_impl(db, imp) else { return };
    let krate = hir_impl.module(db).krate(db);
    let self_ty = hir_impl.self_ty(db);
    let open = match self_ty.as_adt() {
        Some(adt) => adt.module(db).krate(db) != krate || is_sim(adt.name(db).as_str()),
        None => true,
    };
    let facts = fold.at(subject);
    if !facts.impls.contains(&at) {
        facts.impls.push(at);
    }
    facts.open |= open;
}

/// Whether a type's name says it is a simulation's: the sim side of a sans-IO core.
pub(crate) fn is_sim(name: &str) -> bool {
    name.starts_with("Sim") || name.ends_with("Sim") || name.contains("Simulat")
}

/// The rows of the traits bounding `adt`'s type parameters.
fn bound_rows(db: &Db, adt: AdtId, fold: &mut Fold) {
    let generic = hir::GenericDef::Adt(hir::Adt::from(adt));
    for param in generic.type_or_const_params(db) {
        let Some(param) = param.as_type_param(db) else { continue };
        for bound in param.trait_bounds(db) {
            fold.note(Subject::of_trait(db, bound.into()), Role::Plain, Way::Bound);
        }
    }
}

/// The role of the body of `def`.
pub(crate) fn role(db: &Db, def: DefWithBodyId) -> Role {
    let module = def.module(db);
    let krate = hir::Crate::from(def.krate(db));
    // By the manifest's target: a library's production bodies are `Plain` in its `--test`
    // build too, as in its plain build.
    if crate::blaze::test_code::is_test_target(db, krate) {
        return Role::Test;
    }
    let container = match def {
        DefWithBodyId::FunctionId(f) => {
            let func = hir::Function::from(f);
            if func.is_test(db) || func.is_bench(db) || func.attrs(db).cfgs(db).is_some_and(check::names_test) {
                return Role::Test;
            }
            Some(f.lookup(db).container)
        }
        DefWithBodyId::ConstId(c) => Some(c.lookup(db).container),
        _ => None,
    };
    let mut module = Some(hir::Module::from(module));
    while let Some(it) = module.take() {
        if it.attrs(db).cfgs(db).is_some_and(check::names_test) {
            return Role::Test;
        }
        module = it.parent(db);
    }
    if let Some(ItemContainerId::ImplId(imp)) = container
        && let Some(trait_) = hir::Impl::from(imp).trait_(db)
        && ["serde", "serde_core"].iter().any(|it| type_facts::crate_is(db, trait_.module(db).krate(db), it))
    {
        return Role::Stored;
    }
    Role::Plain
}

/// One body's rows.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn body_uses(db: &Db, def: DefWithBodyId) -> Fold {
    let mut fold = Fold::new();
    let role = role(db, def);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    // The expressions a call calls and those a plain assignment writes: neither is a use by
    // value of what it names.
    // alloc: two sets of one body's expressions
    let mut callees: HashSet<ExprId, Global> = HashSet::new_in(Global);
    let mut written: HashSet<ExprId, Global> = HashSet::new_in(Global);
    for (_, expr) in body.exprs() {
        match expr {
            Expr::Call { callee, .. } => {
                callees.insert(callee);
            }
            Expr::Assignment { target, .. } => {
                if let Pat::Expr(place) = body.pat(target) {
                    written.insert(place);
                }
            }
            _ => {}
        }
    }
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Field { .. } => {
                let Some(Either::Left(field)) = infer.field_resolution(id) else { continue };
                let way = if written.contains(&id) { Way::Write } else { Way::Read };
                fold.note(Subject::of_field_id(db, field), role, way);
            }
            Expr::RecordLit { fields, .. } => {
                let Some(variant) = infer.variant_resolution_for_expr(id) else { continue };
                fold.note(Subject::of_variant(db, variant), role, Way::Construct);
                let shape = VariantFields::of(db, variant);
                for field in fields {
                    let Some(local) = shape.field(&field.name) else { continue };
                    let index = local.into_raw().into_u32();
                    fold.note(Subject::of_field(db, variant, index), role, Way::Write);
                }
            }
            Expr::Call { callee, args } => {
                match infer.expr_ty(callee).kind() {
                    TyKind::FnDef(callable, _) => match callable.0 {
                        CallableDefId::FunctionId(f) => {
                            let skip = usize::from(hir::Function::from(f).has_self_param(db));
                            call_rows(db, infer, body, f, args.get(skip..).unwrap_or_default(), role, &mut fold);
                        }
                        CallableDefId::StructId(it) => {
                            fold.note(Subject::of_variant(db, it.into()), role, Way::Construct);
                        }
                        CallableDefId::EnumVariantId(it) => {
                            fold.note(Subject::of_variant(db, it.into()), role, Way::Construct);
                        }
                    },
                    _ => {}
                }
            }
            Expr::MethodCall { args, .. } => {
                let Some((f, _)) = infer.method_resolution(id) else { continue };
                call_rows(db, infer, body, f, args, role, &mut fold);
            }
            Expr::Path(_) => {
                if callees.contains(&id) {
                    continue;
                }
                let named = match infer.value_resolution(id.into()) {
                    Some(ValueNs::FunctionId(f)) => Some(f),
                    _ => match infer.assoc_resolutions_for_expr(id) {
                        Some((CandidateId::FunctionId(f), _)) => Some(f),
                        _ => None,
                    },
                };
                if let Some(f) = named {
                    fold.note(Subject::of_fn(db, f), role, Way::Named);
                    continue;
                }
                match infer.value_resolution(id.into()) {
                    Some(ValueNs::EnumVariantId(it)) => {
                        fold.note(Subject::of_variant(db, it.into()), role, Way::Construct);
                    }
                    Some(ValueNs::StructId(it)) => {
                        fold.note(Subject::of_variant(db, it.into()), role, Way::Construct);
                    }
                    _ => {}
                }
            }
            Expr::BinaryOp { lhs, op: Some(BinaryOp::CmpOp(CmpOp::Eq { .. })), .. } => {
                compare_rows(db, infer, lhs, role, &mut fold);
            }
            Expr::BinaryOp { lhs, op: Some(BinaryOp::Assignment { op: Some(_) }), .. } => {
                // `x.f += 1` reads the field as it writes it.
                if let Expr::Field { .. } = body.expr(lhs)
                    && let Some(Either::Left(field)) = infer.field_resolution(lhs)
                {
                    fold.note(Subject::of_field_id(db, field), role, Way::Read);
                }
            }
            Expr::Cast { expr, .. } => compare_rows(db, infer, expr, role, &mut fold),
            _ => {}
        }
    }
    for (id, pat) in body.pats() {
        let variant = match pat {
            Pat::Record { .. } | Pat::TupleStruct { .. } | Pat::Path(_) => {
                infer.variant_resolution_for_pat(id)
            }
            _ => None,
        };
        let Some(variant) = variant else { continue };
        if matches!(variant, VariantId::EnumVariantId(_)) {
            fold.note(Subject::of_variant(db, variant), role, Way::Match);
        }
        let shape = VariantFields::of(db, variant);
        match pat {
            Pat::Record { args, .. } => {
                for field in args {
                    if matches!(body.pat(field.pat), Pat::Wild) {
                        continue;
                    }
                    let Some(local) = shape.field(&field.name) else { continue };
                    let index = local.into_raw().into_u32();
                    fold.note(Subject::of_field(db, variant, index), role, Way::Read);
                }
            }
            Pat::TupleStruct { args, ellipsis, .. } => {
                let len = shape.len();
                for (place, &arg) in args.iter().enumerate() {
                    if matches!(body.pat(arg), Pat::Wild | Pat::Rest) {
                        continue;
                    }
                    // Past a `..`, a pattern counts from the end.
                    let index = match ellipsis {
                        Some(gap) if place >= gap as usize => len - (args.len() - place),
                        _ => place,
                    };
                    let Ok(index) = u32::try_from(index) else { continue };
                    fold.note(Subject::of_field(db, variant, index), role, Way::Read);
                }
            }
            _ => {}
        }
    }
    fold
}

/// The rows of a call of `f` with `args`, those after any `self`.
fn call_rows(
    db: &Db,
    infer: &InferenceResult<'_>,
    body: &Body,
    f: FunctionId,
    args: &[ExprId],
    role: Role,
    fold: &mut Fold,
) {
    let Some(subject) = Subject::of_fn(db, f) else { return };
    let facts = fold.at(subject);
    facts.add(role, Way::Call);
    if role == Role::Stored {
        return;
    }
    if facts.args.len() < args.len() {
        facts.args.resize(args.len(), Arg::Unseen);
    }
    for (arg, &expr) in facts.args.iter_mut().zip(args) {
        arg.join(constant(db, infer, body, expr).as_ref());
    }
}

/// The rows of a comparison or cast of a value of an enum.
fn compare_rows(db: &Db, infer: &InferenceResult<'_>, expr: ExprId, role: Role, fold: &mut Fold) {
    let ty = infer.expr_ty(expr);
    let Some((adt, _)) = ty.strip_references().as_adt() else { return };
    if let AdtId::EnumId(_) = adt {
        fold.note(Subject::of_adt(db, adt), role, Way::Compare);
    }
}

/// The constant `expr` is: a literal (negated or not), or a path to a `const`, a unit variant
/// or a unit struct.
pub(crate) fn constant(
    db: &Db,
    infer: &InferenceResult<'_>,
    body: &Body,
    expr: ExprId,
) -> Option<Constant> {
    match body.expr(expr) {
        Expr::Literal(it) => Some(Constant::Literal(Literal::clone(it))),
        Expr::UnaryOp { expr: inner, op: UnaryOp::Neg } => match body.expr(inner) {
            Expr::Literal(it) => Literal::clone(it).negate().map(Constant::Literal),
            _ => None,
        },
        Expr::Path(_) => {
            if let TyKind::FnDef(..) = infer.expr_ty(expr).kind() {
                return None;
            }
            let subject = match infer.value_resolution(expr.into())? {
                ValueNs::ConstId(it) => Subject::at(it.lookup(db).id, 0),
                ValueNs::EnumVariantId(it) => Subject::of_variant(db, it.into()),
                ValueNs::StructId(it) => Subject::of_variant(db, it.into()),
                _ => None,
            };
            subject.map(Constant::Item)
        }
        _ => None,
    }
}
