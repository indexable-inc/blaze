//! `unbuffered_io`: a file, socket, pipe or stdout written (or read in small pieces) call by
//! call with no `BufWriter` (`BufReader`) in between. Each `write`/`read` on a `File`,
//! `TcpStream`, `UnixStream`, `ChildStdin` (`ChildStdout`, `ChildStderr` for reads) is a system
//! call; a buffer turns many small ones into a few large ones. `Stdout` is line-buffered, so a
//! write of many lines still makes one call per line.
//!
//! Found, per place written (a local, or a field of one), in one body:
//!
//! - writes: [`WRITES`] (`write!`/`writeln!` are `write_fmt`) at least [`TIMES`] times
//!   (`unbuffered-io-writes` in `clippy.toml`), or a `write_all`/`write_fmt` inside a loop. A
//!   bare `write` in a loop is the partial-write retry (`while !buf.is_empty() { let n =
//!   f.write(buf)?; .. }`) and does not count there; one write of a prebuilt buffer is never
//!   found.
//! - reads: `read`/`read_exact` into an array of at most [`SMALL`] bytes
//!   (`unbuffered-io-small-read`), at least twice or inside a loop. A read into a larger buffer
//!   is already a buffered read.
//!
//! A `Stdout` written inside a loop is `stdout_lock_in_loop`'s finding, not this one's. Test code
//! is not checked. No fix: the buffer's scope (and its `flush`) is the author's choice.

use blaze_hash::HashMap;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{BindingId, Expr, ExprId, UnaryOp},
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use std::alloc::Global;
use syntax::TextRange;

use super::{
    Finding, Lint, alloc::written_callee, exempt, facts, hash_order_output::local_of, loops,
};
use crate::check::Cx;

const LINT: Lint = Lint::UnbufferedIo;

/// How many writes to one place, by default, make it worth a buffer.
const TIMES: u64 = 2;

/// The largest read, in bytes, that counts as small by default: below about a cache line, the
/// system call costs far more than the copy.
const SMALL: u64 = 64;

/// The writes that reach the system with no buffer.
const WRITES: &[&str] = &["write", "write_all", "write_fmt"];

/// The reads that reach the system with no buffer.
const READS: &[&str] = &["read", "read_exact"];

/// std's types a write goes straight to the system from.
const WRITERS: &[&str] = &["File", "TcpStream", "UnixStream", "ChildStdin", "Stdout"];

/// std's types a read comes straight from the system to.
const READERS: &[&str] = &["File", "TcpStream", "UnixStream", "ChildStdout", "ChildStderr"];

/// One call of a write or read on a place.
struct Call {
    range: TextRange,
    in_loop: bool,
}

/// Which way a group of calls goes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Way {
    Write,
    Read,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// The place `expr` names, its references and derefs peeled: a local, or a field of one.
fn root(
    cx: &Cx<'_, '_>,
    def: DefWithBodyId,
    body: &Body,
    expr: ExprId,
) -> Option<(BindingId, Option<Name>)> {
    let mut at = expr;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
            Expr::Path(_) => return local_of(cx.db(), def, body, at).map(|it| (it, None)),
            Expr::Field { expr, name } => {
                // clone: an interned name, one per place written
                return local_of(cx.db(), def, body, expr).map(|it| (it, Some(name.clone())));
            }
            _ => return None,
        }
    }
    None
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let times =
        u64::try_from(cx.conf().int("unbuffered-io-writes", TIMES as i64)).unwrap_or(TIMES).max(1);
    let small =
        u64::try_from(cx.conf().int("unbuffered-io-small-read", SMALL as i64)).unwrap_or(SMALL);
    let mut parents = None;
    // alloc: the places of one body written or read, a few
    let mut groups: HashMap<(BindingId, Option<Name>, Way), (&'static str, Vec<Call>), Global> =
        HashMap::new_in(Global);
    // alloc: the order the places were first seen, so the findings come out in source order
    let mut order = Vec::new();
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, args, .. } = expr else { continue };
        let Some((callee, _)) = written_callee(infer, body, id) else { continue };
        let name = FunctionSignature::of(db, callee).name.as_str();
        let way = match (WRITES.contains(&name), READS.contains(&name)) {
            (true, _) => Way::Write,
            (_, true) => Way::Read,
            _ => continue,
        };
        let Some(adt) = analysis.ty(infer.expr_ty(receiver)).strip_references().as_adt() else {
            continue;
        };
        let kinds = match way {
            Way::Write => WRITERS,
            Way::Read => READERS,
        };
        let Some(kind) = kinds.iter().copied().find(|it| facts::adt_is(db, adt, "std", it)) else {
            continue;
        };
        if way == Way::Read {
            let Some(&buf) = args.first() else { continue };
            let size = analysis
                .ty(infer.expr_ty(buf))
                .strip_references()
                .as_array(db)
                .map(|(_, n)| n as u64);
            if !size.is_some_and(|it| it <= small) {
                continue;
            }
        }
        let Some((local, field)) = root(cx, def, body, receiver) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let in_loop = loops::enclosing_loop(cx, analysis, parents, id).is_some();
        // A `Stdout` in a loop is `stdout_lock_in_loop`'s; a bare `write` in a loop is a retry.
        if in_loop && (kind == "Stdout" || name == "write") {
            continue;
        }
        let key = (local, field, way);
        if !groups.contains_key(&key) {
            // clone: the key of one place, kept for the order
            order.push(key.clone());
        }
        groups.entry(key).or_insert_with(|| (kind, Vec::new())).1.push(Call { range, in_loop });
    }
    for key in order {
        let Some((kind, calls)) = groups.get(&key) else { continue };
        let in_loop = calls.iter().any(|it| it.in_loop);
        if !in_loop && (calls.len() as u64) < times {
            continue;
        }
        let (local, field, way) = &key;
        let place = match field {
            Some(field) => format!("{}.{}", body[*local].name.as_str(), field.as_str()),
            None => body[*local].name.as_str().to_owned(), // alloc: a message's part
        };
        let (done, wrapper) = match way {
            Way::Write => ("written", "BufWriter"),
            Way::Read => ("read in small pieces", "BufReader"),
        };
        let how = match in_loop {
            true => "inside a loop".to_owned(), // alloc: a message's part
            false => format!("{} times", calls.len()),
        };
        let message = format!(
            "`{place}` is a `{kind}` {done} {how} with no `{wrapper}`: each call is a system call"
        );
        let note = format!(
            "wrap `{place}` once in `std::io::{wrapper}::new(..)` and make the calls on the wrapper, so they fill a \
             buffer and reach the system in large pieces; a `BufWriter` writes what is left when it is dropped, and \
             `flush()` returns the error that drop would ignore"
        );
        facts::push(&mut found, LINT, calls[0].range, message, Some(note));
    }
    found
}
