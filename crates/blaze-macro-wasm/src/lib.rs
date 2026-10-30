//! Proc macros compiled to WebAssembly and run under Wasmtime in blaze's own process, as
//! dtolnay's watt runs them.
//!
//! blaze compiles a proc-macro crate for `wasm32-wasip1` with [`GUEST`] in place of the
//! standard library's `proc_macro`: the same API, whose token streams live in the module's
//! memory and whose spans are numbers the host gives out, so syn, quote and proc-macro2 build
//! against it unchanged. An expansion is given its input encoded ([`wire`]), asks the host
//! what only the host knows (what a span joins to, a string lexed), and hands back its output,
//! encoded the same way.
//!
//! Whether a module keeps state between expansions is seen, not guessed: the guest's
//! allocator counts the bytes live, and an expansion that leaves some behind tells the host
//! ([`Verdict`]). A module none of whose expansions did is stateless: each expansion is a
//! fresh instance ([`expand`]), kept under what it was given and what it read ([`keyed`]),
//! and crates that call it are frozen as any other. A module one of whose expansions did, or
//! one not seen yet, keeps an instance per calling crate for one database revision
//! ([`Session`]): its statics, stored tokens and their spans survive its calls, a source
//! change starts a fresh session and reruns demanded expansions, and nothing it makes is kept.
//!
//! What a module can learn of the machine is what it is given: its package's directory to
//! read, at a path that is the same everywhere ([`wasi::PACKAGE`]), and cargo's variables for
//! its package. Every read of the package is recorded, so that what one machine expanded
//! every other finds, whatever it runs on: neither the module, which is the same bytes for
//! every host, nor its expansions are kept under anything of the host's. What an expansion
//! asked the database of a span of its input (its text, file, line or bytes) is kept with it,
//! each question with its answer, and it is found while the database answers each the same.
//! One that was given a span it was not ([`Value`]) is not kept.

pub mod bench;
mod keyed;
mod lex;
mod wasi;
mod wire;

use std::{
    any::Any,
    fmt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base_db::{Crate, Env, Nonce, SourceDatabase};
use hir_expand::proc_macro::{
    ProcMacro, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind, ProcMacroLoadResult,
};
use intern::Symbol;
use load_cargo::{SubRequest, SubResponse};
use paths::AbsPath;
use rustc_hash::FxHashMap;
use span::{
    EditionedFileId, ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, Span, SpanAnchor,
    SyntaxContext, TextRange, TextSize,
};
use wasmtime::{
    Caller, Config, Engine, Extern, Instance, InstancePre, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder,
};

use crate::wire::{Reader, Spans, Tree, Writer};

/// The `proc_macro` crate a proc macro compiled to WebAssembly is built against.
pub const GUEST: &str = include_str!("../guest/proc_macro.rs");

/// The functions a module of proc macros exports, which blaze puts in the crate
/// (`crates/blaze-engine/src/macro_list.rs`).
pub mod exports {
    /// `(len) -> ptr`: room for an input of `len` bytes.
    pub const ALLOC: &str = "blaze_proc_macro_alloc";
    /// `()`: the list of the crate's macros, as output, a line each: kind (0 derive, 1
    /// attribute, 2 function-like), name and a derive's helper attributes, tab-separated.
    pub const LIST: &str = "blaze_proc_macro_list";
    /// `(index, attr_ptr, attr_len, ptr, len)`: expands the macro at `index` of the list.
    pub const EXPAND: &str = "blaze_proc_macro_expand";
    /// The custom section that holds what [`LIST`] gives, for the host to read without
    /// compiling the module.
    pub const LIST_SECTION: &str = "blaze_proc_macros";
}

/// The host's functions the module imports.
const HOST: &str = "blaze_proc_macro";

/// What the first key of an expansion starts with: another when the key or the value is made
/// otherwise.
const DOMAIN: &str = "blaze wasm proc-macro expansion 2";

/// The most memory one instance may have.
const MEMORY: usize = 4 << 30;

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.cranelift_nan_canonicalization(true);
        config.parallel_compilation(true);
        // A target given turns off Cranelift's reading of the host's CPU features, which
        // would put them in `code_key` and part hosts of one kind (CI's runners are AMD and
        // Intel) that should share machine code.
        config.target(&target_lexicon::HOST.to_string()).expect("the host is a Wasmtime target");
        for flag in CPU_FEATURES {
            // SAFETY: each is a feature Wasmtime checks the host has before it runs code
            // compiled with it, or (the return-address signing) what the host's ABI needs.
            unsafe { config.cranelift_flag_enable(flag) };
        }
        Engine::new(&config).expect("Wasmtime's configuration for proc macros")
    })
}

/// The CPU features machine code is compiled for, the same on every host of an arch: on
/// x86-64 its v2 level, which every x86-64 CPU since 2009 has and which keeps the floating
/// point Wasm needs off host calls; on arm64 none beyond the base but the return-address
/// signing macOS requires.
const CPU_FEATURES: &[&str] = if cfg!(target_arch = "x86_64") {
    &["has_cmpxchg16b", "has_sse3", "has_ssse3", "has_sse41", "has_sse42", "has_popcnt"]
} else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
    &["sign_return_address", "sign_return_address_with_bkey"]
} else {
    &[]
};

/// The database a running expansion asks, for as long as it runs.
#[derive(Clone, Copy)]
struct Db(*const (dyn SourceDatabase + 'static));

// SAFETY: a `Db` is only read on the thread that expands, while `Session::run` holds the
// database it points to, and clears the pointer even when the host unwinds.
unsafe impl Send for Db {}
unsafe impl Sync for Db {}

/// What an instance has while it runs.
pub(crate) struct State {
    spans: Spans,
    sites: [u32; 3],
    files: wasi::Files,
    env: Vec<(String, String)>,
    /// What a function that gives text or a stream back put there, for the module's `take`.
    buffer: Vec<u8>,
    output: Option<Vec<u8>>,
    panic: Option<String>,
    /// Whether the expansion left what it allocated live for a later one.
    retained: bool,
    /// Whether the expansion learned of a span what neither its key nor [`State::asks`] says:
    /// then it is not kept.
    asked: bool,
    /// The anchors and contexts of the input's spans, which a question is written against.
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
    /// Each question the expansion asked of a span of its input and what it was answered, as a
    /// read set has them ([`Value::read`]).
    asks: Vec<(String, String)>,
    db: Option<Db>,
    limits: StoreLimits,
}

impl State {
    pub(crate) fn empty() -> State {
        State {
            spans: Spans::default(),
            sites: [0; 3],
            files: wasi::Files::new(None),
            env: Vec::new(),
            buffer: Vec::new(),
            output: None,
            panic: None,
            retained: false,
            asked: false,
            anchors: Vec::new(),
            contexts: Vec::new(),
            asks: Vec::new(),
            db: None,
            limits: StoreLimitsBuilder::new().memory_size(MEMORY).build(),
        }
    }

    fn ask(&mut self, req: SubRequest) -> Option<SubResponse> {
        self.asked = true;
        let db = self.db?;
        // SAFETY: see `Db`.
        Some(load_cargo::answer(unsafe { &*db.0 }, req))
    }

    /// Asks the database `value` of `span`, and notes the question and its answer, or that
    /// the expansion is not to be kept when the span has an anchor or a context the input has
    /// not.
    fn ask_value(&mut self, value: Value, span: Span) -> Option<SubResponse> {
        // SAFETY: see `Db`.
        let answer = self.db.map(|db| load_cargo::answer(unsafe { &*db.0 }, value.request(span)));
        let anchor = self.anchors.iter().position(|it| *it == span.anchor);
        let context = self.contexts.iter().position(|it| *it == span.ctx);
        match anchor.zip(context) {
            Some((anchor, context)) => {
                let (start, end) = (span.range.start().into(), span.range.end().into());
                let read = value.read([start, end, anchor as u32, context as u32]);
                self.asks.push((read, value.said(answer.as_ref())));
            }
            None => self.asked = true,
        }
        answer
    }

    fn span(&self, number: u32) -> Option<Span> {
        self.spans.get(number)
    }

    fn give(&mut self, text: Option<String>) -> i32 {
        match text {
            Some(text) => {
                self.buffer = text.into_bytes();
                self.buffer.len() as i32
            }
            None => -1,
        }
    }
}

/// A span query answered with a value rather than another span.
#[derive(Clone, Copy, PartialEq)]
enum Value {
    LineColumn,
    SourceText,
    ByteRange,
    File,
    LocalFile,
}

impl Value {
    const ALL: [(Value, char); 5] = [
        (Value::LineColumn, 'l'),
        (Value::SourceText, 't'),
        (Value::ByteRange, 'b'),
        (Value::File, 'f'),
        (Value::LocalFile, 'p'),
    ];

    fn request(self, span: Span) -> SubRequest {
        let (file_id, ast_id) = (span.anchor.file_id.as_u32(), span.anchor.ast_id.into_raw());
        let (start, end) = (span.range.start().into(), span.range.end().into());
        match self {
            Value::LineColumn => SubRequest::LineColumn { file_id, ast_id, offset: start },
            Value::SourceText => SubRequest::SourceText { file_id, ast_id, start, end },
            Value::ByteRange => SubRequest::ByteRange { file_id, ast_id, start, end },
            Value::File => SubRequest::FilePath { file_id: span.anchor.file_id.file_id().index() },
            Value::LocalFile => {
                SubRequest::LocalFilePath { file_id: span.anchor.file_id.file_id().index() }
            }
        }
    }

    /// The question of the span of `shape` (as [`Shapes`] writes one) as a read set has it:
    /// a NUL first, which no path read has.
    fn read(self, [start, end, anchor, context]: [u32; 4]) -> String {
        let letter = Value::ALL.iter().find(|it| it.0 == self).map_or('?', |it| it.1);
        format!("\0{letter}{start},{end},{anchor},{context}")
    }

    /// The question `read` (as [`Value::read`] wrote it) of the input that has the anchors and
    /// contexts of `shapes`.
    fn parse(read: &str, shapes: &Shapes) -> Option<(Value, Span)> {
        let mut chars = read.strip_prefix('\0')?.chars();
        let letter = chars.next()?;
        let value = Value::ALL.iter().find(|it| it.1 == letter)?.0;
        let mut fields = chars.as_str().split(',').map(|it| it.parse::<u32>().ok());
        let mut next = || fields.next().flatten();
        let (start, end, anchor, context) = (next()?, next()?, next()?, next()?);
        let span = Span {
            range: range(start, end),
            anchor: *shapes.anchors.get(anchor as usize)?,
            ctx: *shapes.contexts.get(context as usize)?,
        };
        Some((value, span))
    }

    /// What a read set has of `answer`: the same for the same answer, short.
    fn said(self, answer: Option<&SubResponse>) -> String {
        match answer {
            Some(SubResponse::LineColumnResult { line, column }) => format!("{line}:{column}"),
            Some(SubResponse::SourceTextResult { text: Some(text) }) => {
                keyed::digest_bytes(text.as_bytes())
            }
            Some(SubResponse::ByteRangeResult { range }) => {
                format!("{}..{}", range.start, range.end)
            }
            Some(SubResponse::FilePathResult { name }) => keyed::digest_bytes(name.as_bytes()),
            Some(SubResponse::LocalFilePathResult { name: Some(name) }) => {
                keyed::digest_bytes(name.as_bytes())
            }
            _ => "-".to_owned(),
        }
    }
}

fn memory(caller: &mut Caller<'_, State>) -> wasmtime::Result<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(wasmtime::Error::msg("the module exports no memory")),
    }
}

fn read_bytes(caller: &mut Caller<'_, State>, ptr: u32, len: u32) -> wasmtime::Result<Vec<u8>> {
    let memory = memory(caller)?;
    let mut bytes = vec![0; len as usize];
    memory.read(&*caller, ptr as usize, &mut bytes)?;
    Ok(bytes)
}

fn read_text(caller: &mut Caller<'_, State>, ptr: u32, len: u32) -> wasmtime::Result<String> {
    Ok(String::from_utf8_lossy(&read_bytes(caller, ptr, len)?).into_owned())
}

fn subspan(span: Span, start: u64, end: u64) -> Option<Span> {
    if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
        return Some(span);
    }
    let length: u32 = span.range.len().into();
    let bound = |it: u64, unbounded: u32, included_end: bool| -> Option<u32> {
        let at = u32::try_from(it / 4).ok()?;
        match (it, it % 4) {
            (0, _) => Some(unbounded),
            (_, 1) if included_end => at.checked_add(1),
            (_, 1) => Some(at),
            (_, 3) if included_end => Some(at),
            (_, 3) => at.checked_add(1),
            _ => None,
        }
    };
    let start = bound(start, 0, false)?;
    let end = bound(end, length, true)?;
    let span_start: u32 = span.range.start().into();
    if (u32::MAX - start) < span_start
        || (u32::MAX - end) < span_start
        || start >= end
        || end > length
    {
        return None;
    }
    let range = TextRange::new(TextSize::from(start), TextSize::from(end)) + span.range.start();
    Some(Span { range, ..span })
}

fn join(state: &mut State, first: Span, second: Span) -> Option<Span> {
    if first.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
        return Some(second);
    }
    if second.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
        return Some(first);
    }
    if first.anchor != second.anchor {
        if first.anchor.file_id != second.anchor.file_id {
            return None;
        }
        let req = SubRequest::SpanJoin {
            file_id: first.anchor.file_id.as_u32(),
            ast_id_first: first.anchor.ast_id.into_raw(),
            start_first: first.range.start().into(),
            end_first: first.range.end().into(),
            ctx_first: first.ctx.into_u32(),
            ast_id_second: second.anchor.ast_id.into_raw(),
            start_second: second.range.start().into(),
            end_second: second.range.end().into(),
            ctx_second: second.ctx.into_u32(),
        };
        let Some(SubResponse::SpanJoinResult { span: Some(it) }) = state.ask(req) else {
            return None;
        };
        let anchor = SpanAnchor {
            file_id: first.anchor.file_id,
            ast_id: ErasedFileAstId::from_raw(it.ast_id),
        };
        // SAFETY: the context is one the database gave.
        let ctx = unsafe { SyntaxContext::from_u32(it.ctx) };
        return Some(Span { range: range(it.start, it.end), anchor, ctx });
    }
    if first.ctx != second.ctx {
        return Some(first);
    }
    Some(Span { range: first.range.cover(second.range), anchor: second.anchor, ctx: second.ctx })
}

fn range(start: u32, end: u32) -> TextRange {
    TextRange::new(TextSize::new(start), TextSize::new(end.max(start)))
}

fn spanned(file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32) -> Span {
    let anchor = SpanAnchor {
        file_id: EditionedFileId::from_raw(file_id),
        ast_id: ErasedFileAstId::from_raw(ast_id),
    };
    // SAFETY: the context is one the database gave.
    Span { range: range(start, end), anchor, ctx: unsafe { SyntaxContext::from_u32(ctx) } }
}

fn line_column(state: &mut State, span: Span) -> Option<(u32, u32)> {
    match state.ask_value(Value::LineColumn, span)? {
        SubResponse::LineColumnResult { line, column } => Some((line, column)),
        _ => None,
    }
}

/// Adds the host's functions to `linker`.
fn add_host(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    linker.func_wrap(HOST, "def_site", |caller: Caller<'_, State>| caller.data().sites[0])?;
    linker.func_wrap(HOST, "call_site", |caller: Caller<'_, State>| caller.data().sites[1])?;
    linker.func_wrap(HOST, "mixed_site", |caller: Caller<'_, State>| caller.data().sites[2])?;
    linker.func_wrap(HOST, "span_join", |mut caller: Caller<'_, State>, a: u32, b: u32| {
        let state = caller.data_mut();
        let (Some(first), Some(second)) = (state.span(a), state.span(b)) else { return 0 };
        join(state, first, second).map_or(0, |it| state.spans.number(it))
    })?;
    linker.func_wrap(
        HOST,
        "span_resolved_at",
        |mut caller: Caller<'_, State>, a: u32, at: u32| {
            let state = caller.data_mut();
            let (Some(span), Some(at)) = (state.span(a), state.span(at)) else { return a };
            state.spans.number(Span { ctx: at.ctx, ..span })
        },
    )?;
    linker.func_wrap(
        HOST,
        "span_located_at",
        |mut caller: Caller<'_, State>, a: u32, b: u32| {
            let state = caller.data_mut();
            let (Some(span), Some(other)) = (state.span(a), state.span(b)) else { return b };
            state.spans.number(Span { ctx: span.ctx, ..other })
        },
    )?;
    linker.func_wrap(HOST, "span_parent", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return 0 };
        let req = SubRequest::SpanParent {
            file_id: span.anchor.file_id.as_u32(),
            ast_id: span.anchor.ast_id.into_raw(),
            start: span.range.start().into(),
            end: span.range.end().into(),
            ctx: span.ctx.into_u32(),
        };
        match state.ask(req) {
            Some(SubResponse::SpanParentResult { parent_span: Some(it) }) => {
                state.spans.number(spanned(it.file_id, it.ast_id, it.start, it.end, it.ctx))
            }
            _ => 0,
        }
    })?;
    linker.func_wrap(HOST, "span_source", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return a };
        let req = SubRequest::SpanSource {
            file_id: span.anchor.file_id.as_u32(),
            ast_id: span.anchor.ast_id.into_raw(),
            start: span.range.start().into(),
            end: span.range.end().into(),
            ctx: span.ctx.into_u32(),
        };
        match state.ask(req) {
            Some(SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx }) => {
                state.spans.number(spanned(file_id, ast_id, start, end, ctx))
            }
            _ => a,
        }
    })?;
    linker.func_wrap(HOST, "span_start", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return a };
        if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return a;
        }
        state.spans.number(Span { range: TextRange::empty(span.range.start()), ..span })
    })?;
    linker.func_wrap(HOST, "span_end", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return a };
        if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return a;
        }
        state.spans.number(Span { range: TextRange::empty(span.range.end()), ..span })
    })?;
    linker.func_wrap(
        HOST,
        "span_subspan",
        |mut caller: Caller<'_, State>, a: u32, start: u64, end: u64| {
            let state = caller.data_mut();
            let Some(span) = state.span(a) else { return 0 };
            subspan(span, start, end).map_or(0, |it| state.spans.number(it))
        },
    )?;
    linker.func_wrap(
        HOST,
        "span_byte_range",
        |mut caller: Caller<'_, State>, a: u32, out: u32| -> wasmtime::Result<()> {
            let state = caller.data_mut();
            let Some(span) = state.span(a) else { return Ok(()) };
            let range = match state.ask_value(Value::ByteRange, span) {
                Some(SubResponse::ByteRangeResult { range }) => range,
                _ => usize::from(span.range.start())..usize::from(span.range.end()),
            };
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&(range.start as u32).to_le_bytes());
            bytes[4..].copy_from_slice(&(range.end as u32).to_le_bytes());
            let memory = memory(&mut caller)?;
            memory.write(&mut caller, out as usize, &bytes)?;
            Ok(())
        },
    )?;
    linker.func_wrap(HOST, "span_line", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let span = state.span(a);
        span.and_then(|span| line_column(state, span)).map_or(1, |(line, _)| line)
    })?;
    linker.func_wrap(HOST, "span_column", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let span = state.span(a);
        span.and_then(|span| line_column(state, span)).map_or(1, |(_, column)| column)
    })?;
    linker.func_wrap(HOST, "span_debug", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        // What it says is of the database's numbering, which the key does not have.
        state.asked = true;
        let text = state.span(a).map(|it| format!("{it:?}"));
        state.give(text)
    })?;
    linker.func_wrap(HOST, "span_file", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let name = match state.ask_value(Value::File, span) {
            Some(SubResponse::FilePathResult { name }) => name,
            _ => String::new(),
        };
        state.give(Some(name))
    })?;
    linker.func_wrap(HOST, "span_local_file", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let name = match state.ask_value(Value::LocalFile, span) {
            Some(SubResponse::LocalFilePathResult { name }) => name,
            _ => None,
        };
        state.give(name)
    })?;
    linker.func_wrap(HOST, "span_source_text", |mut caller: Caller<'_, State>, a: u32| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let text = match state.ask_value(Value::SourceText, span) {
            Some(SubResponse::SourceTextResult { text }) => text,
            _ => None,
        };
        state.give(text)
    })?;
    linker.func_wrap(
        HOST,
        "ts_from_str",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<i32> {
            let text = read_text(&mut caller, ptr, len)?;
            let state = caller.data_mut();
            let call_site = state.span(state.sites[1]).expect("the current call site is numbered");
            Ok(match lex::stream(&text, call_site) {
                Ok(trees) => {
                    let mut writer = Writer { out: Vec::new(), spans: &mut state.spans };
                    writer.trees(&trees);
                    state.buffer = writer.out;
                    state.buffer.len() as i32
                }
                Err(e) => {
                    let e = format!("failed to parse str to token stream: {e}");
                    -1 - state.give(Some(e))
                }
            })
        },
    )?;
    linker.func_wrap(
        HOST,
        "literal_from_str",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<i32> {
            let text = read_text(&mut caller, ptr, len)?;
            let state = caller.data_mut();
            let call_site = state.span(state.sites[1]).expect("the current call site is numbered");
            Ok(match lex::literal_from_str(&text, call_site) {
                Some(literal) => {
                    let mut writer = Writer { out: Vec::new(), spans: &mut state.spans };
                    writer.trees(&[Tree::Leaf(tt::Leaf::Literal(literal))]);
                    state.buffer = writer.out;
                    state.buffer.len() as i32
                }
                None => -1,
            })
        },
    )?;
    linker.func_wrap(
        HOST,
        "take",
        |mut caller: Caller<'_, State>, to: u32| -> wasmtime::Result<()> {
            let buffer = std::mem::take(&mut caller.data_mut().buffer);
            let memory = memory(&mut caller)?;
            memory.write(&mut caller, to as usize, &buffer)?;
            Ok(())
        },
    )?;
    // These declarations do not grant access beyond the WASI sandbox.
    linker.func_wrap(HOST, "track_path", |_: u32, _: u32| {})?;
    linker.func_wrap(HOST, "track_env_var", |_: u32, _: u32| {})?;
    // As rust-analyzer's proc-macro server, which shows no diagnostic of a macro's.
    linker.func_wrap(HOST, "diagnostic", |_: u32, _: u32, _: u32, _: u32| {})?;
    linker.func_wrap(
        HOST,
        "output",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<()> {
            let bytes = read_bytes(&mut caller, ptr, len)?;
            caller.data_mut().output = Some(bytes);
            Ok(())
        },
    )?;
    linker.func_wrap(
        HOST,
        "panicked",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<()> {
            let text = read_text(&mut caller, ptr, len)?;
            caller.data_mut().panic = Some(text);
            Ok(())
        },
    )?;
    linker.func_wrap(HOST, "retained", |mut caller: Caller<'_, State>, _bytes: u32| {
        caller.data_mut().retained = true;
    })?;
    Ok(())
}

/// A module of proc macros, compiled for this host.
pub struct Wasm {
    path: PathBuf,
    digest: [u8; 32],
    /// Source identity recorded with the module listing.
    identity: [u8; 32],
    pre: InstancePre<State>,
    /// How long Cranelift took to compile it.
    pub compiled_in: Duration,
    /// Whether the store has its machine code.
    kept: std::sync::atomic::AtomicBool,
}

impl fmt::Debug for Wasm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wasm").field("path", &self.path).finish_non_exhaustive()
    }
}

/// A macro of a module, as [`Wasm::macros`] lists them.
#[derive(Debug, Clone)]
pub struct Listed {
    pub kind: ProcMacroKind,
    pub name: String,
    pub helpers: Vec<String>,
}

/// What an expansion is given.
pub struct Call<'a> {
    pub subtree: &'a tt::TopSubtree,
    pub attrs: Option<&'a tt::TopSubtree>,
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
}

/// How an expansion went: what it made, whether it was found kept, whether it left what it
/// allocated live for a later one, and the time.
pub struct Ran {
    pub output: Result<tt::TopSubtree, ProcMacroExpansionError>,
    pub found: bool,
    pub retained: bool,
    pub took: Duration,
}

impl Wasm {
    /// The module at `path`, compiled once a process, and its machine code kept beside it and
    /// in `store` under [`code_key`], which says the host it is for: unlike the module, the
    /// code is for one kind of host.
    pub fn at(path: &Path, store: Option<&blaze_store::Store>) -> Result<Arc<Wasm>, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let known = Known { digest: keyed::sha256(&bytes), identity: None };
        Wasm::known(path, known, Some(&bytes), store)
    }

    /// As [`Wasm::at`], of a module whose digest is known: its bytes are read only if its
    /// machine code is not beside it.
    fn known(
        path: &Path,
        known: Known,
        bytes: Option<&[u8]>,
        store: Option<&blaze_store::Store>,
    ) -> Result<Arc<Wasm>, String> {
        // A module being compiled (`prepare_soon`) is waited for, not compiled again.
        type Once = Arc<OnceLock<Result<Arc<Wasm>, String>>>;
        static COMPILED: Mutex<Option<FxHashMap<[u8; 32], Once>>> = Mutex::new(None);
        let once = {
            let mut compiled = COMPILED.lock().unwrap();
            compiled.get_or_insert_default().entry(known.digest).or_default().clone()
        };
        let compiled = once.get_or_init(|| Wasm::compile(path, known, bytes, store).map(Arc::new));
        let wasm = compiled.clone()?;
        wasm.keep_code(store);
        Ok(wasm)
    }

    fn compile(
        path: &Path,
        Known { digest, identity }: Known,
        given: Option<&[u8]>,
        store: Option<&blaze_store::Store>,
    ) -> Result<Wasm, String> {
        let start = Instant::now();
        let failed = |e: wasmtime::Error| format!("{}: {e:#}", path.display());
        let read = std::cell::OnceCell::new();
        let bytes = || module_bytes(path, &digest, given, &read);
        let key = code_key(&digest);
        let file = code_file(path, &key);
        let stats = std::env::var_os("BLAZE_STATS").is_some();
        // SAFETY: the file beside the module and what the store has under the key are what
        // `serialize` wrote for this key, the store's checked against its checksum when read;
        // Wasmtime refuses code of another version, configuration or CPU.
        let beside = unsafe { Module::deserialize_file(engine(), &file) }.ok();
        let (module, was_kept) = match beside {
            // Written beside it by the process that compiled it, which kept it in the store.
            Some(module) => (module, true),
            None => match store
                .and_then(|it| it.get_lasting(&key))
                .and_then(|it| unsafe { Module::deserialize(engine(), &it) }.ok().map(|m| (m, it)))
            {
                Some((module, code)) => {
                    write_whole(&file, &code);
                    (module, true)
                }
                None => {
                    let module = Module::new(engine(), bytes()?).map_err(failed)?;
                    if stats {
                        eprintln!(
                            "blaze: Cranelift compiled the proc-macro module {} in {:.1?}, none kept as {}",
                            path.display(),
                            start.elapsed(),
                            file.display()
                        );
                    }
                    let code = module.serialize().map_err(failed)?;
                    write_whole(&file, &code);
                    if let Some(store) = store {
                        store.insert_lasting(&key, &code);
                        store.flush();
                    }
                    (module, store.is_some())
                }
            },
        };
        let mut linker = Linker::new(engine());
        wasi::add(&mut linker).map_err(failed)?;
        add_host(&mut linker).map_err(failed)?;
        wasi::rest(&mut linker, &module).map_err(failed)?;
        let pre = linker.instantiate_pre(&module).map_err(failed)?;
        let identity = match identity {
            Some(identity) => identity,
            None => made_of(path, bytes()?).unwrap_or(digest),
        };
        let compiled_in = start.elapsed();
        let kept = was_kept.into();
        let wasm = Wasm { path: path.to_owned(), digest, identity, pre, compiled_in, kept };
        wasm.keep_code(store);
        LOADED.add(start.elapsed());
        Ok(wasm)
    }

    /// Keeps its machine code in `store`, if it has not: a module compiled before the daemon
    /// told the store the toolchain is kept once it has.
    fn keep_code(&self, store: Option<&blaze_store::Store>) {
        let Some(store) = store else { return };
        if self.kept.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Ok(code) = self.pre.module().serialize() {
            store.insert_lasting(&code_key(&self.digest), &code);
            store.flush();
            if std::env::var_os("BLAZE_STATS").is_some() {
                eprintln!("blaze: the store keeps the machine code of {}", self.path.display());
            }
        }
    }

    /// An instance, its C library set up: a module linked as a library (a WASI reactor) is
    /// started by its `_initialize`.
    fn instance(&self, store: &mut Store<State>) -> wasmtime::Result<Instance> {
        let instance = self.pre.instantiate(&mut *store)?;
        if let Ok(initialize) = instance.get_typed_func::<(), ()>(&mut *store, "_initialize") {
            initialize.call(&mut *store, ())?;
        }
        Ok(instance)
    }

    /// The module's macros.
    pub fn macros(&self) -> Result<Vec<Listed>, String> {
        Ok(listed(&self.list()?))
    }

    /// What its list export says: a line per macro, kind, name and helpers by tabs.
    fn list(&self) -> Result<String, String> {
        let mut store = Store::new(engine(), State::empty());
        store.limiter(|state| &mut state.limits);
        let instance = self.instance(&mut store).map_err(|e| format!("{e:#}"))?;
        let list = instance
            .get_typed_func::<(), ()>(&mut store, exports::LIST)
            .map_err(|e| format!("{e:#}"))?;
        list.call(&mut store, ()).map_err(|e| format!("{e:#}"))?;
        let text = store.data_mut().output.take().unwrap_or_default();
        String::from_utf8(text).map_err(|e| e.to_string())
    }
}

/// The macros of a module's list ([`Wasm::list`]).
fn listed(text: &str) -> Vec<Listed> {
    let mut macros = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let kind = match fields.next() {
            Some("0") => ProcMacroKind::CustomDerive,
            Some("1") => ProcMacroKind::Attr,
            _ => ProcMacroKind::Bang,
        };
        let name = fields.next().unwrap_or_default().to_owned();
        let helpers = fields.next().unwrap_or_default();
        let helpers = helpers.split(',').filter(|it| !it.is_empty()).map(str::to_owned);
        macros.push(Listed { kind, name, helpers: helpers.collect() });
    }
    macros
}

/// A module instance for one crate's macro expansions in one database revision.
/// Guest tokens can outlive a call, so their spans and WASI descriptors live here too.
pub struct Session {
    store: Store<State>,
    instance: Instance,
    root: Option<PathBuf>,
    failed: bool,
}

impl Wasm {
    pub fn session(&self, env: &Env) -> Result<Session, ProcMacroExpansionError> {
        let root = package_dir(env);
        let mut state = State::empty();
        state.files = wasi::Files::new(root.as_deref());
        state.env = guest_env(env);
        self.instance_with(state, root)
    }

    /// A fresh instance whose host side starts as `state`.
    fn instance_with(
        &self,
        state: State,
        root: Option<PathBuf>,
    ) -> Result<Session, ProcMacroExpansionError> {
        let mut store = Store::new(engine(), state);
        store.limiter(|state| &mut state.limits);
        let instance = self
            .instance(&mut store)
            .map_err(|e| ProcMacroExpansionError::System(format!("{e:#}")))?;
        Ok(Session { store, instance, root, failed: false })
    }

    /// Expands the macro at `index` of [`Wasm::macros`] in a fresh instance: what is kept of
    /// it when the store has it, else by running it, and keeps what it made unless `store` is
    /// `None` or the expansion left state behind.
    pub fn expand(
        &self,
        index: u32,
        listed: &Listed,
        db: Option<&dyn SourceDatabase>,
        call: &Call<'_>,
        store: Option<&blaze_store::Store>,
    ) -> Ran {
        expand(&self.identity, &|| Ok(self), index, listed, db, call, store)
    }
}

impl Session {
    /// Executes every call: returning cached tokens would skip changes to guest statics.
    pub fn expand(
        &mut self,
        index: u32,
        listed: &Listed,
        db: Option<&dyn SourceDatabase>,
        call: &Call<'_>,
    ) -> Ran {
        let start = Instant::now();
        if self.failed {
            return Ran {
                output: Err(ProcMacroExpansionError::System(
                    "an earlier expansion failed; the macro session cannot continue in this revision".to_owned(),
                )),
                found: false,
                retained: false,
                took: start.elapsed(),
            };
        }
        let state = self.store.data_mut();
        state.sites =
            [call.def_site, call.call_site, call.mixed_site].map(|span| state.spans.number(span));
        state.buffer.clear();
        state.output = None;
        state.panic = None;
        state.retained = false;
        state.files.output.clear();
        let mut writer = Writer { out: Vec::new(), spans: &mut state.spans };
        writer.top(call.subtree);
        let input = std::mem::take(&mut writer.out);
        let attr = call.attrs.map(|it| {
            writer.top(it);
            std::mem::take(&mut writer.out)
        });
        let attr = match listed.kind {
            ProcMacroKind::Attr => Some(attr.unwrap_or_else(|| 0u32.to_le_bytes().to_vec())),
            _ => attr,
        };
        let output = self.run(index, db, &input, attr.as_deref()).and_then(|output| {
            let trees = Reader::new(&output, &self.store.data().spans)
                .stream()
                .map_err(|e| ProcMacroExpansionError::System(format!("`{}`: {e}", listed.name)))?;
            let trees = match &self.root {
                Some(root) => placed(trees, root),
                None => trees,
            };
            Ok(wire::top_subtree(trees, call.call_site))
        });
        if output.is_err() {
            self.failed = true;
        }
        let took = start.elapsed();
        RAN.add(took);
        Ran { output, found: false, retained: self.store.data().retained, took }
    }

    fn run(
        &mut self,
        index: u32,
        db: Option<&dyn SourceDatabase>,
        input: &[u8],
        attr: Option<&[u8]>,
    ) -> Result<Vec<u8>, ProcMacroExpansionError> {
        // SAFETY: the pointer is used only during this call and cleared before returning
        // or resuming an unwind. The session retains no database reference between calls.
        self.store.data_mut().db = db.map(|db| {
            Db(unsafe {
                std::mem::transmute::<
                    *const dyn SourceDatabase,
                    *const (dyn SourceDatabase + 'static),
                >(db)
            })
        });
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let system = |e: wasmtime::Error| ProcMacroExpansionError::System(format!("{e:#}"));
            let alloc = self
                .instance
                .get_typed_func::<u32, u32>(&mut self.store, exports::ALLOC)
                .map_err(system)?;
            let expand = self
                .instance
                .get_typed_func::<(u32, u32, u32, u32, u32), ()>(&mut self.store, exports::EXPAND)
                .map_err(system)?;
            let memory = self.instance.get_memory(&mut self.store, "memory").ok_or_else(|| {
                ProcMacroExpansionError::System("the module exports no memory".to_owned())
            })?;
            let mut put = |bytes: &[u8]| -> Result<u32, ProcMacroExpansionError> {
                let at = alloc.call(&mut self.store, bytes.len() as u32).map_err(system)?;
                memory.write(&mut self.store, at as usize, bytes).map_err(|e| system(e.into()))?;
                Ok(at)
            };
            let input_at = put(input)?;
            let attr_at = attr.map(&mut put).transpose()?.unwrap_or(0);
            let attr_len = attr.map_or(0, |it| it.len() as u32);
            expand
                .call(&mut self.store, (index, attr_at, attr_len, input_at, input.len() as u32))
                .map_err(|trap| {
                    let state = self.store.data_mut();
                    match state.panic.take() {
                        Some(message) => ProcMacroExpansionError::Panic(message),
                        None => ProcMacroExpansionError::System(format!(
                            "{trap:#}\n{}",
                            String::from_utf8_lossy(&state.files.output),
                        )),
                    }
                })?;
            self.store.data_mut().output.take().ok_or_else(|| {
                ProcMacroExpansionError::System("the macro gave no output".to_owned())
            })
        }));
        self.store.data_mut().db = None;
        match ran {
            Ok(result) => result,
            Err(payload) => {
                self.failed = true;
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// What the store keeps the machine code of the module of `digest` under: the module's bytes
/// and what Wasmtime says code must match to load (its version, its configuration, the
/// target and the CPU features it compiled for, [`CPU_FEATURES`] rather than the host's),
/// and nothing of the checkout, the build of blaze or the toolchain, so every checkout and
/// every host of the same kind shares it.
fn code_key(digest: &[u8; 32]) -> [u8; 32] {
    use std::hash::Hash as _;
    struct Fields<'a>(&'a mut keyed::Key);
    impl std::hash::Hasher for Fields<'_> {
        fn write(&mut self, bytes: &[u8]) {
            self.0.field(bytes);
        }
        fn finish(&self) -> u64 {
            0
        }
    }
    let mut key = keyed::Key::new("wasmtime machine code of a proc-macro module 2");
    key.field(digest).field(std::env::consts::ARCH.as_bytes());
    engine().precompile_compatibility_hash().hash(&mut Fields(&mut key));
    key.done()
}

/// The file beside the module at `path` its machine code of `key` is kept in, for a process to
/// map rather than read.
fn code_file(path: &Path, key: &[u8; 32]) -> PathBuf {
    let hex: String = key[..8].iter().map(|it| format!("{it:02x}")).collect();
    let mut named = path.as_os_str().to_owned();
    named.push(format!(".{hex}.cwasm"));
    PathBuf::from(named)
}

/// Writes `bytes` to `path` whole or not at all: a process that reads it while it is written
/// sees the old file or none.
fn write_whole(path: &Path, bytes: &[u8]) {
    let new = path.with_extension(format!("{}.new", std::process::id()));
    if std::fs::write(&new, bytes).is_err() || std::fs::rename(&new, path).is_err() {
        _ = std::fs::remove_file(&new);
    }
}

/// Readies the module at `path` for the processes that expand with it, in the build that
/// made it: compiles it with Cranelift (or finds its machine code in `store`), keeps the code
/// beside it and in `store`, and writes the list of its macros beside it, so that a load
/// compiles nothing and reads the list without running it.
pub fn prepare(path: &Path, store: Option<&blaze_store::Store>) -> Result<(), String> {
    let _span =
        tracing::info_span!(target: "blaze", "prepare proc macro", subject = %path.display())
            .entered();
    if let Some((known, _)) = listing(path)
        && code_file(path, &code_key(&known.digest)).is_file()
    {
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let known = Known { digest: keyed::sha256(&bytes), identity: None };
    let wasm = Wasm::known(path, known, Some(&bytes), store)?;
    list(path, &wasm, &wasm.list()?);
    Ok(())
}

/// As [`prepare`], but a module that carries its list of macros ([`exports::LIST_SECTION`]) is
/// compiled on a thread of its own: its list is written now, and the first of its macros to
/// expand in this process waits for the compile.
pub fn prepare_soon(path: &Path, store: Option<&'static blaze_store::Store>) -> Result<(), String> {
    if let Some((known, _)) = listing(path)
        && code_file(path, &code_key(&known.digest)).is_file()
    {
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(text) = carried_list(&bytes) else { return prepare(path, store) };
    let digest = keyed::sha256(&bytes);
    let identity = made_of(path, &bytes).unwrap_or(digest);
    list_known(path, &digest, &identity, &text);
    let path = path.to_owned();
    let known = Known { digest, identity: Some(identity) };
    std::thread::spawn(move || {
        let span =
            tracing::info_span!(target: "blaze", "prepare proc macro", subject = %path.display());
        let done = span.in_scope(|| Wasm::known(&path, known, Some(&bytes), store));
        if let Err(why) = &done
            && std::env::var_os("BLAZE_STATS").is_some()
        {
            eprintln!("blaze: the proc-macro module {} failed: {why}", path.display());
        }
    });
    Ok(())
}

/// What the module of `bytes` carries in its [`exports::LIST_SECTION`] custom section, read
/// without compiling it: `None` for a module built before modules carried one.
fn carried_list(bytes: &[u8]) -> Option<String> {
    fn leb(bytes: &[u8], at: &mut usize) -> Option<usize> {
        let mut value = 0usize;
        for shift in (0..35).step_by(7) {
            let byte = *bytes.get(*at)?;
            *at += 1;
            value |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    if bytes.get(..4)? != b"\0asm" {
        return None;
    }
    let mut at = 8;
    while at < bytes.len() {
        let id = bytes[at];
        at += 1;
        let size = leb(bytes, &mut at)?;
        let end = at.checked_add(size).filter(|it| *it <= bytes.len())?;
        if id == 0 {
            let mut named = at;
            let len = leb(bytes, &mut named)?;
            let name = bytes.get(named..named.checked_add(len)?)?;
            if name == exports::LIST_SECTION.as_bytes() {
                return String::from_utf8(bytes.get(named + len..end)?.to_vec()).ok();
            }
        }
        at = end;
    }
    None
}

/// What is known of a module without reading it: its digest and source identity.
#[derive(Clone, Copy)]
struct Known {
    digest: [u8; 32],
    identity: Option<[u8; 32]>,
}

/// The bytes of the module at `path` of `digest`: `given`, or read into `read` once.
fn module_bytes<'a>(
    path: &Path,
    digest: &[u8; 32],
    given: Option<&'a [u8]>,
    read: &'a std::cell::OnceCell<Result<Vec<u8>, String>>,
) -> Result<&'a [u8], String> {
    if let Some(given) = given {
        return Ok(given);
    }
    let read = read.get_or_init(|| {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        match keyed::sha256(&bytes) == *digest {
            true => Ok(bytes),
            false => Err(format!("{} changed while it was loaded", path.display())),
        }
    });
    read.as_deref().map_err(Clone::clone)
}

/// What tells that the module at `path` and the file beside it that says what it was built of
/// are the files a list was written for: each one's length, time of writing and inode. Both
/// are written whole under a new name when they change.
fn stamp(path: &Path) -> Option<String> {
    let one = |path: &Path| -> Option<String> {
        let Ok(meta) = std::fs::metadata(path) else { return Some("-".to_owned()) };
        let written = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        Some(format!("{}.{}.{inode}", meta.len(), written.as_nanos()))
    };
    let mut made_of = path.as_os_str().to_owned();
    made_of.push(".made-of");
    Some(format!("{}:{}", one(path)?, one(Path::new(&made_of))?))
}

/// What the list file beside the module at `path` knows of it, and its list of macros, when
/// it was written for the files there now ([`stamp`]): then the module need not be read.
fn listing(path: &Path) -> Option<(Known, String)> {
    let text = std::fs::read_to_string(list_file(path)).ok()?;
    let (digest, rest) = text.split_once('\n')?;
    let (head, list) = rest.split_once('\n').unwrap_or((rest, ""));
    let (identity, stamped) = head.strip_prefix("@ ")?.split_once(' ')?;
    (stamp(path)? == stamped).then_some(())?;
    let known = Known { digest: unhex(digest)?, identity: Some(unhex(identity)?) };
    Some((known, list.to_owned()))
}

/// Writes the list file of the module at `path`, compiled as `wasm`, whose macros `text` lists.
fn list(path: &Path, wasm: &Wasm, text: &str) {
    list_known(path, &wasm.digest, &wasm.identity, text);
}

fn list_known(path: &Path, digest: &[u8; 32], identity: &[u8; 32], text: &str) {
    let Some(stamp) = stamp(path) else { return };
    let (digest, identity) = (hex(digest), hex(identity));
    write_whole(&list_file(path), format!("{digest}\n@ {identity} {stamp}\n{text}").as_bytes());
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|it| format!("{it:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    let mut out = [0; 32];
    if text.len() != 64 {
        return None;
    }
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// What the module of `bytes` at `path` was built of, as the file beside it says, when that
/// file was written for these bytes.
fn made_of(path: &Path, bytes: &[u8]) -> Option<[u8; 32]> {
    let mut named = path.as_os_str().to_owned();
    named.push(".made-of");
    let text = std::fs::read_to_string(PathBuf::from(named)).ok()?;
    let (key, file) = text.trim_end().split_once(' ')?;
    let mut identity = keyed::Key::new("a proc-macro module built of");
    identity.field(key.as_bytes());
    (keyed::digest_bytes(bytes) == file).then(|| identity.done())
}

/// The package directory of the crate a macro expands in, from its `CARGO_MANIFEST_DIR`.
fn package_dir(env: &Env) -> Option<PathBuf> {
    env.get("CARGO_MANIFEST_DIR").map(PathBuf::from).filter(|it| it.is_absolute())
}

/// What a module is told of its environment: cargo's variables for a crate's compilation
/// that the crate's env has, each with what it says on every machine, sorted. No other
/// variable, of the crate's or of the process's.
fn guest_env(env: &Env) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env
        .iter()
        .filter_map(|(name, value)| {
            let value = match name {
                "CARGO_MANIFEST_DIR" => wasi::PACKAGE.to_owned(),
                "CARGO_MANIFEST_PATH" => format!("{}/Cargo.toml", wasi::PACKAGE),
                "OUT_DIR" => wasi::OUT_DIR.to_owned(),
                "CARGO_CRATE_NAME" | "CARGO_PRIMARY_PACKAGE" | "CARGO_BIN_NAME" => value.to_owned(),
                _ if name.starts_with("CARGO_PKG_") => value.to_owned(),
                _ => return None,
            };
            Some((name.to_owned(), value))
        })
        .collect();
    out.sort();
    out
}

/// `trees` with the package's path where the module put [`wasi::PACKAGE`] in a string, as
/// a path `include_str!` is to read.
fn placed(trees: Vec<Tree>, root: &Path) -> Vec<Tree> {
    let root = root.to_string_lossy();
    trees
        .into_iter()
        .map(|tree| match tree {
            Tree::Group { kind, open, close, entire, trees } => {
                Tree::Group { kind, open, close, entire, trees: placed(trees, Path::new(&*root)) }
            }
            Tree::Leaf(tt::Leaf::Literal(literal))
                if matches!(literal.kind, tt::LitKind::Str | tt::LitKind::StrRaw(_))
                    && literal.text().contains(wasi::PACKAGE) =>
            {
                let (text, suffix) = literal.text_and_suffix();
                let text = text.replace(wasi::PACKAGE, &root);
                Tree::Leaf(tt::Leaf::Literal(tt::Literal::new(
                    &text,
                    literal.span,
                    literal.kind,
                    suffix,
                )))
            }
            other => other,
        })
        .collect()
}

/// The spans of an expansion's input as its key has them: each its byte range and which of
/// the input's anchors and contexts it has, numbered as they first appear. A kept output's
/// spans are written the same way, so the next call, with other anchors and contexts in the
/// same places, is given them back as its own.
struct Shapes {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
    bytes: Vec<u8>,
}

fn position<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
    let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
        all.push(it);
        all.len() - 1
    });
    at as u32
}

impl Shapes {
    fn of(spans: &[Span]) -> Shapes {
        let mut shapes = Shapes { anchors: Vec::new(), contexts: Vec::new(), bytes: Vec::new() };
        for span in spans {
            let anchor = position(&mut shapes.anchors, span.anchor);
            let context = position(&mut shapes.contexts, span.ctx);
            for it in [span.range.start().into(), span.range.end().into(), anchor, context] {
                shapes.bytes.extend_from_slice(&u32::to_le_bytes(it));
            }
        }
        shapes
    }

    /// What is kept of an output: the shapes of every span the expansion numbered, then the
    /// output. `None` when one has an anchor or a context the input has not.
    fn written(&self, spans: &Spans, output: &[u8]) -> Option<Vec<u8>> {
        let mut value = Vec::new();
        value.extend_from_slice(&(spans.all().len() as u32).to_le_bytes());
        for span in spans.all() {
            let anchor = self.anchors.iter().position(|it| *it == span.anchor)? as u32;
            let context = self.contexts.iter().position(|it| *it == span.ctx)? as u32;
            for it in [span.range.start().into(), span.range.end().into(), anchor, context] {
                value.extend_from_slice(&u32::to_le_bytes(it));
            }
        }
        value.extend_from_slice(output);
        Some(value)
    }

    fn read(&self, value: &[u8]) -> Option<Vec<Tree>> {
        let count = u32::from_le_bytes(value.get(..4)?.try_into().ok()?) as usize;
        let table = value.get(4..4 + count.checked_mul(16)?)?;
        let mut spans = Spans::default();
        for shape in table.as_chunks::<16>().0 {
            let field = |at: usize| u32::from_le_bytes(shape[at..at + 4].try_into().unwrap());
            let span = Span {
                range: range(field(0), field(4)),
                anchor: *self.anchors.get(field(8) as usize)?,
                ctx: *self.contexts.get(field(12) as usize)?,
            };
            spans.push(span);
        }
        Reader::new(&value[4 + count * 16..], &spans).stream().ok()
    }
}

/// Expands the macro at `index` of a module whose expansions are kept under `identity` in a
/// fresh instance: what is kept of it when the store has it, else by running it in what
/// `wasm` gives, which is asked for only then, and keeps what it made unless `store` is `None`
/// or the expansion left state behind.
#[allow(clippy::too_many_arguments)]
fn expand<'a>(
    identity: &[u8; 32],
    wasm: &dyn Fn() -> Result<&'a Wasm, String>,
    index: u32,
    listed: &Listed,
    db: Option<&dyn SourceDatabase>,
    call: &Call<'_>,
    store: Option<&blaze_store::Store>,
) -> Ran {
    let start = Instant::now();
    let root = package_dir(call.env);
    let env = guest_env(call.env);
    // The module's `def_site`, `call_site` and `mixed_site` are 1, 2 and 3, the same
    // spans or not.
    let mut spans = Spans::default();
    for span in [call.def_site, call.call_site, call.mixed_site] {
        spans.push(span);
    }
    let mut writer = Writer { out: Vec::new(), spans: &mut spans };
    writer.top(call.subtree);
    let input = std::mem::take(&mut writer.out);
    let attr = call.attrs.map(|it| {
        writer.top(it);
        std::mem::take(&mut writer.out)
    });
    // rust-analyzer gives an attribute written without arguments (`#[twice]`) none.
    let attr = match listed.kind {
        ProcMacroKind::Attr => Some(attr.unwrap_or_else(|| 0u32.to_le_bytes().to_vec())),
        _ => attr,
    };
    let shapes = Shapes::of(spans.all());
    let mut first = keyed::Key::new(DOMAIN);
    first.field(identity).field(listed.name.as_bytes()).field(&[listed.kind as u8]);
    first.field(&input).field(attr.as_deref().unwrap_or(b"-")).field(&shapes.bytes);
    for (name, value) in &env {
        first.field(name.as_bytes()).field(value.as_bytes());
    }
    let first = first.done();
    let restore = |trees: Vec<Tree>| {
        let trees = match &root {
            Some(root) => placed(trees, root),
            None => trees,
        };
        wire::top_subtree(trees, call.call_site)
    };
    // A module given no package has nothing to read. A question asked of a span is asked of
    // this call's span in its place.
    let now = |read: &str| match Value::parse(read, &shapes) {
        Some((value, span)) => {
            value.said(db.map(|db| load_cargo::answer(db, value.request(span))).as_ref())
        }
        None if read.starts_with('\0') => "?".to_owned(),
        None => root.as_deref().map_or("-".to_owned(), |it| keyed::now(it, read)),
    };
    if let Some(store) = store
        && let Some(kept) = keyed::find(store, &first, &now)
        && let Some(trees) = shapes.read(&kept)
    {
        FOUND.add(start.elapsed());
        let took = start.elapsed();
        return Ran { output: Ok(restore(trees)), found: true, retained: false, took };
    }
    let ran = wasm().map_err(ProcMacroExpansionError::System).and_then(|wasm| {
        let mut state = State::empty();
        state.anchors = shapes.anchors.clone();
        state.contexts = shapes.contexts.clone();
        state.spans = spans;
        state.sites = [1, 2, 3];
        state.files = wasi::Files::new(root.as_deref());
        state.env = env;
        let mut instance = wasm.instance_with(state, root.clone())?;
        let output = instance.run(index, db, &input, attr.as_deref())?;
        Ok((instance.store.into_data(), output))
    });
    let mut retained = false;
    let output = ran.and_then(|(state, output)| {
        let trees = Reader::new(&output, &state.spans)
            .stream()
            .map_err(|e| ProcMacroExpansionError::System(format!("`{}`: {e}", listed.name)))?;
        retained = state.retained;
        if state.asked {
            ASKED.add(Duration::ZERO);
        }
        if let Some(store) = store
            && !state.asked
            && !state.retained
            && let Some(value) = shapes.written(&state.spans, &output)
        {
            let mut reads: Vec<(String, String)> = state.files.reads.into_iter().collect();
            reads.extend(state.asks);
            keyed::keep(store, &first, &reads, &value);
            KEPT.store(true, Ordering::Relaxed);
        }
        Ok(restore(trees))
    });
    RAN.add(start.elapsed());
    Ran { output, found: false, retained, took: start.elapsed() }
}

/// What blaze has seen of a module's expansions, kept in the store under the module's digest
/// ([`verdict_key`]) for every process and machine that loads the same module.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Verdict {
    /// None has run in any process that told the store.
    Unseen,
    /// Some ran, in a build, and none left what it allocated live.
    Stateless,
    /// One left what it allocated live, for a later one to find.
    Stateful,
}

const STATELESS: &[u8] = b"stateless";
const STATEFUL: &[u8] = b"stateful";

fn verdict_key(digest: &[u8; 32]) -> [u8; 32] {
    let mut key = keyed::Key::new("whether a proc-macro module keeps state 1");
    key.field(digest);
    key.done()
}

fn verdict(digest: &[u8; 32]) -> Verdict {
    match store().and_then(|it| it.get_lasting(&verdict_key(digest))).as_deref() {
        Some(STATELESS) => Verdict::Stateless,
        Some(STATEFUL) => Verdict::Stateful,
        _ => Verdict::Unseen,
    }
}

/// The modules loaded in this process, for [`flush`] to tell the store what it saw of them.
static MODULES: Mutex<Vec<std::sync::Weak<Lazy>>> = Mutex::new(Vec::new());

/// A macro of a module, as rust-analyzer expands it.
pub struct Expander {
    wasm: AssertUnwindSafe<Arc<Lazy>>,
    index: u32,
    listed: Listed,
}

/// A module compiled when it first expands, by when the store can give its machine code.
struct Lazy {
    path: PathBuf,
    known: Known,
    compiled: OnceLock<Result<Arc<Wasm>, String>>,
    /// What the store said of it when it was loaded, which decides for as long as it is:
    /// whether a macro needs a session is asked of the database's queries, which must not
    /// see it change.
    verdict: Verdict,
    /// Whether one of its expansions ran in this process, and whether one left state.
    ran: AtomicBool,
    retained: AtomicBool,
    /// Each crate's session has its own lock, so crates expand the library at once.
    sessions: Mutex<FxHashMap<(Nonce, Crate), (salsa::Revision, Slot)>>,
}

/// A crate's session, started by its first expansion in the revision.
type Slot = Arc<Mutex<Option<Result<Session, String>>>>;

impl Lazy {
    fn new(path: &Path, known: Known) -> Lazy {
        Lazy {
            path: path.to_owned(),
            known,
            compiled: OnceLock::new(),
            verdict: verdict(&known.digest),
            ran: AtomicBool::new(false),
            retained: AtomicBool::new(false),
            sessions: Mutex::new(FxHashMap::default()),
        }
    }

    fn get(&self) -> Result<&Arc<Wasm>, String> {
        let compiled =
            self.compiled.get_or_init(|| Wasm::known(&self.path, self.known, None, store()));
        let wasm = compiled.as_ref().map_err(Clone::clone)?;
        wasm.keep_code(store());
        Ok(wasm)
    }

    /// Whether its macros expand in a session: unless the store says none of its expansions
    /// left state.
    fn stateful(&self) -> bool {
        self.verdict != Verdict::Stateless
    }

    /// Notes what an expansion that ran did. One that left state is told the store at once;
    /// that none did, by [`flush`], once a build has run them.
    fn saw(&self, ran: &Ran) {
        if ran.found || ran.output.is_err() {
            return;
        }
        if !ran.retained {
            self.ran.store(true, Ordering::Relaxed);
            return;
        }
        if self.retained.swap(true, Ordering::Relaxed) || self.verdict == Verdict::Stateful {
            return;
        }
        if let Some(store) = store() {
            store.insert_lasting(&verdict_key(&self.known.digest), STATEFUL);
            store.flush();
        }
        if !self.stateful() {
            eprintln!(
                "blaze: the proc macros of {} keep state between expansions, which the store \
                 said they did not: they expand in a session from the next load",
                self.path.display()
            );
        }
    }
}

/// The file beside a module that has its list of macros, for the module whose digest is on
/// its first line, with its source identity and the [`stamp`] of the files it
/// was written for on the second: a load reads it there and compiles nothing.
fn list_file(path: &Path) -> PathBuf {
    let mut named = path.as_os_str().to_owned();
    named.push(".macros");
    PathBuf::from(named)
}

impl fmt::Debug for Expander {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Expander").field("name", &self.listed.name).finish_non_exhaustive()
    }
}

/// Counts macro expansions and loaded modules for the daemon's build statistics.
struct Count {
    calls: AtomicU64,
    nanos: AtomicU64,
}

impl Count {
    const fn new() -> Count {
        Count { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) }
    }

    fn add(&self, took: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, f64) {
        (self.calls.load(Ordering::Relaxed), self.nanos.load(Ordering::Relaxed) as f64 / 1e9)
    }
}

/// Whether an expansion was kept since [`flush`].
static KEPT: AtomicBool = AtomicBool::new(false);

/// Expansions run in a module, with the time each took from its call.
static RAN: Count = Count::new();
/// Expansions the store had, with the time each took from its call.
static FOUND: Count = Count::new();
/// Expansions run that were not kept, as they asked the database.
static ASKED: Count = Count::new();
/// Modules made ready to run in this process (machine code loaded or compiled, linked).
static LOADED: Count = Count::new();

/// The expansions so far, for the daemon's log: `None` before the first.
pub fn report() -> Option<String> {
    let (ran, running) = RAN.get();
    let (found, finding) = FOUND.get();
    if ran + found == 0 {
        return None;
    }
    let (loaded, loading) = LOADED.get();
    let mut said = format!(
        "wasm proc macros: {ran} ran in {running:.3}s, {found} from the store in \
         {finding:.3}s, {loaded} modules loaded in {loading:.3}s"
    );
    let (asked, _) = ASKED.get();
    if asked != 0 {
        said += &format!("; {asked} not kept, as they asked the database");
    }
    let modules = MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = modules.iter().filter_map(std::sync::Weak::upgrade).filter(|it| it.stateful());
    let sessions = sessions.count();
    if sessions != 0 {
        said += &format!("; {sessions} modules expand in sessions (keep state or not seen yet)");
    }
    Some(said)
}

/// Writes out the expansions kept since the last call, and that a module whose expansions
/// ran and none of which left state is stateless, for the next process to find: the store
/// holds what is added in memory until it is flushed.
pub fn flush() {
    let Some(store) = store() else { return };
    let mut told = KEPT.swap(false, Ordering::Relaxed);
    let mut modules = MODULES.lock().unwrap_or_else(|e| e.into_inner());
    modules.retain(|it| it.strong_count() != 0);
    for lazy in modules.iter().filter_map(std::sync::Weak::upgrade) {
        // Another process that saw one leave state may have told the store since the load.
        if lazy.verdict == Verdict::Unseen
            && lazy.ran.swap(false, Ordering::Relaxed)
            && !lazy.retained.load(Ordering::Relaxed)
            && verdict(&lazy.known.digest) == Verdict::Unseen
        {
            store.insert_lasting(&verdict_key(&lazy.known.digest), STATELESS);
            told = true;
        }
    }
    if told {
        store.flush();
    }
}

/// The store, once the daemon has told it the toolchain.
fn store() -> Option<&'static blaze_store::Store> {
    if matches!(std::env::var("BLAZE_PROC_MACRO_CACHE").as_deref(), Ok("off" | "0")) {
        return None;
    }
    blaze_store::toolchain()?;
    blaze_store::Store::global()
}

impl ProcMacroExpander for Expander {
    fn requires_session(&self) -> bool {
        self.wasm.stateful()
    }

    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        _current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if self.wasm.stateful() {
            return Err(ProcMacroExpansionError::System(
                "a proc macro that keeps state expands only for a calling crate".to_owned(),
            ));
        }
        let call = Call { subtree, attrs, env, def_site, call_site, mixed_site };
        // What its expansions are kept under is known without the module when the list file
        // says: then one the store has neither loads nor compiles it.
        let wasm = || self.wasm.get().map(|it| &**it);
        let ran = match self.wasm.known.identity {
            Some(identity) => {
                expand(&identity, &wasm, self.index, &self.listed, Some(db), &call, store())
            }
            None => match wasm() {
                Ok(wasm) => wasm.expand(self.index, &self.listed, Some(db), &call, store()),
                Err(e) => return Err(ProcMacroExpansionError::System(e)),
            },
        };
        self.wasm.saw(&ran);
        ran.output
    }

    fn expand_in(
        &self,
        db: &dyn SourceDatabase,
        calling_crate: base_db::Crate,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if !self.wasm.stateful() {
            return self.expand(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            );
        }
        // A new revision must rerun unchanged registrations before changed uses.
        db.report_untracked_read();
        let (nonce, revision) = db.nonce_and_revision();
        let wasm = self.wasm.get().map_err(ProcMacroExpansionError::System)?;
        let slot = {
            let mut sessions = self.wasm.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.retain(|(owner, _), (seen, _)| *owner != nonce || *seen == revision);
            let entry = sessions.entry((nonce, calling_crate));
            entry.or_insert_with(|| (revision, Slot::default())).1.clone()
        };
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        let session = slot.get_or_insert_with(|| {
            wasm.session(env).map_err(|e| match e {
                ProcMacroExpansionError::Panic(why) | ProcMacroExpansionError::System(why) => why,
            })
        });
        let session = session.as_mut().map_err(|e| ProcMacroExpansionError::System(e.clone()))?;
        let call = Call { subtree, attrs, env, def_site, call_site, mixed_site };
        let ran = session.expand(self.index, &self.listed, Some(db), &call);
        self.wasm.saw(&ran);
        ran.output
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Expander>().is_some_and(|other| {
            self.wasm.known.digest == other.wasm.known.digest && self.index == other.index
        })
    }
}

/// The tokens of `text`, each at `span`'s anchor and context over its range of the text, as
/// rust-analyzer's proc-macro server lexes a string: a macro's input for a test or a bench.
pub fn tokens(text: &str, span: Span) -> Result<tt::TopSubtree, String> {
    Ok(wire::top_subtree(lex::stream(text, span)?, span))
}

/// The macros of the module at `path`, for rust-analyzer: what blaze registers with
/// [`load_cargo::wasm_macros_with`].
pub fn load(path: &AbsPath) -> ProcMacroLoadResult {
    let failed = |e: String| {
        base_db::ProcMacroLoadingError::ProcMacroSrvError(
            format!("proc macros compiled to WebAssembly: {e}").into(),
        )
    };
    let path: &Path = path.as_ref();
    let (lazy, text) = match listing(path) {
        Some((known, text)) => (Lazy::new(path, known), text),
        None => {
            let wasm = Wasm::at(path, store()).map_err(failed)?;
            let text = wasm.list().map_err(failed)?;
            list(path, &wasm, &text);
            let known = Known { digest: wasm.digest, identity: Some(wasm.identity) };
            let lazy = Lazy::new(path, known);
            _ = lazy.compiled.set(Ok(wasm));
            (lazy, text)
        }
    };
    let lazy = Arc::new(lazy);
    MODULES.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&lazy));
    Ok(listed(&text)
        .into_iter()
        .enumerate()
        .map(|(index, listed)| ProcMacro {
            name: Symbol::intern(&listed.name),
            kind: listed.kind,
            expander: Arc::new(Expander {
                wasm: AssertUnwindSafe(lazy.clone()),
                index: index as u32,
                listed,
            }),
            disabled: false,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u32, end: u32) -> Span {
        let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
        Span {
            range: range(start, end),
            anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContext::root(span::Edition::CURRENT),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blaze-macro-wasm-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn text_is_lexed_as_the_proc_macro_server_lexes_it() {
        let lexed = tokens("fn f<'a>(x: &'a u8) -> r#u8 { x.0 + -1i32 } /// doc", span(0, 0));
        assert_eq!(
            lexed.unwrap().to_string(),
            "fn f <'a >(x : &'a u8) -> r#u8 {x . 0 + - 1i32} # [doc = \" doc\"]"
        );
        assert!(tokens("(]", span(0, 0)).is_err());
        assert!(tokens("a ` b", span(0, 0)).is_err());
        let one = lex::literal_from_str("-1.5f32", span(0, 0)).unwrap();
        assert_eq!(one.text_and_suffix(), ("-1.5", "f32"));
        assert!(lex::literal_from_str("-\"no\"", span(0, 0)).is_none());
    }

    #[test]
    fn a_stream_comes_back_as_it_went() {
        let text =
            "#[derive(Debug)] struct S<T> { a: [u8; 3], b: (T, \"s\", b'x', r#\"raw\"#, 2.5e3) }";
        let top = tokens(text, span(3, 9)).unwrap();
        let mut spans = Spans::default();
        let mut writer = Writer { out: Vec::new(), spans: &mut spans };
        writer.top(&top);
        let bytes = writer.out;
        let trees = Reader::new(&bytes, &spans).stream().unwrap();
        assert_eq!(wire::top_subtree(trees, span(3, 9)).to_string(), top.to_string());
    }

    #[test]
    fn a_negative_number_comes_out_as_a_minus_and_a_number() {
        let number = |text: &str, kind: tt::LitKind, suffix: &str| {
            Tree::Leaf(tt::Leaf::Literal(tt::Literal::new(text, span(0, 0), kind, suffix)))
        };
        let trees = vec![
            number("-1", tt::LitKind::Integer, "i8"),
            number("-1.5", tt::LitKind::Float, ""),
            number("2", tt::LitKind::Integer, ""),
        ];
        let want = tokens("-1i8 -1.5 2", span(0, 0)).unwrap();
        assert_eq!(wire::top_subtree(trees, span(0, 0)).to_string(), want.to_string());
    }

    #[test]
    fn a_module_carries_its_list_in_a_custom_section() {
        let custom = |name: &str, payload: &[u8]| {
            let mut body = vec![name.len() as u8];
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(payload);
            let mut section = vec![0, body.len() as u8];
            section.extend(body);
            section
        };
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        // A type section of no types, then a custom section of another name.
        module.extend_from_slice(&[1, 1, 0]);
        module.extend(custom("name", b"\0\0"));
        assert_eq!(carried_list(&module), None);
        module.extend(custom(exports::LIST_SECTION, b"0\tSerialize\tserde\n"));
        assert_eq!(carried_list(&module).as_deref(), Some("0\tSerialize\tserde\n"));
        module.truncate(module.len() - 1);
        assert_eq!(carried_list(&module), None, "a section that runs past the end");
    }

    #[test]
    fn a_prepared_module_is_loaded_without_compiling_or_reading_it() {
        let dir = scratch("prepared");
        blaze_store::compiles_against("blaze-macro-wasm tests");
        let store: &'static blaze_store::Store =
            Box::leak(Box::new(blaze_store::Store::open_at(&dir.join("store")).unwrap()));
        let path = dir.join("m.wasm");
        let list = "0\tSerialize\tserde\n";
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        let name = exports::LIST_SECTION.as_bytes();
        module.extend_from_slice(&[0, (1 + name.len() + list.len()) as u8, name.len() as u8]);
        module.extend_from_slice(name);
        module.extend_from_slice(list.as_bytes());
        std::fs::write(&path, &module).unwrap();
        prepare_soon(&path, Some(store)).unwrap();
        let (known, text) = listing(&path).expect("the list is written before the compile ends");
        assert_eq!(text, list);
        assert_eq!(known.digest, keyed::sha256(&module));
        // Waits for the compile `prepare_soon` started, which keeps the code beside the module
        // and in the store, under a key of no path.
        Wasm::known(&path, known, None, Some(store)).unwrap();
        let key = code_key(&known.digest);
        assert!(code_file(&path, &key).is_file());
        store.seal();
        assert!(store.get_lasting(&key).is_some());
        // Another write of the module, even of the same bytes, is listed again.
        std::fs::write(&path, &module).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(listing(&path).is_none());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_question_of_a_span_is_asked_again_of_the_span_in_its_place() {
        let asked = span(3, 9);
        let read = Value::LineColumn.read([3, 9, 0, 0]);
        let (value, again) = Value::parse(&read, &Shapes::of(&[asked])).unwrap();
        assert!(value == Value::LineColumn);
        assert_eq!(again, asked);
        // The next call's span in the same place, of another anchor.
        let mut moved = asked;
        moved.anchor.ast_id = ErasedFileAstId::from_raw(5);
        assert_eq!(Value::parse(&read, &Shapes::of(&[moved])).unwrap().1, moved);
        assert!(Value::parse(&Value::File.read([0, 0, 1, 0]), &Shapes::of(&[asked])).is_none());
        assert!(Value::parse("src/lib.rs", &Shapes::of(&[asked])).is_none());
        let answer = SubResponse::LineColumnResult { line: 3, column: 4 };
        assert_eq!(Value::LineColumn.said(Some(&answer)), "3:4");
        assert_eq!(Value::SourceText.said(None), "-");
    }

    #[test]
    fn a_subspan_is_of_the_span_and_within_it() {
        let it = span(10, 20);
        // Unbounded, included and excluded bounds as the module writes them.
        assert_eq!(subspan(it, 0, 0).unwrap().range, range(10, 20));
        assert_eq!(subspan(it, 1 + 4 * 2, 3 + 4 * 5).unwrap().range, range(12, 15));
        assert_eq!(subspan(it, 3 + 4 * 2, 1 + 4 * 5).unwrap().range, range(13, 16));
        assert!(subspan(it, 1 + 4 * 5, 3 + 4 * 5).is_none());
        assert!(subspan(it, 0, 3 + 4 * 11).is_none());
    }

    #[test]
    fn a_module_reads_its_package_and_nothing_else() {
        let dir = scratch("files");
        std::fs::create_dir_all(dir.join("package/src")).unwrap();
        std::fs::write(dir.join("package/src/a.txt"), "a").unwrap();
        std::fs::write(dir.join("outside.txt"), "o").unwrap();
        let files = wasi::Files::new(Some(&dir.join("package")));
        let resolve = |rel: &str, path: &str| files_resolve(&files, rel, path);
        assert_eq!(resolve("", "src/./a.txt").unwrap(), "src/a.txt");
        assert_eq!(resolve("src", "../src/a.txt").unwrap(), "src/a.txt");
        assert_eq!(resolve("", "../outside.txt"), Err(76));
        assert_eq!(resolve("", "/etc/passwd"), Err(76));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("outside.txt"), dir.join("package/link")).unwrap();
            assert_eq!(resolve("", "link"), Err(76));
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    fn files_resolve(files: &wasi::Files, rel: &str, path: &str) -> Result<String, i32> {
        files.resolve_for_test(rel, path)
    }

    #[test]
    fn an_expansion_is_found_while_what_it_read_is_as_it_was() {
        let dir = scratch("keyed");
        let package = dir.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("schema.json"), "1").unwrap();
        blaze_store::compiles_against("blaze-macro-wasm tests");
        let open = || blaze_store::Store::open_at(&dir.join("store")).unwrap();
        let first = keyed::Key::new("test").done();
        let now = |read: &str| keyed::now(&package, read);
        let reads = vec![
            ("schema.json".to_owned(), keyed::digest_of(&package.join("schema.json"))),
            (keyed::kind("missing"), "-".to_owned()),
        ];
        {
            let store = open();
            assert!(keyed::find(&store, &first, &now).is_none());
            keyed::keep(&store, &first, &reads, b"one");
            store.flush();
        }
        assert_eq!(keyed::find(&open(), &first, &now).unwrap(), b"one");
        std::fs::write(package.join("schema.json"), "2").unwrap();
        assert!(keyed::find(&open(), &first, &now).is_none(), "a file it read changed");
        std::fs::write(package.join("schema.json"), "1").unwrap();
        std::fs::create_dir_all(package.join("missing")).unwrap();
        assert!(keyed::find(&open(), &first, &now).is_none(), "what it found missing is there");
        std::fs::remove_dir_all(package.join("missing")).unwrap();
        // The same package elsewhere, as on another machine, finds it.
        let moved = dir.join("moved");
        std::fs::rename(&package, &moved).unwrap();
        let now = |read: &str| keyed::now(&moved, read);
        assert_eq!(keyed::find(&open(), &first, &now).unwrap(), b"one");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_environment_is_cargos_and_the_same_on_every_machine() {
        let mut env = Env::default();
        env.set("CARGO_MANIFEST_DIR", "/home/someone/pkg");
        env.set("CARGO_PKG_NAME", "pkg");
        env.set("OUT_DIR", "/home/someone/target/out");
        env.set("AWS_SECRET_ACCESS_KEY", "no");
        env.set("PATH", "/usr/bin");
        let names: Vec<_> = guest_env(&env).into_iter().collect();
        assert_eq!(
            names,
            [
                ("CARGO_MANIFEST_DIR".to_owned(), wasi::PACKAGE.to_owned()),
                ("CARGO_PKG_NAME".to_owned(), "pkg".to_owned()),
                ("OUT_DIR".to_owned(), wasi::OUT_DIR.to_owned()),
            ]
        );
    }
}
