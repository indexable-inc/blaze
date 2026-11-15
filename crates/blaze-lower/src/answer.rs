//! What a proc macro asks of the database while it expands (a span's text, file, line, bytes,
//! source or parent), answered as rustc's proc-macro server answers it: the one place that
//! answers it, whichever server or runtime asked.
//!
//! The server is user code in a sandbox, and the ids in its questions are numbers it sends: each
//! is checked against the database before it is used, and one that names nothing is answered
//! with [`SubResponse::Cancel`] naming the field and its value, never a panic.

#![forbid(unsafe_code)]

use core::fmt;

use base_db::{Db, EditionedFileId, FileId};
use blaze_memo::ArenaId;
use hir_expand::{FileRange, HirFileId};
use proc_macro_api::bridge::questions::{ParentSpan, SpanJoin};
use span::{
    ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext, TextRange, TextSize,
};

pub use proc_macro_api::bridge::questions::{SubRequest, SubResponse};

/// `req` answered from `db`, the database of the expansion that asked it. `contexts` is the
/// table the question's syntax contexts are words of, and the contexts of the answer are
/// given as words of: the calling crate's arena, where the expansion's call is homed
/// (`span::MacroCallId::arena`).
pub fn answer(db: &Db, contexts: ArenaId, req: SubRequest) -> SubResponse {
    // The protocol's reason is text: the refusal is written once, here.
    answered(db, contexts, req).unwrap_or_else(|refused| SubResponse::Cancel { reason: refused.to_string() })
}

/// Why a question is refused: the field of the question and the value that names nothing.
#[derive(Clone, Copy, Debug)]
enum Refused {
    /// The file `file_id` has no path.
    NoPath { file_id: u32 },
    /// `offset` past item `ast_id` of `file_id` is not a place in the file's text.
    NotInText { offset: u32, ast_id: u32, file_id: u32 },
    /// `file_id` is past the largest file id.
    PastLargestFile { file_id: u32 },
    /// `file_id` names no file of the build.
    NoFile { file_id: u32 },
    /// `start..end` ends before it starts.
    Backwards { start: u32, end: u32 },
    /// `file_id` has its reserved bit set.
    Reserved { file_id: u32 },
    /// `file_id` names an edition or syntax extensions there are not.
    NoEdition { file_id: u32 },
    /// `ast_id` names no item of `file_id`.
    NoItem { ast_id: u32, file_id: u32 },
    /// `ctx` names no syntax context of the table `contexts`.
    NoContext { ctx: u32, contexts: ArenaId },
    /// `range` of `file_id` is past the largest offset of a file.
    PastLargestOffset { range: TextRange, file_id: u32 },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refused::NoPath { file_id } => write!(f, "file_id {file_id} names a file with no path"),
            Refused::NotInText { offset, ast_id, file_id } => {
                write!(f, "offset {offset} of ast_id {ast_id} of file_id {file_id} is not a place in the file's text")
            }
            Refused::PastLargestFile { file_id } => {
                write!(f, "file_id {file_id} is past the largest file id, {}", span::EditionedFileId::MAX_FILE_ID)
            }
            Refused::NoFile { file_id } => write!(f, "file_id {file_id} names no file of the build"),
            Refused::Backwards { start, end } => write!(f, "the range {start}..{end} ends before it starts"),
            Refused::Reserved { file_id } => write!(f, "file_id {file_id:#x} has its reserved bit set"),
            Refused::NoEdition { file_id } => write!(
                f,
                "file_id {file_id:#x} names an edition past {} or syntax extensions there are not",
                span::Edition::LATEST
            ),
            Refused::NoItem { ast_id, file_id } => write!(f, "ast_id {ast_id} names no item of file_id {file_id:#x}"),
            Refused::NoContext { ctx, contexts } => {
                write!(f, "ctx {ctx} names no syntax context of the calling crate's table ({contexts:?})")
            }
            Refused::PastLargestOffset { range, file_id } => {
                write!(f, "the range {range:?} of file_id {file_id:#x} is past the largest offset of a file")
            }
        }
    }
}

fn answered(db: &Db, contexts: ArenaId, req: SubRequest) -> Result<SubResponse, Refused> {
    Ok(match req {
        SubRequest::LocalFilePath { file_id } => {
            SubResponse::LocalFilePathResult { name: path_of(db, file(db, file_id)?) }
        }
        SubRequest::FilePath { file_id } => {
            let file = file(db, file_id)?;
            let name = path_of(db, file).ok_or(Refused::NoPath { file_id })?;
            SubResponse::FilePathResult { name }
        }
        // Not incremental: requires full file text.
        SubRequest::SourceText { file_id, ast_id, start, end } => {
            let range = resolve(db, file_id, ast_id, range(start, end)?)?;
            let source = db.file_text(range.file_id.file_id(db)).text(db);
            let text = source
                .get(usize::from(range.range.start())..usize::from(range.range.end()))
                .map(ToOwned::to_owned);
            SubResponse::SourceTextResult { text }
        }
        // Not incremental: requires building line index.
        SubRequest::LineColumn { file_id, ast_id, offset } => {
            let range = resolve(db, file_id, ast_id, TextRange::empty(TextSize::from(offset)))?;
            // proc_macro::Span line/column are 1-based
            let (line, column) = db
                .line_column(range.file_id.file_id(db), range.range.start())
                .map(|(line, col)| (line + 1, col + 1))
                .map_err(|()| Refused::NotInText { offset, ast_id, file_id })?;
            SubResponse::LineColumnResult { line, column }
        }
        // Not incremental: requires global span resolution.
        SubRequest::ByteRange { file_id, ast_id, start, end } => {
            let range = resolve(db, file_id, ast_id, range(start, end)?)?;
            SubResponse::ByteRangeResult { range: range.range.into() }
        }
        SubRequest::SpanSource { file_id, ast_id, start, end, ctx } => {
            let mut current_span = span_of(db, contexts, file_id, ast_id, start, end, ctx)?;
            let mut current_ctx = current_span.ctx;
            // The table `current_ctx` is a word of: a call's site is of its calling crate's
            // table, the call's own arena.
            let mut table = contexts;
            while let Some(macro_call_id) = current_ctx.outer_expn(db, table) {
                let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);
                let call_site_file = macro_call_loc.kind.file_id();
                let resolved = hir_expand::resolve_span(db, current_span);
                current_ctx = macro_call_loc.ctxt;
                table = macro_call_id.arena();
                current_span = Span {
                    range: resolved.range,
                    anchor: SpanAnchor {
                        file_id: resolved.file_id.span_file_id(db),
                        ast_id: ROOT_ERASED_FILE_AST_ID,
                    },
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
                // A word of the asker's table, where the context is carried from the call's.
                ctx: current_span.ctx.translate(db, table, contexts).into_u32(),
            }
        }
        SubRequest::SpanParent { file_id, ast_id, start, end, ctx } => {
            let span = span_of(db, contexts, file_id, ast_id, start, end, ctx)?;
            let parent_span = span.ctx.outer_expn(db, contexts).and_then(|macro_call_id| {
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
                    // The call's site is of the call's table: carried into the asker's.
                    ctx: macro_call_loc.ctxt.translate(db, macro_call_id.arena(), contexts).into_u32(),
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
            context(db, contexts, ctx_first)?;
            context(db, contexts, ctx_second)?;
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
fn file(db: &Db, raw: u32) -> Result<FileId, Refused> {
    if raw > span::EditionedFileId::MAX_FILE_ID {
        return Err(Refused::PastLargestFile { file_id: raw });
    }
    let file = FileId::from_raw(raw);
    match db.has_file_text(file) {
        true => Ok(file),
        false => Err(Refused::NoFile { file_id: raw }),
    }
}

/// `start..end`, if it is a range.
fn range(start: u32, end: u32) -> Result<TextRange, Refused> {
    match start <= end {
        true => Ok(TextRange::new(TextSize::from(start), TextSize::from(end))),
        false => Err(Refused::Backwards { start, end }),
    }
}

/// The file and item of a span's anchor, each checked against the database, and where the item
/// starts in the file.
fn anchor(
    db: &Db,
    raw_file: u32,
    raw_ast: u32,
) -> Result<(SpanAnchor, EditionedFileId, TextSize), Refused> {
    use span::EditionedFileId as Raw;
    if raw_file & Raw::RESERVED_MASK != 0 {
        return Err(Refused::Reserved { file_id: raw_file });
    }
    if !Raw::is_valid_raw(raw_file) {
        return Err(Refused::NoEdition { file_id: raw_file });
    }
    file(db, raw_file & Raw::FILE_ID_MASK)?;
    let anchor =
        SpanAnchor { file_id: Raw::from_raw(raw_file), ast_id: ErasedFileAstId::from_raw(raw_ast) };
    let editioned = EditionedFileId::from_span_file_id(db, anchor.file_id);
    let item = HirFileId::from(editioned)
        .ast_id_map(db)
        .try_get_erased(anchor.ast_id)
        .ok_or(Refused::NoItem { ast_id: raw_ast, file_id: raw_file })?;
    Ok((anchor, editioned, item.text_range().start()))
}

/// The syntax context `raw` numbers in `contexts`' table, checked (`SyntaxContext::checked`):
/// a root, or a context of that table. A number that names none (past the table, of another
/// edition than the context there, `u32::MAX`) is refused by name, never looked up: the
/// number is user code's. A number of another table's context is a context of this one or
/// none, so every lookup made with it reads this table.
fn context(db: &Db, contexts: ArenaId, raw: u32) -> Result<SyntaxContext, Refused> {
    SyntaxContext::checked(db, contexts, raw).ok_or(Refused::NoContext { ctx: raw, contexts })
}

fn span_of(
    db: &Db,
    contexts: ArenaId,
    file_id: u32,
    ast_id: u32,
    start: u32,
    end: u32,
    ctx: u32,
) -> Result<Span, Refused> {
    let range = range(start, end)?;
    let (anchor, _, at) = anchor(db, file_id, ast_id)?;
    within(range, at, file_id)?;
    Ok(Span { range, anchor, ctx: context(db, contexts, ctx)? })
}

/// `range` past `at`, if it fits a file's offsets.
fn within(range: TextRange, at: TextSize, file_id: u32) -> Result<TextRange, Refused> {
    range.checked_add(at).ok_or(Refused::PastLargestOffset { range, file_id })
}

/// `range` relative to item `ast_id` of `file_id`, resolved to the file.
fn resolve(db: &Db, file_id: u32, ast_id: u32, range: TextRange) -> Result<FileRange, Refused> {
    let (_, editioned, at) = anchor(db, file_id, ast_id)?;
    Ok(FileRange { file_id: editioned, range: within(range, at, file_id)? })
}
