//! The syntax tree of a `.lark` file, mirroring the shape of the trees
//! Lark's own grammar parser builds (`expansions`, `expansion`, `expr`,
//! `value`, …) closely enough that the loader logic ports directly.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Whether a literal is a plain string (`"..."`, matched verbatim) or a
/// regular expression (`/.../`, or a `".."` range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternKind {
    Str,
    Re,
}

/// A terminal pattern: Lark's `PatternStr` / `PatternRE`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pattern {
    pub kind: PatternKind,
    /// The processed value: the string to match for `Str`, the regex
    /// source for `Re`.
    pub value: String,
    /// The regex flags, each a letter from `imslux`, sorted and
    /// deduplicated.
    pub flags: String,
}

impl Pattern {
    pub fn str(value: impl Into<String>) -> Self {
        Pattern {
            kind: PatternKind::Str,
            value: value.into(),
            flags: String::new(),
        }
    }

    pub fn re(value: impl Into<String>) -> Self {
        Pattern {
            kind: PatternKind::Re,
            value: value.into(),
            flags: String::new(),
        }
    }

    pub fn with_flags(mut self, flags: &str) -> Self {
        let mut chars: Vec<char> = flags.chars().collect();
        chars.sort_unstable();
        chars.dedup();
        self.flags = chars.into_iter().collect();
        self
    }
}

/// A symbol reference in a rule or terminal body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Symbol {
    Terminal(String),
    Rule(String),
}

impl Symbol {
    pub fn name(&self) -> &str {
        match self {
            Symbol::Terminal(n) | Symbol::Rule(n) => n,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Symbol::Terminal(_))
    }
}

/// One alternative of an [`Expr::Alternatives`]: a sequence of items and
/// its optional `-> alias`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Alternative {
    pub items: Vec<Expr>,
    pub alias: Option<String>,
}

/// An expression in a rule or terminal body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    /// `a | b | c`, also the body of a parenthesized group.
    Alternatives(Vec<Alternative>),
    Symbol(Symbol),
    Literal(Pattern),
    /// `x?`, `x*`, `x+`, `x ~ n`, `x ~ n..m` and `[x]`: between `min` and
    /// `max` repetitions of `inner`, unbounded when `max` is `None`.
    Repeat {
        inner: Box<Expr>,
        min: u32,
        max: Option<u32>,
    },
    /// `name{arg, ...}`.
    Template {
        name: String,
        args: Vec<Expr>,
    },
}

impl Expr {
    /// Whether this is exactly one alternative with no items.
    pub fn is_single_empty(&self) -> bool {
        matches!(self, Expr::Alternatives(alts) if alts.len() == 1 && alts[0].items.is_empty())
    }
}

/// Modifiers in front of a rule name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleModifiers {
    /// `?rule`: inline the rule's node when it has one child.
    pub expand1: bool,
    /// `!rule`: keep all tokens in the tree.
    pub keep_all_tokens: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDef {
    pub name: String,
    pub params: Vec<String>,
    pub priority: Option<i64>,
    pub modifiers: RuleModifiers,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermDef {
    pub name: String,
    pub priority: i64,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Definition {
    Rule(RuleDef),
    Terminal(TermDef),
}

/// What an `%import` brings in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportNames {
    /// `%import a.b.NAME` or `%import a.b.NAME -> ALIAS`.
    Single { name: String, alias: Option<String> },
    /// `%import a.b (X, y, Z)`.
    Multi(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportStmt {
    /// The dotted path of the grammar to import from, without the final
    /// name for a single import.
    pub path: Vec<String>,
    /// `%import .path` (relative to the importing grammar) rather than
    /// `%import path` (from the import paths); the bundled grammars are
    /// the last resort for both.
    pub relative: bool,
    pub names: ImportNames,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Define(Definition),
    Override(Definition),
    Extend(Definition),
    Ignore(Expr),
    Declare(Vec<Symbol>),
    Import(ImportStmt),
}
