//! `print_in_lib`: std's `print!`, `println!`, `eprint!` or `eprintln!` in library code. The
//! process's stdout and stderr belong to the program, not to a library inside it: a library
//! can't know whether they are a terminal, a pipe another program parses, or a protocol (an LSP
//! server's JSON-RPC, a JSON-lines stream, a test harness's output), and one stray line breaks
//! that. A print is also no error: nothing can match on it, and a caller that wants it quiet
//! can't make it so.
//!
//! Found: a macro call resolved to std's own macro of one of those names (a renamed import of
//! it too, since the definition is what is read), in library code (`library_code`: a library
//! target, or a binary's modules but its crate root), outside test code. The right forms: hand
//! the caller what it shows (a value, a typed error), take a `&mut impl Write`, or emit a
//! `tracing` event, which the binary routes.
//!
//! `dbg!` is not this lint's: clippy's `dbg_macro` (ported, `deny` in blaze's workspace) finds
//! it everywhere, with `allow-dbg-in-tests`. clippy's `print_stdout` and `print_stderr` find
//! every print, the binary's too; where the one for a macro is on, the place is left to it, so
//! nothing is reported twice. No fix: where the text goes instead is the author's choice.

use syntax::view::{self as ast, AstNode as _};

use super::{in_test, library_code, report};
use crate::{
    blaze::{Lint, test_code},
    check::Cx,
    lints::Implemented,
};

const LINT: Lint = Lint::PrintInLib;

/// std's printing macros, with whether each writes to stdout.
const PRINTS: &[(&str, bool)] =
    &[("print", true), ("println", true), ("eprint", false), ("eprintln", false)];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    let Some(name) = cx.std_macro(call) else { return };
    let Some(&(name, stdout)) = PRINTS.iter().find(|(it, _)| *it == name.as_str()) else {
        return;
    };
    let clippy = if stdout { Implemented::PrintStdout } else { Implemented::PrintStderr };
    if cx.on(clippy) {
        return;
    }
    let Some(module) = test_code::module_of(cx, node) else { return };
    if !library_code(cx, module) || in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let stream = if stdout { "stdout" } else { "stderr" };
    report(
        cx,
        LINT,
        node,
        range,
        format!("`{name}!` in library code writes to the process's {stream}, which the program owns"),
        "a library can't know whether the stream is a terminal, a pipe another program parses or a protocol, and nothing can match on or silence a print",
        None,
    );
}
