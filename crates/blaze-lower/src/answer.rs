//! What a proc macro asks of the database while it expands (a span's text, file, line, bytes,
//! source or parent), answered as rustc's proc-macro server answers it: the one place that
//! answers it, whichever server or runtime asked.
//!
//! The server is user code in a sandbox, and the ids in its questions are numbers it sends: each
//! is checked against the database before it is used, and one that names nothing is answered
//! with [`SubResponse::Cancel`] naming the field and its value, never a panic.

#![forbid(unsafe_code)]

use base_db::{Db, EditionedFileId, FileId};
use blaze_query::db::{Database, Id};
use hir_expand::{FileRange, HirFileId};
use proc_macro_api::bridge::questions::{ParentSpan, SpanJoin};
use span::{ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext, TextRange, TextSize};

pub use proc_macro_api::bridge::questions::{SubRequest, SubResponse};

/// `req` answered from `db`, the database of the expansion that asked it.
pub fn answer(db: &Db, req: SubRequest) -> SubResponse {
    answered(db, req).unwrap_or_else(|reason| SubResponse::Cancel { reason })
}

fn answered(db: &Db, req: SubRequest) -> Result<SubResponse, String> {
    Ok(match req {
        SubRequest::LocalFilePath { file_id } => {
            SubResponse::LocalFilePathResult { name: path_of(db, file(db, file_id)?) }
        }
        SubRequest::FilePath { file_id } => {
            let file = file(db, file_id)?;
            let name = path_of(db, file)
                // alloc: the message of a question refused.
                .ok_or_else(|| format!("file_id {file_id} names a file with no path"))?;
            SubResponse::FilePathResult { name }
        }
        // Not incremental: requires full file text.
        SubRequest::SourceText { file_id, ast_id, start, end } => {
            let range = resolve(db, file_id, ast_id, range(start, end)?)?;
            let source = db.file_text(range.file_id.file_id(db)).text(db);
            let text = source.get(usize::from(range.range.start())..usize::from(range.range.end())).map(ToOwned::to_owned);
            SubResponse::SourceTextResult { text }
        }
        // Not incremental: requires building line index.
        SubRequest::LineColumn { file_id, ast_id, offset } => {
            let range = resolve(db, file_id, ast_id, TextRange::empty(TextSize::from(offset)))?;
            // proc_macro::Span line/column are 1-based
            let (line, column) = db
                .line_column(range.file_id.file_id(db), range.range.start())
                .map(|(line, col)| (line + 1, col + 1))
                .map_err(|()| {
                    // alloc: the message of a question refused.
                    format!(
                        "offset {offset} of ast_id {ast_id} of file_id {file_id} is not a place in the file's text"
                    )
                })?;
            SubResponse::LineColumnResult { line, column }
        }
        // Not incremental: requires global span resolution.
        SubRequest::ByteRange { file_id, ast_id, start, end } => {
            let range = resolve(db, file_id, ast_id, range(start, end)?)?;
            SubResponse::ByteRangeResult { range: range.range.into() }
        }
        SubRequest::SpanSource { file_id, ast_id, start, end, ctx } => {
            let mut current_span = span_of(db, file_id, ast_id, start, end, ctx)?;
            let mut current_ctx = current_span.ctx;
            while let Some(macro_call_id) = current_ctx.outer_expn(db) {
                let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);
                let call_site_file = macro_call_loc.kind.file_id();
                let resolved = hir_expand::resolve_span(db, current_span);
                current_ctx = macro_call_loc.ctxt;
                current_span = Span {
                    range: resolved.range,
                    anchor: SpanAnchor { file_id: resolved.file_id.span_file_id(db), ast_id: ROOT_ERASED_FILE_AST_ID },
                    ctx: current_ctx,
                };
                if call_site_file.file_id().is_some() {
                    break;
                }
            }
            let resolved = hir_expand::resolve_span(db, current_span);
            SubResponse::SpanSourceResult {
                file_id: resolved.file_id.span_file_id(db).as_u32(),
                ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
                start: u32::from(resolved.range.start()),
                end: u32::from(resolved.range.end()),
                ctx: current_span.ctx.into_u32(),
            }
        }
        SubRequest::SpanParent { file_id, ast_id, start, end, ctx } => {
            let span = span_of(db, file_id, ast_id, start, end, ctx)?;
            let parent_span = span.ctx.outer_expn(db).and_then(|macro_call_id| {
                let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);
                let editioned_file_id = macro_call_loc.kind.file_id().file_id()?;
                let range = HirFileId::from(editioned_file_id)
                    .ast_id_map(db)
                    .get_erased(macro_call_loc.kind.erased_ast_id())
                    .text_range();
                Some(ParentSpan {
                    file_id: editioned_file_id.span_file_id(db).as_u32(),
                    ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
                    start: u32::from(range.start()),
                    end: u32::from(range.end()),
                    ctx: macro_call_loc.ctxt.into_u32(),
                })
            });
            SubResponse::SpanParentResult { parent_span }
        }
        // Two spans of one file, of different items (the caller joins spans of one anchor
        // itself): the span from where the first starts to where the second ends, both
        // resolved to the file, as rustc's `Span::join` gives it. Spans of other contexts join
        // to the first, as they do under one anchor.
        SubRequest::SpanJoin {
            file_id,
            ast_id_first,
            start_first,
            end_first,
            ctx_first,
            ast_id_second,
            start_second,
            end_second,
            ctx_second,
        } => {
            context(db, ctx_first)?;
            context(db, ctx_second)?;
            let first = resolve(db, file_id, ast_id_first, range(start_first, end_first)?)?;
            let second = resolve(db, file_id, ast_id_second, range(start_second, end_second)?)?;
            let range = match ctx_first == ctx_second {
                true => first.range.cover(second.range),
                false => first.range,
            };
            SubResponse::SpanJoinResult {
                span: Some(SpanJoin {
                    ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
                    start: u32::from(range.start()),
                    end: u32::from(range.end()),
                    ctx: ctx_first,
                }),
            }
        }
    })
}

/// The absolute path of `file`, if it has one.
fn path_of(db: &Db, file: FileId) -> Option<String> {
    let source_root_id = db.file_source_root(file).source_root_id(db);
    let source_root = db.source_root(source_root_id).source_root(db);
    // alloc: the answer owns its text.
    source_root.path_for_file(&file).and_then(|path| path.as_path()).map(|path| path.to_string())
}

/// The file `raw` numbers, if the database has it.
fn file(db: &Db, raw: u32) -> Result<FileId, String> {
    if raw > span::EditionedFileId::MAX_FILE_ID {
        // alloc: the message of a question refused.
        return Err(format!("file_id {raw} is past the largest file id, {}", span::EditionedFileId::MAX_FILE_ID));
    }
    let file = FileId::from_raw(raw);
    match db.has_file_text(file) {
        true => Ok(file),
        // alloc: the message of a question refused.
        false => Err(format!("file_id {raw} names no file of the build")),
    }
}

/// `start..end`, if it is a range.
fn range(start: u32, end: u32) -> Result<TextRange, String> {
    match start <= end {
        true => Ok(TextRange::new(TextSize::from(start), TextSize::from(end))),
        // alloc: the message of a question refused.
        false => Err(format!("the range {start}..{end} ends before it starts")),
    }
}

/// The file and item of a span's anchor, each checked against the database, and where the item
/// starts in the file.
fn anchor(db: &Db, raw_file: u32, raw_ast: u32) -> Result<(SpanAnchor, EditionedFileId, TextSize), String> {
    use span::EditionedFileId as Raw;
    if raw_file & Raw::RESERVED_MASK != 0 {
        // alloc: the message of a question refused.
        return Err(format!("file_id {raw_file:#x} has its reserved bit set"));
    }
    if (raw_file & Raw::EDITION_MASK) >> Raw::FILE_ID_BITS > span::Edition::LATEST as u32 {
        // alloc: the message of a question refused.
        return Err(format!("file_id {raw_file:#x} names an edition past {}", span::Edition::LATEST));
    }
    file(db, raw_file & Raw::FILE_ID_MASK)?;
    let anchor = SpanAnchor { file_id: Raw::from_raw(raw_file), ast_id: ErasedFileAstId::from_raw(raw_ast) };
    let editioned = EditionedFileId::from_span_file_id(db, anchor.file_id);
    let item = HirFileId::from(editioned).ast_id_map(db).try_get_erased(anchor.ast_id).ok_or_else(|| {
        // alloc: the message of a question refused.
        format!("ast_id {raw_ast} names no item of file_id {raw_file:#x}")
    })?;
    Ok((anchor, editioned, item.text_range().start()))
}

/// The syntax context `raw` numbers, if it is a root or one the database interned. A context
/// is an interned value: its id must be on a page the contexts' ingredient owns.
fn context(db: &Db, raw: u32) -> Result<SyntaxContext, String> {
    if raw == u32::MAX {
        // alloc: the message of a question refused.
        return Err(format!("ctx {raw} names no syntax context"));
    }
    let ctx = SyntaxContext::from_u32(raw);
    if ctx.is_root() {
        return Ok(ctx);
    }
    let storage = db.storage();
    let owned = raw < Id::MAX_U32
        && storage
            .owner(Id::from_u32(raw + 1))
            .is_some_and(|owner| storage.ingredient_name(owner) == "SyntaxContextImpl");
    match owned {
        true => Ok(ctx),
        // alloc: the message of a question refused.
        false => Err(format!("ctx {raw} names no syntax context of the build")),
    }
}

fn span_of(db: &Db, file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32) -> Result<Span, String> {
    let range = range(start, end)?;
    let (anchor, _, at) = anchor(db, file_id, ast_id)?;
    within(range, at, file_id)?;
    Ok(Span { range, anchor, ctx: context(db, ctx)? })
}

/// `range` past `at`, if it fits a file's offsets.
fn within(range: TextRange, at: TextSize, file_id: u32) -> Result<TextRange, String> {
    range.checked_add(at).ok_or_else(|| {
        // alloc: the message of a question refused.
        format!("the range {range:?} of file_id {file_id:#x} is past the largest offset of a file")
    })
}

/// `range` relative to item `ast_id` of `file_id`, resolved to the file.
fn resolve(db: &Db, file_id: u32, ast_id: u32, range: TextRange) -> Result<FileRange, String> {
    let (_, editioned, at) = anchor(db, file_id, ast_id)?;
    Ok(FileRange { file_id: editioned, range: within(range, at, file_id)? })
}
