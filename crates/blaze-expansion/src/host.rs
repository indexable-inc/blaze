//! Every call the outside of an expander or a script can be reached by, and what each is to its
//! record ([`Rule`]): an entry of the read list, a part of the key, answered from the key, part
//! of the value, or refused with a typed error naming the macro or script and the call. There is
//! no fifth class: a call not in [`HostCall`] is refused by the host before it is served, and
//! nothing is "not kept" (outside.md section 3's fallback class, deleted).
//!
//! The proc-macro rows are the requests of blaze-macro-eval's server (`server.rs`, the pinned
//! bridge's `Method`s and the `SubRequest`s it asks the database) and the WASI calls its
//! module's std makes (`wasm/wasi.rs`, `wasm/host.rs`); the script rows are the lines the
//! sandbox's trace writes (blaze-usercode's `trace::Seen`).

use std::fmt;

use crate::reads::SpanQuestion;

/// A call an expander or a script makes of what is outside its own code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HostCall {
    // The proc_macro bridge.
    /// `proc_macro::tracked_env::var` (and `getenv` through wasi-libc): a variable by name.
    EnvVar,
    /// `proc_macro::tracked_path::path`: a file named as an input.
    TrackPath,
    /// `TokenStream::from_str`, `Literal::from_str`, `TokenStream::to_string`, `Ident::new`,
    /// `Span::resolved_at`, `Span::located_at`, the three sites: functions of the tokens.
    Tokens,
    /// A span's `Debug` text: answered from the span as the call numbers it.
    SpanDebug,
    /// `Span::file`, `Span::local_file`.
    SpanFile,
    /// `Span::line`, `Span::column`, `Span::start`, `Span::end`.
    SpanLineColumn,
    /// `Span::byte_range`.
    SpanByteRange,
    /// `Span::source_text`.
    SpanSourceText,
    /// `Span::parent`.
    SpanParent,
    /// `Span::source`.
    SpanSource,
    /// `Span::join` of two spans.
    SpanJoin,
    /// The edition of a span's context, asked to print a keyword of some editions.
    SpanEdition,
    /// `Diagnostic::emit`, a lexer error of `from_str`: part of the value.
    EmitDiagnostic,
    /// `TokenStream::expand_expr`: blaze's server does not expand an expression for a macro.
    ExpandExpr,
    // WASI, as the module's std calls it.
    /// `environ_get`, `environ_sizes_get`: the whole environment walked.
    EnvironWalk,
    /// `path_open` to read, `fd_read`, `fd_pread` of an opened file.
    FileRead,
    /// `path_filestat_get`, `fd_filestat_get`, `path_readlink`.
    FileLook,
    /// `fd_readdir`.
    DirList,
    /// `path_open` to write or create, `fd_write` to a file, `path_create_directory`,
    /// `path_unlink_file`, `path_rename`, `path_remove_directory`, `fd_allocate`.
    FileWrite,
    /// `fd_write` to stdout or stderr: part of the value, as rustc shows it.
    Print,
    /// `clock_time_get`, `clock_res_get`, `poll_oneoff` of a clock.
    Clock,
    /// `random_get`.
    Random,
    /// `sock_*`.
    Network,
    /// `proc_exit`, `proc_raise`: the run ends, its end is the value.
    Exit,
    // The script sandbox's trace.
    /// `R`: a path opened to read or run.
    TraceRead,
    /// `L`: a path looked at.
    TraceLook,
    /// `D`: a directory listed.
    TraceList,
    /// `E` with a name: a variable read by name.
    TraceEnv,
    /// `E *`: the whole environment walked.
    TraceEnvWalk,
    /// `T clock`, `T random`, `T answered random`, `T answered device`.
    TraceAnswered,
    /// `T network` of a script whose manifest lets it reach the network.
    TraceNetwork,
    /// `T descriptor`: a descriptor inherited from outside the sandbox was used.
    TraceDescriptor,
    /// `W`, `N`: a write or an address the sandbox refused. The refusal is the same every run.
    TraceRefused,
    /// `X`: the trace is not whole.
    TraceIncomplete,
}

/// What a call is to its record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rule {
    /// An entry of the read list: the ask, with the digest of its answer.
    Entry(EntryKind),
    /// Decided by what the key holds: no entry.
    InKey,
    /// Answered by blaze from the run's key (a fixed clock, randomness from the key's provider):
    /// no input, no entry.
    FromKey,
    /// Part of the run's value (a diagnostic, printed text, how it ended).
    Value,
    /// The run stops with a typed error naming the macro or script, the call and why.
    Refused(&'static str),
}

/// Which [`crate::reads::Ask`] a call's entry is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    File,
    Look,
    List,
    Env,
    EnvWalk,
    Span(SpanQuestion),
    Join,
    Edition,
}

impl HostCall {
    pub const ALL: [HostCall; 34] = [
        HostCall::EnvVar,
        HostCall::TrackPath,
        HostCall::Tokens,
        HostCall::SpanDebug,
        HostCall::SpanFile,
        HostCall::SpanLineColumn,
        HostCall::SpanByteRange,
        HostCall::SpanSourceText,
        HostCall::SpanParent,
        HostCall::SpanSource,
        HostCall::SpanJoin,
        HostCall::SpanEdition,
        HostCall::EmitDiagnostic,
        HostCall::ExpandExpr,
        HostCall::EnvironWalk,
        HostCall::FileRead,
        HostCall::FileLook,
        HostCall::DirList,
        HostCall::FileWrite,
        HostCall::Print,
        HostCall::Clock,
        HostCall::Random,
        HostCall::Network,
        HostCall::Exit,
        HostCall::TraceRead,
        HostCall::TraceLook,
        HostCall::TraceList,
        HostCall::TraceEnv,
        HostCall::TraceEnvWalk,
        HostCall::TraceAnswered,
        HostCall::TraceNetwork,
        HostCall::TraceDescriptor,
        HostCall::TraceRefused,
        HostCall::TraceIncomplete,
    ];

    /// What the call is to its record.
    pub const fn rule(self) -> Rule {
        match self {
            HostCall::EnvVar | HostCall::TraceEnv => Rule::Entry(EntryKind::Env),
            HostCall::TrackPath | HostCall::FileRead | HostCall::TraceRead => {
                Rule::Entry(EntryKind::File)
            }
            HostCall::FileLook | HostCall::TraceLook => Rule::Entry(EntryKind::Look),
            HostCall::DirList | HostCall::TraceList => Rule::Entry(EntryKind::List),
            HostCall::EnvironWalk | HostCall::TraceEnvWalk => Rule::Entry(EntryKind::EnvWalk),
            HostCall::SpanFile => Rule::Entry(EntryKind::Span(SpanQuestion::File)),
            HostCall::SpanLineColumn => Rule::Entry(EntryKind::Span(SpanQuestion::LineColumn)),
            HostCall::SpanByteRange => Rule::Entry(EntryKind::Span(SpanQuestion::ByteRange)),
            HostCall::SpanSourceText => Rule::Entry(EntryKind::Span(SpanQuestion::SourceText)),
            HostCall::SpanParent => Rule::Entry(EntryKind::Span(SpanQuestion::Parent)),
            HostCall::SpanSource => Rule::Entry(EntryKind::Span(SpanQuestion::Source)),
            HostCall::SpanJoin => Rule::Entry(EntryKind::Join),
            HostCall::SpanEdition => Rule::Entry(EntryKind::Edition),
            HostCall::Tokens | HostCall::SpanDebug => Rule::InKey,
            HostCall::Clock | HostCall::Random | HostCall::TraceAnswered => Rule::FromKey,
            HostCall::EmitDiagnostic | HostCall::Print | HostCall::Exit | HostCall::TraceRefused => {
                Rule::Value
            }
            HostCall::ExpandExpr => Rule::Refused(
                "asked the compiler to expand an expression (`TokenStream::expand_expr`), which \
                 blaze's server does not do",
            ),
            HostCall::FileWrite => Rule::Refused(
                "wrote to the file system: an expander writes nothing (rule sandbox-everything)",
            ),
            HostCall::Network => Rule::Refused(
                "reached for the network: an expander has none (rule sandbox-everything)",
            ),
            HostCall::TraceNetwork => Rule::Refused(
                "reached the network, which no read list can say: a script that needs it is \
                 given what it fetches as a pinned input instead (docs/cargo.md, \"Pinned native \
                 inputs\")",
            ),
            HostCall::TraceDescriptor => Rule::Refused(
                "used a descriptor it inherited from outside the sandbox, whose reads no trace \
                 can see",
            ),
            HostCall::TraceIncomplete => {
                Rule::Refused("left a trace that is not whole, so what it read is not known")
            }
        }
    }
}

impl fmt::Display for HostCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
