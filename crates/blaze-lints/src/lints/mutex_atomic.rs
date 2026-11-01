//! `mutex_atomic` and `mutex_integer`: a `Mutex` around one `bool`, integer or `*mut T`, where
//! an atomic does the same without a lock. AGENTS.md rule 9: a counter or flag is an atomic.
//!
//! Follows clippy's `clippy_lints/src/mutex_atomic.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a `static` or a `let` written in the
//! file whose value's type is a `Mutex` of a `bool`, an integer of at most 64 bits, a `usize`,
//! an `isize` or a `*mut T`; `mutex_integer` for the fixed-size integers, `mutex_atomic` for the
//! rest; `Mutex::new(arg)` is rewritten to `std::sync::atomic::AtomicX::new(arg)`, a static's
//! type with it and a `let`'s type removed. Unlike clippy, `lock_api`'s `Mutex` (`parking_lot`'s)
//! counts too, the message names the binding, and the fix is machine-applicable when it is
//! whole: a `let` written in the file whose every use is a read `*m.lock()` (std's with its
//! `.unwrap()` or `.expect(..)`), rewritten to `m.load(SeqCst)`, or an assignment
//! `*m.lock() = e`, rewritten to `m.store(e, SeqCst)`. Any other use (a reference, a compound
//! assignment, a `match` on it, a use in a macro's output) leaves the fix maybe incorrect, its
//! uses named in a note. A static's fix is maybe incorrect: its uses are everywhere.

use hir::HirDisplay as _;
use syntax::{
    SyntaxKind as K, TextRange,
    view::{self as ast, AstNode as _, HasArgList as _, HasName as _},
};

use super::{
    Implemented,
    box_collection::{Std, std_adt},
    utils,
};
use crate::{blaze::facts, check::Cx, diag::Applicability};

/// clippy's `MUTEX_ATOMIC`: a `bool`, `usize`, `isize` or `*mut T`.
const ATOMIC: Implemented = Implemented::MutexAtomic;
/// clippy's `MUTEX_INTEGER`: a fixed-size integer.
const INTEGER: Implemented = Implemented::MutexInteger;

/// The ordering the fix's loads and stores use: a lock's, the strongest.
const SEQ_CST: &str = "std::sync::atomic::Ordering::SeqCst";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lock {
    /// `std::sync::Mutex`, whose `lock` returns a `LockResult`.
    Std,
    /// `lock_api::Mutex`, whose `lock` returns the guard.
    LockApi,
}

pub(super) fn check_let<'t>(cx: &mut Cx<'_, 't>, stmt: ast::LetStmt<'t>) {
    if !cx.on_any(ATOMIC.bit() | INTEGER.bit()) || stmt.let_else().is_some() {
        return;
    }
    let Some(init) = stmt.initializer() else { return };
    let binding = match stmt.pat() {
        Some(ast::Pat::IdentPat(it)) if it.pat().is_none() && it.ref_token().is_none() => Some(it),
        _ => None,
    };
    check(cx, stmt.syntax(), init, Ascription::Optional(stmt), binding);
}

pub(super) fn check_static<'t>(cx: &mut Cx<'_, 't>, item: ast::Static<'t>) {
    if !cx.on_any(ATOMIC.bit() | INTEGER.bit()) {
        return;
    }
    let (Some(init), Some(ty)) = (item.body(), item.ty()) else { return };
    check(cx, item.syntax(), init, Ascription::Required(item, ty), None);
}

/// What the fix does with the written type: clippy's `TypeAscriptionKind`.
#[derive(Clone, Copy)]
enum Ascription<'t> {
    /// A static's type, replaced by the atomic's.
    Required(ast::Static<'t>, ast::Type<'t>),
    /// A `let`'s type, if written, removed: the atomic's type is concrete.
    Optional(ast::LetStmt<'t>),
}

fn check<'t>(
    cx: &mut Cx<'_, 't>,
    at: ast::Node<'t>,
    init: ast::Expr<'t>,
    ascription: Ascription<'t>,
    binding: Option<ast::IdentPat<'t>>,
) {
    let Some(ty) = cx.ty(init) else { return };
    let Some((lock, inner)) = guarded(cx, &ty) else { return };
    let Some((atomic, fixed_size)) = atomic_of(&inner) else { return };
    let lint = if fixed_size { INTEGER } else { ATOMIC };
    if !cx.on(lint) {
        return;
    }
    let init = utils::peel_blocks(init);
    let Some(range) = cx.range(init.syntax()) else { return };
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let shown = ty.display(db, target).to_string();
    let inner_shown = inner.display(db, target).to_string();
    let named = match ascription {
        Ascription::Required(item, _) => item.name().map_or("this static".into(), |it| format!("static `{}`", it.text())),
        Ascription::Optional(_) => binding.and_then(|it| it.name()).map_or("this value".into(), |it| format!("`{}`", it.text())),
    };
    let mut edits = Vec::new(); // alloc: the edits of one finding
    let mut whole = matches!(ascription, Ascription::Optional(_))
        && std::ptr::eq(at.file(), cx.root().file())
        && binding.is_some_and(|it| it.mut_token().is_none());
    let mut blocked = Vec::new(); // alloc: as `edits`
    if let Some(arg) = new_arg(init) {
        edits.push((range, format!("std::sync::atomic::{atomic}::new({})", cx.source_of(arg.syntax()))));
        match ascription {
            Ascription::Required(_, ty) => {
                if let Some(ty) = cx.range(ty.syntax()) {
                    edits.push((ty, format!("std::sync::atomic::{atomic}")));
                }
            }
            Ascription::Optional(stmt) => {
                if let (Some(colon), Some(ty)) = (stmt.colon_token(), stmt.ty())
                    && let Some(ty) = cx.range(ty.syntax())
                {
                    let start = stmt.pat().and_then(|it| cx.range(it.syntax())).map_or(colon.text_range().start(), |it| it.end());
                    edits.push((TextRange::new(start, ty.end()), String::new()));
                }
            }
        }
        if whole && let Some(pat) = binding {
            uses(cx, at, pat, lock, &mut edits, &mut blocked);
            whole = blocked.is_empty();
        }
    } else {
        whole = false;
    }
    let span = cx.span(range);
    let message = format!(
        "{named} is a `{shown}` around one `{inner_shown}`: `{atomic}` does that without a lock"
    );
    let notes: Vec<_> = blocked.iter().map(|it| cx.span(*it)).collect(); // alloc: as `edits`
    let edits: Vec<_> = edits.into_iter().map(|(at, text)| (cx.span(at), text)).collect(); // alloc: as `edits`
    let diagnostic = cx.lint(lint, at, span, message);
    if edits.is_empty() {
        diagnostic.help(format!("use a `std::sync::atomic::{atomic}`"));
    } else {
        let applicability = if whole { Applicability::MachineApplicable } else { Applicability::MaybeIncorrect };
        diagnostic.multipart_suggestion(format!("use a `{atomic}`"), edits, applicability);
    }
    for note in notes {
        diagnostic.span_note(note, "a use the fix can't rewrite to `load` or `store`");
    }
    if matches!(ascription, Ascription::Required(..)) {
        diagnostic.note("the static's uses are not rewritten: each `lock()` becomes a `load` or `store`");
    }
    diagnostic.help("if the lock orders a critical section rather than guarding the value, `Mutex<()>` says so");
}

/// The lock and the type it guards, when `ty` is std's or `lock_api`'s `Mutex`.
fn guarded<'q>(cx: &Cx<'q, '_>, ty: &hir::Type<'q>) -> Option<(Lock, hir::Type<'q>)> {
    if ty.is_reference() {
        return None;
    }
    let adt = ty.as_adt()?;
    if std_adt(cx, adt) == Some(Std::Mutex) {
        return Some((Lock::Std, ty.type_arguments().next()?));
    }
    if facts::adt_is(cx.db(), adt, "lock_api", "Mutex") {
        return Some((Lock::LockApi, ty.type_arguments().nth(1)?));
    }
    None
}

/// clippy's `get_atomic_name`, and whether it is `mutex_integer`'s (a fixed-size integer).
/// 128-bit atomics are unstable and on few targets: none.
fn atomic_of(ty: &hir::Type<'_>) -> Option<(&'static str, bool)> {
    if ty.is_mutable_raw_ptr() {
        return Some(("AtomicPtr", false));
    }
    let builtin = ty.as_builtin()?;
    if builtin.is_bool() {
        return Some(("AtomicBool", false));
    }
    let atomic = match builtin.name().as_str() {
        "u8" => "AtomicU8",
        "u16" => "AtomicU16",
        "u32" => "AtomicU32",
        "u64" => "AtomicU64",
        "i8" => "AtomicI8",
        "i16" => "AtomicI16",
        "i32" => "AtomicI32",
        "i64" => "AtomicI64",
        "usize" => return Some(("AtomicUsize", false)),
        "isize" => return Some(("AtomicIsize", false)),
        _ => return None,
    };
    Some((atomic, true))
}

/// `arg` of `Mutex::new(arg)`, `Mutex` any path.
fn new_arg(init: ast::Expr<'_>) -> Option<ast::Expr<'_>> {
    let ast::Expr::CallExpr(call) = init else { return None };
    let ast::Expr::PathExpr(callee) = call.expr()? else { return None };
    let path = callee.path()?;
    if path.qualifier().is_none() || path.segment()?.name_ref()?.text() != "new" {
        return None;
    }
    let mut args = call.arg_list()?.args();
    let arg = args.next()?;
    args.next().is_none().then_some(arg)
}

/// The rewrite of each use of the local `pat` binds after `stmt`, added to `edits`; where each
/// use that is not a load or store shape is, added to `blocked`.
fn uses<'t>(
    cx: &Cx<'_, 't>,
    stmt: ast::Node<'t>,
    pat: ast::IdentPat<'t>,
    lock: Lock,
    edits: &mut Vec<(TextRange, String)>,
    blocked: &mut Vec<TextRange>,
) {
    let Some(local) = cx.local(pat) else {
        blocked.push(pat.syntax().text_range());
        return;
    };
    let root = cx.root();
    let after = stmt.text_range().end();
    utils::each_path(cx, root, Some(after), &mut |path| {
        if utils::local_of(cx, path) != Some(local) {
            return;
        }
        let in_file = std::ptr::eq(path.syntax().file(), root.file());
        match in_file.then(|| rewrite(cx, path, lock)).flatten() {
            Some(edit) => edits.push(edit),
            None => blocked.push(cx.range(path.syntax()).unwrap_or(pat.syntax().text_range())),
        }
    });
}

/// `*m.lock()` read, to `m.load(..)`, or assigned, to `m.store(e, ..)`: the range rewritten and
/// its new text.
fn rewrite<'t>(cx: &Cx<'_, 't>, path: ast::PathExpr<'t>, lock: Lock) -> Option<(TextRange, String)> {
    let name = path.syntax().text();
    let call = ast::MethodCallExpr::cast(path.syntax().parent()?)?;
    if call.receiver()?.syntax() != path.syntax()
        || call.name_ref()?.text() != "lock"
        || call.arg_list()?.args().next().is_some()
    {
        return None;
    }
    let mut guard = call.syntax();
    if lock == Lock::Std {
        let unwrap = ast::MethodCallExpr::cast(guard.parent()?)?;
        let args = unwrap.arg_list()?.args().count();
        let ok = match unwrap.name_ref()?.text() {
            "unwrap" => args == 0,
            "expect" => args == 1,
            _ => false,
        };
        if !ok || unwrap.receiver()?.syntax() != guard {
            return None;
        }
        guard = unwrap.syntax();
    }
    let deref = ast::PrefixExpr::cast(guard.parent()?)?;
    if deref.op_kind() != Some(ast::UnaryOp::Deref) {
        return None;
    }
    let parent = deref.syntax().parent()?;
    if let Some(assign) = ast::BinExpr::cast(parent)
        && assign.op_kind() == Some(ast::BinaryOp::Assignment { op: None })
        && assign.lhs()?.syntax() == deref.syntax()
    {
        let value = cx.source_of(assign.rhs()?.syntax());
        return Some((assign.syntax().text_range(), format!("{name}.store({value}, {SEQ_CST})")));
    }
    is_read(parent, deref.syntax()).then(|| (deref.syntax().text_range(), format!("{name}.load({SEQ_CST})")))
}

/// Whether `expr`, a child of `parent`, is only read for its value there, the guard dropped at
/// the end of the statement as an atomic's load would be.
fn is_read(parent: ast::Node<'_>, expr: ast::Node<'_>) -> bool {
    match parent.kind() {
        K::LET_STMT => ast::LetStmt::cast(parent).is_some_and(|it| it.let_else().is_none()),
        K::ARG_LIST | K::RETURN_EXPR | K::PAREN_EXPR | K::ARRAY_EXPR | K::TUPLE_EXPR | K::RECORD_EXPR_FIELD | K::STMT_LIST => true,
        K::IF_EXPR | K::WHILE_EXPR => true,
        K::PREFIX_EXPR => ast::PrefixExpr::cast(parent)
            .is_some_and(|it| matches!(it.op_kind(), Some(ast::UnaryOp::Not | ast::UnaryOp::Neg))),
        K::BIN_EXPR => ast::BinExpr::cast(parent).is_some_and(|it| match it.op_kind() {
            Some(ast::BinaryOp::Assignment { .. }) => it.rhs().is_some_and(|rhs| rhs.syntax() == expr),
            Some(_) => true,
            None => false,
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::{fixed, lint_only};

    const FOUND: &str = "does that without a lock";

    #[test]
    fn a_lock_around_a_flag_or_size_is_mutex_atomic_and_around_an_integer_mutex_integer() {
        let main = r#"
use std::sync::Mutex;
pub static FLAG: Mutex<bool> = Mutex::new(false);
pub fn f() {
    let size = Mutex::new(0usize);
    let count: Mutex<u32> = Mutex::new(0);
    let wide = Mutex::new(0u128);
    let many = Mutex::new([0u8; 4]);
    let _ = (size, count, wide, many);
}
"#;
        let atomic = lint_only("mutex_atomic", main);
        assert_eq!(atomic.matches(FOUND).count(), 2, "{atomic}");
        assert!(atomic.contains("static `FLAG` is a `Mutex<bool>` around one `bool`: `AtomicBool`"), "{atomic}");
        assert!(atomic.contains("`size` is a `Mutex<usize>`"), "{atomic}");
        let integer = lint_only("mutex_integer", main);
        assert_eq!(integer.matches(FOUND).count(), 1, "{integer}");
        assert!(integer.contains("`count` is a `Mutex<u32>` around one `u32`: `AtomicU32`"), "{integer}");
    }

    #[test]
    fn parking_lots_mutex_through_a_renamed_import_and_one_a_macro_is_handed_are_found() {
        let found = lint_only(
            "mutex_atomic",
            r#"
use parking_lot::Mutex as Lock;
macro_rules! stmt { ($($t:tt)*) => { $($t)* }; }
pub fn f() {
    let a = Lock::new(true);
    stmt! { let b = Lock::new(false); }
    let _ = (a, b);
}
"#,
        );
        assert!(found.matches(FOUND).count() >= 1, "{found}");
        assert!(found.contains("`a` is a `Mutex<"), "{found}");
        assert!(found.contains("around one `bool`: `AtomicBool`"), "{found}");
    }

    #[test]
    fn loads_and_stores_of_a_local_are_rewritten() {
        let after = fixed(
            "mutex_atomic",
            r#"
pub fn f() -> bool {
    let done = std::sync::Mutex::new(false);
    *done.lock().unwrap() = true;
    let now = *done.lock().expect("not poisoned");
    now && *done.lock().unwrap()
}
"#,
        );
        assert_eq!(
            after,
            "pub fn f() -> bool {\n    let done = std::sync::atomic::AtomicBool::new(false);\n    done.store(true, std::sync::atomic::Ordering::SeqCst);\n    let now = done.load(std::sync::atomic::Ordering::SeqCst);\n    now && done.load(std::sync::atomic::Ordering::SeqCst)\n}\n"
        );
    }

    #[test]
    fn another_use_leaves_the_fix_maybe_incorrect() {
        let found = lint_only(
            "mutex_atomic",
            r#"
pub fn take(_: &std::sync::Mutex<bool>) {}
pub fn f() {
    let done = std::sync::Mutex::new(false);
    take(&done);
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("a use the fix can't rewrite"), "{found}");
    }
}
