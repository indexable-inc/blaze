//! What a diagnostic's printer reads of the check: its crate and edition, the tables of
//! [`Paths`](super::Paths) its paths are trimmed and made visible by, and the source its
//! closures are named by the span of and its anonymous constants are written as.

use hir_def::{
    ExpressionStoreOwnerId, ExternCrateId, HasModule as _,
    expr_store::ExpressionStore,
    hir::{ClosureKind, CoroutineSource, Expr, ExprId},
};
use hir_expand::name::Name;
use hir_ty_print::{Def, DiagnosticHost};
use span::Edition;
use std::fmt::Write as _;
use syntax::view::AstNode as _;

use crate::cx::Cx;

impl DiagnosticHost for Cx<'_, '_> {
    fn krate(&self) -> base_db::Crate {
        self.krate.into()
    }

    fn edition(&self) -> Edition {
        self.edition
    }

    fn trimmed(&self, def: Def) -> Option<Name> {
        self.paths().trimmed(self, def)
    }

    fn visible_parent(&self, def: Def) -> Option<Def> {
        self.paths().visible_parent(self, def)
    }

    fn reexport_name(&self, parent: Def, def: Def) -> Option<Name> {
        self.paths().reexport_name(self, parent, def)
    }

    fn extern_crate(&self, krate: base_db::Crate) -> Option<ExternCrateId> {
        self.paths().extern_crate(self, krate)
    }

    /// rustc's `def_span` of a closure-like definition: from the start of a closure (its
    /// `for<..>`, `move` and `async` included, its attributes not) to the end of its
    /// parameters or return type; all of an `async` or `gen` block. The coroutine of an
    /// `async` closure's body has the closure's: it has no syntax of its own, and is the body
    /// of the closure.
    fn closure_span(
        &self,
        owner: ExpressionStoreOwnerId,
        expr: ExprId,
        kind: ClosureKind,
        short: bool,
        out: &mut String,
    ) {
        let db = self.db;
        let (store, map) = ExpressionStore::with_source_map(db, owner);
        let src = map
            .expr_syntax(expr)
            .or_else(|err| match kind {
                ClosureKind::Coroutine { source: CoroutineSource::Closure, .. } => store
                    .exprs()
                    .find_map(|(id, it)| match it {
                        Expr::Closure { body, .. } if body == expr => Some(id),
                        _ => None,
                    })
                    .map_or(Err(err), |closure| map.expr_syntax(closure)),
                _ => Err(err),
            })
            .unwrap_or_else(|_| {
                panic!(
                    "the closure-like expression {expr:?} of {owner:?} (of {:?}) has no syntax",
                    owner.krate(db)
                )
            });
        let node = self.node_view(src);
        let span =
            self.span(node.with_value(crate::cx::closure_decl_range_view(node.value.syntax())));
        let file = self.file_of(&span).expect("a span the context made");
        let text = self.file_text(file);
        let (lo_line, lo_col) = line_col(text, span.lo);
        if short {
            let name = span.file.rsplit(['/', '\\']).next().unwrap_or(&span.file);
            write!(out, "{name}:{lo_line}:{lo_col}").expect("a `String` takes every write");
        } else {
            let (hi_line, hi_col) = line_col(text, span.hi);
            write!(out, "{}:{lo_line}:{lo_col}: {hi_line}:{hi_col}", span.file)
                .expect("a `String` takes every write");
        }
    }

    fn anon_const_text(&self, owner: ExpressionStoreOwnerId, expr: ExprId, out: &mut String) {
        let (_, map) = ExpressionStore::with_source_map(self.db, owner);
        let src = map.expr_syntax(expr).unwrap_or_else(|_| {
            panic!("the anonymous constant {expr:?} of {owner:?} is not written")
        });
        let span = self.span(src.map(|it| it.text_range()));
        out.push_str(self.snippet(&span).expect("a span the context made is in its file"));
    }
}

/// rustc's 1-based line and 1-based column in chars of a byte offset.
fn line_col(text: &str, offset: u32) -> (usize, usize) {
    let offset = (offset as usize).min(text.len());
    let before = &text[..offset];
    let line_start = memchr::memrchr(b'\n', before.as_bytes()).map_or(0, |it| it + 1);
    let line = memchr::memchr_iter(b'\n', before.as_bytes()).count() + 1;
    (line, before[line_start..].chars().count() + 1)
}

#[cfg(test)]
mod tests {
    use super::line_col;

    #[test]
    fn columns_count_chars_from_one() {
        let text = "fn f() {\n    let é = |x| x;\n}";
        let at = text.find('|').unwrap() as u32;
        assert_eq!(line_col(text, at), (2, 13));
        assert_eq!(line_col(text, 0), (1, 1));
    }
}
