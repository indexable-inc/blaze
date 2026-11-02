//! Documentation extraction and source mapping.
//!
//! This module handles the extraction and processing of doc comments and `#[doc = "..."]`
//! attributes, including macro expansion for `#[doc = macro!()]` patterns.
//! It builds a concatenated string of the full docs as well as a source map
//! to map it back to AST (which is needed for things like resolving links in doc comments
//! and highlight injection).

use std::{
    borrow::Cow,
    convert::Infallible,
    ops::{ControlFlow, Range},
};

use base_db::{Db, Crate};
use cfg::CfgOptions;
use either::Either;
use hir_expand::{
    AstId, ExpandTo, HirFileId, InFile, MacroCallId,
    attrs::expand_cfg_attr_with_doc_comments_view,
    mod_path::ModPath,
    span_map::SpanMap,
};
use span::AstIdMap;
use syntax::view::{self as ast, AstNode, HasAttrs, IsString};
use thin_vec::ThinVec;
use tt::{TextRange, TextSize};

use crate::{macro_call_as_call_id, nameres::MacroSubNs, resolver::Resolver};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DocsSourceMapLine {
    /// The offset in [`Docs::docs`].
    string_offset: TextSize,
    /// The offset in the AST of the text. `None` for macro-expanded doc strings
    /// where we cannot provide a faithful source mapping.
    ast_offset: Option<TextSize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Docs {
    /// The concatenated string of all `#[doc = "..."]` attributes and documentation comments.
    docs: String,
    /// A sorted map from an offset in `docs` to an offset in the source code.
    docs_source_map: Vec<DocsSourceMapLine>,
    /// If the item is an outlined module (`mod foo;`), `docs_source_map` stores the concatenated
    /// list of the outline and inline docs (outline first). Then, this field contains the [`HirFileId`]
    /// of the outline declaration, and the index in `docs` from which the inline docs
    /// begin.
    outline_mod: Option<(HirFileId, usize)>,
    inline_file: HirFileId,
    /// The size of the prepended prefix, which does not map to real doc comments.
    prefix_len: TextSize,
    /// The offset in `docs` from which the docs are inner attributes/comments.
    inline_inner_docs_start: Option<TextSize>,
    /// Like `inline_inner_docs_start`, but for `outline_mod`. This can happen only when merging `Docs`
    /// (as outline modules don't have inner attributes).
    outline_inner_docs_start: Option<TextSize>,
    /// All macro calls in `#[doc = ...]` attributes, recursively.
    macro_calls: ThinVec<(AstId<syntax::ast::MacroCall>, MacroCallId)>,
}

#[derive(Clone, Copy)]
enum DocCommentKind {
    /// `///` etc..
    Sugared(ast::CommentShape),
    /// `#[doc = ""]`.
    Desugared,
}

#[derive(Default)]
struct Indent {
    lines: Vec<Option<(usize, DocCommentKind)>>,
    seen_sugared: bool,
}

/// The buffers a run of [`extract_docs`] builds in, kept by the worker: the text, its source
/// map and the indent of each line grow to the largest docs seen and are reused.
#[derive(Default)]
struct DocBuffers {
    docs: String,
    source_map: Vec<DocsSourceMapLine>,
    indent: Vec<Option<(usize, DocCommentKind)>>,
}

thread_local! {
    static SPARE_DOC_BUFFERS: std::cell::RefCell<Option<DocBuffers>> =
        const { std::cell::RefCell::new(None) };
}

impl blaze_mem::scratch::Shelved for DocBuffers {
    fn empty(&mut self) {
        self.docs.clear();
        self.source_map.clear();
        self.indent.clear();
    }

    fn held(&self) -> usize {
        self.docs.capacity()
            + self.source_map.capacity() * size_of::<DocsSourceMapLine>()
            + self.indent.capacity() * size_of::<Option<(usize, DocCommentKind)>>()
    }

    fn unshelve() -> Option<Self> {
        SPARE_DOC_BUFFERS.try_with(|spare| spare.borrow_mut().take()).ok().flatten()
    }

    fn shelve(self) {
        // A thread that is ending, or a nested run's smaller set: freed.
        let _ = SPARE_DOC_BUFFERS.try_with(|spare| {
            let mut spare = spare.borrow_mut();
            if spare.as_ref().is_none_or(|it| it.held() < self.held()) {
                *spare = Some(self);
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsInnerDoc {
    No,
    Yes,
}

impl IsInnerDoc {
    #[inline]
    pub fn yes(self) -> bool {
        self == IsInnerDoc::Yes
    }
}

impl Docs {
    #[inline]
    pub fn docs(&self) -> &str {
        &self.docs
    }

    #[inline]
    pub fn into_docs(self) -> String {
        self.docs
    }

    #[inline]
    pub fn macro_calls(
        &self,
    ) -> impl Iterator<Item = (AstId<syntax::ast::MacroCall>, MacroCallId)> {
        self.macro_calls.iter().copied()
    }

    fn is_empty(&self) -> bool {
        let Self {
            docs,
            docs_source_map: _,
            outline_mod: _,
            inline_file: _,
            prefix_len: _,
            inline_inner_docs_start: _,
            outline_inner_docs_start: _,
            macro_calls,
        } = self;
        docs.is_empty() && macro_calls.is_empty()
    }

    pub fn find_ast_range(
        &self,
        mut string_range: TextRange,
    ) -> Option<(InFile<TextRange>, IsInnerDoc)> {
        if string_range.start() < self.prefix_len {
            return None;
        }
        string_range -= self.prefix_len;

        let mut file = self.inline_file;
        let mut inner_docs_start = self.inline_inner_docs_start;
        // Check whether the range is from the outline, the inline, or both.
        let source_map = if let Some((outline_mod_file, outline_mod_end)) = self.outline_mod {
            if let Some(first_inline) = self.docs_source_map.get(outline_mod_end) {
                // An empty range where the inline starts is in the inline: the outline can have
                // no lines at all.
                if string_range.start() >= first_inline.string_offset {
                    // The range is completely in the inline.
                    &self.docs_source_map[outline_mod_end..]
                } else if string_range.end() <= first_inline.string_offset {
                    // The range is completely in the outline.
                    file = outline_mod_file;
                    inner_docs_start = self.outline_inner_docs_start;
                    &self.docs_source_map[..outline_mod_end]
                } else {
                    // The range is combined from the outline and the inline - cannot map it back.
                    return None;
                }
            } else {
                // There is no inline.
                file = outline_mod_file;
                inner_docs_start = self.outline_inner_docs_start;
                &self.docs_source_map
            }
        } else {
            // There is no outline.
            &self.docs_source_map
        };

        // Nothing starts at or before the range when the docs have no line that is written out.
        let after_range = source_map
            .partition_point(|line| line.string_offset <= string_range.start())
            .checked_sub(1)?;
        let after_range = &source_map[after_range..];
        let line = after_range.first()?;
        // Unmapped lines (from macro-expanded docs) cannot be mapped back to AST.
        let ast_offset = line.ast_offset?;
        if after_range.get(1).is_some_and(|next_line| next_line.string_offset < string_range.end())
        {
            // The range is combined from two lines - cannot map it back.
            return None;
        }
        let ast_range = string_range - line.string_offset + ast_offset;
        let is_inner = if inner_docs_start
            .is_some_and(|inner_docs_start| string_range.start() >= inner_docs_start)
        {
            IsInnerDoc::Yes
        } else {
            IsInnerDoc::No
        };
        Some((InFile::new(file, ast_range), is_inner))
    }

    #[inline]
    pub fn shift_by(&mut self, offset: TextSize) {
        self.prefix_len += offset;
    }

    pub fn prepend_str(&mut self, s: &str) {
        self.prefix_len += TextSize::of(s);
        self.docs.insert_str(0, s);
    }

    pub fn append_str(&mut self, s: &str) {
        self.docs.push_str(s);
    }

    pub fn append(&mut self, other: &Docs) {
        let other_offset = TextSize::of(&self.docs);

        assert!(
            self.outline_mod.is_none() && other.outline_mod.is_none(),
            "cannot merge `Docs` that have `outline_mod` set"
        );
        self.outline_mod = Some((self.inline_file, self.docs_source_map.len()));
        self.inline_file = other.inline_file;
        self.outline_inner_docs_start = self.inline_inner_docs_start;
        self.inline_inner_docs_start = other.inline_inner_docs_start.map(|it| it + other_offset);

        self.docs.push_str(&other.docs);
        self.docs_source_map.extend(other.docs_source_map.iter().map(
            |&DocsSourceMapLine { string_offset, ast_offset }| DocsSourceMapLine {
                ast_offset,
                string_offset: string_offset + other_offset,
            },
        ));
    }

    fn extend_with_doc_comment(&mut self, comment: ast::DocComment<'_>, indent: &mut Indent) {
        let doc = comment.text();
        let offset = comment.syntax().text_range().start() + ast::DocComment::PREFIX_LEN;
        self.extend_with_doc_str(doc, offset, DocCommentKind::Sugared(comment.shape()), indent);
    }

    fn extend_with_doc_attr(&mut self, value: ast::String<'_>, indent: &mut Indent) {
        let Some(value_offset) = value.text_range_between_quotes() else { return };
        let value_offset = value_offset.start();
        let Ok(unescaped) = value.value() else { return };
        // `value()` borrows the source text only when the string has no escapes. With escapes,
        // offsets in the unescaped text don't match offsets in the source, so leave it unmapped.
        // FIXME: Map the text between escapes.
        let ast_offset = matches!(unescaped, Cow::Borrowed(_)).then_some(value_offset);
        self.push_doc_lines(&unescaped, ast_offset, DocCommentKind::Desugared, indent);
    }

    fn extend_with_doc_str(
        &mut self,
        doc: &str,
        offset_in_ast: TextSize,
        comment_kind: DocCommentKind,
        indent: &mut Indent,
    ) {
        self.push_doc_lines(doc, Some(offset_in_ast), comment_kind, indent);
    }

    fn extend_with_unmapped_doc_str(&mut self, doc: &str, indent: &mut Indent) {
        self.push_doc_lines(doc, None, DocCommentKind::Desugared, indent);
    }

    /// Beautifies `doc` and appends the result to `self.docs`, one line at a time via
    /// [`Docs::push_doc_line`]. Mirrors rustc's [`beautify_doc_string`], delegating to
    /// [`get_vertical_trim`] and [`get_horizontal_trim`] for the multi-line case.
    ///
    /// Individual `///` line comments always reach us as a single-line `doc`, so the
    /// `!doc.contains('\n')` fast path fires and the multi-line logic never runs on them.
    /// Desugared `#[doc = "..."]` strings and macro-expanded docs also route through here
    /// with `shape = CommentShape::Line`, matching rustc.
    ///
    /// Unlike rustc's version, which joins the beautified lines into a new interned `Symbol`,
    /// this port pushes each line individually via [`Docs::push_doc_line`] and pairs it with
    /// its byte offset relative to `doc`'s start so the source-map records accurate per-line
    /// offsets.
    ///
    /// [`beautify_doc_string`]: https://github.com/rust-lang/rust/blob/16a623ad672a92409b5c04beb303583c6cf72a7e/compiler/rustc_ast/src/util/comments.rs#L37
    fn push_doc_lines(
        &mut self,
        doc: &str,
        ast_offset: Option<TextSize>,
        comment_kind: DocCommentKind,
        indent: &mut Indent,
    ) {
        // Note: this is pushed even if there are only empty lines here, because that's what rustdoc does.
        let shape = match comment_kind {
            DocCommentKind::Sugared(shape) => {
                indent.seen_sugared = true;
                shape
            }
            // rustc uses `Line` for desugared comments.
            DocCommentKind::Desugared => ast::CommentShape::Line,
        };

        if !doc.contains('\n') {
            self.push_doc_line(doc, ast_offset, comment_kind, indent);
            return;
        }

        let (i, j) = get_vertical_trim(doc).unwrap_or((0, doc.lines().count()));
        let lines = || doc.lines().skip(i).take(j - i);
        let horizontal = get_horizontal_trim(lines(), shape);
        for line in lines() {
            let mut line_offset = TextSize::new(doc.substr_range(line).unwrap().start as u32);
            let mut line = line;
            // Strip `"[ \t]*\*"` from each line where present, exactly like rustc.
            if let Some(horizontal) = horizontal
                && let Some(rest) = line.strip_prefix(horizontal)
            {
                line = rest;
                line_offset += TextSize::of(horizontal);
                if shape == ast::CommentShape::Block
                    && (line == "*" || line.starts_with("* ") || line.starts_with("**"))
                {
                    line = &line[1..];
                    line_offset += TextSize::of("*");
                }
            }
            self.push_doc_line(line, ast_offset.map(|it| it + line_offset), comment_kind, indent);
        }
    }

    /// Appends a single beautified line to `self.docs` and records its source-map row.
    fn push_doc_line(
        &mut self,
        line: &str,
        ast_offset: Option<TextSize>,
        comment_kind: DocCommentKind,
        indent: &mut Indent,
    ) {
        self.docs_source_map
            .push(DocsSourceMapLine { string_offset: TextSize::of(&self.docs), ast_offset });

        let line = line.trim_end();
        let line_indent = if line.chars().any(|ch| !ch.is_whitespace()) {
            // Empty lines are handled because `any()` returns `false` for them.
            let line_indent = line.bytes().take_while(|c| *c == b' ' || *c == b'\t').count();
            Some((line_indent, comment_kind))
        } else {
            None
        };
        indent.lines.push(line_indent);
        self.docs.push_str(line);
        self.docs.push('\n');
    }

    fn remove_indent(&mut self, indent: &Indent) {
        /// In case of panics the text is half shifted and already moved out of `self.docs`, so
        /// we drop everything that points into it.
        struct Guard<'a>(&'a mut Docs);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let Docs {
                    docs,
                    docs_source_map,
                    outline_mod,
                    inline_file: _,
                    prefix_len: _,
                    inline_inner_docs_start: _,
                    outline_inner_docs_start: _,
                    macro_calls: _,
                } = self.0;
                docs.clear();
                // This is just to avoid panics down the road.
                docs_source_map.clear();
                *outline_mod = None;
            }
        }

        if self.docs.is_empty() {
            return;
        }

        // `add` is used in case the most common sugared doc syntax is used ("/// "). The other
        // fragments kind's lines are never starting with a whitespace unless they are using some
        // markdown formatting requiring it. Therefore, if the doc block have a mix between the two,
        // we need to take into account the fact that the minimum indent minus one (to take this
        // whitespace into account).
        //
        // For example:
        //
        // /// hello!
        // #[doc = "another"]
        //
        // In this case, you want "hello! another" and not "hello!  another".
        let add_indent = if indent.seen_sugared { 1 } else { 0 };

        let Some(min_indent) = indent
            .lines
            .iter()
            .filter_map(|it| *it)
            .map(|(line_indent, line_kind)| {
                line_indent
                    + match line_kind {
                        DocCommentKind::Sugared(_) => 0,
                        DocCommentKind::Desugared => add_indent,
                    }
            })
            .min()
        else {
            return;
        };

        let docs_len = TextSize::of(&self.docs);
        let mut bytes = std::mem::take(&mut self.docs).into_bytes();
        let guard = Guard(&mut *self);
        let source_map = guard.0.docs_source_map.as_mut_slice();
        let Some(&DocsSourceMapLine { string_offset: mut copy_into, .. }) = source_map.first()
        else {
            return;
        };
        // We remove multiple ranges from the text in place, without O(N^2) or allocations: for
        // each line, we copy the line minus the indent right after the previous line (which may
        // have moved). Then at the end we truncate and check the UTF-8 once.
        let mut accumulated_offset = TextSize::new(0);
        for idx in 0..source_map.len() {
            let string_end_offset =
                source_map.get(idx + 1).map_or(docs_len, |next_attr| next_attr.string_offset);
            let line_source = &mut source_map[idx];
            let line_range = TextRange::new(line_source.string_offset, string_end_offset);
            let line_docs = &bytes[Range::<usize>::from(line_range)];
            let line_docs_len = line_range.len();
            let indent_size = if let Some((_, DocCommentKind::Desugared)) = indent.lines[idx]
                && min_indent > 0
            {
                min_indent - add_indent
            } else {
                min_indent
            };
            // Every copy lands at or before its own line's start, so this line's bytes are
            // still the UTF-8 that was pushed and its char starts are its non-continuation bytes.
            let indent_size = char_starts(line_docs).nth(indent_size).map_or_else(
                || line_docs_len - TextSize::of("\n"),
                |offset| TextSize::new(offset as u32),
            );
            bytes.copy_within(
                Range::<usize>::from(TextRange::new(
                    line_source.string_offset + indent_size,
                    string_end_offset,
                )),
                copy_into.into(),
            );
            copy_into += line_docs_len - indent_size;

            if let Some(inner_attrs_start) = &mut guard.0.inline_inner_docs_start
                && *inner_attrs_start == line_source.string_offset
            {
                *inner_attrs_start -= accumulated_offset;
            }
            // The removals in the string accumulate, but in the AST not, because it already points
            // to the beginning of each attribute.
            // Also, we need to shift the AST offset of every line, but the string offset of the first
            // line should not get shifted (in general, the shift for the string offset is by the
            // number of lines until the current one, excluding the current one).
            line_source.string_offset -= accumulated_offset;
            if let Some(ref mut ast_offset) = line_source.ast_offset {
                *ast_offset += indent_size;
            }

            accumulated_offset += indent_size;
        }
        bytes.truncate(copy_into.into());

        std::mem::forget(guard);
        // alloc: none: `from_utf8` keeps `bytes`' buffer
        self.docs = String::from_utf8(bytes)
            .expect("doc lines end in `\\n` and their indent ends on a char start");
    }

    fn remove_last_newline(&mut self) {
        self.docs.truncate(self.docs.len().saturating_sub(1));
    }
}

/// rustc's [`beautify_doc_string`]'s vertical trim, modulo `CommentKind`/`CommentShape`
/// renaming, over the lines of `doc`.
///
/// [`beautify_doc_string`]: https://github.com/rust-lang/rust/blob/16a623ad672a92409b5c04beb303583c6cf72a7e/compiler/rustc_ast/src/util/comments.rs#L38
fn get_vertical_trim(doc: &str) -> Option<(usize, usize)> {
    let len = doc.lines().count();
    let mut i = 0;
    let mut j = len;
    // first line of all-stars should be omitted
    if doc.lines().next().is_some_and(|line| line.chars().all(|c| c == '*')) {
        i += 1;
    }

    // like the first, a last line of all stars should be omitted
    if j > i
        && let Some(last) = doc.lines().next_back()
        && !last.is_empty()
        && last.chars().all(|c| c == '*')
    {
        j -= 1;
    }

    if i != 0 || j != len { Some((i, j)) } else { None }
}

/// rustc's [`beautify_doc_string`]'s horizontal trim, modulo `CommentKind`/`CommentShape`
/// renaming, over `lines`, returning the prefix borrowed from the first line rather than
/// interning it.
///
/// [`beautify_doc_string`]: https://github.com/rust-lang/rust/blob/16a623ad672a92409b5c04beb303583c6cf72a7e/compiler/rustc_ast/src/util/comments.rs#L54
fn get_horizontal_trim<'a>(
    lines: impl Iterator<Item = &'a str> + Clone,
    kind: ast::CommentShape,
) -> Option<&'a str> {
    let mut i = usize::MAX;
    let mut first = true;

    // In case we have doc comments like `/**` or `/*!`, we want to remove stars if they are
    // present. However, we first need to strip the empty lines so they don't get in the middle
    // when we try to compute the "horizontal trim".
    // clone: of `lines`, here and below, a copy of a `str::Lines` cursor, no allocation
    let len = lines.clone().count();
    let (start, end) = match kind {
        ast::CommentShape::Block => {
            // Whatever happens, we skip the first line.
            // clone: a cursor
            let skip = lines.clone().next().map_or(0, |l| {
                if l.trim_start().starts_with('*') { 0 } else { 1 }
            });
            // The first and one past the last line from `skip` on that is not blank; both the
            // end when every one is blank, as rustc's two trims meet there.
            let (mut start, mut end) = (len, len);
            // clone: a cursor
            for (at, line) in lines.clone().enumerate().skip(skip) {
                if !line.trim().is_empty() {
                    if start == len {
                        start = at;
                    }
                    end = at + 1;
                }
            }
            (start, end)
        }
        ast::CommentShape::Line => (0, len),
    };
    let mut lines = lines.skip(start).take(end - start);

    // clone: a cursor
    for line in lines.clone() {
        for (j, c) in line.chars().enumerate() {
            if j > i || !"* \t".contains(c) {
                return None;
            }
            if c == '*' {
                if first {
                    i = j;
                    first = false;
                } else if i != j {
                    return None;
                }
                break;
            }
        }
        if i >= line.len() {
            return None;
        }
    }
    let first = lines.next()?;
    Some(&first[..i])
}

struct DocMacroExpander<'db> {
    db: &'db Db,
    krate: Crate,
    macro_depth: u32,
    recursion_limit: u32,
    resolver: Resolver<'db>,
    file_id: HirFileId,
    ast_id_map: &'db AstIdMap,
    span_map: SpanMap<'db>,
}

fn expand_doc_expr_via_macro_pipeline<'db>(
    expander: &mut DocMacroExpander<'db>,
    macro_calls: &mut ThinVec<(AstId<syntax::ast::MacroCall>, MacroCallId)>,
    expr: ast::Expr<'_>,
) -> Option<String> {
    match expr {
        ast::Expr::ParenExpr(paren_expr) => {
            expand_doc_expr_via_macro_pipeline(expander, macro_calls, paren_expr.expr()?)
        }
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::String(string) => string.value().ok().map(Into::into),
            _ => None,
        },
        ast::Expr::MacroExpr(macro_expr) => {
            let macro_call = macro_expr.macro_call()?;
            expand_doc_macro_call(expander, macro_calls, macro_call)
        }
        _ => None,
    }
}

fn expand_doc_macro_call<'db>(
    expander: &mut DocMacroExpander<'db>,
    macro_calls: &mut ThinVec<(AstId<syntax::ast::MacroCall>, MacroCallId)>,
    macro_call: ast::MacroCall<'_>,
) -> Option<String> {
    if expander.macro_depth >= expander.recursion_limit {
        return None;
    }

    let path = macro_call.path()?;
    let mod_path = ModPath::from_src_view(expander.db, path, &mut |range| {
        expander.span_map.span_for_range(range).ctx
    })?;
    let call_site = expander.span_map.span_for_range(macro_call.syntax().text_range());
    let ast_id = AstId::new(
        expander.file_id,
        expander.ast_id_map.ast_id_of_view::<syntax::ast::MacroCall>(macro_call),
    );
    let call_id = macro_call_as_call_id(
        expander.db,
        ast_id,
        &mod_path,
        call_site.ctx,
        ExpandTo::Expr,
        expander.krate,
        expander.macro_depth + 1,
        expander.recursion_limit,
        |path| {
            expander.resolver.resolve_path_as_macro_def(expander.db, path, Some(MacroSubNs::Bang))
        },
        // alloc: none unless an eager input holds macro calls; a doc macro's are not kept
        &mut Vec::new(),
    )
    .ok()?
    .value?;
    macro_calls.push((ast_id, call_id));

    let (parse, span_map) = &call_id.parse_macro_expansion(expander.db).value;
    let expr = ast::Expr::cast(parse.root())?;

    // Build a new source context for the expansion file so that any further
    // recursive expansion (e.g. a user macro expanding to `concat!(...)`)
    // correctly resolves AstIds and spans in the expansion.
    let expansion_file_id: HirFileId = call_id.into();
    let old_file_id = std::mem::replace(&mut expander.file_id, expansion_file_id);
    let old_span_map =
        std::mem::replace(&mut expander.span_map, SpanMap::ExpansionSpanMap(span_map));
    let old_ast_id_map =
        std::mem::replace(&mut expander.ast_id_map, expansion_file_id.ast_id_map(expander.db));
    expander.macro_depth += 1;

    let expansion = expand_doc_expr_via_macro_pipeline(expander, macro_calls, expr);

    expander.file_id = old_file_id;
    expander.span_map = old_span_map;
    expander.ast_id_map = old_ast_id_map;
    expander.macro_depth -= 1;

    expansion
}

/// The string a `#[key = value]` attribute's value is once the macros in it are expanded, as
/// `#[link_name = concat!(prefix!(), "name")]`.
pub(crate) fn expand_attr_value<'db>(
    db: &'db Db,
    krate: Crate,
    resolver: Resolver<'db>,
    value: InFile<ast::Expr<'_>>,
) -> Option<String> {
    let file_id = value.file_id;
    let recursion_limit = resolver.top_level_def_map().recursion_limit();
    let mut expander = DocMacroExpander {
        db,
        krate,
        macro_depth: file_id.macro_expansion_depth(db),
        recursion_limit,
        resolver,
        file_id,
        ast_id_map: file_id.ast_id_map(db),
        span_map: file_id.span_map(db),
    };
    expand_doc_expr_via_macro_pipeline(&mut expander, &mut ThinVec::new(), value.value)
}

fn extend_with_attrs<'a, 'db>(
    result: &mut Docs,
    db: &'db Db,
    krate: Crate,
    node: ast::Node<'_>,
    file_id: HirFileId,
    expect_inner_attrs: bool,
    indent: &mut Indent,
    get_cfg_options: &impl Fn() -> &'a CfgOptions,
    cfg_options: &mut Option<&'a CfgOptions>,
    make_resolver: &impl Fn() -> Resolver<'db>,
) {
    // Lazily initialised when we first encounter a `#[doc = macro!()]`.
    let mut expander = None;

    expand_cfg_attr_with_doc_comments_view::<_, Infallible>(
        node.children()
            .filter_map(ast::AnyAttr::cast)
            .filter(|attr| attr.kind().is_inner() == expect_inner_attrs)
            .map(|attr| match attr {
                ast::AnyAttr::Attr(it) => Either::Left(it),
                ast::AnyAttr::DocComment(it) => Either::Right(it),
            }),
        || *cfg_options.get_or_insert_with(get_cfg_options),
        |attr| {
            match attr {
                Either::Right(doc_comment) => result.extend_with_doc_comment(doc_comment, indent),
                Either::Left((attr, _)) => match attr {
                    ast::Meta::KeyValueMeta(attr) if super::path_is(attr.path(), "doc") => {
                        if let Some(value) = attr.expr() {
                            if let ast::Expr::Literal(value) = value
                                && let ast::LiteralKind::String(value) = value.kind()
                            {
                                result.extend_with_doc_attr(value, indent);
                            } else {
                                let exp = expander.get_or_insert_with(|| {
                                    let resolver = make_resolver();
                                    let def_map = resolver.top_level_def_map();
                                    let recursion_limit = def_map.recursion_limit();
                                    DocMacroExpander {
                                        db,
                                        krate,
                                        macro_depth: file_id.macro_expansion_depth(db),
                                        recursion_limit,
                                        resolver,
                                        file_id,
                                        ast_id_map: file_id.ast_id_map(db),
                                        span_map: file_id.span_map(db),
                                    }
                                });
                                if let Some(expanded) = expand_doc_expr_via_macro_pipeline(
                                    exp,
                                    &mut result.macro_calls,
                                    value,
                                ) {
                                    result.extend_with_unmapped_doc_str(&expanded, indent);
                                }
                            }
                        }
                    }
                    _ => {}
                },
            }
            ControlFlow::Continue(())
        },
    );
}

/// The node holding `owner`'s inner attributes and inner doc comments.
/// `HasAttrs::inner_attributes_node` leaves an outline's unparsed fn body (#1294) out when its
/// text can't start with `#`, and an inner doc comment (`//!`, `/*!`) is a comment to that check:
/// such a body is entered here when it may start with one.
pub(super) fn inner_docs_node(owner: ast::AnyHasAttrs<'_>) -> Option<ast::Node<'_>> {
    if let Some(node) = owner.inner_attributes_node() {
        return Some(node);
    }
    let body = ast::Fn::cast(owner.syntax())?.body()?;
    if !body.syntax().is_lazy() {
        return None;
    }
    if !ast::block_may_start_with_inner_attr(body.syntax().text(), true) {
        return None;
    }
    Some(body.stmt_list()?.syntax())
}

/// The offsets at which a char of `utf8` starts, as `str::char_indices` gives them.
fn char_starts(utf8: &[u8]) -> impl Iterator<Item = usize> + '_ {
    utf8.iter().enumerate().filter(|&(_, &byte)| byte & 0xC0 != 0x80).map(|(at, _)| at)
}

pub(crate) fn extract_docs<'a, 'db>(
    db: &'db Db,
    krate: Crate,
    resolver: &impl Fn() -> Resolver<'db>,
    get_cfg_options: &impl Fn() -> &'a CfgOptions,
    source: InFile<ast::AnyHasAttrs<'_>>,
    outer_mod_decl: Option<InFile<ast::Module<'_>>>,
    inner_attrs_node: Option<ast::Node<'_>>,
) -> Option<Box<Docs>> {
    blaze_mem::scratch::with::<DocBuffers, _>(|buffers| {
        extract_docs_into(
            buffers,
            db,
            krate,
            resolver,
            get_cfg_options,
            source,
            outer_mod_decl,
            inner_attrs_node,
        )
    })
}

/// [`extract_docs`] in the worker's `buffers`: the memo gets an exact copy of the text and the
/// source map, and the buffers go back to the worker, grown to the largest docs seen.
#[allow(clippy::too_many_arguments)]
fn extract_docs_into<'a, 'db>(
    buffers: &mut DocBuffers,
    db: &'db Db,
    krate: Crate,
    resolver: &impl Fn() -> Resolver<'db>,
    get_cfg_options: &impl Fn() -> &'a CfgOptions,
    source: InFile<ast::AnyHasAttrs<'_>>,
    outer_mod_decl: Option<InFile<ast::Module<'_>>>,
    inner_attrs_node: Option<ast::Node<'_>>,
) -> Option<Box<Docs>> {
    let mut result = Docs {
        docs: std::mem::take(&mut buffers.docs),
        docs_source_map: std::mem::take(&mut buffers.source_map),
        outline_mod: None,
        inline_file: source.file_id,
        prefix_len: TextSize::new(0),
        inline_inner_docs_start: None,
        outline_inner_docs_start: None,
        macro_calls: ThinVec::new(),
    };

    let mut cfg_options = None;

    let mut indent = Indent { lines: std::mem::take(&mut buffers.indent), seen_sugared: false };
    if let Some(outer_mod_decl) = outer_mod_decl {
        // For outer docs (the `mod foo;` declaration), use the module's own resolver.
        extend_with_attrs(
            &mut result,
            db,
            krate,
            outer_mod_decl.value.syntax(),
            outer_mod_decl.file_id,
            false,
            &mut indent,
            get_cfg_options,
            &mut cfg_options,
            resolver,
        );
        result.outline_mod = Some((outer_mod_decl.file_id, result.docs_source_map.len()));
    }

    // For inline docs, use the item's own resolver.
    extend_with_attrs(
        &mut result,
        db,
        krate,
        source.value.syntax(),
        source.file_id,
        false,
        &mut indent,
        get_cfg_options,
        &mut cfg_options,
        resolver,
    );
    if let Some(inner_attrs_node) = inner_attrs_node {
        result.inline_inner_docs_start = Some(TextSize::of(&result.docs));
        extend_with_attrs(
            &mut result,
            db,
            krate,
            inner_attrs_node,
            source.file_id,
            true,
            &mut indent,
            get_cfg_options,
            &mut cfg_options,
            resolver,
        );
    }
    result.remove_indent(&indent);

    result.remove_last_newline();

    // alloc: the memo's text and source map, exact; nothing for empty docs
    let docs = String::from(result.docs.as_str());
    // alloc: (the same)
    let source_map = Vec::from(result.docs_source_map.as_slice());
    buffers.docs = std::mem::replace(&mut result.docs, docs);
    buffers.source_map = std::mem::replace(&mut result.docs_source_map, source_map);
    buffers.indent = indent.lines;
    // A `#[doc = macro!()]` is rare: its calls keep the `ThinVec` they were pushed to.
    result.macro_calls.shrink_to_fit();

    // alloc: the memo's box, only for an item with docs
    if result.is_empty() { None } else { Some(Box::new(result)) }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use expect_test::expect;
    use hir_expand::InFile;
    use syntax::view::{self as ast, AstNode, AstToken, block_may_start_with_inner_attr};
    use hir_expand::fixture;
    use thin_vec::ThinVec;
    use tt::{TextRange, TextSize};


    use super::{DocCommentKind, Docs, Indent, IsInnerDoc};

    #[test]
    fn a_block_may_start_with_inner_docs() {
        for maybe in [
            "{ //! inner docs\n x }",
            "{ /*! inner docs */ x }",
            "{//!}",
            "{ // a comment\n //! inner }",
            "{ /* a /* nested */ comment */ /*! inner */ }",
            "{ #![allow(unused)] }",
            "{ \u{2028} x }",
            "{ /* unterminated ",
            "{ // no newline",
            "not a block",
            "{",
        ] {
            assert!(block_may_start_with_inner_attr(maybe, true), "{maybe:?}");
        }
        for not in [
            "{}",
            "{ let x = 1; //! late }",
            "{ // not //! docs\n x }",
            "{ /// outer docs\n x }",
            "{ /** outer docs */ x }",
            "{ /* //! */ x }",
        ] {
            assert!(!block_may_start_with_inner_attr(not, true), "{not:?}");
        }
    }

    #[test]
    fn docs() {
        let fx = fixture::db("");
        let file_id = fx.only_file();
        let _db = fx.into_db();
        let mut docs = Docs {
            docs: String::new(),
            docs_source_map: Vec::new(),
            outline_mod: None,
            inline_file: file_id.into(),
            prefix_len: TextSize::new(0),
            inline_inner_docs_start: None,
            outline_inner_docs_start: None,
            macro_calls: ThinVec::new(),
        };
        let mut indent = Indent::default();

        let outer = " foo\n\tbar  baz";
        let mut ast_offset = TextSize::new(123);
        for line in outer.split('\n') {
            docs.extend_with_doc_str(
                line,
                ast_offset,
                DocCommentKind::Sugared(ast::CommentShape::Line),
                &mut indent,
            );
            ast_offset += TextSize::of(line) + TextSize::of("\n");
        }

        docs.inline_inner_docs_start = Some(TextSize::of(&docs.docs));
        ast_offset += TextSize::new(123);
        let inner = " bar \n baz";
        for line in inner.split('\n') {
            docs.extend_with_doc_str(
                line,
                ast_offset,
                DocCommentKind::Sugared(ast::CommentShape::Line),
                &mut indent,
            );
            ast_offset += TextSize::of(line) + TextSize::of("\n");
        }

        expect![[r#"
            [
                DocsSourceMapLine {
                    string_offset: 0,
                    ast_offset: Some(
                        123,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 5,
                    ast_offset: Some(
                        128,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 15,
                    ast_offset: Some(
                        261,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 20,
                    ast_offset: Some(
                        267,
                    ),
                },
            ]
        "#]]
        .assert_debug_eq(&docs.docs_source_map);

        docs.remove_indent(&indent);

        assert_eq!(docs.inline_inner_docs_start, Some(TextSize::new(13)));

        assert_eq!(docs.docs, "foo\nbar  baz\nbar\nbaz\n");
        expect![[r#"
            [
                DocsSourceMapLine {
                    string_offset: 0,
                    ast_offset: Some(
                        124,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 4,
                    ast_offset: Some(
                        129,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 13,
                    ast_offset: Some(
                        262,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 17,
                    ast_offset: Some(
                        268,
                    ),
                },
            ]
        "#]]
        .assert_debug_eq(&docs.docs_source_map);

        docs.append(&docs.clone());
        docs.prepend_str("prefix---");
        assert_eq!(docs.docs, "prefix---foo\nbar  baz\nbar\nbaz\nfoo\nbar  baz\nbar\nbaz\n");
        expect![[r#"
            [
                DocsSourceMapLine {
                    string_offset: 0,
                    ast_offset: Some(
                        124,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 4,
                    ast_offset: Some(
                        129,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 13,
                    ast_offset: Some(
                        262,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 17,
                    ast_offset: Some(
                        268,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 21,
                    ast_offset: Some(
                        124,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 25,
                    ast_offset: Some(
                        129,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 34,
                    ast_offset: Some(
                        262,
                    ),
                },
                DocsSourceMapLine {
                    string_offset: 38,
                    ast_offset: Some(
                        268,
                    ),
                },
            ]
        "#]]
        .assert_debug_eq(&docs.docs_source_map);

        let range = |start, end| TextRange::new(TextSize::new(start), TextSize::new(end));
        let in_file = |range| InFile::new(file_id.into(), range);
        assert_eq!(docs.find_ast_range(range(0, 2)), None);
        assert_eq!(docs.find_ast_range(range(8, 10)), None);
        assert_eq!(
            docs.find_ast_range(range(9, 10)),
            Some((in_file(range(124, 125)), IsInnerDoc::No))
        );
        assert_eq!(docs.find_ast_range(range(20, 23)), None);
        assert_eq!(
            docs.find_ast_range(range(23, 25)),
            Some((in_file(range(263, 265)), IsInnerDoc::Yes))
        );
    }

    #[test]
    fn sugared_desugared_mix() {
        let fx = fixture::db("");
        let file_id = fx.only_file();
        let _db = fx.into_db();
        let mut docs = Docs {
            docs: String::new(),
            docs_source_map: Vec::new(),
            outline_mod: None,
            inline_file: file_id.into(),
            prefix_len: TextSize::new(0),
            inline_inner_docs_start: None,
            outline_inner_docs_start: None,
            macro_calls: ThinVec::new(),
        };
        let mut indent = Indent::default();

        docs.push_doc_lines(
            " hello!",
            None,
            DocCommentKind::Sugared(ast::CommentShape::Line),
            &mut indent,
        );
        docs.push_doc_lines("another", None, DocCommentKind::Desugared, &mut indent);
        docs.remove_indent(&indent);
        docs.remove_last_newline();

        assert_eq!(docs.docs(), "hello!\nanother");
    }

    /// `remove_indent` as a model: each line cut at its indent with `str::char_indices` and
    /// pushed onto a new string.
    fn remove_indent_by_lines(docs: &Docs, indent: &Indent) -> String {
        let add_indent = if indent.seen_sugared { 1 } else { 0 };
        let Some(min_indent) = indent
            .lines
            .iter()
            .filter_map(|it| *it)
            .map(|(line_indent, line_kind)| match line_kind {
                DocCommentKind::Sugared(_) => line_indent,
                DocCommentKind::Desugared => line_indent + add_indent,
            })
            .min()
        else {
            return docs.docs.clone();
        };
        let map = &docs.docs_source_map;
        let mut out = String::new();
        for (idx, line) in map.iter().enumerate() {
            let end = map.get(idx + 1).map_or(TextSize::of(&docs.docs), |it| it.string_offset);
            let line = &docs.docs[TextRange::new(line.string_offset, end)];
            let skip = match indent.lines[idx] {
                Some((_, DocCommentKind::Desugared)) if min_indent > 0 => min_indent - add_indent,
                _ => min_indent,
            };
            let start = line.char_indices().nth(skip).map_or(line.len() - 1, |(at, _)| at);
            out.push_str(&line[start..]);
        }
        out
    }

    #[test]
    fn remove_indent_as_by_lines() {
        let fx = fixture::db("");
        let file_id = fx.only_file();
        let _db = fx.into_db();
        let lines = [
            "", " ", "  a", "\tb", " \t ü", "ü", "   日本", "x", "  ", " 🦀 c", "\t\td", "    e",
        ];
        let kinds = [
            DocCommentKind::Sugared(ast::CommentShape::Line),
            DocCommentKind::Sugared(ast::CommentShape::Block),
            DocCommentKind::Desugared,
        ];
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for case in 0..2000 {
            let mut docs = Docs {
                docs: String::new(),
                docs_source_map: Vec::new(),
                outline_mod: None,
                inline_file: file_id.into(),
                prefix_len: TextSize::new(0),
                inline_inner_docs_start: None,
                outline_inner_docs_start: None,
                macro_calls: ThinVec::new(),
            };
            let mut indent = Indent::default();
            for _ in 0..1 + case % 6 {
                let line = lines[rng.index(lines.len())];
                let kind = kinds[rng.index(kinds.len())];
                docs.push_doc_lines(line, None, kind, &mut indent);
            }
            let want = remove_indent_by_lines(&docs, &indent);
            docs.remove_indent(&indent);
            assert_eq!(docs.docs, want, "case {case}");
        }
    }

    /// Extracts the docs of the first comment in `source`, running the same normalization as
    /// [`super::extract_docs`] does for inline docs.
    fn comment_docs(source: &str) -> Docs {
        let fx = fixture::db("");
        let file_id = fx.only_file();
        let _db = fx.into_db();
        let parse = syntax::SourceFile::parse(source, span::Edition::CURRENT);
        let comment = parse
            .root()
            .descendants_with_tokens()
            .filter_map(|it| it.into_node())
            .find_map(ast::DocComment::cast)
            .expect("no comment in the fixture");
        let mut docs = Docs {
            docs: String::new(),
            docs_source_map: Vec::new(),
            outline_mod: None,
            inline_file: file_id.into(),
            prefix_len: TextSize::new(0),
            inline_inner_docs_start: None,
            outline_inner_docs_start: None,
            macro_calls: ThinVec::new(),
        };
        let mut indent = Indent::default();
        docs.extend_with_doc_comment(comment, &mut indent);
        docs.remove_indent(&indent);
        docs.remove_last_newline();
        docs
    }

    #[test]
    fn block_doc_comment_stars() {
        #[track_caller]
        fn check(source: &str, expect: expect_test::Expect) {
            expect.assert_eq(&comment_docs(source).docs);
        }

        // The decoration is stripped, but markdown bullets and `*foo` are content.
        // `*bar` doesn't start with `* ` / `**`, so rustc's beautifier only strips the
        // horizontal `[ \t]*` prefix (here a single space) and leaves the leading `*` in
        // place. That in turn pins the block's minimum indent at 0, so surrounding lines
        // aren't re-indented.
        check(
            "/**\n * foo\n *\n *   * bullet\n *bar\n */",
            expect![[r#"
                 foo

                   * bullet
                *bar
            "#]],
        );
        // Single-line block doc comments are left alone, like rustdoc does.
        check("/** * item */", expect!["* item"]);
        // So are blocks without a consistent star column.
        check(
            "/**\n * foo\n   * bar\n */",
            expect![[r#"
                * foo
                  * bar
            "#]],
        );
    }

    #[test]
    fn block_doc_comment_source_map() {
        let docs = comment_docs("/**\n * foo\n * bar\n */");
        // `.lines()` (matching rustc) doesn't emit a leading empty entry for the newline
        // right after `/**`, so the docs body starts at `foo`, not with a blank line.
        assert_eq!(docs.docs, "foo\nbar\n");

        let range = |start, end| TextRange::new(TextSize::new(start), TextSize::new(end));
        let in_file = |range| InFile::new(docs.inline_file, range);
        let mapped = |start, end| docs.find_ast_range(range(start, end));
        // Both `foo` and `bar` map back past the stripped ` * ` decoration.
        assert_eq!(mapped(0, 3), Some((in_file(range(7, 10)), IsInnerDoc::No)));
        assert_eq!(mapped(4, 7), Some((in_file(range(14, 17)), IsInnerDoc::No)));
    }

    #[test]
    fn empty_outline_source_map() {
        // A module in its own file with no docs at its `mod`: the outline has no lines.
        let mut docs = comment_docs("/**\n * foo\n * bar\n */");
        docs.outline_mod = Some((docs.inline_file, 0));
        let at = |offset| TextRange::empty(TextSize::new(offset));
        let mapped = docs.find_ast_range(at(0));
        assert_eq!(mapped, Some((InFile::new(docs.inline_file, at(7)), IsInnerDoc::No)));
    }

    /// Extracts the docs of the first string in `source`, as if it were a `#[doc = "..."]` value.
    fn doc_attr_docs(source: &str) -> Docs {
        let fx = fixture::db("");
        let file_id = fx.only_file();
        let _db = fx.into_db();
        let parse = syntax::SourceFile::parse(source, span::Edition::CURRENT);
        let value = parse
            .root()
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find_map(ast::String::cast)
            .expect("no string in the fixture");
        let mut docs = Docs {
            docs: String::new(),
            docs_source_map: Vec::new(),
            outline_mod: None,
            inline_file: file_id.into(),
            prefix_len: TextSize::new(0),
            inline_inner_docs_start: None,
            outline_inner_docs_start: None,
            macro_calls: ThinVec::new(),
        };
        let mut indent = Indent::default();
        docs.extend_with_doc_attr(value, &mut indent);
        docs.remove_indent(&indent);
        docs.remove_last_newline();
        docs
    }

    #[test]
    fn doc_attr_source_map() {
        let range = |start, end| TextRange::new(TextSize::new(start), TextSize::new(end));
        let mapped = |docs: &Docs, start, end| {
            docs.find_ast_range(range(start, end)).map(|(range, _)| range.value)
        };

        // No escapes: `bar` maps back to the source.
        let docs = doc_attr_docs(r#"#[doc = "foo bar"] fn f() {}"#);
        assert_eq!(docs.docs, "foo bar");
        assert_eq!(mapped(&docs, 4, 7), Some(range(13, 16)));

        // Raw strings have no escapes either.
        let docs = doc_attr_docs(r##"#[doc = r"a\b"] fn f() {}"##);
        assert_eq!(docs.docs, r"a\b");
        assert_eq!(mapped(&docs, 2, 3), Some(range(12, 13)));

        // With an escape, `b` is at offset 2 in the docs but at offset 3 in the source,
        // so the docs can't be mapped back.
        let docs = doc_attr_docs(r#"#[doc = "a\"b"] fn f() {}"#);
        assert_eq!(docs.docs, "a\"b");
        assert_eq!(mapped(&docs, 2, 3), None);
    }

    /// rustc's vertical trim over collected lines, as `get_vertical_trim` was before it read
    /// `doc.lines()`.
    fn reference_vertical_trim(lines: &[&str]) -> Option<(usize, usize)> {
        let mut i = 0;
        let mut j = lines.len();
        if lines.first().is_some_and(|line| line.chars().all(|c| c == '*')) {
            i += 1;
        }
        if j > i && !lines[j - 1].is_empty() && lines[j - 1].chars().all(|c| c == '*') {
            j -= 1;
        }
        if i != 0 || j != lines.len() { Some((i, j)) } else { None }
    }

    /// rustc's horizontal trim over collected lines, as `get_horizontal_trim` was before it
    /// read an iterator.
    fn reference_horizontal_trim(lines: &[&str], kind: ast::CommentShape) -> Option<String> {
        let mut i = usize::MAX;
        let mut first = true;
        let lines = match kind {
            ast::CommentShape::Block => {
                let mut i = lines
                    .first()
                    .map(|l| if l.trim_start().starts_with('*') { 0 } else { 1 })
                    .unwrap_or(0);
                let mut j = lines.len();
                while i < j && lines[i].trim().is_empty() {
                    i += 1;
                }
                while j > i && lines[j - 1].trim().is_empty() {
                    j -= 1;
                }
                &lines[i..j]
            }
            ast::CommentShape::Line => lines,
        };
        for line in lines {
            for (j, c) in line.chars().enumerate() {
                if j > i || !"* \t".contains(c) {
                    return None;
                }
                if c == '*' {
                    if first {
                        i = j;
                        first = false;
                    } else if i != j {
                        return None;
                    }
                    break;
                }
            }
            if i >= line.len() {
                return None;
            }
        }
        Some(lines.first()?[..i].to_string())
    }

    /// The lines of a multi-line `doc` and their offsets in it, as `push_doc_lines` pushed them
    /// when it collected them.
    fn reference_push_lines(doc: &str, shape: ast::CommentShape) -> Vec<(&str, TextSize)> {
        let mut lines: Vec<(&str, TextSize)> = doc
            .lines()
            .map(|line| (line, TextSize::new(doc.substr_range(line).unwrap().start as u32)))
            .collect();
        let raw_lines: Vec<&str> = lines.iter().map(|(l, _)| *l).collect();
        let (i, j) = reference_vertical_trim(&raw_lines).unwrap_or((0, lines.len()));
        let lines = &mut lines[i..j];
        let raw_lines: Vec<&str> = lines.iter().map(|(l, _)| *l).collect();
        if let Some(horizontal) = reference_horizontal_trim(&raw_lines, shape) {
            for (line, line_offset) in lines.iter_mut() {
                if let Some(rest) = line.strip_prefix(horizontal.as_str()) {
                    *line = rest;
                    *line_offset += TextSize::of(horizontal.as_str());
                    if shape == ast::CommentShape::Block
                        && (*line == "*" || line.starts_with("* ") || line.starts_with("**"))
                    {
                        *line = &line[1..];
                        *line_offset += TextSize::of("*");
                    }
                }
            }
        }
        lines.to_vec()
    }

    /// The trims over `doc.lines()` give the lines and offsets the collected ones gave, over
    /// seeded doc strings of stars, blanks, tabs and text, in both shapes.
    #[test]
    fn trims_match_reference() {
        use super::{get_horizontal_trim, get_vertical_trim};

        let fx = fixture::db("");
        let file_id = fx.only_file();
        let _db = fx.into_db();
        let pieces = ["", "*", "**", " * a", "\t* b", "  c", "* ", "   ", "*/", "d *", " ** e"];
        let mut rng = SplitMix64::new(0x646f_6373);
        for case in 0..20_000 {
            let lines = rng.index(7);
            let doc: String = (0..lines)
                .map(|_| pieces[rng.index(pieces.len())])
                .collect::<Vec<_>>()
                .join(if case % 2 == 0 { "\n" } else { "\r\n" });
            if !doc.contains('\n') {
                continue;
            }
            let all: Vec<&str> = doc.lines().collect();
            assert_eq!(get_vertical_trim(&doc), reference_vertical_trim(&all), "{doc:?}");
            let (i, j) = get_vertical_trim(&doc).unwrap_or((0, all.len()));
            let kept = &all[i..j];
            for shape in [ast::CommentShape::Line, ast::CommentShape::Block] {
                assert_eq!(
                    get_horizontal_trim(kept.iter().copied(), shape).map(str::to_owned),
                    reference_horizontal_trim(kept, shape),
                    "{doc:?} {}",
                    matches!(shape, ast::CommentShape::Block),
                );

                let mut docs = Docs {
                    docs: String::new(),
                    docs_source_map: Vec::new(),
                    outline_mod: None,
                    inline_file: file_id.into(),
                    prefix_len: TextSize::new(0),
                    inline_inner_docs_start: None,
                    outline_inner_docs_start: None,
                    macro_calls: ThinVec::new(),
                };
                let base = TextSize::new(3);
                let kind = DocCommentKind::Sugared(shape);
                docs.push_doc_lines(&doc, Some(base), kind, &mut Indent::default());
                let mut want = Docs { docs: String::new(), docs_source_map: Vec::new(), ..docs.clone() };
                let mut indent = Indent::default();
                for (line, offset) in reference_push_lines(&doc, shape) {
                    want.push_doc_line(line, Some(base + offset), kind, &mut indent);
                }
                assert_eq!(docs.docs, want.docs, "{doc:?}");
                assert_eq!(docs.docs_source_map, want.docs_source_map, "{doc:?}");
            }
        }
    }
}
