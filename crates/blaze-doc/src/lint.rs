//! Whether rustdoc reports a broken intra-doc link at a place: the nearest `allow`, `expect`,
//! `warn`, `deny` or `forbid` naming `rustdoc::broken_intra_doc_links`, `rustdoc::all` or
//! `warnings`, on the items around the link, then on the modules it is in, each `cfg_attr` as
//! the crate's cfg says. Levels given on the command line (`RUSTDOCFLAGS`) are not read. A link
//! in docs a macro of another crate wrote is not reported at all, as rustc reports no lint there.

use hir::{CfgExpr, CfgOptions, HirFileId, InFile, view::Semantics};
use base_db::Db;
use syntax::{
    NodeOrToken, TextRange,
    view::{self as ast, AstNode, HasAttrs},
};

/// Whether a broken link at `at`, in `krate`, is reported.
pub(crate) fn broken_links_reported(
    db: &Db,
    krate: hir::Crate,
    at: InFile<TextRange>,
) -> bool {
    !in_external_macro(db, krate, at) && level(db, krate, at).unwrap_or(true)
}

/// Whether rustc drops the lint as one in a macro the crate did not write: the doc attribute
/// over `at` has a `#` an attribute or derive macro made, or a function-like macro of another
/// crate (`Span::in_external_macro` in `compiler/rustc_span/src/lib.rs`, rust
/// 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0; `broken_intra_doc_links` does not ask to be
/// reported there). A doc comment a macro passes on as it was given keeps the caller's `#`,
/// and the text of an included file is the file's.
fn in_external_macro(db: &Db, krate: hir::Crate, at: InFile<TextRange>) -> bool {
    let HirFileId::MacroFile(file) = at.file_id else { return false };
    let root = at.file_id.root(db);
    if !root.text_range().contains_range(at.value) {
        return false;
    }
    let start = match root.covering_element(at.value) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => match it.parent() {
            Some(it) => it,
            None => return false,
        },
    };
    let Some(pound) = start.ancestors().find_map(ast::Attr::cast).and_then(|it| it.pound_token())
    else {
        return false;
    };
    let span = file.expansion_span_map(db).span_at(pound.text_range().start());
    let Some(expn) = span.ctx.outer_expn(db) else { return false };
    let loc = hir_expand::MacroCallId::from(expn).loc(db);
    match loc.kind {
        hir_expand::MacroCallKind::FnLike { .. } => {
            !loc.def.is_include() && loc.def.krate != base_db::Crate::from(krate)
        }
        hir_expand::MacroCallKind::Derive { .. } | hir_expand::MacroCallKind::Attr { .. } => true,
    }
}

fn level(db: &Db, krate: hir::Crate, at: InFile<TextRange>) -> Option<bool> {
    let cfg = krate.cfg(db);
    let sema = Semantics::new(db);
    let root = at.file_id.root(db);
    if !root.text_range().contains_range(at.value) {
        return None;
    }
    let start = match root.covering_element(at.value) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => it.parent()?,
    };
    let mut node = InFile::new(at.file_id, start);
    loop {
        for it in node.value.ancestors() {
            if let Some(it) = ast::AnyHasAttrs::cast(it)
                && let Some(reported) = level_on(it, cfg)
            {
                return Some(reported);
            }
        }
        // Up from a macro's output to its call, from a module's file to its `mod` item.
        node = match node.file_id {
            HirFileId::MacroFile(_) => node.file_id.call_node_view(db)?,
            HirFileId::FileId(file) => {
                let module = sema.file_to_module_def(file.file_id(db))?;
                module.declaration_source_view(db)?.map(|it| it.syntax())
            }
        };
    }
}

/// The level the attributes of `node` give the lint, the last one that names it.
fn level_on(node: ast::AnyHasAttrs<'_>, cfg: &CfgOptions) -> Option<bool> {
    let mut out = None;
    for attr in node.attrs() {
        let Some(meta) = attr.meta() else { continue };
        for meta in active(meta, cfg) {
            let Some((name, tt)) = meta.as_simple_call() else { continue };
            let reported = match name {
                "allow" | "expect" => false,
                "warn" | "deny" | "forbid" => true,
                _ => continue,
            };
            if names_the_lint(tt) {
                out = Some(reported);
            }
        }
    }
    out
}

/// `meta`, or what a `cfg_attr` whose condition holds gives.
fn active<'t>(meta: ast::Meta<'t>, cfg: &CfgOptions) -> Vec<ast::Meta<'t>> {
    match meta {
        ast::Meta::CfgAttrMeta(it) => {
            let holds = it
                .cfg_predicate()
                .is_some_and(|pred| cfg.check(&CfgExpr::parse_from_view(pred)) != Some(false));
            if !holds {
                return vec![];
            }
            it.metas().flat_map(|meta| active(meta, cfg)).collect()
        }
        _ => vec![meta],
    }
}

fn names_the_lint(tt: ast::TokenTree<'_>) -> bool {
    let text: String = tt.syntax().text().chars().filter(|c| !c.is_whitespace()).collect();
    text.trim_start_matches('(').trim_end_matches(')').split(',').any(|lint| {
        matches!(
            lint,
            "rustdoc::broken_intra_doc_links"
                | "broken_intra_doc_links"
                | "rustdoc::all"
                | "warnings"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::names_the_lint;
    use syntax::{
        Edition, SourceFile,
        view::{self as ast, AstNode},
    };

    #[test]
    fn lint_names() {
        let names = |attr: &str| {
            let parse = SourceFile::parse(&format!("{attr} fn f() {{}}"), Edition::CURRENT);
            names_the_lint(parse.root().descendants().find_map(ast::TokenTree::cast).unwrap())
        };
        assert!(names("#[allow(rustdoc::broken_intra_doc_links)]"));
        assert!(names("#[allow(dead_code, rustdoc :: all)]"));
        assert!(names("#[deny(warnings)]"));
        assert!(!names("#[allow(rustdoc::private_intra_doc_links)]"));
    }
}
