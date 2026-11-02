//! The borrow checker: rustc's `rustc_borrowck` over the bodies MIR lowering builds.
//!
//! Each body is checked on its own, closures and coroutines with it, as rustc checks a body
//! and the closures in it together, and the result is a query of the body's owner, so an edit
//! checks again only the bodies it changes. What it reports carries rustc's error codes and the
//! span rustc points at; `docs/borrowck.md` in the repository describes what is checked and how.
//!
//! The analyses run on dense columns: a CFG and every relation as a CSR (one start array, one
//! item array, built by an in-place counting sort), `u64`-word bitsets and bit matrices with a
//! row per block, gen/kill transfer functions composed once per block and iterated to a fixed
//! point over a worklist seeded in reverse postorder, move paths numbered in preorder so a
//! subtree is a range of words, and region inference over the SCCs of the outlives graph.

#![forbid(unsafe_code)]
#![feature(allocator_api, allocator_ext)]

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate rustc_type_ir;

mod bitset;
mod cfg;
mod check;
mod constraints;
mod dataflow;
mod decode;
mod gate;
mod inference;
mod intervals;
mod liveness;
mod loans;
mod mem;
mod moves;
mod names;
mod places;
mod polonius;
mod qualifs;
mod regions;
mod relation;
mod rows;
mod scan;
pub mod timings;

#[cfg(test)]
mod budget;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod missing_body_tests;
#[cfg(test)]
mod served;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{cell::RefCell, sync::LazyLock};

use base_db::{Db, Crate};
use blaze_ids::radix;
use hir_def::{
    DefWithBodyId, HasModule,
    expr_store::ExpressionStore,
    hir::{BindingAnnotation, BindingId, ExprId},
};

use hir_ty::{
    InferBodyId, InferenceResult,
    closure_analysis::CapturedPlace,
    db::{HirDatabase, InternedCoroutineId},
    next_solver::{
        DbInterner, ParamEnv, Region, Ty, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};

use hir_ty_mir::mir::{
    LocalId, MirBody, MirSpan, Operand, OperandKind, PlaceElem, PlaceTy, StoredPlace,
};

pub use gate::{
    Checked, CheckedMir, Pending, Refused, Unchecked, UncheckedMir, checked, speculate,
};

use crate::{
    bitset::bits,
    mem::{ExactLen, FMap, FVec, Frame},
};

/// Which of rustc's borrow checkers to match: the one stable Rust has, or the location
/// sensitive one nightly Rust has by default (`-Zpolonius=next`), which accepts more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Nll,
    Polonius,
    Off,
}

/// `BLAZE_BORROWCK`: `polonius` (the default), `nll` or `off`.
pub fn mode() -> Mode {
    static MODE: LazyLock<Mode> =
        LazyLock::new(|| match std::env::var("BLAZE_BORROWCK").as_deref() {
            Ok("nll") => Mode::Nll,
            Ok("off" | "0") => Mode::Off,
            _ => Mode::Polonius,
        });
    *MODE
}

/// `BLAZE_BORROWCK_DEBUG=1` prints the regions, constraints and loans of every body checked,
/// `BLAZE_BORROWCK_DEBUG=name` those of the functions called `name` and their closures.
pub(crate) fn debugging(db: &Db, owner: InferBodyId<'_>) -> bool {
    static WANTED: LazyLock<Option<String>> =
        LazyLock::new(|| std::env::var("BLAZE_BORROWCK_DEBUG").ok()); // alloc: once per process
    let Some(wanted) = WANTED.as_deref() else { return false };
    match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) if wanted != "1" => {
            hir_def::signatures::FunctionSignature::of(db, it).name.as_str() == wanted
        }
        _ => wanted == "1",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
pub struct BorrowckDiagnostic {
    /// rustc's error code, as `E0382`, or `unused_mut` for the lint the borrow checker gives.
    pub code: &'static str,
    pub span: MirSpan,
    pub message: String,
    /// Reported at the function's signature rather than at `span`, as rustc reports an
    /// `impl Trait`'s hidden type.
    pub at_signature: bool,
    /// rustc's `span_label`s, in the order rustc adds them; one at `span` is the primary
    /// span's own.
    pub labels: Vec<(PartSpan, String)>,
    /// rustc's notes and helps, in the order rustc adds them.
    pub children: Vec<Child>,
    /// rustc's suggestions, in the order rustc adds them.
    pub suggestions: Vec<Suggestion>,
}

/// Where a part of a [`BorrowckDiagnostic`] points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartSpan {
    At(MirSpan),
    /// The empty span where `span`'s source starts, where a suggestion inserts before it.
    Before(MirSpan),
    /// The empty span where `span`'s source ends.
    After(MirSpan),
    /// From where the first expression's source starts to where the second's, an expression
    /// inside it, starts: the `&` or `*` a suggestion removes.
    Head(ExprId, ExprId),
    /// From where the second expression's source ends to where the first's ends.
    Tail(ExprId, ExprId),
    /// The name a method call calls, rustc's span of the callee of a method call.
    MethodName(ExprId),
    /// A closure's or coroutine's head, rustc's `fn_decl_span`: from its start through its
    /// parameters and return type, or `async`/`gen` and `move` of a block.
    ClosureHead(ExprId),
    /// rustc's `def_span` of the type: its head, up to its name and generics.
    Def(hir_def::AdtId),
}

/// rustc's `Level` of a note or help.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sublevel {
    Note,
    Help,
}

/// rustc's `Applicability` of a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Applicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

/// A note or help: rustc's `span_note` and `note`, with no span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub level: Sublevel,
    pub message: String,
    pub spans: Vec<PartSpan>,
    /// Labels of spans of the child's `MultiSpan`, which need not be among `spans`.
    pub labels: Vec<(PartSpan, String)>,
}

/// A suggested edit: each part replaces the source of its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub message: String,
    pub parts: Vec<(PartSpan, String)>,
    pub applicability: Applicability,
    /// rustc's `span_suggestion_verbose` (`SuggestionStyle::ShowAlways`): shown as a diff of
    /// its own, never inline in the message.
    pub verbose: bool,
}

impl BorrowckDiagnostic {
    /// A diagnostic with no labels, notes or suggestions.
    pub fn new(code: &'static str, span: MirSpan, message: String) -> Self {
        BorrowckDiagnostic {
            code,
            span,
            message,
            at_signature: false,
            labels: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
            children: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
            suggestions: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
        }
    }

    pub(crate) fn label(&mut self, span: PartSpan, text: impl Into<String>) {
        self.labels.push((span, text.into()));
    }

    pub(crate) fn note(&mut self, message: impl Into<String>) {
        self.children.push(Child {
            level: Sublevel::Note,
            message: message.into(),
            spans: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
            labels: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
        });
    }

    pub(crate) fn help(&mut self, message: impl Into<String>) {
        self.children.push(Child {
            level: Sublevel::Help,
            message: message.into(),
            spans: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
            labels: Vec::new(), // alloc: the owned diagnostic; `Vec::new` takes no memory
        });
    }

    pub(crate) fn suggest(
        &mut self,
        message: impl Into<String>,
        parts: Vec<(PartSpan, String)>,
        applicability: Applicability,
        verbose: bool,
    ) {
        self.suggestions.push(Suggestion {
            message: message.into(),
            parts,
            applicability,
            verbose,
        });
    }

    /// An error, which keeps the body out of any build (cargo's rule), not a lint: rustc gives
    /// some errors no code ("lifetime may not live long enough"), and a lint a lowercase name.
    pub fn is_error(&self) -> bool {
        !self.code.starts_with(|it: char| it.is_ascii_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, blaze_query::Value)]
pub struct BorrowckResult {
    pub diagnostics: Vec<BorrowckDiagnostic>,
    /// Why the body, or a closure in it, was not checked: MIR lowering does not support
    /// something in it.
    pub skipped: Option<String>,
    /// Why the checker stopped before it checked the body, as rustc does: an error found before
    /// borrow checking, which keeps the body out of a build as a borrow check error would.
    pub not_checked: Option<NotChecked>,
}

/// Why a body was not borrow checked, by the error rustc reports before borrow checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub enum NotChecked {
    /// `BLAZE_BORROWCK=off`.
    Off,
    /// Type checking found an error in it.
    TypeErrors,
    /// Lowering found an error in it (a cast that is not one, `..base` in a destructuring
    /// assignment).
    LoweringErrors,
    /// A macro call in it failed to expand, an error reported at the call: rustc lowers it to
    /// `ExprKind::Err`, which taints the body.
    ExpansionErrors,
    /// Match checking refused it (E0004, E0005, E0030, a constant pattern that does not
    /// evaluate): rustc's `mir_built` gives such a body its error body, which nothing checks.
    MatchErrors,
    /// A `static` of a type that is not `Sized`: rustc's E0277.
    UnsizedStatic,
    /// A const context drops a value of a type with drop glue: rustc's E0493.
    DropInConst,
    /// A where clause of a call does not hold: rustc's E0277, which type checking did not
    /// report.
    Unproven,
}

impl NotChecked {
    /// Whether the body may still go into a build: only when borrow checking is turned off.
    pub fn refuses(self) -> bool {
        self != NotChecked::Off
    }
}

/// The errors rustc's borrow checker reports in `def` and the closures in it.
#[blaze_query::tracked(returns(ref))]
pub fn borrowck<'db>(db: &'db Db, def: DefWithBodyId) -> BorrowckResult {
    let mode = mode();
    let Some(checker) = checker(mode) else { return borrowck_in_mode(db, def, mode) };
    let (result, reads) = hir_ty::body_store::capture(db, || borrowck_in_mode(db, def, mode));
    if gate::verdict(def, &result).is_ok() {
        hir_ty::body_store::offer_passed(db, def, checker, reads);
    }
    result
}

/// The name the item store files `mode`'s verdicts under: each checker accepts other programs.
/// None when no check runs, whose verdict is never stored.
pub(crate) fn checker(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Nll => Some("nll"),
        Mode::Polonius => Some("polonius"),
        Mode::Off => None,
    }
}

/// [`borrowck`] in `mode`, not cached: for measuring the checker in each mode.
pub fn borrowck_in_mode(db: &Db, def: DefWithBodyId, mode: Mode) -> BorrowckResult {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| check_def(db, def, mode, frame))
}

/// Whether `def`'s source has a body: a fn's block, a const's or static's value, a variant's
/// discriminant. Read from its signature, not from the lowered body.
fn written_with_body(db: &Db, def: DefWithBodyId) -> bool {
    use hir_def::signatures::{ConstSignature, FunctionSignature, StaticFlags, StaticSignature};
    match def {
        DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).has_body(),
        DefWithBodyId::ConstId(it) => ConstSignature::of(db, it).has_body(),
        DefWithBodyId::StaticId(it) => {
            StaticSignature::of(db, it).flags.contains(StaticFlags::HAS_BODY)
        }
        DefWithBodyId::VariantId(it) => {
            it.loc(db).parent.enum_variants(db).has_explicit_discriminant(it)
        }
    }
}

fn check_def(db: &Db, def: DefWithBodyId, mode: Mode, frame: &Frame) -> BorrowckResult {
    let mut result = BorrowckResult::default();
    if mode == Mode::Off {
        result.not_checked = Some(NotChecked::Off);
        return result;
    }
    // A fn, const or static written without a body, or a variant without a discriminant: rustc
    // builds no MIR for it, so there is nothing to check and nothing to refuse (where Rust
    // requires a body, AST validation reports its absence). A body that is written but lowered
    // to nothing (`#[rust_analyzer::skip]`, a `#[cfg]`d-out body expression) is refused:
    // rustc checks what is written. Asked before the lowered root is looked at: a bodiless
    // `async fn` lowers to an `async` block around nothing, which is no body either.
    if !written_with_body(db, def) {
        return result;
    }
    let body = hir_def::expr_store::Body::of(db, def);
    if matches!(body.expr(body.root_expr()), hir_def::hir::Expr::Missing) {
        // alloc: the owned result, once, when the body is refused
        result.skipped =
            Some("a body that is written but lowered to no expression: nothing to check".into());
        return result;
    }
    let owner = InferBodyId::DefWithBodyId(def);
    let infer = InferenceResult::of(db, owner);
    if infer.has_type_mismatches() || infer.is_erroneous() || !infer.diagnostics().is_empty() {
        result.not_checked = Some(NotChecked::TypeErrors);
        return result;
    }
    // rustc borrow checks no body its lowering or type check found an error in, which an
    // error of either taints: a cast that is not one, `..base` in a destructuring assignment.
    // A query of whether there was one, which reads no position: an edit elsewhere in the
    // file checks the body again only if it changes that.
    if hir_def::expr_store::Body::has_lowering_errors(db, def) {
        result.not_checked = Some(NotChecked::LoweringErrors);
        return result;
    }
    if hir_ty_mir::mir::has_failed_expansion(db, owner) {
        result.not_checked = Some(NotChecked::ExpansionErrors);
        return result;
    }
    if hir_ty_matchck::match_check(db, def).failed {
        result.not_checked = Some(NotChecked::MatchErrors);
        return result;
    }
    // A `static` of a type that is not `Sized` is rustc's E0277, which keeps it from being
    // borrow checked; a `const` is checked all the same.
    if let DefWithBodyId::StaticId(it) = def
        && let Some(ty) = db.value_ty(it.into())
        && {
            use hir_ty::next_solver::TyKind;
            use rustc_type_ir::inherent::IntoKind;
            matches!(
                ty.instantiate_identity().skip_norm_wip().kind(),
                TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..)
            )
        }
    {
        result.not_checked = Some(NotChecked::UnsizedStatic);
        return result;
    }
    check_owner(db, owner, Some(def), mode, frame)
}

/// The errors rustc's borrow checker reports in a constant of a signature, as an array length
/// or a const generic argument or default, which rustc checks as a body of its own.
#[blaze_query::tracked(returns(ref))]
pub fn borrowck_anon_const<'db>(
    db: &'db Db,
    konst: hir_ty::db::AnonConstId<'db>,
) -> BorrowckResult {
    if konst.is_inline_const(db) {
        return BorrowckResult::default();
    }
    if mode() == Mode::Off {
        return BorrowckResult { not_checked: Some(NotChecked::Off), ..Default::default() };
    }
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let owner = InferBodyId::AnonConstId(konst);
        let infer = InferenceResult::of(db, owner);
        if infer.has_type_mismatches() || infer.is_erroneous() {
            return BorrowckResult {
                not_checked: Some(NotChecked::TypeErrors),
                ..Default::default()
            };
        }
        if hir_ty_mir::mir::has_failed_expansion(db, owner) {
            return BorrowckResult {
                not_checked: Some(NotChecked::ExpansionErrors),
                ..Default::default()
            };
        }
        check_owner(db, owner, None, mode(), frame)
    })
}

/// The errors in `owner`'s body and the closures, coroutines and `const { .. }` blocks in it;
/// `def` is the item when it is one, not a constant of a signature.
fn check_owner<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    def: Option<DefWithBodyId>,
    mode: Mode,
    frame: &Frame,
) -> BorrowckResult {
    let mut result = BorrowckResult::default();
    let infer = InferenceResult::of(db, owner);
    let body = match timings::phase(timings::Phase::Mir, || db.mir_body(owner)) {
        Ok(body) => body,
        Err(error) => {
            // alloc: the owned result, once, when MIR lowering refuses the body
            result.skipped = Some(format!("{error:?}"));
            return result;
        }
    };
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    let mut bodies: FVec<'_, _> = Vec::new_in(frame);
    bodies.push((body, None, None));
    // What each body is written as: `None` for the owner's.
    let mut written: FVec<'_, Option<ExprId>> = Vec::new_in(frame);
    written.push(None);
    let mut seen = mem::set(frame);
    let mut nested: FVec<'_, Nested<'db>> = Vec::new_in(frame);
    let mut index = 0;
    while let Some(&(body, _, _)) = bodies.get(index) {
        let at = written[index];
        index += 1;
        nested.clear();
        nested.extend(body.closures.iter().map(|&it| Nested::Closure(it)));
        // The body of an `async` block or `async fn`, checked as rustc checks a coroutine: as
        // a closure of the body that makes it.
        coroutines(body, |it| nested.push(Nested::Coroutine(it)));
        // A `const { .. }` block, which MIR lowering makes a constant of its own, checked with
        // the body as rustc checks an inline const.
        inline_consts(db, body, |it| nested.push(Nested::Const(it)));
        // Those in code that is never reached, which MIR lowering leaves out: rustc borrow
        // checks every body nested in the item's (`nested_bodies_within`), so a closure after a
        // `return` is checked too.
        nested_bodies_written(db, owner, store, infer, at, frame, &mut nested);
        for &it in nested.iter() {
            if !seen.insert(it) {
                continue;
            }
            let (lowered, what) = timings::phase(timings::Phase::Mir, || match it {
                Nested::Closure(closure) => (
                    db.mir_body_for_closure(closure)
                        .map(|body| (body, Some(closure.loc(db).expr), None)),
                    "closure",
                ),
                Nested::Coroutine(coroutine) => (
                    db.mir_body_for_coroutine(coroutine)
                        .map(|body| (body, Some(coroutine.loc(db).expr), None)),
                    "coroutine",
                ),
                Nested::Const(konst) => (
                    db.mir_body(InferBodyId::AnonConstId(konst))
                        .map(|body| (body, None, Some(konst.loc(db).expr))),
                    "inline const",
                ),
            });
            match lowered {
                Ok(it) => {
                    written.push(it.1.or(it.2));
                    bodies.push(it);
                }
                Err(error) => {
                    // alloc: the owned result, once, when MIR lowering refuses a body
                    result.skipped.get_or_insert_with(|| format!("{what}: {error:?}"));
                }
            }
        }
    }
    // A closure after the closures in it and before the body that makes it, which proves what
    // the closure needs of it. What a closure needs of its creator, and which of its captures
    // it uses mutably: by the closure's expression, in the owner's frame.
    let mut requirements: FMap<'_, ExprId, (usize, FVec<'_, regions::Requirement<'db>>)> =
        mem::map(frame);
    let mut mut_upvars: FMap<'_, ExprId, FVec<'_, usize>> = mem::map(frame);
    let mut diagnostics: FVec<'_, FVec<'_, Option<BorrowckDiagnostic>>> = Vec::new_in(frame);
    // rustc's `buffered_move_errors`, one for the function and the closures in it: the body,
    // the error's place as rustc has it and where the error is, by the moves it is about.
    let mut move_errors: FMap<
        '_,
        FVec<'_, usize>,
        (usize, (LocalId, FVec<'_, PlaceElem>), usize),
    > = mem::map(frame);
    let scan = timings::phase(timings::Phase::Setup, || {
        scan::Scan::new(frame, store, infer, DbInterner::new_with(db, owner.krate(db)))
    });
    for &(body, closure, inline_const) in bodies.iter().rev() {
        // Each body in a task of its own: its analyses' memory goes when it is checked, and
        // what it hands the next body is in the owner's `frame`.
        let checked = blaze_mem::frame::task(blaze_mem::Tag::Mir, |body_frame| {
            let mut ctx = timings::phase(timings::Phase::Setup, || {
                Ctx::new(db, owner, store, infer, &scan, body, closure, mode, body_frame)
            });
            ctx.inline_const = inline_const.is_some();
            if closure.is_none() && inline_const.is_none() && drops_in_const(&ctx, def, body) {
                return Err(NotChecked::DropInConst);
            }
            let mut found = Vec::new_in(frame);
            let mut moved = Vec::new_in(frame);
            let (count, needs, used_mut_upvars) = check::check(
                &ctx,
                frame,
                body,
                &requirements,
                &mut_upvars,
                &mut found,
                &mut moved,
            );
            if ctx.tainted.get() {
                return Err(NotChecked::Unproven);
            }
            Ok((found, moved, count, needs, used_mut_upvars))
        });
        let (found, moved, count, needs, used_mut_upvars) = match checked {
            Ok(it) => it,
            Err(why) => {
                result.not_checked = Some(why);
                return result;
            }
        };
        let this = diagnostics.len();
        let mut found = mem::collect(frame, found.into_iter().map(Some));
        // rustc numbers the moves of each body from 0 but buffers the errors of all of them
        // by those numbers, so a use of a moved value whose moves are numbered as those of
        // an error a closure gave, of a place the closure's is in (the variable `_1` and the
        // capture `_1.0`), is not reported; of another place, it replaces the closure's.
        for error in moved {
            let Some(key) = error.rustc_key else { continue };
            if let Some(&(other, ref place, at)) = move_errors.get(&key)
                && other != this
            {
                let reported = check::PlaceRef { local: place.0, projection: &place.1 };
                let used = check::PlaceRef { local: error.place.0, projection: &error.place.1 };
                if used.is_prefix_of(reported) {
                    found[error.at] = None;
                    continue;
                }
                diagnostics[other][at] = None;
            }
            move_errors.insert(key, (this, error.place, error.at));
        }
        diagnostics.push(found);
        if let Some(nested) = closure.or(inline_const) {
            requirements.insert(nested, (count, needs));
            mut_upvars.insert(nested, used_mut_upvars);
        }
    }
    let reported = diagnostics.iter().flatten().flatten().count();
    result.diagnostics =
        mem::owned(ExactLen::new(diagnostics.into_iter().rev().flatten().flatten(), reported));
    if debugging(db, owner) {
        if let Some(skipped) = &result.skipped {
            eprintln!("skipped {skipped}");
        }
        for it in &result.diagnostics {
            eprintln!("diagnostic {it:?}");
        }
    }
    result
}

/// rustc's E0493, from checking a const context before borrow checking it: a `const` or
/// `static` item or a `const fn` drops a value of a type with drop glue, and rustc does not
/// borrow check it. What is moved out of, in part too, is taken not to be dropped.
fn drops_in_const(ctx: &Ctx<'_, '_>, def: Option<DefWithBodyId>, body: &MirBody<'_>) -> bool {
    use hir_ty_mir::mir::{OperandKind, TerminatorKind};
    let const_context = match def {
        // A constant of a signature.
        None => true,
        Some(DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_)) => true,
        Some(DefWithBodyId::FunctionId(it)) => {
            hir_def::signatures::FunctionSignature::of(ctx.db, it).is_const()
        }
        _ => false,
    };
    if !const_context {
        return false;
    }
    let mut moved = bits(ctx.frame, body.locals.len());
    let mut visit = |operand: &hir_ty_mir::mir::Operand| {
        if let OperandKind::Move(place) = &operand.kind {
            moved.insert(place.local.into_raw().into_u32() as usize);
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for it in &block.statements {
            it.kind.for_each_operand(&mut visit);
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut visit);
        }
    }
    body.basic_blocks.iter().any(|(_, block)| {
        let Some(terminator) = &block.terminator else { return false };
        let TerminatorKind::Drop { place, .. } = &terminator.kind else { return false };
        place.projection.as_slice().is_empty()
            && place.local != return_slot()
            && !moved.contains(place.local.into_raw().into_u32() as usize)
            && ctx.drop_glue(body.locals[place.local].ty.as_ref())
                == hir_ty::drop::DropGlue::HasDropGlue
    })
}

/// The coroutines `body` makes, in the order it makes them.
fn coroutines<'db>(body: &MirBody<'db>, mut found: impl FnMut(InternedCoroutineId<'db>)) {
    use hir_ty::next_solver::TyKind;
    use hir_ty_mir::mir::{AggregateKind, Rvalue, StatementKind};
    use rustc_type_ir::inherent::IntoKind;
    // The caller drops one seen before; this drops the common repeat, the same one again.
    let mut last = None;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), _)) =
                &statement.kind
                && let TyKind::Coroutine(id, _) = ty.as_ref().kind()
                && last != Some(id.0)
            {
                last = Some(id.0);
                found(id.0);
            }
        }
    }
}

/// A body checked with the one it is written in.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Nested<'db> {
    Closure(hir_ty::db::InternedClosureId<'db>),
    Coroutine(InternedCoroutineId<'db>),
    Const(hir_ty::db::AnonConstId<'db>),
}

/// The closures, `async` blocks and `const { .. }` blocks written directly in the body written
/// as `at` (the owner's for `None`), not in another of them, reached or not.
fn nested_bodies_written<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    store: &ExpressionStore,
    infer: &InferenceResult<'db>,
    at: Option<ExprId>,
    frame: &Frame,
    out: &mut FVec<'_, Nested<'db>>,
) {
    use hir_def::hir::{ClosureKind, CoroutineKind, Expr};
    use hir_ty::next_solver::TyKind;
    use rustc_type_ir::inherent::IntoKind;
    let mut stack: FVec<'_, ExprId> = Vec::new_in(frame);
    match at {
        Some(at) => store.walk_child_exprs(at, |it| stack.push(it)),
        None => match owner {
            InferBodyId::DefWithBodyId(def) => {
                stack.push(hir_def::expr_store::Body::of(db, def).root_expr())
            }
            InferBodyId::AnonConstId(konst) => stack.push(konst.loc(db).expr),
        },
    }
    while let Some(expr) = stack.pop() {
        match store.expr(expr) {
            // The kinds MIR lowering makes a body of.
            Expr::Closure {
                closure_kind:
                    ClosureKind::Closure
                    | ClosureKind::Coroutine { kind: CoroutineKind::Async, .. }
                    | ClosureKind::CoroutineClosure(CoroutineKind::Async),
                ..
            } => match infer.expr_ty(expr).kind() {
                TyKind::Closure(id, _) => out.push(Nested::Closure(id.0)),
                TyKind::CoroutineClosure(id, _) => out.push(Nested::Closure(id.0.as_closure(db))),
                TyKind::Coroutine(id, _) => out.push(Nested::Coroutine(id.0)),
                _ => {}
            },
            Expr::Closure { .. } => {}
            Expr::Const(_)
                if let Some(konst) = hir_ty_mir::mir::inline_const_at(db, owner, infer, expr) =>
            {
                out.push(Nested::Const(konst))
            }
            _ => store.walk_child_exprs(expr, |it| stack.push(it)),
        }
    }
}

/// The `const { .. }` blocks `body` evaluates, in the order it does.
fn inline_consts<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    mut found: impl FnMut(hir_ty::db::AnonConstId<'db>),
) {
    use hir_ty::next_solver::{AnonConstIdWrapper, ConstKind};
    use hir_ty_mir::mir::{OperandKind, Rvalue, StatementKind};
    use rustc_type_ir::{AliasConstKind, inherent::IntoKind};
    // As in `coroutines`: the caller drops one seen before.
    let mut last = None;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Use(operand)) = &statement.kind
                && let OperandKind::Constant { konst, .. } = &operand.kind
                && let ConstKind::Alias(_, unevaluated) = konst.as_ref().kind()
                && let AliasConstKind::Anon { def_id: AnonConstIdWrapper(id) } = unevaluated.kind
                && id.is_inline_const(db)
                && last != Some(id)
            {
                last = Some(id);
                found(id);
            }
        }
    }
}

/// What the checks ask of the types and the source of a body.
pub(crate) struct Ctx<'a, 'db> {
    pub(crate) db: &'db Db,
    pub(crate) owner: InferBodyId<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnv<'db>,
    pub(crate) krate: Crate,
    pub(crate) store: &'a ExpressionStore,
    #[allow(dead_code)]
    pub(crate) infer: &'a InferenceResult<'db>,
    #[allow(dead_code)]
    pub(crate) mode: Mode,
    /// The user variable each local is, if it is one.
    pub(crate) bindings: FMap<'a, LocalId, BindingId>,
    /// In the body of a closure, its captures, by field of the closure.
    pub(crate) upvars: FVec<'a, Upvar>,
    /// In the body of a closure, how it is called.
    pub(crate) closure_kind: Option<rustc_type_ir::ClosureKind>,
    pub(crate) closure_local: Option<LocalId>,
    /// The closure or `async` block this is the body of.
    pub(crate) closure_expr: Option<ExprId>,
    /// This is the body of a `const { .. }` block, whose type's lifetimes are its function's.
    pub(crate) inline_const: bool,
    /// What the checks read of `store`, found in one pass for every body of the owner.
    pub(crate) scan: &'a scan::Scan<'a, 'db>,
    /// A where clause of a call does not hold, rustc's E0277, which rust-analyzer does not
    /// report: rustc does not borrow check a body whose type check fails.
    pub(crate) tainted: std::cell::Cell<bool>,
    copy: RefCell<FMap<'a, Ty<'db>, bool>>,
    glue: RefCell<FMap<'a, Ty<'db>, hir_ty::drop::DropGlue>>,
    /// The type of every place the checks name, each found once.
    pub(crate) places: RefCell<places::Places<'a, 'db>>,
    /// The body's task's frame: what the checks of this body build.
    pub(crate) frame: &'a Frame,
}

/// A capture of a closure, in its body.
pub(crate) struct Upvar {
    pub(crate) binding: BindingId,
    /// The local lowering copies the capture into.
    pub(crate) local: LocalId,
    pub(crate) by_ref: bool,
    pub(crate) mutable: bool,
    /// The captured place is behind a reference.
    pub(crate) behind_deref: bool,
    pub(crate) place: CapturedPlace,
}

impl<'a, 'db> Ctx<'a, 'db> {
    fn new(
        db: &'db Db,
        owner: InferBodyId<'db>,
        store: &'a ExpressionStore,
        infer: &'a InferenceResult<'db>,
        scan: &'a scan::Scan<'a, 'db>,
        body: &MirBody<'db>,
        closure: Option<ExprId>,
        mode: Mode,
        frame: &'a Frame,
    ) -> Self {
        let krate = owner.krate(db);
        let interner = DbInterner::new_with(db, krate).with_regions();
        let env = normalized_env(interner, db.trait_environment(owner.generic_def(db)));
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let mut bindings = mem::map(frame);
        bindings.reserve(body.binding_locals.values().count());
        bindings.extend(body.binding_locals.iter().map(|(binding, &local)| (local, binding)));
        let mut upvars = Vec::new_in(frame);
        let mut closure_kind = None;
        let mut closure_local = None;
        if let Some(expr) = closure {
            closure_kind = closure_kind_of(infer, expr);
            closure_local = body.param_locals.first().copied();
            if let Some(data) = infer.closures_data.get(&expr) {
                let mut locals = mem::collect(
                    frame,
                    body.upvar_locals.values().flatten().map(|(local, _)| *local),
                );
                radix::sort_copy_by_key(&mut locals, |it| it.into_raw().into_u32());
                for (capture, local) in data.min_captures.values().flatten().zip(locals) {
                    upvars.push(Upvar {
                        binding: capture.captured_local(),
                        local,
                        by_ref: capture.is_by_ref(),
                        mutable: capture.mutability == rustc_ast_ir::Mutability::Mut,
                        behind_deref: capture.place.deref_tys().any(|ty| {
                            !matches!(
                                rustc_type_ir::inherent::IntoKind::kind(ty),
                                hir_ty::next_solver::TyKind::Adt(def, _) if def.is_box()
                            )
                        }),
                        // clone: hir-ty's `CapturedPlace`, once per capture per closure body
                        place: capture.clone(),
                    });
                }
            }
        }
        Ctx {
            db,
            owner,
            infcx,
            env,
            krate,
            store,
            infer,
            mode,
            bindings,
            upvars,
            closure_kind,
            closure_local,
            closure_expr: closure,
            inline_const: false,
            scan,
            tainted: std::cell::Cell::new(false),
            copy: RefCell::new(mem::map(frame)),
            glue: RefCell::new(mem::map(frame)),
            places: RefCell::new(places::Places::new(frame, body)),
            frame,
        }
    }

    /// Whether reading the discriminant of `place` at `span` is a cast of an enum, which moves
    /// it, as rustc casts the enum value itself (`*e as usize` is a move out of `*e`).
    pub(crate) fn cast_moves(
        &self,
        body: &MirBody<'db>,
        place: &StoredPlace,
        span: MirSpan,
    ) -> bool {
        matches!(span, MirSpan::ExprId(expr) if matches!(self.store.expr(expr), hir_def::hir::Expr::Cast { .. }))
            && !self.is_copy(self.place_ty(body, place.local, place.projection.as_slice()).ty)
    }

    /// Whether moving `local` with `projection` at `span` moves out of a variable its `match`
    /// arm's guard is being evaluated for (rustc's "cannot move out of `x` in pattern guard").
    pub(crate) fn moves_out_of_guard(
        &self,
        local: LocalId,
        projection: &[PlaceElem],
        span: MirSpan,
    ) -> bool {
        let MirSpan::ExprId(expr) = span else { return false };
        let Some(binding) = self.bindings.get(&local) else { return false };
        !projection.iter().any(|it| matches!(it, hir_ty_mir::mir::ProjectionElem::Deref))
            && self.scan.guard_bindings(expr).contains(binding)
    }

    pub(crate) fn place_ty(
        &self,
        _body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> PlaceTy<'db> {
        let place = self.place(local, projection);
        self.places.borrow().ty(place)
    }

    /// The place `local.projection` in the body's place column, made if new.
    pub(crate) fn place(&self, local: LocalId, projection: &[PlaceElem]) -> places::PlaceId {
        self.places.borrow_mut().intern(local, projection, |ty, elem| {
            ty.projection_ty(&self.infcx, elem, self.env)
        })
    }

    pub(crate) fn project(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> PlaceTy<'db> {
        ty.projection_ty(&self.infcx, elem, self.env)
    }

    /// Whether a value of `ty` neither needs dropping nor has interior mutability, which a
    /// constant must to be promoted.
    pub(crate) fn is_plain(&self, ty: Ty<'db>) -> bool {
        self.drop_glue(ty) == hir_ty::drop::DropGlue::None
            && hir_ty::drop::is_freeze(&self.infcx, ty, self.env)
    }

    /// Whether dropping a value of `ty` runs code, as [`Self::is_copy`] asks once per type
    /// modulo regions, which drop glue does not depend on.
    pub(crate) fn drop_glue(&self, ty: Ty<'db>) -> hir_ty::drop::DropGlue {
        use hir_ty::drop::DropGlue;
        use rustc_type_ir::TypeVisitableExt as _;
        // The type as given first: folding its regions interns a new type on every call.
        if let Some(&known) = self.glue.borrow().get(&ty) {
            return known;
        }
        let interner = self.infcx.interner;
        let erased = rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner));
        let cached = self.glue.borrow().get(&erased).copied();
        let known = match cached {
            Some(known) => known,
            None if erased.has_non_region_infer() => {
                hir_ty::drop::has_drop_glue(&self.infcx, erased, self.env)
            }
            None => match *drop_glue_query(
                self.db,
                hir_ty::ParamEnvAndCrate { param_env: self.env, krate: self.krate }.store(),
                erased.store(),
            ) {
                0 => DropGlue::None,
                1 => DropGlue::DependOnParams,
                _ => DropGlue::HasDropGlue,
            },
        };
        let mut glue = self.glue.borrow_mut();
        glue.insert(erased, known);
        if erased != ty {
            glue.insert(ty, known);
        }
        known
    }

    pub(crate) fn has_destructor(&self, adt: hir_def::AdtId) -> bool {
        hir_ty::drop::destructor(self.db, adt).is_some()
    }

    pub(crate) fn is_copy(&self, ty: Ty<'db>) -> bool {
        // Modulo regions, so asked once per type whatever its free regions are: each local's
        // type has region variables of its own. The type as given first: folding its regions
        // interns a new type on every call.
        if let Some(&known) = self.copy.borrow().get(&ty) {
            return known;
        }
        let interner = self.infcx.interner;
        let erased = rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner));
        let cached = self.copy.borrow().get(&erased).copied();
        let known = match cached {
            Some(known) => known,
            None => match rustc_type_ir::TypeVisitableExt::has_non_region_infer(&erased) {
                true => self.infcx.type_is_copy_modulo_regions(self.env, erased),
                false => *is_copy_query(
                    self.db,
                    hir_ty::ParamEnvAndCrate { param_env: self.env, krate: self.krate }.store(),
                    erased.store(),
                ),
            },
        };
        let mut copy = self.copy.borrow_mut();
        copy.insert(erased, known);
        if erased != ty {
            copy.insert(ty, known);
        }
        known
    }

    /// rustc's `implements_clone`: `ty` is `Clone` modulo regions. Asked only as an error is
    /// reported.
    pub(crate) fn implements_clone(&self, ty: Ty<'db>) -> bool {
        use hir_ty::next_solver::infer::select::EvaluationResult;
        let interner = self.infcx.interner;
        let Some(clone) = interner.lang_items().Clone else { return false };
        let ty = rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner));
        matches!(
            self.infcx.type_implements_trait(clone, [ty], self.env),
            EvaluationResult::EvaluatedToOk | EvaluationResult::EvaluatedToOkModuloRegions
        )
    }

    /// The place `operand` moves out of. Lowering says a value a closure captures by value is
    /// moved even when its type is `Copy`.
    pub(crate) fn moved_place<'o>(
        &self,
        body: &MirBody<'db>,
        operand: &'o Operand,
    ) -> Option<&'o StoredPlace> {
        let OperandKind::Move(place) = &operand.kind else { return None };
        let ty = self.place_ty(body, place.local, place.projection.as_slice()).ty;
        (!self.is_copy(ty)).then_some(place)
    }

    /// Whether `local` is a variable declared without `mut`, which can only be assigned once.
    pub(crate) fn is_immutable(&self, local: LocalId) -> bool {
        if self.upvars.iter().any(|it| it.local == local) {
            return false;
        }
        match self.bindings.get(&local) {
            Some(&binding) => self.store[binding].mode != BindingAnnotation::Mutable,
            None => false,
        }
    }

    pub(crate) fn upvar_of_local(&self, local: LocalId) -> Option<usize> {
        self.upvars.iter().position(|it| it.local == local)
    }
}

/// Whether `ty`, its regions `'static`, is `Copy` in `env`: a query, as what it asks of the
/// solver can be long (a future through every type it holds) and is the same in every body of
/// the environment, and after an edit.
#[blaze_query::tracked]
fn is_copy_query(
    db: &Db,
    env: hir_ty::traits::StoredParamEnvAndCrate,
    ty: hir_ty::next_solver::StoredTy,
) -> bool {
    let env = env.as_ref(db);
    let interner = DbInterner::new_with(db, env.krate).with_regions();
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    infcx.type_is_copy_modulo_regions(env.param_env, ty.as_ref())
}

/// [`hir_ty::drop::has_drop_glue`] of `ty`, its regions `'static`, in `env`, as the order of
/// its variants: a query as [`is_copy_query`] is.
#[blaze_query::tracked]
fn drop_glue_query(
    db: &Db,
    env: hir_ty::traits::StoredParamEnvAndCrate,
    ty: hir_ty::next_solver::StoredTy,
) -> u8 {
    let env = env.as_ref(db);
    let interner = DbInterner::new_with(db, env.krate).with_regions();
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    hir_ty::drop::has_drop_glue(&infcx, ty.as_ref(), env.param_env) as u8
}

/// `env` with the projections in its where clauses that name no parameter normalized, as
/// rustc normalizes a param env before it uses it. Unnormalized, a where clause
/// `<Option<&'static C> as Q>::ReadOnly: Q` is a candidate for every `X: Q`, and normalizing it
/// asks those where clauses again: the solver gives up and leaves `<X as Q>::Item` rigid.
fn normalized_env<'db>(interner: DbInterner<'db>, env: ParamEnv<'db>) -> ParamEnv<'db> {
    use hir_ty::next_solver::{
        Clauses, infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt,
    };
    use rustc_type_ir::TypeVisitableExt as _;
    let global = |clause: &hir_ty::next_solver::Clause<'db>| {
        clause.has_aliases() && !clause.has_param() && !clause.has_infer()
    };
    if !env.clauses().iter().any(|it| global(&it)) {
        return env;
    }
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let clauses = env.clauses().iter().map(|clause| {
        if !global(&clause) {
            return clause;
        }
        let ocx = ObligationCtxt::new(&infcx);
        match ocx.deeply_normalize(&ObligationCause::dummy(), ParamEnv::empty(), clause) {
            Ok(normalized) if !normalized.has_infer() => normalized,
            _ => clause,
        }
    });
    blaze_mem::frame::current(|frame| {
        ParamEnv::new(Clauses::new_from_slice(frame.alloc_slice_from_iter(clauses)))
    })
}

fn closure_kind_of(
    infer: &InferenceResult<'_>,
    expr: ExprId,
) -> Option<rustc_type_ir::ClosureKind> {
    use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
    match infer.expr_ty(expr).kind() {
        hir_ty::next_solver::TyKind::Closure(_, args) => Some(args.as_closure().kind()),
        hir_ty::next_solver::TyKind::CoroutineClosure(_, args) => {
            Some(args.as_coroutine_closure().kind())
        }
        _ => None,
    }
}

/// The local a body returns in, as `hir_ty_mir::mir`'s own private `return_slot`.
pub(crate) fn return_slot() -> LocalId {
    LocalId::from_raw(la_arena::RawIdx::from(0))
}

/// Whether `rvalue` compares, which takes its operands by reference whatever their type:
/// rust-analyzer lowers a constant pattern of a type that is not `Copy` to `==` on the value.
pub(crate) fn compares(rvalue: &hir_ty_mir::mir::Rvalue) -> bool {
    use hir_ty_mir::mir::{BinOp, Rvalue};
    match rvalue {
        Rvalue::BinaryOp(op, ..) | Rvalue::CheckedBinaryOp(op, ..) => {
            matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
        }
        _ => false,
    }
}
