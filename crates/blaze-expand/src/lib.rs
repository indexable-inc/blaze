//! Macro expansion for the item layer (docs/v2/plan/design/bottom.md, the `Expansion` and
//! `MacroDef` rows): one pure function per macro call, from the macro's definition and the
//! call's tokens to an [`Expansion`], a value that is written as a record and read in any
//! process.
//!
//! - [`MacroDef`]: what a macro is. Its kind (`macro_rules!`, `macro`, a builtin by name, a
//!   proc macro by its crate's key and export name), its tokens, edition, `#[macro_export]` and
//!   `local_inner_macros` flags, a derive's helper attributes and its transparency, and the
//!   hash of all of that, its [`MacroContent`]: never its item key (rule 21).
//! - [`Call`]: one call: its tokens and kind (fn-like, attribute with the item, derive with the
//!   item), its call-site hygiene ([`CallSite`]) and, for an eager macro re-entered, the
//!   expansions of the calls in its input ([`Eager`]).
//! - [`ExpandCx`]: every outside fact, each one asked for through it and recorded as a
//!   [`Read`]: env vars, included files, cfg outcomes, positions inside eager input, and the
//!   proc-macro runner, which lives elsewhere (no Wasmtime here).
//! - [`expand`]: the function. It gives an [`Expanded`]: the [`Expansion`], or the nested calls
//!   an eager macro needs expanded first ([`Expanded::NeedsExpanded`]).
//! - [`call_key`] and [`expansion_key`]: a call's key before the work, and the record's key
//!   with the reads it made, so that equal calls share one record.
//!
//! # Positions (rule 71)
//!
//! Nothing here holds a byte offset. A token of an expansion names where it came from by
//! [`Origin`]: an index into the call's input, the attribute's arguments, the definition's
//! body or parameters, or made by the expansion with a transparency. `line!()`, `column!()`,
//! `file!()` and `module_path!()` outside eager input expand to a placeholder token and a
//! [`Hole`] naming it; the reader that places the expansion fills it from the outermost call's
//! position when it prints or lowers it, as rustc's `expansion_cause` does. Inside eager
//! input (`concat!(line!())`) the value is asked of the [`ExpandCx`] and recorded as a read, so
//! the record is keyed by it.
//!
//! # Hygiene
//!
//! A token's [`Ctx`] is a context of its stream's own table ([`CtxNode`]): a chain of
//! [`Mark`]s over a root of an edition. A mark names the expansion it is of ([`MarkOf::This`]
//! for the record's own, [`MarkOf::Outer`] by key for an earlier one), its transparency, its
//! edition and its definition's crate and module. That is what `$crate`, def-site `self` and
//! local variable hygiene are resolved by; nothing is an interned id or an arena index.
//!
//! # The old database
//!
//! mbe's matcher and transcriber and hir-expand's builtin derive writers are reused as they
//! are. They take a `&base_db::Db` they only pass down and never read; [`ExpandCx::host_db`]
//! hands one in until they drop the parameter.

#![feature(allocator_api)]

mod bridge;
mod builtin;
mod call;
mod cx;
mod declarative;
mod def;
mod derive;
mod error;
mod expand;
mod expansion;
mod key;
mod limit;
mod literal;
mod nested;
mod tokens;

pub use crate::{
    call::{Call, CallKind, CallShape, CallSite, Eager, Inside},
    cx::{
        ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, Place, PlaceOf, ProcCall,
        Produced,
    },
    def::{
        BuiltinAttr, BuiltinDerive, BuiltinFn, DefAt, Export, LocalInner, MacroContent, MacroDef,
        MacroKind, ProcKind,
    },
    error::{ExpandError, LexError, StreamError, Stream, Table, Unbuilt},
    expand::{Prepared, expand, expand_prepared, prepare},
    expansion::{
        Diagnostic, Expanded, Expansion, Hole, HoleOf, IncludeFailure, NestedCall, NestedKind,
        Origin, PathScope, Range, Read, ReadAnswer, ReadKind, Reads,
    },
    key::{CallKey, ExpansionKey, call_key, expansion_key},
    limit::{Depth, LimitReached, RecursionLimit},
    nested::find_calls,
    tokens::{
        Ctx, CtxNode, DefSite, Delim, Edition, LitKind, Mark, MarkOf, Raw, Sink, Spacing, Text,
        Token, TokenHygiene, TokenKind, TokenStream, Transparency, lex,
    },
};
