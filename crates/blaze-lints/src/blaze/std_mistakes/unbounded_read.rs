//! `unbounded_read`: a read of std's that takes as much as its input holds: `Read::read_to_end`
//! and `read_to_string`, `fs::read` and `fs::read_to_string`, `process::Command::output`, and
//! `BufRead::lines` and `read_line` (and `Stdin`'s own). Whoever writes the input then decides
//! how much memory the program takes: a 40 GiB file, a child that never stops printing, a line
//! with no newline grows one buffer until the process is killed.
//!
//! Found, outside test code: a call resolved to one of those (a trait's method or an impl's of
//! it; the function itself for `fs`'s), on a receiver that holds no `std::io::Take`. A reader
//! that went through `.take(limit)` has its bound and is not a finding.
//!
//! No fix: the limit is the caller's to choose. The message names the bounded form of the
//! call: `.take(limit)` before the read (and a refusal when the read reached the limit), a
//! length read from `metadata` and checked before `fs::read`, a child's output read through a
//! `Take`.

use syntax::view as ast;

use super::{in_test, inherent_of, is_std_fn, of_trait, report};
use crate::{
    blaze::{Lint, facts},
    check::Cx,
};

const LINT: Lint = Lint::UnboundedRead;

/// What each read is and how it is bounded.
#[derive(Clone, Copy)]
enum Reads {
    ToEnd(&'static str),
    File(&'static str),
    Output,
    Line(&'static str),
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some((f, receiver, _)) = super::callee(cx, node) else { return };
    let Some(read) = read_of(cx, f) else { return };
    if let Reads::ToEnd(_) | Reads::Line(_) = read
        && let Some(receiver) = receiver
        && bounded(cx, receiver)
    {
        return;
    }
    if in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let (message, note) = match read {
        Reads::ToEnd(name) => (
            format!("`{name}` reads until the input ends, however much that is"),
            "bound the reader first: `reader.by_ref().take(limit).read_to_end(&mut buf)?`, then refuse the input when `buf.len() as u64 == limit`",
        ),
        Reads::File(name) => (
            format!("`fs::{name}` reads the whole file, however large it is"),
            "check `fs::metadata(path)?.len()` against a cap and refuse above it, or read through `File::open(path)?.take(limit)`",
        ),
        Reads::Output => (
            "`Command::output` keeps everything the child writes to stdout and stderr".to_owned(),
            "`spawn` with `Stdio::piped()` and read each pipe through `.take(limit)`, refusing a child that writes more",
        ),
        Reads::Line(name) => (
            format!("`{name}` reads until a newline, and a line with none grows without bound"),
            "bound the reader first (`reader.by_ref().take(limit)`), or `read_until` into a buffer whose length you check",
        ),
    };
    report(cx, LINT, node, range, message, note, None);
}

/// Which read `f` is.
fn read_of(cx: &Cx<'_, '_>, f: hir::Function) -> Option<Reads> {
    let name = f.name(cx.db());
    let read = match name.as_str() {
        "read_to_end" if of_trait(cx, f, &["std", "io", "Read"]) => Reads::ToEnd("read_to_end"),
        "read_to_string" if of_trait(cx, f, &["std", "io", "Read"]) => {
            Reads::ToEnd("read_to_string")
        }
        "read" if is_std_fn(cx, f, &["std", "fs", "read"]) => Reads::File("read"),
        "read_to_string" if is_std_fn(cx, f, &["std", "fs", "read_to_string"]) => {
            Reads::File("read_to_string")
        }
        "output" if inherent_of(cx, f, &["std", "process", "Command"]) => Reads::Output,
        "lines" | "read_line"
            if of_trait(cx, f, &["std", "io", "BufRead"])
                || inherent_of(cx, f, &["std", "io", "Stdin"]) =>
        {
            match name.as_str() {
                "lines" => Reads::Line("lines"),
                _ => Reads::Line("read_line"),
            }
        }
        _ => return None,
    };
    Some(read)
}

/// Whether the reader `receiver` holds a `std::io::Take`: a bound set before the read. A path
/// call's first argument is the reader behind its `&mut`.
fn bounded<'t>(cx: &Cx<'_, 't>, receiver: ast::Expr<'t>) -> bool {
    let Some(ty) = cx.ty(receiver) else { return false };
    let Some(hir::ModuleDef::Adt(take)) = cx.std_def(&["std", "io", "Take"]) else { return false };
    facts::holds(cx.db(), &ty, facts::HOLDS_DEPTH, &mut |adt| adt == take).is_some()
}
