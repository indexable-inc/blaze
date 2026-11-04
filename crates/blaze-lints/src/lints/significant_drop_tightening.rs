//! `significant_drop_tightening`: a value whose drop matters (a lock guard, a `RefCell`
//! borrow, a type marked as such) kept past its last use, through a call that could run
//! without it.
//!
//! Follows clippy's `clippy_lints/src/significant_drop_tightening.rs` (MIT or Apache-2.0),
//! read from indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the
//! pinned nightly's (rust-lang/rust 12433c30). As in clippy: a value bound by `let` in a
//! function written in the file, used, and then held while the function goes on to other
//! calls, until its scope ends. Unlike clippy, which walks the statements of the value's
//! block, this reads the body's runtime MIR:
//!
//! - a use is any mention of the value or of one borrowed from it (a reference into it, a
//!   call's result whose type may hold a borrow of an argument that is one), not its drop at
//!   the end of its scope; so `let n = guard.len();` ends the guard's uses, and
//!   `let first = &guard[0];` keeps them going while `first` is used;
//! - the calls held through are the calls written in the source (`from_hir_call`) on a path
//!   where the value may be held and is used again on no path after; across branches and
//!   loops, a value given anew in the next round of a loop is a new value;
//! - a value never used is left alone: holding a lock for a section is what it is for;
//! - what counts: std's and `lock_api`'s lock guards and `RefCell`'s `Ref` and `RefMut`
//!   ([`guards`](super::guards)), and any type marked `#[clippy::has_significant_drop]` or
//!   `#[must_not_suspend]` (the front end reads both into `AttrFlags`); a drop that only frees
//!   memory (`Vec`, `String`, a `Box` of them) never counts;
//! - closures and `async` blocks in the function are bodies of their own, each linted.
//!
//! The fix, MachineApplicable, writes `drop(x);` on a line of its own after the statement of
//! the value's last use, where [`hir_ty_borrowck::drop_early::drop_point`] finds that point (a
//! `let` of a plain name in the function's own body, every later use in that block's
//! statements, none after) and the body with that drop lowered in passes the borrow check in
//! the database's mode; Rust's drop order is never changed, only suggested. Anywhere else the
//! finding has no fix: where the value's scope should end is the author's to say.

use hir_def::{AdtId, DefWithBodyId, attrs::AttrFlags, hir::BindingId};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{BasicBlockId, LocalId, MirBody, MirSpan, StatementKind, StoredPlace, TerminatorKind},
};
use hir_ty_borrowck::{
    BorrowckResult, Mode,
    drop_early::{derived, drop_point, statement_places, terminator_places},
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use super::{
    Implemented,
    guards::{self, Guards, Kind},
};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::SignificantDropTightening;

/// What makes a value's drop matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Significant {
    /// A lock guard or a `RefCell` borrow ([`guards`](super::guards)).
    Guard(Kind),
    /// A type marked so; the attribute, as written.
    Marked(&'static str),
}

impl Significant {
    /// What a value of `adt` is, if its drop matters. A type that only owns memory never is.
    fn of(cx: &Cx<'_, '_>, guards: &Guards, adt: hir::Adt) -> Option<Significant> {
        let db = cx.db();
        if let Some(kind) = guards.kind(db, adt) {
            return Some(Significant::Guard(kind));
        }
        let flags = AttrFlags::query(db, hir_def::AttrDefId::AdtId(AdtId::from(adt)));
        if flags.contains(AttrFlags::HAS_SIGNIFICANT_DROP) {
            Some(Significant::Marked("#[clippy::has_significant_drop]"))
        } else if flags.contains(AttrFlags::MUST_NOT_SUSPEND) {
            Some(Significant::Marked("#[must_not_suspend]"))
        } else {
            None
        }
    }

    /// What the value keeps, as the message says it: "holds the lock".
    fn holds(self) -> &'static str {
        match self {
            Significant::Guard(Kind::Lock) => "holds the lock",
            Significant::Guard(Kind::Cell) => "holds the `RefCell` borrow",
            Significant::Marked(_) => "is kept alive",
        }
    }

    /// What its drop does, so why holding it matters.
    fn why(self) -> String {
        match self {
            Significant::Guard(Kind::Lock) => {
                "other threads wait for the lock until it is dropped".to_owned()
            } // alloc: message
            Significant::Guard(Kind::Cell) => {
                "a conflicting borrow of the `RefCell` panics until it is dropped".to_owned() // alloc: message
            }
            Significant::Marked(attr) => {
                format!("its type is marked `{attr}`, so its drop does work that matters")
            }
        }
    }

    /// What the value still keeps through the call, as its note says it.
    fn held(self, name: &str) -> String {
        match self {
            Significant::Guard(kind) => format!("{} still held", kind.held()),
            Significant::Marked(_) => format!("`{name}` still alive"),
        }
    }

    /// What dropping it sooner gives.
    fn sooner(self) -> &'static str {
        match self {
            Significant::Guard(Kind::Lock) => "so other threads can take the lock sooner",
            Significant::Guard(Kind::Cell) => "so the `RefCell` can be borrowed again sooner",
            Significant::Marked(_) => "so its drop runs sooner",
        }
    }
}

/// One value held past its last use, before it is reported.
struct Held {
    /// Where the value is given.
    at: TextRange,
    /// The first call it is held through.
    call: TextRange,
    message: String,
    name: String,
    significant: Significant,
    /// Where `drop(name);` goes and its text, when it is checked to hold.
    fix: Option<(TextRange, String)>,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(function) = ast::Fn::cast(node) else { return };
    if function.body().is_none() || !cx.is_written(node) {
        return;
    }
    let mut held = find(cx, function);
    held.sort_by_key(|it| (it.at.start(), it.at.end()));
    for it in held {
        let span = cx.span(it.at);
        let call = cx.span(it.call);
        let fix = it.fix.map(|(at, text)| (cx.span(at), text));
        let diagnostic = cx.lint(LINT, node, span, it.message);
        diagnostic
            .span_note(call, format!("this call runs with {}", it.significant.held(&it.name)));
        diagnostic.help(format!(
            "drop `{0}` after its last use (`drop({0});`), or end its scope with a block, {1}",
            it.name,
            it.significant.sooner()
        ));
        if let Some(part) = fix {
            // alloc: the fix's one part
            let parts = vec![part];
            diagnostic.multipart_suggestion(
                format!("drop `{}` here, after its last use", it.name),
                parts,
                Applicability::MachineApplicable,
            );
        }
    }
}

fn find<'t>(cx: &Cx<'_, 't>, function: ast::Fn<'t>) -> Vec<Held> {
    // alloc: the values found in one function, reported at the edge
    let mut out = Vec::new();
    let Some(f) = cx.fn_def(function) else { return out };
    let Some(analysis) = cx.analysis() else { return out };
    // The walk of a body meets the functions nested in it, each linted in its own body's query.
    if analysis.def() != hir::DefWithBody::from(f) {
        return out;
    }
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return out };
    let db = cx.db();
    let guards = Guards::new(cx);
    let function_name = guards::function_name(cx, analysis);
    // Whether the function's body as written passes the borrow check, asked once.
    let mut checked = None;
    for body in guards::bodies(db, analysis, def) {
        let mir = body.mir;
        let owner = match body.expr {
            None => function_name.clone(), // clone: once per body
            Some(_) if body.coroutine => format!("an `async` block in {function_name}"),
            Some(_) => format!("a closure in {function_name}"),
        };
        for (binding, &local) in mir.binding_locals.iter() {
            let name = analysis.body()[binding].name.as_str();
            if name.starts_with('_') {
                continue;
            }
            let ty = analysis.ty(mir.locals[local].ty.as_ref());
            let Some(significant) = ty.as_adt().and_then(|adt| Significant::of(cx, &guards, adt))
            else {
                continue;
            };
            let Some(call) = held_through(mir, local) else { continue };
            let Some(at) =
                guards::first_assigned(mir, local).and_then(|it| guards::range_of(analysis, it))
            else {
                continue;
            };
            let Some(call) = guards::range_of(analysis, call) else { continue };
            let message = format!(
                "{} {} in {owner} after its last use: {}",
                guards::say_local(cx, analysis, mir, local),
                significant.holds(),
                significant.why()
            );
            // A closure's or `async` block's body is lowered on its own: the fix is the
            // function's own body's.
            let fix = match body.expr {
                None if *checked
                    .get_or_insert_with(|| passes(hir_ty_borrowck::borrowck(db, def))) =>
                {
                    fix(cx, analysis, def, binding, name, call)
                }
                _ => None,
            };
            // alloc: reported at the edge
            out.push(Held { at, call, message, name: name.to_owned(), significant, fix });
        }
    }
    out
}

/// Where `drop(name);` goes for `binding` of `def`'s own body, and its text: after the
/// statement of its last use, before `call`, written in the file, and only when the body with
/// that drop lowered in passes the borrow check.
fn fix(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    def: DefWithBodyId,
    binding: BindingId,
    name: &str,
    call: TextRange,
) -> Option<(TextRange, String)> {
    let db = cx.db();
    let mode = hir_ty_borrowck::mode(db);
    if mode == Mode::Off {
        return None;
    }
    let mir = db.mir_body(InferBodyId::DefWithBodyId(def)).ok()?;
    let point = drop_point(db, def, mir, binding).ok()?;
    // Both written in the file the function is, not by a macro: a macro's statements are not
    // the file's.
    let (_, file) = analysis.tree();
    let written = |span: MirSpan| -> Option<TextRange> {
        let map = &analysis.source_map().store;
        let source = match span {
            MirSpan::ExprId(it) => map.expr_syntax(it).ok()?,
            MirSpan::PatId(it) => map.pat_syntax(it).ok()?,
            _ => return None,
        };
        (source.file_id == file).then(|| source.value.syntax_node_ptr().text_range())
    };
    let (after, block) = (written(point.after)?, written(MirSpan::ExprId(point.block))?);
    let root = cx.root();
    if !root.text_range().contains_range(after) {
        return None;
    }
    let node = match root.covering_element(after) {
        NodeOrToken::Node(node) => node,
        NodeOrToken::Token(token) => token.parent()?,
    };
    let statement = node
        .ancestors()
        .find(|it| matches!(it.kind(), SyntaxKind::LET_STMT | SyntaxKind::EXPR_STMT))?;
    // One of the block's own statements, and the call held through after it.
    let list = statement.parent().filter(|it| it.kind() == SyntaxKind::STMT_LIST)?;
    if list.parent()?.text_range() != block || call.start() < statement.text_range().end() {
        return None;
    }
    if !passes(&hir_ty_borrowck::drop_early::check(db, def, &point, mode).ok()?) {
        return None;
    }
    let indent = statement
        .prev_sibling_or_token()
        .and_then(|it| it.into_token())
        .filter(|it| it.kind() == SyntaxKind::WHITESPACE)
        .and_then(|it| it.text().rsplit_once('\n'))
        .map_or("", |(_, indent)| indent);
    Some((TextRange::empty(statement.text_range().end()), format!("\n{indent}drop({name});")))
}

/// Whether a borrow check found no error and checked the whole body.
fn passes(result: &BorrowckResult) -> bool {
    result.skipped.is_none()
        && result.not_checked.is_none()
        && !result.diagnostics.iter().any(|it| it.is_error())
}

/// The first call, by its span, that `guard` is held through: one written in the source,
/// with the guard maybe holding a value before it and used on no path after it.
fn held_through(mir: &MirBody<'_>, guard: LocalId) -> Option<MirSpan> {
    let derived = derived(mir, guard);
    let uses = |places: &mut dyn FnMut(&mut dyn FnMut(&StoredPlace))| {
        let mut any = false;
        places(&mut |place| any |= derived[index(place.local)]);
        any
    };
    let blocks = mir.basic_blocks.len();
    let mut used_somewhere = false;
    // Backward: whether a use comes on some path from the start of each block, the guard's
    // new value ending the old one's.
    // alloc: a flag per block of one body
    let mut live_in = vec![false; blocks];
    // Every pass that changes nothing ends it; each change sets a flag that stays set.
    for _ in 0..=blocks {
        let mut changed = false;
        for (id, block) in mir.basic_blocks.iter().rev() {
            if block.is_cleanup {
                continue;
            }
            let Some(terminator) = &block.terminator else { continue };
            let mut live = terminator.kind.successors().any(|it| live_in[block_index(it)]);
            if let TerminatorKind::Call { destination, .. } = &terminator.kind
                && whole(destination, guard)
            {
                live = false;
            }
            if uses(&mut |f| terminator_places(&terminator.kind, guard, f)) {
                live = true;
                used_somewhere = true;
            }
            for statement in block.statements.iter().rev() {
                if let StatementKind::Assign(place, _) = &statement.kind
                    && whole(place, guard)
                {
                    live = false;
                }
                if uses(&mut |f| statement_places(&statement.kind, f)) {
                    live = true;
                    used_somewhere = true;
                }
            }
            if live && !live_in[block_index(id)] {
                live_in[block_index(id)] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if !used_somewhere {
        return None;
    }
    let init_in = guards::maybe_init(mir, guard);
    let mut found = None;
    for (id, block) in mir.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        let TerminatorKind::Call { target: Some(target), from_hir_call: true, .. } =
            &terminator.kind
        else {
            continue;
        };
        if block.is_cleanup
            || live_in[block_index(*target)]
            || matches!(terminator.span, MirSpan::Unknown)
            || uses(&mut |f| terminator_places(&terminator.kind, guard, f))
            || !guards::init_before_terminator(mir, id, guard, init_in[block_index(id)])
        {
            continue;
        }
        found = Some(terminator.span);
        break;
    }
    found
}

fn whole(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local && place.projection.is_empty()
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

#[cfg(test)]
mod tests {
    use super::super::drops_tests::{fixed, lint_only};

    const LINT: &[&str] = &["significant_drop_tightening"];
    /// A finding's message, before why the value matters; the help and the fix say "after its
    /// last use" too.
    const FOUND: &str = "after its last use: ";
    /// The fix's message, which a finding with no fix never writes.
    const FIX: &str = "drop `guard` here";

    #[test]
    fn a_guard_held_through_a_call_after_its_last_use_is_found() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn count(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    work();
    n
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`guard`, a `MutexGuard<"), "{found}");
        assert!(found.contains("holds the lock in `count` after its last use"), "{found}");
        assert!(found.contains("this call runs with the lock still held"), "{found}");
        assert!(found.contains("`drop(guard);`"), "{found}");
        assert!(found.contains("other threads wait for the lock until it is dropped"), "{found}");
        assert!(found.contains(FIX), "{found}");
    }

    /// The fix writes `drop(guard);` on its own line after the statement of the last use, and
    /// the crate it leaves passes the borrow check and lints clean.
    #[test]
    fn the_fix_drops_the_guard_after_its_last_use_and_the_result_borrow_checks() {
        let after = fixed(
            "significant_drop_tightening",
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn count(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    work();
    n
}
"#,
        );
        assert!(
            after.contains("    let n = guard.len();\n    drop(guard);\n    work();\n"),
            "{after}"
        );
    }

    /// Used again on one branch and held through a call on the other: the drop could go only
    /// after the whole `if`, past the call, so the finding has no fix.
    #[test]
    fn a_guard_used_again_on_one_branch_gets_no_fix() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn branch(m: &Mutex<Vec<u32>>, b: bool) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    if b {
        work();
    } else {
        work();
        return n + guard.len();
    }
    n
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(!found.contains(FIX), "{found}");
    }

    /// A `RefCell` borrow and a type marked `#[clippy::has_significant_drop]` or
    /// `#[must_not_suspend]` count, each saying why; a `Vec`, or a type whose `Drop` only frees
    /// memory and is marked nothing, never does.
    #[test]
    fn a_refcell_borrow_and_marked_types_are_found_and_a_vec_never_is() {
        let found = lint_only(
            LINT,
            r#"
use std::{cell::RefCell, vec::Vec};
pub fn work() {}
#[clippy::has_significant_drop]
pub struct Token;
impl Token { pub fn id(&self) -> usize { 0 } }
impl Drop for Token { fn drop(&mut self) {} }
#[must_not_suspend]
pub struct Span;
impl Span { pub fn id(&self) -> usize { 0 } }
impl Drop for Span { fn drop(&mut self) {} }
pub struct Buf(Vec<u32>);
impl Drop for Buf { fn drop(&mut self) {} }
pub fn cell(c: &RefCell<Vec<u32>>) -> usize {
    let b = c.borrow_mut();
    let n = b.len();
    work();
    n
}
pub fn marked(make: fn() -> Token) -> usize {
    let t = make();
    let n = t.id();
    work();
    n
}
pub fn suspends(make: fn() -> Span) -> usize {
    let s = make();
    let n = s.id();
    work();
    n
}
pub fn plain(v: Vec<u32>) -> usize {
    let v = v;
    let n = v.len();
    work();
    n
}
pub fn owned(b: Buf) -> usize {
    let b = b;
    let n = b.0.len();
    work();
    n
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(
            found.contains(
                "holds the `RefCell` borrow in `cell` after its last use: a conflicting borrow"
            ),
            "{found}"
        );
        assert!(
            found.contains("is kept alive in `marked` after its last use: its type is marked `#[clippy::has_significant_drop]`"),
            "{found}"
        );
        assert!(
            found.contains(
                "in `suspends` after its last use: its type is marked `#[must_not_suspend]`"
            ),
            "{found}"
        );
        assert!(!found.contains("in `plain`") && !found.contains("in `owned`"), "{found}");
    }

    /// Used after the call, dropped before it, never used (a critical section), named `_`,
    /// or a borrow of it still used after the call: none is held past its last use.
    #[test]
    fn a_guard_used_later_dropped_unused_or_still_borrowed_is_left_alone() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn used_after(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    work();
    n + guard.len()
}
pub fn dropped(m: &Mutex<Vec<u32>>) -> usize {
    let guard = m.lock().unwrap();
    let n = guard.len();
    std::mem::drop(guard);
    work();
    n
}
pub fn section(m: &Mutex<Vec<u32>>) {
    let guard = m.lock().unwrap();
    work();
}
pub fn underscore(m: &Mutex<Vec<u32>>) -> usize {
    let _guard = m.lock().unwrap();
    let n = _guard.len();
    work();
    n
}
pub fn borrowed(m: &Mutex<Vec<u32>>) -> u32 {
    let guard = m.lock().unwrap();
    let first = guard.first();
    work();
    match first { Some(x) => *x, None => 0 }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    /// The guard of the next round of a loop is a new value: its uses are not the old one's.
    #[test]
    fn a_guard_made_again_in_each_round_of_a_loop_is_found() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work() {}
pub fn rounds(m: &Mutex<Vec<u32>>, k: usize) {
    let mut i = 0;
    while i < k {
        let guard = m.lock().unwrap();
        let n = guard.len();
        work();
        i += n;
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("in `rounds` after its last use"), "{found}");
    }

    #[test]
    fn a_renamed_lock_apis_guard_and_one_written_in_a_macro_call_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod locks { pub use parking_lot::Mutex as Lock; }
use locks::Lock;
use std::sync::Mutex;
pub fn work() {}
macro_rules! item { ($($t:tt)*) => { $($t)* }; }
pub fn parked(m: &Lock<Vec<u32>>) -> usize {
    let held = m.lock();
    let n = held.len();
    work();
    n
}
item! {
    pub fn handed(m: &Mutex<Vec<u32>>) -> usize {
        let guard = m.lock().unwrap();
        let n = guard.len();
        work();
        n
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("`held`, a `MutexGuard<"), "{found}");
        assert!(found.contains("in `parked` after its last use"), "{found}");
        assert!(found.contains("in `handed` after its last use"), "{found}");
    }
}
