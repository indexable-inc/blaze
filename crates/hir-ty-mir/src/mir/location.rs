//! Where code is written, as rustc counts it: the line, column and file that `line!()`,
//! `column!()` and `file!()` expand to, which rust-analyzer expands to placeholders, and that
//! `#[track_caller]` and panics report. The evaluator of constants and the code generator
//! both read them.

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_query::db::{Database as _, Setter as _};
use hir_def::{
    DefWithBodyId, FunctionId, ItemContainerId, Lookup,
    attrs::AttrFlags,
    expr_store::{Body, body::BodySourceMap},
    hir::ExprId,
    signatures::FunctionSignature,
};
use hir_expand::{
    FileRange, HirFileId, InFile, MacroDefKind,
    builtin::{BuiltinFnLikeExpander, EagerExpander},
};
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode, AstToken, Element, Node},
};

use crate::{
    InferenceResult,
    db::HirDatabase,
    mir::{MirSpan, is_builtin_index},
};

pub use syntax::view::written_range;

/// Whether lowering `body` met a call of `file!` or `concat!`: only then can a literal of it be
/// what `file!` expanded to, and only then is its source map read. `line!()` and `column!()`
/// expand to their values (hir-expand's `place_expand`), so they need nothing here.
fn calls_location_macros(db: &Db, body: &Body) -> bool {
    body.macro_calls().iter().any(|call| {
        matches!(
            call.loc(db).def.kind,
            MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::File)
                | MacroDefKind::BuiltInEager(_, EagerExpander::Concat)
        )
    })
}

/// The literals of a body that `file!()` expanded to, each with its expansion: the path is the
/// build's rule's ([`name_sources`]), not the expander's. The source map changes with every
/// edit above the body, and this keeps code without such literals from changing with it.
#[blaze_query::tracked(returns(ref))]
pub fn location_macros<'db>(db: &'db Db, owner: DefWithBodyId) -> HashMap<ExprId, HirFileId> {
    let body = Body::of(db, owner);
    if !calls_location_macros(db, body) {
        return HashMap::default();
    }
    let source_map = Body::unmoved_source_map(db, owner);
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let kind = file_id.macro_file()?.loc(db).def.kind;
            matches!(kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::File))
                .then_some((id, file_id))
        })
        // alloc: the query's value, of a body that called a location macro
        .collect()
}

/// The literals of a body that `line!()` expanded to. Lowering reads each in code where the
/// code runs ([`slots_lines`]) from the `Location` of the literal, which the link fills, so a
/// move above the body changes no MIR of it. `column!()` stays a literal: a `Location`'s column
/// is rustc's display width, `column!()`'s counts chars, and no move above a body changes it.
#[blaze_query::tracked(returns(ref))]
pub fn line_literals<'db>(db: &'db Db, owner: DefWithBodyId) -> HashSet<ExprId> {
    let line = |kind| matches!(kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::Line));
    let body = Body::of(db, owner);
    if !body.macro_calls().iter().any(|call| line(call.loc(db).def.kind)) {
        return HashSet::default();
    }
    let source_map = Body::unmoved_source_map(db, owner);
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter(|&(id, _)| {
            (source_map.expr_syntax(id).ok())
                .and_then(|source| source.file_id.macro_file())
                .is_some_and(|call| line(call.loc(db).def.kind))
        })
        .map(|(id, _)| id)
        // alloc: the query's value, of a body that called `line!`
        .collect()
}

/// Whether lowering `owner` reads its `line!()` literals where its code runs: in a function
/// that is neither `#[track_caller]`, where `Location::caller` is its caller's, nor `const`,
/// where the value is evaluated.
pub fn slots_lines(db: &Db, owner: crate::InferBodyId<'_>) -> bool {
    match owner {
        crate::InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => {
            !track_caller(db, func) && !FunctionSignature::of(db, func).is_const()
        }
        _ => false,
    }
}

/// `Location::caller` of `core`'s `Location`, the lang item `location` is.
pub fn location_caller(db: &Db, location: hir_def::StructId) -> Option<FunctionId> {
    let self_ty = crate::method_resolution::SelfTyKey(crate::next_solver::SimplifiedType::Adt(
        hir_def::AdtId::StructId(location).into(),
    ));
    let impls = crate::method_resolution::InherentImpls::in_crate_for_self_ty(
        db,
        hir_def::HasModule::krate(&location, db),
        self_ty,
    );
    impls.iter().find_map(|imp| {
        imp.impl_items(db).items.iter().find_map(|(name, item)| match *item {
            hir_def::AssocItemId::FunctionId(func) if name.as_str() == "caller" => Some(func),
            _ => None,
        })
    })
}

/// The literals of a body that `concat!` made of arguments with `file!()` among them, each with
/// the expansion it is in.
#[blaze_query::tracked(returns(ref))]
pub fn located_concats<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
) -> HashMap<ExprId, HirFileId> {
    let body = Body::of(db, owner);
    if !calls_location_macros(db, body) {
        return HashMap::default();
    }
    let source_map = Body::unmoved_source_map(db, owner);
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let loc = file_id.macro_file()?.loc(db);
            let concat =
                matches!(loc.def.kind, MacroDefKind::BuiltInEager(_, EagerExpander::Concat));
            (concat && names_location(loc.to_node_view(db).value)).then_some((id, file_id))
        })
        // alloc: the query's value, of a body that called a location macro
        .collect()
}

// dyn: the build's rule, told once a build and called once per file (`file_source_path`)
type SourcePaths = dyn Fn(&Db, base_db::FileId) -> Option<String> + Send + Sync;

/// How a build names files, told by [`name_sources`]. Two rules with the same `key` name every
/// file the same: the key is all the rule depends on besides what it reads of the database.
#[derive(Clone)] // clone: a field of the `SourceNames` input, which the database copies once per image it writes
pub struct NamingRule {
    key: Box<str>,
    paths: std::sync::Arc<SourcePaths>,
}

impl std::fmt::Debug for NamingRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NamingRule").field(&self.key).finish()
    }
}

/// What the build calls each file, for a constant with `file!()` in its value: the evaluator
/// has no build to ask, and a constant's value depends on it, so it is an input.
#[blaze_query::input(singleton, debug)]
pub struct SourceNames {
    #[returns(ref)]
    pub rule: NamingRule,
}

/// Tells the evaluator of constants what the build calls each file: `Symbols::source_path` in
/// blaze. `key` names everything `paths` depends on besides the database (the checkout and its
/// base, say); telling the same key again changes nothing, so no constant is evaluated again.
pub fn name_sources(
    db: &mut Db,
    key: &str,
    paths: impl Fn(&Db, base_db::FileId) -> Option<String> + Send + Sync + 'static,
) {
    // alloc: once per naming rule a build tells
    let rule = NamingRule { key: key.into(), paths: std::sync::Arc::new(paths) };
    match SourceNames::try_get(db) {
        Some(names) if names.rule(db).key == rule.key => {}
        Some(names) => {
            names.set_rule(db).with_durability(blaze_query::db::Durability::HIGH).to(rule);
        }
        None => {
            _ = SourceNames::builder(rule).durability(blaze_query::db::Durability::HIGH).new(db);
            // Reading a singleton that does not exist yet is not tracked: what read it before
            // reported an untracked read (`file_source_path`), which a new revision makes stale.
            // The read may be under a memo of any durability, so the write is of the highest.
            db.storage_mut().synthetic_write(blaze_query::db::Durability::HIGH);
        }
    }
}

/// What the build calls `file`, as `file!()` and `Location::file` say: none when the
/// evaluator was not told ([`name_sources`]).
pub fn source_path<'db>(db: &'db Db, file: base_db::FileId) -> Option<&'db str> {
    file_source_path(db, db.file_text(file))
}

#[blaze_query::tracked(returns(as_deref))]
fn file_source_path<'db>(db: &'db Db, file: base_db::FileText) -> Option<String> {
    let Some(names) = SourceNames::try_get(db) else {
        blaze_query::db::report_untracked_read(db);
        return None;
    };
    (names.rule(db).paths)(db, *file.file_id(db))
}

/// The text of the literal `expr` of `owner` when it is what `file!()`, or a `concat!` with
/// `file!()` among its arguments, expanded to: the expander gives a placeholder for the path,
/// and a constant is evaluated from what the build names the file. `None` for any other
/// expression; an error, never the placeholder, for one of those it can not place.
pub fn placed(db: &Db, owner: DefWithBodyId, expr: ExprId) -> Option<Result<String, &'static str>> {
    if let Some(&file_id) = located_concats(db, owner).get(&expr) {
        return Some(placed_concat(db, file_id));
    }
    let &file_id = location_macros(db, owner).get(&expr)?;
    Some(placed_file(db, file_id))
}

/// What the build calls `file`, or why there is no name.
fn named(db: &Db, file: base_db::FileId) -> Result<&str, &'static str> {
    source_path(db, file).ok_or(
        "`file!()` evaluated with no source names: the build did not tell the evaluator \
         what it calls the files (`name_sources`)",
    )
}

fn placed_concat(db: &Db, file_id: HirFileId) -> Result<String, &'static str> {
    use std::fmt::Write;
    let call = file_id.macro_file().ok_or("a `concat!` of `file!()` not in a macro file")?;
    let call = call.loc(db).to_node_view(db);
    // alloc: the value of the `concat!`, made only where one of a place is evaluated
    let mut text = String::new();
    let parts = concat_parts(call.value).ok_or("a `concat!` of `file!()` whose arguments do not parse")?;
    for part in parts {
        match part {
            ConcatPart::Text(part) => text.push_str(&part),
            ConcatPart::Location(expander, range) => {
                let (file, line, col) = macro_location_of(db, call.file_id, range)
                    .ok_or("a `file!()` in a `concat!` with no place in a source file")?;
                match expander {
                    BuiltinFnLikeExpander::Line => _ = write!(text, "{line}"),
                    BuiltinFnLikeExpander::Column => _ = write!(text, "{col}"),
                    _ => text.push_str(named(db, file)?),
                }
            }
        }
    }
    Ok(text)
}

fn placed_file(db: &Db, file_id: HirFileId) -> Result<String, &'static str> {
    let site = written_call(db, file_id).ok_or("a `file!()` whose call is not written in the source")?;
    let range = InFile::new(site.file_id, written_range(site.value));
    let FileRange { file_id, range } = range.original_node_file_range_rooted(db);
    let (file, _, _) = macro_location_of(db, file_id.into(), range)
        .ok_or("a `file!()` with no place in a source file")?;
    // alloc: the value of `file!()`, made only where one is evaluated
    Ok(named(db, file)?.to_owned())
}

/// The macro of `name` that expands to a place in the source.
fn location_expander(name: &str) -> Option<BuiltinFnLikeExpander> {
    match name {
        "line" => Some(BuiltinFnLikeExpander::Line),
        "column" => Some(BuiltinFnLikeExpander::Column),
        "file" => Some(BuiltinFnLikeExpander::File),
        _ => None,
    }
}

/// Whether a macro call is passed a call of `file!`. A `concat!` of only `line!()` and
/// `column!()` already holds their values.
fn names_location(call: Node<'_>) -> bool {
    call.descendants_with_tokens().filter_map(|it| it.into_token()).any(|token| {
        token.kind() == SyntaxKind::IDENT
            && token.text() == "file"
            && std::iter::successors(token.next_token(), |it| it.next_token())
                .find(|next| !next.kind().is_trivia())
                .is_some_and(|next| next.kind() == SyntaxKind::BANG)
    })
}

/// An argument of a `concat!`.
pub enum ConcatPart {
    Text(String),
    /// A call of `line!`, `column!` or `file!`, and where it is.
    Location(BuiltinFnLikeExpander, TextRange),
}

/// The arguments of a `concat!` call, when each is a literal or a call of `line!`, `column!`
/// or `file!`.
pub fn concat_parts(call: Node<'_>) -> Option<Vec<ConcatPart>> {
    let tree = ast::MacroCall::cast(call)?.token_tree()?;
    let mut inside =
        tree.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).peekable();
    // The delimiters: one opens the arguments, the last one closes them.
    inside.next()?;
    inside.peek()?;
    // alloc: the parts, made only where a `concat!` of a place is evaluated
    let mut parts = Vec::new();
    let mut argument = Argument::default();
    while let Some(element) = inside.next() {
        if inside.peek().is_none() {
            break;
        }
        match element.kind() == SyntaxKind::COMMA {
            true => {
                if argument.len != 0 {
                    parts.push(concat_part(std::mem::take(&mut argument))?);
                }
            }
            false => argument.push(element),
        }
    }
    if argument.len != 0 {
        parts.push(concat_part(argument)?);
    }
    Some(parts)
}

/// What [`concat_part`] reads of an argument as its elements go by: the first, the last three
/// and how many.
#[derive(Clone, Copy, Default)]
struct Argument<'t> {
    first: Option<Element<'t>>,
    last: [Option<Element<'t>>; 3],
    len: usize,
}

impl<'t> Argument<'t> {
    fn push(&mut self, element: Element<'t>) {
        self.first.get_or_insert(element);
        self.last = [self.last[1], self.last[2], Some(element)];
        self.len += 1;
    }
}

fn concat_part(argument: Argument<'_>) -> Option<ConcatPart> {
    if let (1, Some(NodeOrToken::Token(token))) = (argument.len, argument.first) {
        let text = match token.kind() {
            // alloc: the part's text
            SyntaxKind::STRING => ast::String::cast(token)?.value().ok()?.into_owned(),
            // alloc: the part's text
            SyntaxKind::INT_NUMBER => ast::IntNumber::cast(token)?.value().ok()?.to_string(),
            // alloc: the part's text
            SyntaxKind::CHAR => ast::Char::cast(token)?.value().ok()?.to_string(),
            // alloc: the part's text
            SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => token.text().to_owned(),
            _ => return None,
        };
        return Some(ConcatPart::Text(text));
    }
    let [Some(NodeOrToken::Token(name)), Some(bang), Some(NodeOrToken::Node(arguments))] =
        argument.last
    else {
        return None;
    };
    let (expander, first) = (location_expander(name.text())?, argument.first?);
    let empty = arguments.children_with_tokens().filter(|it| !it.kind().is_trivia()).count() == 2;
    (bang.kind() == SyntaxKind::BANG && empty).then(|| {
        let range = first.text_range().cover(arguments.text_range());
        ConcatPart::Location(expander, range)
    })
}

/// The macro call an expansion comes from that is itself written out, in the source or in
/// what a macro was passed, and not by another macro.
pub fn written_call<'db>(db: &'db Db, file_id: HirFileId) -> Option<InFile<Node<'db>>> {
    let mut call = file_id.macro_file()?.loc(db).to_node_view(db);
    while let Some(outer) = call.file_id.macro_file() {
        let start = TextRange::empty(call.value.text_range().start());
        if call.file_id.span_map(db).span_for_range(start).ctx.is_root() {
            break;
        }
        call = outer.loc(db).to_node_view(db);
    }
    Some(call)
}

/// File, line and column of where `range` of `file_id` begins as rustc reports it (the column
/// in display width, as [`line_and_column`] gives it).
pub fn location_of(
    db: &Db,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    line_and_column(db, reported_file_range(db, file_id, range))
}

/// [`location_of`] as `line!()` and `column!()` expand to it: the column is rustc's
/// `lookup_char_pos(..).col + 1`, in chars (a tab is one), not the display width.
pub fn macro_location_of(
    db: &Db,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let FileRange { file_id, range } = reported_file_range(db, file_id, range);
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let place = base_db::line_index(db, text).place(range.start())?;
    Some((file_id, place.line(), place.char_column(text.text(db))?))
}

/// Where in a file rustc reports `range` of `file_id`: what a macro writes itself from where
/// the outermost macro is called, and what it is passed at its own place.
fn reported_file_range(db: &Db, file_id: HirFileId, range: TextRange) -> FileRange {
    let own_tokens = file_id.macro_file().is_some()
        && !file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root();
    match written_call(db, file_id) {
        Some(call) if own_tokens => InFile::new(call.file_id, written_range(call.value))
            .original_node_file_range_rooted(db),
        _ => InFile::new(file_id, range).original_node_file_range_rooted(db),
    }
}

/// File, line and column of where a range of a file begins: rustc's line, and its
/// `col_display + 1` (what `Location::caller` and debug info are given).
pub fn line_and_column(
    db: &Db,
    FileRange { file_id, range }: FileRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let place = base_db::line_index(db, text).place(range.start())?;
    Some((file_id, place.line(), place.display_column(text.text(db))?))
}

/// File, line and column of a place in a body, as rustc reports them, found from where the
/// body is in its file: the code generator also finds them from the body alone.
#[blaze_query::tracked]
pub fn source_location_in_file<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    location_of(db, file_id, range)
}

/// Where a span of a body is written, as one of the body's source maps has it.
pub enum SpanSource {
    /// Its range: the closing brace of a block, which is where rustc has a scope left, or a
    /// pattern's node.
    Range(HirFileId, TextRange),
    /// An expression, the range of which as rustc reports it [`reported_range`] reads from its
    /// node.
    Expr(InFile<SyntaxNodePtr>, ExprId),
}

/// Where `span` of `owner` is written by `source_map`: the map moved into the body's file
/// ([`Body::with_source_map`]) or that of the tree of its anchor ([`Body::relative`]).
pub fn span_source(
    db: &Db,
    owner: DefWithBodyId,
    source_map: &BodySourceMap,
    span: MirSpan,
) -> Option<SpanSource> {
    let source = match span {
        MirSpan::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
        // The block an `async fn` is taken to wrap its body in is not written anywhere, and
        // ends where the body does.
        MirSpan::ExprEnd(expr) => {
            match (source_map.expr_syntax(expr), Body::of(db, owner).expr(expr)) {
                (Ok(written), _) => written,
                (Err(_), hir_def::hir::Expr::Block { tail: Some(tail), .. }) => {
                    source_map.expr_syntax(tail).ok()?
                }
                (Err(_), _) => return None,
            }
        }
        MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
        // An inlined span indexes the optimized body's `Inlined`, not `owner`'s map: it is
        // resolved through that body first.
        MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown | MirSpan::Inlined(_) => {
            return None;
        }
    };
    let ptr = source.value.syntax_node_ptr();
    Some(match span {
        MirSpan::ExprEnd(_) => {
            let range = ptr.text_range();
            let last = range.end().checked_sub(TextSize::new(1)).filter(|it| *it >= range.start());
            SpanSource::Range(source.file_id, TextRange::new(last?, range.end()))
        }
        MirSpan::ExprId(expr) => SpanSource::Expr(InFile::new(source.file_id, ptr), expr),
        _ => SpanSource::Range(source.file_id, ptr.text_range()),
    })
}

/// The file a place in a body is written in, which may be what a macro expands to, and
/// where in it rustc has the place begin.
pub fn source_range<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(HirFileId, TextRange)> {
    match span_source(db, owner, &Body::with_source_map(db, owner).1, span)? {
        SpanSource::Range(file_id, range) => Some((file_id, range)),
        SpanSource::Expr(source, expr) => {
            let node = ast::to_node(source.value, source.file_id.root(db));
            Some((source.file_id, reported_range(db, owner, expr, source.file_id, node)?))
        }
    }
}

/// The part of an expression that rustc's span of it covers where it panics: a method call
/// from the method's name on, an index through `Index` from its bracket on, and any other
/// expression together with the parentheses it is written in.
pub fn reported_range(
    db: &Db,
    owner: DefWithBodyId,
    expr: ExprId,
    file_id: HirFileId,
    node: Node<'_>,
) -> Option<TextRange> {
    let end = node.text_range().end();
    if let Some(call) = ast::MethodCallExpr::cast(node) {
        return Some(TextRange::new(call.name_ref()?.syntax().text_range().start(), end));
    }
    if let Some(written) = ast::IndexExpr::cast(node)
        && let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner).expr(expr)
        && !is_builtin_index(InferenceResult::of(db, owner), base, index)
    {
        return Some(TextRange::new(written.l_brack_token()?.text_range().start(), end));
    }
    Some(parenthesized(db, file_id, node))
}

/// The range of an expression with the parentheses around it. rustc's span of an expression
/// includes them when both are written in one place, and not when a macro put the
/// parentheses around what it was passed.
fn parenthesized(db: &Db, file_id: HirFileId, node: Node<'_>) -> TextRange {
    let passed_in = |range: TextRange| {
        file_id.macro_file().is_none()
            || file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root()
    };
    let inner = passed_in(node.text_range());
    node.ancestors()
        .skip(1)
        .take_while(|it| it.kind() == SyntaxKind::PAREN_EXPR && passed_in(it.text_range()) == inner)
        .last()
        .map_or(node.text_range(), |it| it.text_range())
}

/// Whether calls to `func` pass a `&Location` after the declared arguments. A method of a trait
/// impl inherits the attribute from the trait's declaration.
pub fn track_caller(db: &Db, func: FunctionId) -> bool {
    let has =
        |func: FunctionId| AttrFlags::query(db, func.into()).contains(AttrFlags::TRACK_CALLER);
    if has(func) {
        return true;
    }
    let ItemContainerId::ImplId(impl_id) = func.lookup(db).container else {
        return false;
    };
    let Some(trait_ref) = db.impl_trait(impl_id) else {
        return false;
    };
    let name = &FunctionSignature::of(db, func).name;
    trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name).is_some_and(has)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of the first macro call in `text`: a literal's text, or where a call of a
    /// place is.
    fn parts(text: &str) -> Option<Vec<Result<String, TextRange>>> {
        let parse = syntax::SourceFile::parse(text, syntax::Edition::CURRENT);
        let call = parse.root().descendants().find(|it| it.kind() == SyntaxKind::MACRO_CALL)?;
        let parts = concat_parts(call)?.into_iter().map(|it| match it {
            ConcatPart::Text(text) => Ok(text),
            ConcatPart::Location(_, range) => Err(range),
        });
        Some(parts.collect())
    }

    fn range(text: &str, of: &str) -> TextRange {
        let start = text.find(of).unwrap();
        TextRange::new(TextSize::new(start as u32), TextSize::new((start + of.len()) as u32))
    }

    #[test]
    fn concat_arguments_are_read_in_place() {
        let text = r#"const C: &str = concat!("a\n", 12, 'c', true,);"#;
        let read = ["a\n", "12", "c", "true"].map(|it| Ok(it.to_owned()));
        assert_eq!(parts(text).unwrap(), read);
        let text = r#"const C: &str = concat!("x", line!(), file! ( ));"#;
        let read = vec![Ok("x".to_owned()), Err(range(text, "line!()")), Err(range(text, "file! ( )"))];
        assert_eq!(parts(text), Some(read));
        assert_eq!(parts("const C: &str = concat!();"), Some(Vec::new()));
        assert_eq!(parts("const C: &str = concat!(,,);"), Some(Vec::new()));
        // What is not a literal or a call of a place with nothing passed to it.
        assert_eq!(parts("const C: &str = concat!(x);"), None);
        assert_eq!(parts("const C: &str = concat!(\"a\" \"b\");"), None);
        assert_eq!(parts("const C: &str = concat!(line!(1));"), None);
        assert_eq!(parts("const C: &str = concat!(other!());"), None);
    }

    #[test]
    fn a_call_of_a_place_is_found_past_blank_space() {
        let call = |text: &str| {
            let parse = syntax::SourceFile::parse(text, syntax::Edition::CURRENT);
            let call = parse.root().descendants().find(|it| it.kind() == SyntaxKind::MACRO_CALL);
            names_location(call.unwrap())
        };
        assert!(call("const C: &str = concat!(file /* here */ !());"));
        assert!(!call("const C: &str = concat!(file, line);"));
        // Their values are already in the expansion.
        assert!(!call("const C: &str = concat!(line!(), column!());"));
    }
}
