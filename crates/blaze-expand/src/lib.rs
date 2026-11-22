//! # For a successor
//!
//! State on 2026-11-16, for whoever takes this crate next:
//!
//! - `macro_rules!` runs on this crate's own engine (`rules/`): a definition is compiled once
//!   (`rules::compile`, at [`prepare`]) to flat arrays with jump targets, matched and
//!   transcribed on explicit stacks, bounded by `STEP_BOUND` and `DEPTH_BOUND`
//!   ([`ExpandError::RulesBound`]). mbe, tt, span, intern and syntax-bridge are gone.
//! - The gate: `tests/rules_diff.rs` (Rust's defined cases against renderings recorded from
//!   mbe, planted bugs) and `examples/rules_core.rs --check
//!   examples/rules_compare/library-nightly-2026-10-27.digests` (every call in core, alloc,
//!   std: 0 differ). Re-record only for a change you can explain token by token.
//! - Open: the compiled `rules::Program` is held in [`Prepared`] in memory, not yet a stored
//!   record keyed by the definition's bytes; the output tables grow by reallocation and
//!   `find_calls` runs after the write (`Out::finish`): size from a counting pass and index
//!   calls as written. Cost now: 1.83x the floor's time, 1.48x its bytes.
//! - Body-position calls (expression, statements, pattern, type) are being wired into
//!   blaze-lower; see that crate's doc for where that stands.
//!
//! Macro expansion for the item layer (docs/v2/plan/design/bottom.md, the `Expansion` and
//! `MacroDef` rows): one pure function per macro call, from the macro's definition and the
//! call's tokens to an [`Expansion`], a value that is written as a record and read in any
//! process.
//!
//! - [`MacroDefinition`]: what a macro is. Its kind (`macro_rules!`, `macro`, a builtin by
//!   name, a proc macro by its crate's key and export name), its tokens, edition,
//!   `#[macro_export]` and `local_inner_macros` flags, a derive's helper attributes and its
//!   transparency, and the hash of all of that, its [`MacroContent`]: never its item key (rule
//!   21). Stored as blaze-surface's `MacroDef` record and its token streams' records
//!   ([`MacroDefinition::write`], [`MacroDefinition::from`]).
//! - [`Call`]: one call: its tokens and kind (fn-like, attribute with the item, derive with the
//!   item and its [`DeriveFacts`]), its call-site hygiene ([`CallSite`]) and, for an eager macro re-entered, the
//!   expansions of the calls in its input ([`Eager`]).
//! - [`ExpandCx`]: every outside fact, each one asked for through it and recorded as a
//!   [`Read`]: env vars, included files, cfg outcomes, positions inside eager input, and the
//!   proc-macro runner, which lives elsewhere (no Wasmtime here).
//! - [`expand`]: the function. It gives an [`Expanded`]: the [`Expansion`], or the nested calls
//!   an eager macro needs expanded first ([`Expanded::NeedsExpanded`]).
//! - [`call_key`] and [`expansion_key`]: a call's key before the work, and the record's key
//!   with the reads it made, so that equal calls share one record.
//! - [`Expansion::write_record`] and [`Expansion::read_record`]: the record itself, one
//!   blaze-format buffer of columns ([`RecordColumn`]), and its value read back, checked once.
//!
//! # Positions (rule 71)
//!
//! Nothing here holds a byte offset. A token of an expansion names where it came from by
//! [`TokenOrigin`]: an index into the call's input, the attribute's arguments, the definition's
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
//! # What is reused
//!
//! The lexer's kinds and the parser's fragment entry points (`expr`, `ty`, `pat`, ...) are
//! rust-analyzer's `parser` crate. Everything else is written here over the portable form:
//! `macro_rules!` and `macro` are compiled once into a matching program (`rules`), the builtin
//! derives and function-like macros write token to token, and a literal's escapes are read
//! here too. No token tree, interner, database or syntax tree is involved.

#![no_std]
#![feature(allocator_api)]

mod adt;
pub mod atoms;
mod attr_path;
mod attr_value;
mod body;
mod builtin;
mod call;
mod cfg;
mod cx;
mod def;
mod derive;
mod error;
mod expand;
mod expansion;
mod extent;
#[cfg(test)]
mod extent_tests;
mod failed;
mod global_allocator;
mod item;
pub mod kept;
pub mod kept_set;
mod key;
mod limit;
mod literal;
mod nested;
pub mod origin_column;
#[cfg(test)]
mod origin_column_tests;
mod record;
mod rules;
pub mod seen;
mod token_record;
mod tokens;
mod writes;
#[cfg(test)]
mod writes_tests;

pub use crate::writes::{AnyWhy, By, CallWrites, Callee, Callees, MOST_NESTED, MOST_WALK_DEPTH, MacroWrites, Nested, Written, WrittenNs, macro_writes};
pub use crate::attr_path::{PathWord, PathWords, path_words, same_path};
pub use crate::attr_value::{AttrLiteralError, MOST_ATTR_VALUE_DEPTH, attr_literal};
pub use crate::rules::{CopyPlant, rules_copy_plant, rules_oracle, rules_oracle_counts};
pub use crate::{
    body::{
        BodyWord, BodyWords, Fragment, ParserWords, body_words, glued_in_text, local_hygiene,
        parser_words, write_token, write_tokens,
    },
    call::{AlsoCopy, Call, CallKind, CallShape, CallSite, Constness, DeriveFacts, Eager, Inside},
    cfg::{CfgAttrError, CfgPredicateError, Tri, cfg_attr_metas, cfg_predicate, cfg_predicate_at, cfg_predicate_tri},
    derive::{attr_call, derive_attr, derive_meta},
    cx::{
        ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, OutsideRoot, Place, PlaceOf,
        ProcCall, ProcDiagnostic, ProcLimit, Produced, ReadRoot, SourceOrigin, include_target,
        is_root_relative,
    },
    def::{
        BuiltinAttr, BuiltinDerive, BuiltinFn, DefAt, DefinitionError, DefinitionRecords, Export,
        LocalInner, MacroContent, MacroDefinition, MacroKind, ProcKind,
    },
    error::{ExpandError, LexError, RulesLimit, Stream, StreamError, Table, Unbuilt},
    failed::Failed,
    extent::{Extent, ExtentError, ExtentWhy},
    expand::{Prepared, expand, expand_prepared, prepare},
    expansion::{
        Diagnostic, Expanded, Expansion, Held, Hole, HoleOf, IncludeFailure, NestedCall, NestedKind,
        PathScope, ProcLevel, Range, Read, ReadAnswer, ReadKind, Reads, TokenOrigin,
    },
    item::{AttrCall, DeriveUse, Derives, ItemAttrs, ItemAttrsError, NextAttr},
    key::{
        CallKey, ExpansionKey, call_key, call_key_of, expansion_key, kept_key,
    },
    limit::{Depth, LimitReached, RecursionLimit},
    nested::find_calls,
    record::{
        EXPANSION_SCHEMA, RecordBytes, RecordColumn, RecordError, STREAM_SCHEMA, expansion_tokens,
        stream_record_id,
    },
    token_record::{OriginMaps, SourceIndexes, TokenRecordError, record_indexes, token_record},
    tokens::{
        Ctx, CtxNode, DefSite, Delim, Edition, LitKind, Mark, MarkOf, Raw, Sink, Spacing, Text,
        Token, TokenHygiene, TokenKind, TokenStream, Transparency, lex, lex_inside, lex_marked,
    },
};
