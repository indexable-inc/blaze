//! The Rust parser.
//!
//! NOTE: The crate is undergoing refactors, don't believe everything the docs
//! say :-)
//!
//! The parser doesn't know about concrete representation of tokens
//! and syntax trees. Abstract [`Input`] and [`Output`] traits are
//! used to provide tokens instead. As a consequence, this crate does
//! not contain a lexer.
//!
//! The [`Parser`] struct from the [`parser`] module is a cursor into the
//! sequence of tokens.  Parsing routines use [`Parser`] to inspect current
//! state and advance the parsing.
//!
//! The actual parsing happens in the [`grammar`] module.
//!
//! Tests for this crate live in the `syntax` crate.
//!
//! [`Parser`]: crate::parser::Parser

#![allow(rustdoc::private_intra_doc_links)]
#![deny(unsafe_code)]
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;
extern crate rustc_lexer;

pub mod cook;
mod event;
mod frontmatter;
mod grammar;
mod input;
mod lexed_str;
mod outline;
mod output;
mod parser;
mod shortcuts;
mod syntax_kind;
mod token_set;

pub use T_ as T;

#[cfg(test)]
mod tests;

pub(crate) use token_set::TokenSet;

pub use edition::Edition;

pub use crate::{
    input::{FRAGMENT, Input, JOINT, Tokens, word},
    lexed_str::LexedStr,
    outline::lazy_block_len,
    output::{OutlineStep, Output, Step},
    shortcuts::{OutlineStrStep, StrStep},
    syntax_kind::SyntaxKind,
};

/// True if `c` is whitespace in Rust source (Unicode Pattern_White_Space as in the language reference).
///
/// See <https://doc.rust-lang.org/reference/whitespace.html>.
pub fn is_rust_whitespace(c: char) -> bool {
    rustc_lexer::is_whitespace(c)
}

/// Parse the whole of the input as a given syntactic construct.
///
/// This covers two main use-cases:
///
///   * Parsing a Rust file.
///   * Parsing a result of macro expansion.
///
/// That is, for something like
///
/// ```ignore
/// quick_check! {
///    fn prop() {}
/// }
/// ```
///
/// the input to the macro will be parsed with [`PrefixEntryPoint::Item`], and
/// the result will be [`TopEntryPoint::MacroItems`].
///
/// [`TopEntryPoint::parse`] makes a guarantee that
///   * all input is consumed
///   * the result is a valid tree (there's one root node)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopEntryPoint {
    SourceFile,
    MacroStmts,
    MacroItems,
    Pattern,
    Type,
    Expr,
    /// Edge case -- macros generally don't expand to attributes, with the
    /// exception of `cfg_attr` which does!
    MetaItem,
}

impl TopEntryPoint {
    pub fn parse<'t>(&self, input: impl Into<Tokens<'t>>) -> Output {
        self.parse_with(input.into(), false)
    }

    /// As [`TopEntryPoint::parse`], as an outline: each `fn` item's body a lazy node
    /// ([`OutlineStep::Lazy`], [`lazy_block_len`]) when it can be one, else parsed in full.
    pub fn parse_outline<'t>(&self, input: impl Into<Tokens<'t>>) -> Output {
        self.parse_with(input.into(), true)
    }

    fn parse_with(&self, input: Tokens<'_>, outline: bool) -> Output {
        let _p = tracing::info_span!("TopEntryPoint::parse", ?self, outline).entered();
        let entry_point: fn(&'_ mut parser::Parser<'_>) = match self {
            TopEntryPoint::SourceFile => grammar::entry::top::source_file,
            TopEntryPoint::MacroStmts => grammar::entry::top::macro_stmts,
            TopEntryPoint::MacroItems => grammar::entry::top::macro_items,
            TopEntryPoint::Pattern => grammar::entry::top::pattern,
            TopEntryPoint::Type => grammar::entry::top::type_,
            TopEntryPoint::Expr => grammar::entry::top::expr,
            TopEntryPoint::MetaItem => grammar::entry::top::meta_item,
        };
        let mut p = match outline {
            true => parser::Parser::new_outline(input),
            false => parser::Parser::new(input),
        };
        entry_point(&mut p);
        let (events, errors) = p.finish();
        let res = event::process(events, errors);

        if cfg!(debug_assertions) {
            let mut depth = 0;
            let mut first = true;
            for step in res.steps() {
                assert!(depth > 0 || first);
                first = false;
                match step {
                    OutlineStep::Step(Step::Enter { .. }) => depth += 1,
                    OutlineStep::Step(Step::Exit) => depth -= 1,
                    OutlineStep::Step(Step::FloatSplit { ends_in_dot: has_pseudo_dot }) => {
                        depth -= 1 + !has_pseudo_dot as usize
                    }
                    OutlineStep::Step(Step::Token { .. } | Step::Error { .. })
                    | OutlineStep::Lazy { .. } => (),
                }
            }
            assert!(!first, "no tree at all");
            assert_eq!(depth, 0, "unbalanced tree");
        }

        res
    }
}

/// Parse a prefix of the input as a given syntactic construct.
///
/// This is used by macro-by-example parser to implement things like `$i:item`
/// and the naming of variants follows the naming of macro fragments.
///
/// Note that this is generally non-optional -- the result is intentionally not
/// `Option<Output>`. The way MBE work, by the time we *try* to parse `$e:expr`
/// we already commit to expression. In other words, this API by design can't be
/// used to implement "rollback and try another alternative" logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixEntryPoint {
    Vis,
    Block,
    Stmt,
    Pat,
    PatTop,
    Ty,
    Expr,
    Path,
    Item,
    MetaItem,
}

impl PrefixEntryPoint {
    pub fn parse<'t>(&self, input: impl Into<Tokens<'t>>) -> Output {
        let entry_point: fn(&'_ mut parser::Parser<'_>) = match self {
            PrefixEntryPoint::Vis => grammar::entry::prefix::vis,
            PrefixEntryPoint::Block => grammar::entry::prefix::block,
            PrefixEntryPoint::Stmt => grammar::entry::prefix::stmt,
            PrefixEntryPoint::Pat => grammar::entry::prefix::pat,
            PrefixEntryPoint::PatTop => grammar::entry::prefix::pat_top,
            PrefixEntryPoint::Ty => grammar::entry::prefix::ty,
            PrefixEntryPoint::Expr => grammar::entry::prefix::expr,
            PrefixEntryPoint::Path => grammar::entry::prefix::path,
            PrefixEntryPoint::Item => grammar::entry::prefix::item,
            PrefixEntryPoint::MetaItem => grammar::entry::prefix::meta_item,
        };
        let mut p = parser::Parser::new(input);
        entry_point(&mut p);
        let (events, errors) = p.finish();
        event::process(events, errors)
    }
}

/// A parsing function for a specific braced-block, or for one item of a source file.
#[derive(Clone, Copy)]
pub struct Reparser(fn(&mut parser::Parser<'_>));

impl Reparser {
    /// If the node is a braced block, return the corresponding `Reparser`.
    pub fn for_node(
        node: SyntaxKind,
        first_child: Option<SyntaxKind>,
        parent: Option<SyntaxKind>,
    ) -> Option<Reparser> {
        grammar::reparser(node, first_child, parent).map(Reparser)
    }

    /// One item of a source file's top level, parsed as that level parses it. The parse goes
    /// on past the item when the tokens do (an error node for a `;` after it): take the item
    /// alone with [`Output::into_first_node`].
    pub fn top_level_item() -> Reparser {
        Reparser(grammar::top_level_item)
    }

    /// Re-parse given tokens using this `Reparser`.
    ///
    /// For a block, tokens must start with `{`, end with `}` and form a valid brace
    /// sequence.
    pub fn parse<'t>(self, tokens: impl Into<Tokens<'t>>) -> Output {
        self.parse_with(parser::Parser::new(tokens))
    }

    /// As [`Reparser::parse`], as an outline ([`TopEntryPoint::parse_outline`]).
    pub fn parse_outline<'t>(self, tokens: impl Into<Tokens<'t>>) -> Output {
        self.parse_with(parser::Parser::new_outline(tokens))
    }

    fn parse_with(self, mut p: parser::Parser<'_>) -> Output {
        let Reparser(r) = self;
        r(&mut p);
        let (events, errors) = p.finish();
        event::process(events, errors)
    }
}
