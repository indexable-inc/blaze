//! Duplicated code and the copy-paste mistakes copies carry, found by the shape of code: a
//! syntax tree read as tokens with every local numbered by first use ([`shape`]), so code that
//! differs only in what its locals are called has one shape, and code whose literals, paths,
//! fields, methods or types differ has another. The bodies' shapes are queries per body; the
//! crate's pass groups them ([`index`]), and a body is told the findings it holds.
//!
//! - `same_fn`: two functions (or methods) the same up to the names of their locals, whose
//!   code identity agrees too (`blaze_items::code_hash`: a path that reaches two items makes
//!   two functions); `same_impl_fn` for the bodies of trait impls, `same_const` for constants.
//! - `same_block`: a block, `if`, `match`, arm, loop, closure or row of statements of at least
//!   [`index::BLOCK_FLOOR`] tokens written twice or more, in one function or in several; a
//!   piece inside a copy reported is not reported again.
//! - `near_duplicate`: two functions whose shingles mostly agree ([`near`]: MinHash, banded
//!   LSH, exact Jaccard), with the lines that differ.
//! - `paste_mismatch`: a copy whose rename was not finished ([`paste`], after CP-Miner), and a
//!   row of like statements that repeats the row above where the rest change.
//! - `same_type`: a struct or enum whose fields or variants another one has.
//! - `same_behaviour`: two functions of another shape whose canonical runtime MIR is one
//!   ([`semantic`]): the same helper written again in another style.
//!
//! Every message names every copy, deterministic in order: the dependencies' first, then the
//! crate's, by file and offset.

pub(super) mod index;
mod near;
mod paste;
mod semantic;
mod shape;
#[cfg(test)]
mod tests;
mod unify;

use std::alloc::Global;

use hir::HirDisplay as _;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

use super::Lint;
use crate::{
    check::{Cx, body_at},
    diag::{self, Applicability, Span},
};

/// The lints of this module.
const LINTS: &[Lint] = &[
    Lint::SameFn,
    Lint::SameImplFn,
    Lint::SameBlock,
    Lint::NearDuplicate,
    Lint::PasteMismatch,
    Lint::SameType,
    Lint::SameConst,
    Lint::ExtractableDuplicate,
    Lint::TestsDifferByLiteral,
    Lint::ImplsDifferByType,
    Lint::SameBehaviour,
];

/// The lints of a body: the rows of its statement lists, then what the crate's pass found in it.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !LINTS.iter().any(|&it| cx.on_blaze(it)) {
        return;
    }
    let Some(def) = cx.body_def() else { return };
    semantic::check(cx, def);
    let root = cx.root();
    if cx.on_blaze(Lint::PasteMismatch) && !cx.is_in_test(root) {
        odd_rows(cx);
    }
    let db = cx.db();
    for found in index::body_findings(db, def).iter() {
        if !root.text_range().contains_range(found.at) {
            continue;
        }
        if let index::Note::Extract { first, holes, near } = &found.note {
            extract(cx, found, *first, holes, *near);
            continue;
        }
        if cx.on_blaze(found.lint) {
            report(cx, found, &[]);
        }
    }
}

/// `extractable_duplicate` when inference gives each hole one type on both sides; else the
/// pair's `near_duplicate`, when `near` paired it.
fn extract(
    cx: &mut Cx<'_, '_>,
    found: &index::Found,
    first: hir_def::DefWithBodyId,
    holes: &[index::HoleAt],
    near: Option<u32>,
) {
    let on_extract = cx.on_blaze(Lint::ExtractableDuplicate);
    let types = if on_extract { hole_types(cx, first, holes) } else { None };
    match (types, near) {
        (Some(types), _) => report(cx, found, &types),
        (None, Some(percent)) if cx.on_blaze(Lint::NearDuplicate) => {
            let near = index::Found {
                lint: Lint::NearDuplicate,
                at: found.at,
                sites: found.sites.clone(), // clone: the copies, for the weaker finding
                note: index::Note::Near { percent, diff: None },
            };
            report(cx, &near, &[]);
        }
        _ => {}
    }
}

/// The type of each hole, when both its sides have one and it is the same: the body linted
/// asked through the query's analysis, `first` through its own. `None` where inference
/// cannot say for one, or the two differ.
fn hole_types(
    cx: &Cx<'_, '_>,
    first: hir_def::DefWithBodyId,
    holes: &[index::HoleAt],
) -> Option<Box<[String]>> {
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(first));
    let (other, file) = analysis.tree();
    let mut out: Vec<String, Global> = Vec::new_in(Global);
    for hole in holes {
        // A callee's type is its function's own item type, one per function: two callees are
        // one parameter when their calls are of one type (the arguments are the copies' own,
        // compared by the generalisation already).
        let (here, called) = callee_call(expr_at(cx.root(), hole.b, hole.class)?);
        let (there, _) = callee_call(expr_at(other, hole.a, hole.class)?);
        let mine = cx.ty(here)?.display(db, target).to_string();
        let ptr = hir::InFile::new(file, syntax::AstPtr::<syntax::ast::Expr>::of(there));
        let theirs = analysis.type_of_expr(ptr)?.original.display(db, target).to_string();
        if mine != theirs || mine.contains("{unknown}") {
            return None;
        }
        out.push(if called { format!("fn(..) -> {mine}") } else { mine });
    }
    Some(out.into_boxed_slice())
}

/// The expression a hole at `range` of `root` stands for: the field access of a field's
/// name, else the smallest expression covering it.
fn expr_at<'t>(root: ast::Node<'t>, range: TextRange, class: unify::HoleClass) -> Option<ast::Expr<'t>> {
    if !root.text_range().contains_range(range) {
        return None;
    }
    let mut node = node_at(root, range);
    if class == unify::HoleClass::Field {
        node = node.parent()?;
    }
    node.ancestors().find_map(ast::Expr::cast)
}

/// The call `expr` is the callee of, and whether it is one.
fn callee_call(expr: ast::Expr<'_>) -> (ast::Expr<'_>, bool) {
    let node = expr.syntax();
    let call = node.parent().filter(|it| {
        it.kind() == SyntaxKind::CALL_EXPR && it.children().next() == Some(node)
    });
    match call.and_then(ast::Expr::cast) {
        Some(call) => (call, true),
        None => (expr, false),
    }
}

/// How a message names a hole's class.
fn hole_word(class: unify::HoleClass) -> &'static str {
    match class {
        unify::HoleClass::Literal => "the literal",
        unify::HoleClass::Path => "the path",
        unify::HoleClass::Field => "the field",
        unify::HoleClass::Method => "the method",
        unify::HoleClass::Type => "the type",
        unify::HoleClass::Expr => "the expression",
        unify::HoleClass::Statements => "the statements",
    }
}

/// The holes of a pair as a message lists them: "the literal (`64` / `256`, usize) and the
/// path (`read_head` / `read_tail`)", each with its type when `types` gives one.
fn holes_listed(holes: &[index::HoleAt], types: &[String]) -> String {
    let mut out = String::new(); // alloc: the message's list
    for (index, hole) in holes.iter().enumerate() {
        if index > 0 {
            out.push_str(if index + 1 == holes.len() { " and " } else { ", " });
        }
        let ty = types.get(index).map_or(String::new(), |it| format!(", {it}"));
        let uses = if hole.uses > 1 { format!(", in {} places", hole.uses) } else { String::new() };
        out.push_str(&format!(
            "{} (`{}` / `{}`{ty}{uses})",
            hole_word(hole.class),
            hole.text_a,
            hole.text_b
        ));
    }
    out
}

/// The node of `root` that covers `range`.
fn node_at<'t>(root: ast::Node<'t>, range: TextRange) -> ast::Node<'t> {
    match root.covering_element(range) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => it.parent().unwrap_or(root),
    }
}

/// The span of `range` of `def`'s anchor: the query's own tree for the body linted, the file
/// for another.
fn span_of(cx: &Cx<'_, '_>, def: hir_def::DefWithBodyId, range: TextRange) -> Option<Span> {
    if cx.body_def() == Some(def) {
        return Some(cx.span(range));
    }
    let db = cx.db();
    let at = body_at(db, def)?;
    Some(diag::of_file(at.file.file_id(db), range + at.offset))
}

/// The labels of the copies other than this one, joined: "`a`, `b` and 2 more".
fn others(sites: &[index::Site]) -> String {
    let mut labels: Vec<&str, Global> = Vec::new_in(Global);
    labels.extend(sites.iter().filter(|it| !it.here).map(|it| it.label.as_str()));
    match labels.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [one, two] => format!("{one} and {two}"),
        [one, rest @ ..] => format!("{one} and {} more", rest.len()),
    }
}

/// Reports a finding of the crate's pass in the body linted.
fn report<'t>(cx: &mut Cx<'_, 't>, found: &index::Found, types: &[String]) {
    let root = cx.root();
    let lint = found.lint;
    let here = found.sites.iter().find(|it| it.here).map_or("", |it| it.label.as_str());
    let copies = found.sites.len();
    let message = match (&found.note, lint) {
        (index::Note::Whole { .. }, Lint::SameConst) => {
            format!("{here} has the value of {}: one constant written twice", others(&found.sites))
        }
        (index::Note::Whole { .. }, Lint::SameImplFn) => format!(
            "the body of {here} is written {copies} times, in impls of traits, up to the names of its locals"
        ),
        (index::Note::Whole { .. }, _) => format!(
            "{here} is the same function as {}, up to the names of its locals",
            others(&found.sites)
        ),
        (index::Note::Block { weight, .. }, _) => format!(
            "this code ({weight} tokens) is written {copies} times, up to the names of its locals"
        ),
        (index::Note::Near { percent, .. }, _) => format!(
            "{here} is a near copy of {}: {percent}% of their runs of {} tokens agree",
            others(&found.sites),
            near::WIDTH
        ),
        (index::Note::Paste { from, to, renamed, kept }, _) => format!(
            "`{from}` is left as it was in a copy that renames `{from}` to `{to}` everywhere else ({renamed} places renamed, {kept} kept)"
        ),
        (index::Note::Extract { holes, .. }, _) => format!(
            "{} and {here} are one function with {} parameter{}: {}",
            others(&found.sites),
            holes.len(),
            if holes.len() == 1 { "" } else { "s" },
            holes_listed(holes, types)
        ),
        (index::Note::Table { rows, values: 0 }, _) => {
            format!("these {rows} tests are one test, written {rows} times")
        }
        (index::Note::Table { rows, values }, _) => format!(
            "these {rows} tests are one test over a table of {rows} rows of {values} value{}",
            if *values == 1 { "" } else { "s" }
        ),
        (index::Note::ByType { holes }, _) => format!(
            "{here} is {} with other types: {}",
            others(&found.sites),
            holes_listed(holes, &[])
        ),
    };
    let fix = match &found.note {
        index::Note::Whole { forward: Some(forward) } if lint == Lint::SameFn => {
            forward_fix(cx, &forward.name)
        }
        _ => None,
    };
    let has_fix = fix.is_some();
    let mut notes: Vec<(Option<Span>, String), Global> = Vec::new_in(Global);
    for site in found.sites.iter().filter(|it| !it.here) {
        match &site.place {
            index::Place::Body { def: at, range } => {
                let note = match &found.note {
                    index::Note::Paste { .. } => format!("{} it was pasted from starts here", site.label),
                    _ => format!("the copy {} is here", site.label),
                };
                if let Some(span) = span_of(cx, *at, *range) {
                    notes.push((Some(span), note));
                }
            }
            index::Place::Crate => {
                notes.push((None, format!("{} is a copy too, in a crate this one depends on", site.label)));
            }
        }
    }
    let node = node_at(root, found.at);
    let span = cx.span(found.at);
    let paste_to = match &found.note {
        index::Note::Paste { to, .. } => Some(to.clone()), // clone: the fix's text
        _ => None,
    };
    let diagnostic = cx.blaze(lint, node, span, message);
    for (span, note) in notes {
        match span {
            Some(span) => diagnostic.span_note(span, note),
            None => diagnostic.note(note),
        };
    }
    match &found.note {
        index::Note::Block { free, .. } => {
            let takes = match free.as_ref() {
                [] => "nothing".to_owned(),
                names => {
                    let mut out = String::new(); // alloc: the note's list of names
                    for (index, name) in names.iter().enumerate() {
                        if index > 0 {
                            out.push_str(", ");
                        }
                        out.push('`');
                        out.push_str(name);
                        out.push('`');
                    }
                    out
                }
            };
            diagnostic.note(format!("a function made of it takes {takes}"));
        }
        index::Note::Near { diff: Some(diff), .. } => {
            diagnostic.note(format!("the lines that differ:{diff}"));
        }
        index::Note::Whole { .. } if lint == Lint::SameImplFn => {
            diagnostic.note("a default method of the trait, or one generic impl, holds it once");
        }
        index::Note::Extract { holes, .. } => {
            let mut params = String::new(); // alloc: the help's signature
            for (index, ty) in types.iter().enumerate().take(holes.len()) {
                params.push_str(&format!(", p{index}: {ty}"));
            }
            diagnostic.help(format!(
                "one function holds both: `fn ..(<their parameters>{params})`, which each copy calls with its own values"
            ));
        }
        index::Note::Table { .. } => {
            diagnostic.help(
                "write one test over the table (`for (input, want) in [..] { .. }`), or a Hegel property where the expected value can be computed from the input",
            );
        }
        index::Note::ByType { .. } => {
            diagnostic.help(
                "one generic function, or a blanket impl, holds both; where the types are a closed list, `for const` reflection over them",
            );
        }
        _ => {}
    }
    match (fix, paste_to) {
        (Some((range, text, name)), _) => {
            // alloc: the fix's one part
            diagnostic.multipart_suggestion(
                format!("forward to `{name}`"),
                vec![(span_in(range), text)],
                Applicability::MaybeIncorrect,
            );
        }
        (None, Some(to)) => {
            // alloc: the fix's one part
            diagnostic.multipart_suggestion(
                format!("rename it to `{to}`"),
                vec![(span_in(found.at), to)],
                Applicability::MaybeIncorrect,
            );
        }
        (None, None) => {}
    }
    if lint == Lint::SameFn && !has_fix {
        diagnostic.help("keep one: delete this copy and call the other where it is called");
    }
    diagnostic
        .help(lint.help())
        .help(format!("for further information visit {}", lint.docs_url()));
}

/// The span of `range` of the query's tree, outside any expansion: a fix's part.
fn span_in(range: TextRange) -> Span {
    diag::in_query(range)
}

/// The fix of a same function the crate exports: its body made a call of the first copy with
/// its parameters, the range of the body, the text and the name called. `None` where the call
/// would not mean the same or a reader would not want it: a function `async`, `unsafe` or
/// `extern`, generic, with a `self` or a parameter that is not a plain name, or not exported
/// (where `forwarding_fn` would say the forward is a second name; the help says to delete it).
fn forward_fix(cx: &Cx<'_, '_>, name: &str) -> Option<(TextRange, String, String)> {
    let root = cx.root();
    if root.kind() != SyntaxKind::FN || !cx.is_exported(root) {
        return None;
    }
    let refused = root.children_with_tokens().any(|it| {
        matches!(
            it.kind(),
            SyntaxKind::ASYNC_KW | SyntaxKind::UNSAFE_KW | SyntaxKind::ABI | SyntaxKind::GENERIC_PARAM_LIST
        )
    });
    if refused {
        return None;
    }
    let params = root.children().find(|it| it.kind() == SyntaxKind::PARAM_LIST)?;
    let mut args = String::new(); // alloc: the fix's text
    for param in params.children() {
        if param.kind() != SyntaxKind::PARAM {
            return None;
        }
        let pat = param.children().find(|it| it.kind() == SyntaxKind::IDENT_PAT)?;
        let plain = pat.children().all(|it| it.kind() == SyntaxKind::NAME)
            && !pat.children_with_tokens().any(|it| it.kind() == SyntaxKind::REF_KW);
        let name = pat.children().find(|it| it.kind() == SyntaxKind::NAME)?;
        if !plain {
            return None;
        }
        if !args.is_empty() {
            args.push_str(", ");
        }
        args.push_str(cx.snippet(name.text_range()));
    }
    let body = root.children().find(|it| it.kind() == SyntaxKind::BLOCK_EXPR)?;
    // alloc: the fix's text and name
    Some((body.text_range(), format!("{{ {name}({args}) }}"), name.to_owned()))
}

/// The second form of `paste_mismatch`, in the body linted: rows of statements of one structure
/// in which one row repeats the name of the row above where every other row has its own.
fn odd_rows<'t>(cx: &mut Cx<'_, 't>) {
    let root = cx.root();
    let toks = shape::tokens(root);
    let mut found: Vec<(ast::Node<'t>, TextRange, TextRange, usize), Global> = Vec::new_in(Global);
    let mut walk = root.preorder();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node != root && ast::Item::can_cast(node.kind()) {
            walk.skip_subtree();
            continue;
        }
        if node.kind() != SyntaxKind::STMT_LIST {
            continue;
        }
        let stmts = shape::statements(node);
        let mut start = 0;
        while start < stmts.len() {
            let key = shape::structure(shape::within(&toks, stmts[start].text_range()));
            let end = start
                + stmts[start..]
                    .iter()
                    .take_while(|it| shape::structure(shape::within(&toks, it.text_range())) == key)
                    .count();
            if end - start >= paste::ROWS_FLOOR {
                let mut rows: Vec<&[shape::Tok], Global> = Vec::new_in(Global);
                rows.extend(stmts[start..end].iter().map(|it| shape::within(&toks, it.text_range())));
                for odd in paste::odd_rows(&rows) {
                    let row = rows[odd.row];
                    let (Some(at), Some(with)) = (row.get(odd.at), row.get(odd.with)) else { continue };
                    found.push((stmts[start + odd.row], at.range, with.range, end - start));
                }
            }
            start = end.max(start + 1);
        }
    }
    for (stmt, at, with, rows) in found {
        let was = cx.snippet(at);
        let own = cx.snippet(with);
        let message = format!(
            "this row repeats `{was}` of the row above where each of the {rows} rows has its own, as `{own}` here"
        );
        let span = cx.span(at);
        let diagnostic = cx.blaze(Lint::PasteMismatch, stmt, span, message);
        // alloc: the fix's one part
        diagnostic
            .multipart_suggestion(
                format!("write `{own}`"),
                vec![(span_in(at), own.to_owned())],
                Applicability::MaybeIncorrect,
            )
            .help(Lint::PasteMismatch.help())
            .help(format!("for further information visit {}", Lint::PasteMismatch.docs_url()));
    }
}

/// `same_type` at a struct or enum of the file linted.
pub(super) fn check_item<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(Lint::SameType) || cx.body_def().is_some() || !cx.in_file() {
        return;
    }
    if !matches!(node.kind(), SyntaxKind::STRUCT | SyntaxKind::ENUM) {
        return;
    }
    let Some(name) = node.children().find(|it| it.kind() == SyntaxKind::NAME) else { return };
    let Some(range) = cx.range(name) else { return };
    let db = cx.db();
    let file = cx.file();
    let findings = index::type_findings(db, cx.krate().base(), file);
    let Some(found) = findings.iter().find(|it| it.at == range) else { return };
    let here = found.sites.iter().find(|it| it.2).map_or("", |it| it.1.as_str());
    let mut labels: Vec<&str, Global> = Vec::new_in(Global);
    labels.extend(found.sites.iter().filter(|it| !it.2).map(|it| it.1.as_str()));
    let first = labels.first().copied().unwrap_or_default();
    let what = if found.is_enum { "variants" } else { "fields" };
    let more = match labels.len() {
        0 | 1 => String::new(),
        n => format!(" (and {} more copies)", n - 1),
    };
    let message = format!("{here} has the {what} of {first}{more}: one type written twice");
    let mut notes: Vec<(Option<Span>, String), Global> = Vec::new_in(Global);
    for (place, label, here) in found.sites.iter() {
        if *here {
            continue;
        }
        match place {
            Some((at_file, at)) if *at_file == file => notes.push((Some(cx.span(*at)), format!("{label} is here"))),
            Some((at_file, at)) => {
                notes.push((Some(diag::of_file(at_file.file_id(db), *at)), format!("{label} is here")));
            }
            None => notes.push((None, format!("{label} is a copy too"))),
        }
    }
    let span = cx.span(range);
    let same_order = found.same_order;
    let diagnostic = cx.blaze(Lint::SameType, node, span, message);
    for (span, note) in notes {
        match span {
            Some(span) => diagnostic.span_note(span, note),
            None => diagnostic.note(note),
        };
    }
    if !same_order {
        diagnostic.note(format!("the copies list their {what} in different orders"));
    }
    diagnostic
        .help("keep one and name it from the other's places (a `use`, or a type alias)")
        .help(Lint::SameType.help())
        .help(format!("for further information visit {}", Lint::SameType.docs_url()));
}
