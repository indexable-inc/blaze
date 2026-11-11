//! `could_be_program_lifetime` (in `blaze::edition`, allowed by default): a type of the
//! crate whose every instance already lives until the program exits, and so could be
//! `#[blaze::program_lifetime]`, held in the program arena by 4-byte `Ref32`s with no
//! reference count (docs/v2/plan/design/facts.md "The two lints").
//!
//! Found: a struct with named fields, of the crate, that
//!
//! - is not declared `#[blaze::program_lifetime]` already, and whose shape could be
//!   (`hir_ty_pure::program_lifetime::shape`: no parameter, no `Drop`, nothing in it that
//!   drops or borrows);
//! - other crates can't name, unless the crate has `#![blaze::program_lifetime]` at its root;
//! - is built somewhere in the crate, and every record literal of it ([`constructions`]) is
//!   the argument of a `Box::leak(Box::new(..))`, or is in the initializer of a `static`
//!   that is not `mut` and holds no cell, lock or atomic (so nothing can take the value out
//!   or replace it), a `LazyLock`'s or `OnceLock`'s closure there included.
//!
//! Silent for anything it cannot follow: a literal bound to a local, returned, passed to any
//! other call or pushed into a collection from a function (a table in a static filled at run
//! time is not followed: something may remove from it); a tuple struct, an enum or a union
//! (their constructors are not record literals); a type a macro wrote; test code.
//!
//! The finding names the type, where its instances are kept (the first leak or static), how
//! many `Arc`/`Rc` clones of it the crate makes (each a count written, which a `Ref32` copy
//! is not), and the gain. No fix: the type's holders change with it (`could_be_ref32` finds
//! them once the type is marked).

use base_db::Db;
use hir_def::{
    AdtId, DefWithBodyId, VariantId,
    expr_store::Body,
    hir::{Expr, ExprId},
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Lint, alloc::written_callee, facts, frozen_collection, general_facts};
use crate::{check::Cx, diag};

const LINT: Lint = Lint::CouldBeProgramLifetime;

/// What a record literal's value goes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kept {
    /// The argument of `blaze_mem::ProgramArena::alloc`.
    Arena,
    /// The argument of `Box::leak(Box::new(..))`.
    Leak,
    /// The initializer of a `static` nothing can take it out of.
    Static,
    /// Anything else: a local, a return, another call.
    Other,
}

/// A record literal of an ADT of the crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Construction {
    pub(crate) adt: AdtId,
    pub(crate) def: DefWithBodyId,
    pub(crate) expr: ExprId,
    pub(crate) into: Kept,
}

/// Every record literal in the bodies of `krate`, with what its value goes into, in the
/// order of the crate's bodies; and how many `clone` calls give an `Arc` or `Rc` of each ADT.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Constructions {
    sites: Vec<Construction>,
    clones: Vec<(AdtId, u32)>,
}

impl Constructions {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Construction> {
        self.sites.iter()
    }

    fn clones_of(&self, adt: AdtId) -> u32 {
        self.clones.iter().find(|it| it.0 == adt).map_or(0, |it| it.1)
    }
}

/// Whether the std ADT `adt` lets a shared reference change or take what it holds.
fn is_cell(db: &Db, adt: hir::Adt) -> bool {
    let name = adt.name(db);
    let name = name.as_str();
    general_facts::std_adt(db, adt, "cell", &[name])
        || (general_facts::std_adt(db, adt, "sync", &[name])
            && !matches!(name, "LazyLock" | "OnceLock" | "Arc"))
        || facts::is_blaze_core_lock(db, adt)
}

/// Whether nothing can take a value out of the static `def` or replace it: it is not `mut`
/// and its type holds no cell, lock or atomic as far as [`facts::holds`] looks.
fn frozen_static(db: &Db, def: DefWithBodyId) -> bool {
    let DefWithBodyId::StaticId(id) = def else { return false };
    if StaticSignature::of(db, id).flags.contains(StaticFlags::MUTABLE) {
        return false;
    }
    let ty = hir::Static::from(id).ty(db);
    // Through fields (`holds`) and through type arguments (`walk`): an `Arc<Mutex<..>>` holds
    // its lock behind a pointer no field walk follows.
    let mut cell = facts::holds(db, &ty, facts::HOLDS_DEPTH, &mut |adt| is_cell(db, adt)).is_some();
    ty.walk(db, |inner| cell |= inner.as_adt().is_some_and(|adt| is_cell(db, adt)));
    !cell
}

/// What the value of the literal `expr` goes into, from the calls around it.
fn into<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    parents: &facts::Parents,
    def: DefWithBodyId,
    expr: ExprId,
) -> Kept {
    let named = |call: ExprId, name: &str| {
        general_facts::callee(db, analysis, call).filter(|it| it.name == name)
    };
    let is_box = |adt: Option<hir::Adt>| {
        adt.is_some_and(|it| general_facts::std_adt(db, it, "boxed", &["Box"]))
    };
    if let Some(parent) = parents.of_expr(expr) {
        if named(parent, "alloc").is_some_and(|it| {
            it.self_adt.is_some_and(|it| general_facts::crate_adt(db, it, "blaze_mem", &["ProgramArena"]))
        }) {
            return Kept::Arena;
        }
        if named(parent, "new").is_some_and(|it| is_box(it.self_adt))
            && parents
                .of_expr(parent)
                .is_some_and(|outer| named(outer, "leak").is_some_and(|it| is_box(it.self_adt)))
        {
            return Kept::Leak;
        }
    }
    if frozen_static(db, def) { Kept::Static } else { Kept::Other }
}

/// [`Constructions`] of `krate`, once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn constructions(db: &Db, krate: base_db::Crate) -> Constructions {
    let mut out = Constructions::default();
    let shared = |adt: hir::Adt| {
        general_facts::std_adt(db, adt, "sync", &["Arc"])
            || general_facts::std_adt(db, adt, "rc", &["Rc"])
    };
    for def in frozen_collection::crate_bodies(db, krate) {
        let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
        let (body, infer) = (analysis.body(), analysis.infer());
        let mut parents = None;
        for (id, expr) in body.exprs() {
            match expr {
                Expr::RecordLit { .. } => {
                    let adt = match infer.variant_resolution_for_expr(id) {
                        Some(VariantId::StructId(it)) => AdtId::StructId(it),
                        Some(VariantId::UnionId(it)) => AdtId::UnionId(it),
                        Some(VariantId::EnumVariantId(it)) => {
                            AdtId::EnumId(hir_def::Lookup::lookup(&it, db).parent)
                        }
                        None => continue,
                    };
                    let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
                    let into = into(db, &analysis, parents, def, id);
                    out.sites.push(Construction { adt, def, expr: id, into });
                }
                Expr::MethodCall { .. } | Expr::Call { .. } => {
                    let Some((callee, _)) = written_callee(infer, body, id) else { continue };
                    if FunctionSignature::of(db, callee).name.as_str() != "clone" {
                        continue;
                    }
                    let ty = analysis.ty(infer.expr_ty(id));
                    if !ty.as_adt().is_some_and(shared) {
                        continue;
                    }
                    let Some(held) = ty.type_arguments().next().and_then(|it| it.as_adt()) else {
                        continue;
                    };
                    let held = AdtId::from(held);
                    match out.clones.iter_mut().find(|it| it.0 == held) {
                        Some(found) => found.1 += 1,
                        None => out.clones.push((held, 1)),
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Where `site` is written: its file and its range there; `None` for one a macro wrote.
pub(crate) fn place_of(db: &Db, site: &Construction) -> Option<(span::FileId, TextRange)> {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(site.def));
    let file = analysis.tree().1.file_id()?.file_id(db);
    let offset = Body::anchor_offset(db, site.def)?;
    Some((file, analysis.expr_range(site.expr)? + offset))
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(adt.syntax())
        || cx.is_in_test(adt.syntax())
    {
        return;
    }
    // Only a struct with named fields is built by record literals alone.
    let ast::Adt::Struct(item) = adt else { return };
    if !matches!(item.field_list(), Some(ast::FieldList::RecordFieldList(_))) {
        return;
    }
    let Some(def) = cx.adt_def(adt) else { return };
    let db = cx.db();
    let id = AdtId::from(def);
    let krate = cx.krate().base();
    if hir_ty_pure::program_lifetime::declared(db, id)
        || hir_ty_pure::program_lifetime::shape(db, id).is_some()
        || (cx.is_exported(adt.syntax()) && !super::program_lifetime::opted_in(db, krate))
    {
        return;
    }
    let all = constructions(db, krate);
    let mut sites = all.iter().filter(|it| it.adt == id).peekable();
    let Some(&first) = sites.peek() else { return };
    let mut built = 0usize;
    for site in sites {
        if !matches!(site.into, Kept::Leak | Kept::Static) {
            return;
        }
        built += 1;
    }
    let name = hir_ty_pure::program_lifetime::adt_name(db, id);
    let kept = match first.into {
        Kept::Leak => "leaked with `Box::leak`",
        _ => "kept in a `static`",
    };
    let message = format!(
        "every `{name}` the crate builds lives until the program exits ({built} built, the first {kept}): it could be `#[blaze::program_lifetime]`"
    );
    let clones = all.clones_of(id);
    let counted = match clones {
        0 => String::new(),
        n => format!("; the crate clones an `Arc` or `Rc` of it in {n} places, each a count written that a `Ref32` copy is not"),
    };
    let note = format!(
        "in the program arena a `{name}` is named by a `Ref32<{name}>`: 4 bytes where a `&'static {name}` or a `Box` is 8 and an `Arc` is 8 and two counts beside the value{counted}"
    );
    let node = item.name().map_or(adt.syntax(), |it| it.syntax());
    let Some(range) = cx.range(node) else { return };
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, adt.syntax(), span, message);
    if let Some((file, range)) = place_of(db, first) {
        diagnostic.span_note(diag::of_file(file, range), "the first is built here");
    }
    diagnostic
        .note(note)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}
