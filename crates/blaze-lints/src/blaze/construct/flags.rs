//! `state_in_flags` and `bool_pair_state`: states kept as `bool`s, whose combinations the type
//! allows and the program does not.
//!
//! `state_in_flags`: a struct with two or more `bool` fields whose names are states of one
//! life (`started`, `running`, `closed`, `done`, `failed`, with or without `is_`/`has_`/`was_`).
//! Two flags are four combinations; a life of three states leaves one of them meaning nothing,
//! and every method checks the flags to find which state it is in. An enum of the states (or a
//! typestate, one type per state) has exactly the states, and a method of one state can't be
//! called in another.
//!
//! `bool_pair_state`: a field, parameter or return type `(bool, bool)` (any tuple of two or more
//! `bool`s) or `Option<bool>`, and a `match` on a tuple of two or more `bool`s. Each is three or
//! four states with no names: an enum names them, and its `match` says what each one is.
//!
//! Not looked at: test code, what a macro wrote, a trait impl's method (its signature is the
//! trait's), and a `#[repr(C)]` struct.

use hir::AsAssocItem as _;
use hir_def::hir::Expr;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use super::{Found, item_on, report, report_item, words, written};
use crate::{
    blaze::{Lint, frozen_collection, general_facts},
    check::Cx,
};

const STATES: Lint = Lint::StateInFlags;
const PAIR: Lint = Lint::BoolPairState;

/// The fewest state-word `bool` fields that make a state machine kept as flags.
const FLAGS_MIN: usize = 2;
/// The fewest `bool`s of a tuple that make unnamed states.
const PAIR_MIN: usize = 2;

/// The words a `bool` field's name says a state of a life with.
const STATE_WORDS: &[&str] = &[
    "started", "running", "stopped", "finished", "done", "closed", "open", "opened", "connected",
    "disconnected", "paused", "ready", "failed", "cancelled", "canceled", "completed", "pending",
    "active", "initialized", "shutdown", "aborted", "dead", "alive", "idle", "loaded", "spawned",
    "exited", "committed", "flushed", "sealed", "frozen", "poisoned",
];

/// Whether a field's `name` says a state of a life.
fn is_state(name: &str) -> bool {
    words(name).any(|word| STATE_WORDS.iter().any(|it| word.eq_ignore_ascii_case(it)))
}

pub(super) fn check_struct<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    let states = item_on(cx, STATES, item.syntax());
    let pair = item_on(cx, PAIR, item.syntax());
    if !states && !pair {
        return;
    }
    if item.attrs().any(|it| it.simple_name() == Some("repr")) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let owner = def.name(db);
    let fields = def.fields(db);
    if states {
        // alloc: the names of one struct's state flags
        let flags: Vec<String> = fields
            .iter()
            .filter(|it| it.ty(db).is_bool() && is_state(it.name(db).as_str()))
            .map(|it| format!("`{}`", it.name(db).as_str())) // alloc: a message's part
            .collect();
        if flags.len() >= FLAGS_MIN
            && let Some(name) = item.name()
        {
            let message = format!(
                "`{}` keeps its state in {} `bool`s ({}): {} combinations, and every method tests them to find which state it is in",
                owner.as_str(),
                flags.len(),
                flags.join(", "),
                1u64 << flags.len().min(63)
            ); // alloc: a message
            report_item(
                cx,
                STATES,
                name.syntax(),
                message,
                format!("an `enum {}State {{ .. }}` of the states it has (or one type per state)", owner.as_str()), // alloc: a message's part
                "the combinations that mean nothing, and the flag tests at the top of each method",
            );
        }
    }
    if pair {
        for (index, field) in fields.iter().enumerate() {
            let ty = field.ty(db);
            let Some(shape) = unnamed_states(db, &ty) else { continue };
            let name = field.name(db);
            let Some(node) = frozen_collection::field_node(item, &name, index) else { continue };
            let message = format!("the field `{}` of `{}` is {shape}: states with no names", name.as_str(), owner.as_str()); // alloc: a message
            report_item(cx, PAIR, node, message, enum_of(shape), "the reader's decoding of which `bool` means what");
        }
    }
}

pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !item_on(cx, PAIR, f.syntax()) || f.abi().is_some() {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return;
    }
    let fn_name = func.name(db);
    if let Some(list) = f.param_list() {
        let params = func.params_without_self(db);
        for (param, hir_param) in list.params().zip(params.iter()) {
            let Some(shape) = unnamed_states(db, hir_param.ty()) else { continue };
            let Some(ty) = param.ty() else { continue };
            let message = format!("`fn {}` takes {shape}: states with no names", fn_name.as_str()); // alloc: a message
            report_item(cx, PAIR, ty.syntax(), message, enum_of(shape), "the caller's decoding of which `bool` means what");
        }
    }
    if let Some(ret) = f.ret_type().and_then(|it| it.ty())
        && let Some(shape) = unnamed_states(db, &func.ret_type(db))
    {
        let message = format!("`fn {}` returns {shape}: states with no names", fn_name.as_str()); // alloc: a message
        report_item(cx, PAIR, ret.syntax(), message, enum_of(shape), "the caller's decoding of which `bool` means what");
    }
}

/// What unnamed states `ty` is: a tuple of [`PAIR_MIN`] or more `bool`s, or `Option<bool>`.
fn unnamed_states(db: &base_db::Db, ty: &hir::Type<'_>) -> Option<&'static str> {
    if ty.is_tuple() {
        let fields = ty.tuple_fields(db);
        return (fields.len() >= PAIR_MIN && fields.iter().all(|it| it.is_bool()))
            .then_some("a tuple of `bool`s");
    }
    let adt = ty.as_adt()?;
    let option = general_facts::std_adt(db, adt, "option", &["Option"]);
    (option && ty.type_arguments().next().is_some_and(|it| it.is_bool())).then_some("an `Option<bool>`")
}

/// The type that names the states `shape` has.
fn enum_of(shape: &str) -> String {
    match shape {
        "an `Option<bool>`" => "an enum of its three states, each named for what it means (`enum Answer { Yes, No, Unknown }`)".to_owned(), // alloc: a message's part
        _ => "an enum with one variant per state that occurs, each named for what it means".to_owned(), // alloc: a message's part
    }
}

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(PAIR) {
        return;
    }
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return };
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let Expr::Match { expr: scrutinee, .. } = expr else { continue };
        let Expr::Tuple { exprs } = body.expr(scrutinee) else { continue };
        if exprs.len() < PAIR_MIN || !written(analysis, id) {
            continue;
        }
        if !exprs.iter().all(|&it| analysis.ty(infer.expr_ty(it)).strip_references().is_bool()) {
            continue;
        }
        let Some(range) = analysis.expr_range(scrutinee) else { continue };
        found.push(Found {
            lint: PAIR,
            range,
            what: format!("this `match` decides on {} `bool`s at once: {} states with no names", exprs.len(), 1u64 << exprs.len().min(63)), // alloc: a message
            carry: "an enum with one variant per state that occurs, made once where the `bool`s are, and matched here".to_owned(), // alloc: a message's part
            removes: "the arms that decode which combination means what, repeated at each `match`",
            fix: None,
        });
    }
    report(cx, found);
}
