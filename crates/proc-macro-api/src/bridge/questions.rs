//! What a macro may ask about a span that only the database knows (its file, text, line, parent
//! or join with another), and the answers: `blaze-lower` answers them for blaze-macro-eval's
//! server ([`super::spans`]).

use std::ops::Range;

#[derive(Debug, PartialEq, Eq)]
pub enum SubRequest {
    FilePath {
        file_id: u32,
    },
    SourceText {
        file_id: u32,
        ast_id: u32,
        start: u32,
        end: u32,
    },
    LocalFilePath {
        file_id: u32,
    },
    LineColumn {
        file_id: u32,
        ast_id: u32,
        offset: u32,
    },
    ByteRange {
        file_id: u32,
        ast_id: u32,
        start: u32,
        end: u32,
    },
    SpanSource {
        file_id: u32,
        ast_id: u32,
        start: u32,
        end: u32,
        ctx: u32,
    },
    SpanParent {
        file_id: u32,
        ast_id: u32,
        start: u32,
        end: u32,
        ctx: u32,
    },
    SpanJoin {
        file_id: u32,
        ast_id_first: u32,
        start_first: u32,
        end_first: u32,
        ctx_first: u32,
        ast_id_second: u32,
        start_second: u32,
        end_second: u32,
        ctx_second: u32,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubResponse {
    FilePathResult {
        name: String,
    },
    SourceTextResult {
        text: Option<String>,
    },
    LocalFilePathResult {
        name: Option<String>,
    },
    /// Line and column are 1-based.
    LineColumnResult {
        line: u32,
        column: u32,
    },
    ByteRangeResult {
        range: Range<usize>,
    },
    SpanSourceResult {
        file_id: u32,
        ast_id: u32,
        start: u32,
        end: u32,
        ctx: u32,
    },
    SpanParentResult {
        parent_span: Option<ParentSpan>,
    },
    SpanJoinResult {
        span: Option<SpanJoin>,
    },
    Cancel {
        reason: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParentSpan {
    pub file_id: u32,
    pub ast_id: u32,
    pub start: u32,
    pub end: u32,
    pub ctx: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SpanJoin {
    pub ast_id: u32,
    pub start: u32,
    pub end: u32,
    pub ctx: u32,
}
