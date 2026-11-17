//! An expansion's token record (docs/v2/plan/design/drive.md, D122 "An expansion is its token
//! record"): what a macro call's output is, written once by its expander and read in place by
//! every reader, with no text between them.
//!
//! Per token the record holds the parser's word ([`Tokens::word`]: kind, contextual kind,
//! edition and the parser's joint, fragment and extension bits), whether it is joined to the next
//! token ([`Tokens::is_joined`], its own bit column: the parser's joint bit is not the same fact),
//! its lexeme by number into the record's table of distinct lexemes in first-seen order
//! ([`Tokens::lexeme`]), its hygiene context ([`Tokens::ctx`], a root or a node of the record's
//! context table) and its origin by token index ([`Tokens::origin`]). Invisible groups (a
//! `macro_rules!` fragment passed on) are a column of [`TokenRange`]s.
//!
//! A position inside an expansion is a [`TokenRange`]; a reader slices the record
//! ([`Tokens::slice`]) and hands the parser its words ([`Tokens::words`],
//! [`Tokens::parser_tokens`], [`Tokens::copy_words_into`]): nothing lexes an expansion. The one
//! digest of expansion-made things is [`Tokens::digest`]; the one rendering, for a person only,
//! is [`Tokens::shown`].
//!
//! Its own crate, below the writer (blaze-expand) and every reader (blaze-file-items,
//! blaze-lower, blaze-scope, blaze-ladder), so no reader depends on the expander.

#![no_std]
#![feature(allocator_api)]

#[cfg(test)]
extern crate std;

mod build;
mod form;
mod view;

#[cfg(test)]
mod tests;

pub use crate::build::{BuildError, NewToken, TokensBuilder, TokensBytes};
pub use crate::form::{Ctx, CtxNode, FORM, Lexeme, Mark, MarkOf, Origin, TOKENS_SCHEMA, TokenRange};
pub use crate::view::{Column, DIGEST_DOMAIN, OpenError, Tokens};
