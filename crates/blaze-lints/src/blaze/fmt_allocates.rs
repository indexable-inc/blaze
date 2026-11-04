//! `fmt_allocates`: inside a `fmt` method of `Display`, `Debug` or another formatting trait, a
//! `String` built (`to_string()`, `format!`, `String::from`, `collect`, `to_owned`, `join`, ..)
//! only to be written to the formatter. Formatting already writes into the caller's buffer; the
//! `String` is a heap allocation and a copy on every `fmt` call, and a `Display` value's
//! `to_string()` inside another `fmt` formats it twice over.
//!
//! Found: the body is `fmt` of an impl of one of [`FMT_TRAITS`] (core's), a call of one of
//! [`MAKERS`] returns a `String`, and its value (inline, or every use of the plain `let` it
//! initializes, through further `let`s and block tails) only reaches the arguments of a
//! `write_str`, `write_fmt` (`write!`, `writeln!`), `pad` or `write_char` call on a `Formatter`.
//! A `String` that is also kept, compared, measured or passed elsewhere, or reaches the
//! formatter through a `match`, an `if`, a loop or a closure, is left alone.
//! No fix: which of `write!(f, ..)`, `f.write_str(..)` or `x.fmt(f)` fits depends on the parts.

use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{Expr, ExprId, Pat, Statement},
    signatures::FunctionSignature,
};

use super::{Finding, Lint, alloc::written_callee, exempt, facts, hash_order_output::local_of};
use crate::check::Cx;

const LINT: Lint = Lint::FmtAllocates;

/// How many steps a value is followed to the formatter.
const UP: usize = 24;

/// core's formatting traits.
const FMT_TRAITS: &[&str] = &[
    "Display", "Debug", "LowerHex", "UpperHex", "Octal", "Binary", "LowerExp", "UpperExp",
    "Pointer",
];

/// The calls that build a `String`.
const MAKERS: &[&str] = &[
    "to_string",
    "format",
    "from",
    "collect",
    "to_owned",
    "into",
    "concat",
    "join",
    "repeat",
    "replace",
    "to_uppercase",
    "to_lowercase",
];

/// `Formatter`'s methods that write what they are given.
const WRITES: &[&str] = &["write_str", "write_fmt", "pad", "write_char"];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Whether the body `analysis` is of is `fmt` of an impl of a formatting trait of core.
fn in_fmt(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>) -> bool {
    let db = cx.db();
    let hir::DefWithBody::Function(f) = analysis.def() else { return false };
    if f.name(db).as_str() != "fmt" {
        return false;
    }
    let Some(trait_) = f.as_assoc_item(db).and_then(|it| it.implemented_trait(db)) else {
        return false;
    };
    let krate = trait_.module(db).krate(db);
    FMT_TRAITS.contains(&trait_.name(db).as_str())
        && ["core", "std", "alloc"].iter().any(|it| facts::crate_is(db, krate, it))
}

/// What a body's walk knows.
struct Walk<'a, 'db> {
    db: &'db base_db::Db,
    def: DefWithBodyId,
    body: &'db Body,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: &'a facts::Parents,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    if !in_fmt(cx, analysis) {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let Some(string) = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("String")) else {
        return found;
    };
    let parents = facts::Parents::of(body);
    let walk = Walk { db, def, body, analysis, parents: &parents };
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, _)) = written_callee(infer, body, id) else { continue };
        let name = FunctionSignature::of(db, callee).name.as_str();
        if !MAKERS.contains(&name) || analysis.ty(infer.expr_ty(id)).as_adt() != Some(string) {
            continue;
        }
        // `format!` writes `let res = format(..); res`: the `format` inside is the `format!`.
        if !walk.written(id, 0) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let what = match cx.snippet(range).trim_start().starts_with("format!") {
            true => "`format!`".to_owned(), // alloc: a message's part
            false => format!("`{name}`"),
        };
        let message = format!(
            "this {what} builds a `String` only to write it to the formatter: it allocates and copies on every `fmt` call"
        );
        let note = "write the parts straight into `f`: `write!(f, \"..\", ..)`, `f.write_str(..)`, or a `Display` \
                    value's own `fmt` (`x.fmt(f)`, `write!(f, \"{}\", x)`), with no `String` between"
            .to_owned(); // alloc: a message's part
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

impl Walk<'_, '_> {
    /// Whether the value of `value` only reaches what a `Formatter` writes.
    fn written(&self, value: ExprId, depth: usize) -> bool {
        if depth > 4 {
            return false;
        }
        let mut at = value;
        for _ in 0..UP {
            let Some(parent) = self.parents.of_expr(at) else { return false };
            match self.body.expr(parent) {
                Expr::Block { statements, tail, .. } => {
                    if tail == Some(at) {
                        at = parent;
                        continue;
                    }
                    let pat = statements.iter().find_map(|it| match it {
                        Statement::Let {
                            pat, initializer: Some(init), else_branch: None, ..
                        } if init == at => Some(pat),
                        _ => None,
                    });
                    let Some(pat) = pat else { return false };
                    let Pat::Bind { id: local, subpat: None } = self.body.pat(pat) else {
                        return false;
                    };
                    let db = self.db;
                    // alloc: the uses of one local, a few
                    let uses: Vec<ExprId> = self
                        .body
                        .exprs()
                        .filter(|(id, expr)| {
                            matches!(expr, Expr::Path(_))
                                && local_of(db, self.def, self.body, *id) == Some(local)
                        })
                        .map(|(id, _)| id)
                        .collect();
                    return !uses.is_empty() && uses.iter().all(|&it| self.written(it, depth + 1));
                }
                Expr::MethodCall { receiver, .. }
                    if receiver != at && self.formatter_write(parent, receiver) =>
                {
                    return true;
                }
                Expr::Closure { .. } | Expr::Loop { .. } | Expr::Match { .. } | Expr::If { .. } => {
                    return false;
                }
                _ => at = parent,
            }
        }
        false
    }

    /// Whether the call `call` on `receiver` is one of [`WRITES`] of a `Formatter`.
    fn formatter_write(&self, call: ExprId, receiver: ExprId) -> bool {
        let db = self.db;
        let infer = self.analysis.infer();
        let Some((callee, _)) = written_callee(infer, self.body, call) else { return false };
        if !WRITES.contains(&FunctionSignature::of(db, callee).name.as_str()) {
            return false;
        }
        let ty = self.analysis.ty(infer.expr_ty(receiver)).strip_references();
        ty.as_adt().is_some_and(|it| {
            let krate = it.module(db).krate(db);
            it.name(db).as_str() == "Formatter"
                && ["core", "std", "alloc"].iter().any(|name| facts::crate_is(db, krate, name))
        })
    }
}
