//! `stdout_lock_in_loop`: `print!`, `println!`, `eprint!` or `eprintln!` inside a loop, or a
//! write to `std::io::stdout()` / `stderr()` there with no `.lock()`. Each one takes the stream's
//! reentrant lock, writes, and lets go; `Stdout` then flushes at every newline, so a loop of
//! `println!` makes one lock and one system call per line. Locking once before the loop (and a
//! `BufWriter` over the lock) does the same output with one lock and a few large writes.
//!
//! Found: a call of std's `_print` or `_eprint` (what the four macros expand to), or a
//! `write`/`write_all`/`write_fmt` on a `Stdout` or `Stderr` (not a `StdoutLock`), inside a
//! `loop`, `while`, `for` or an iterator closure; one finding per loop, at its first such call,
//! counting the rest. Test code is not checked. No fix: the lock's scope is the author's choice
//! (nothing else in the loop may print while it is held).

use hir_def::{
    DefWithBodyId,
    hir::{Expr, ExprId},
    signatures::FunctionSignature,
};

use super::{Finding, Lint, alloc::written_callee, exempt, facts, loops};
use crate::check::Cx;

const LINT: Lint = Lint::StdoutLockInLoop;

/// The functions of std the print macros call.
const PRINTS: &[&str] = &["_print", "_eprint"];

/// The writes of `io::Write`.
const WRITES: &[&str] = &["write", "write_all", "write_fmt"];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    if DefWithBodyId::try_from(analysis.def()).is_err() {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    // alloc: each loop's first call and its count, a few per body
    let mut loops_seen: Vec<(ExprId, ExprId, usize, bool)> = Vec::new();
    for (id, expr) in body.exprs() {
        let print = match expr {
            Expr::Call { .. } => {
                let Some((callee, _)) = written_callee(infer, body, id) else { continue };
                let name = FunctionSignature::of(db, callee).name.as_str();
                let krate = hir::Function::from(callee).module(db).krate(db);
                if !PRINTS.contains(&name) || !facts::crate_is(db, krate, "std") {
                    continue;
                }
                true
            }
            Expr::MethodCall { receiver, .. } => {
                let Some((callee, _)) = written_callee(infer, body, id) else { continue };
                if !WRITES.contains(&FunctionSignature::of(db, callee).name.as_str()) {
                    continue;
                }
                let Some(adt) = analysis.ty(infer.expr_ty(receiver)).strip_references().as_adt()
                else {
                    continue;
                };
                if !(facts::adt_is(db, adt, "std", "Stdout")
                    || facts::adt_is(db, adt, "std", "Stderr"))
                {
                    continue;
                }
                false
            }
            _ => continue,
        };
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let Some(loop_) = loops::enclosing_loop(cx, analysis, parents, id) else { continue };
        match loops_seen.iter_mut().find(|it| it.0 == loop_) {
            Some(it) => it.2 += 1,
            None => loops_seen.push((loop_, id, 1, !print)),
        }
    }
    for (_, first, count, method) in loops_seen {
        let Some(range) = analysis.expr_range(first) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let what = match method {
            true => "this write to the unlocked standard stream",
            false => "this print",
        };
        let others = match count {
            1 => String::new(),
            n => format!(" (and {} more in the same loop)", n - 1),
        };
        let message = format!(
            "{what} is inside a loop{others}: each one locks the stream and writes on its own"
        );
        let note = "lock once before the loop and write to the lock: `let mut out = std::io::stdout().lock();` (wrap \
                    it in `std::io::BufWriter::new(..)` for few large writes), then `writeln!(out, ..)?` in the loop"
            .to_owned(); // alloc: a message's part
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}
