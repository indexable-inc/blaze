//! The items declared in the blocks of ONE body-like text (a `fn`, `const` or `static` item's
//! whole text), found by tokens: no tree, no lowering.
//!
//! The Reference, "Statements", "Item declarations": "An item declaration statement has a
//! syntactic form identical to an item declaration within a module. Declaring an item within a
//! statement block restricts its scope to the block containing the statement." An `impl` among
//! them is not a name, so it holds for its type in the whole program: the names library asks
//! this scan which bodies write one (blaze-scope resolve.rs `make_blocks`).
//!
//! # What counts
//!
//! A statement starts after a block's `{`, after a `;` and after a `}` directly in a brace
//! group. An item is a statement that starts (after outer attributes and a visibility) with an
//! item's keywords; it ends at its first `;` outside brackets or, for the forms with a body,
//! at the `}` closing its first `{` outside brackets. Nothing inside an item is listed: its own
//! bodies are other texts, scanned when they are lowered. One bit is read from a local `fn`'s,
//! `const`'s or `static`'s own text, scanned the same way: whether it writes a block
//! ([`NestedItem::body_writes`]), since then the names library must make the item a module's
//! item to give its body's blocks modules. A word inside a string or a comment
//! is one token, and the tokens of a macro call's arguments are skipped whole, so `impl`
//! there is never an item; a call in statement position is listed ([`Nested::calls`]): it may
//! write items, and only its expansion can say.
//!
//! A brace group in the body that is not a block (a struct literal, a `match`'s arms) starts
//! no statement with an item keyword (a field or a pattern is never one; `const { .. }` is told
//! apart by its `{`), so every brace group is scanned alike.

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};
use blaze_surface::Range;

/// One brace group of the body: the group around it, and where its braces are in the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub parent: Option<u32>,
    /// The outer attributes of the statement of its parent block it is written in: before it
    /// (`#[cfg(feature = "tracing")] { .. }`) or before the statement holding it
    /// (`#[cfg(x)] let s = { .. };`, `#[cfg(x)] f(|| { .. });`); empty at [`Block::open`] when
    /// none are. The Reference, "Statements" and "Expression attributes": a statement's outer
    /// attributes, `cfg` among them, apply to the whole statement, so a false `cfg` removes
    /// every block in it.
    pub attrs: Range,
    /// The offset of its `{`.
    pub open: u32,
    /// The offset after its `}`.
    pub close: u32,
}

/// What an item statement is, by its keywords.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NestedKind {
    Impl,
    Use,
    ExternCrate,
    Fn,
    Const,
    Static,
    Struct,
    Enum,
    Union,
    Trait,
    TypeAlias,
    Mod,
    MacroRules,
    /// `macro name(..) { .. }` or `macro name { .. }` (`decl_macro`; std's `macro spec_str`
    /// in `OsString::push`'s body, called right after it).
    Macro,
    ExternBlock,
}

/// An item statement: its block and its text's range (outer attributes included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedItem {
    pub block: u32,
    pub kind: NestedKind,
    pub range: Range,
    /// For a `fn`, `const` or `static`: whether its own text, scanned the same way, writes a
    /// block ([`Nested::writes`]): an item that makes a module, a statement call, or such an
    /// item in turn, at any depth. std's `sys::pal::unix::init` declares `unsafe fn
    /// reset_sigpipe(..)`, whose `#[cfg(..)]` block declares `mod sigpipe { pub const DEFAULT:
    /// u8 = 0; .. }` and matches on `sigpipe::DEFAULT`. The Reference, "Statements", "Item
    /// declarations": the `mod`'s scope is that block, whatever item's body holds it; so the
    /// fn's block is written too ([`NestedItem::writes`]), the fn is an item of that block's
    /// module with the library's key, and its own body's blocks are modules under it.
    /// `false` for every other kind.
    pub body_writes: bool,
    /// Whether this item is seen outside its body (D189, point 1: [`Nested::surface_by_text`]):
    /// an `impl` (it holds for its type in the whole program: the Reference, "Implementations"),
    /// an extern block (its items link by name), an outer attribute outside
    /// [`INERT_ATTRS`] (a derive or an attribute macro writes items the body does not hold in
    /// its text; `macro_export` puts a macro at the crate's root; `no_mangle`, `export_name`,
    /// `used`, `lang`, `panic_handler`, `global_allocator` name the item to the link or to the
    /// language), or an item whose own text (a local `fn`'s body, a block `mod`'s or a
    /// trait's items), scanned the same way, holds one. A `macro_rules!` or `macro` is not read
    /// inside: what it writes is its calls' business (the surface pass, D189 point 4).
    pub surface: bool,
}

impl NestedItem {
    /// Whether this item makes the block it is in written as a module of the names library:
    /// it makes one itself ([`NestedKind::makes_module`]), or it is a `fn`, `const` or
    /// `static` whose own body writes a block ([`NestedItem::body_writes`]).
    pub fn writes(&self) -> bool {
        self.kind.makes_module() || self.body_writes
    }
}

/// How many item bodies deep [`scan`] reads a `fn`'s, `const`'s or `static`'s own text for
/// [`NestedItem::body_writes`]. An item deeper than this is taken to write its block: the
/// names library then writes the blocks down to its own depth limit and files a note there
/// (blaze-scope resolve.rs `MAX_BLOCK_DEPTH`, `MadeNote::TooMany`), never drops a module.
const MAX_BODY_DEPTH: u16 = 64;

/// A macro call in a block: its innermost block, the range of its path (`a::b` of
/// `a::b!(..)`, after any outer attributes written before it) and the range of the whole call
/// (from its outer attributes at a statement's start, or its path, through the bracket closing
/// its arguments; a `;` after it is not in it). Not expanded here: the items it may write are
/// not in [`Nested::items`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedCall {
    pub block: u32,
    pub path: Range,
    pub range: Range,
    /// The call stands in an expression (`let x = m!()`, `f(m!())`), not at a statement's
    /// start. The Reference, "Macro invocation": an expression call's expansion is an
    /// expression, so it declares no item of `block` itself: an item it writes is in a block
    /// expression of its own (tokio's `select!`), never beside the block's other items.
    pub expr: bool,
    /// The outer attributes of the statement an expression call stands in (`#[cfg(x)]` of
    /// `#[cfg(x)] let d = m!(..);`); empty at [`NestedCall::path`]'s start when it has none,
    /// and always for a statement call, whose [`NestedCall::range`] holds its own. The
    /// Reference, "Statements": a statement's outer attributes apply to the whole statement,
    /// and "Conditional compilation": a false `cfg` removes it, every call in it with it,
    /// before expansion.
    pub attrs: Range,
}

/// How the scan ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NestedEnd {
    /// Every bracket closed.
    Whole,
    /// The text ended inside a bracket, or closed one it did not open: the parser's error to
    /// report; nothing of this scan is to be used.
    Unbalanced,
}

/// A body's blocks in the order their `{` is written, and the items and statement calls in
/// them in text order.
#[derive(Debug)]
pub struct Nested<'a> {
    pub blocks: AVec<'a, Block>,
    pub items: AVec<'a, NestedItem>,
    pub calls: AVec<'a, NestedCall>,
    pub end: NestedEnd,
    /// D189, point 1: whether the body's TEXT alone shows something seen outside it (an item
    /// with [`NestedItem::surface`]); a body with it is a surface body whatever its calls are.
    /// Its calls are not read here: the surface pass asks each call's definition
    /// (`MacroEffect`) and its input. True for an [`NestedEnd::Unbalanced`] scan (its parse
    /// error is reported; nothing is taken to be inside only).
    pub surface_by_text: bool,
}

impl Nested<'_> {
    /// Whether any block holds an `impl`.
    pub fn has_impl(&self) -> bool {
        self.items.iter().any(|i| i.kind == NestedKind::Impl)
    }

    /// Whether any block holds an item that makes it a scope record
    /// ([`NestedKind::makes_module`]).
    pub fn makes_module(&self) -> bool {
        self.items.iter().any(|i| i.kind.makes_module())
    }

    /// Whether any block of the body is written as a module of the names library: one holds
    /// an item that writes it ([`NestedItem::writes`]: an item that makes a module, or a
    /// local `fn`, `const` or `static` whose own body writes one) or a statement call.
    pub fn writes(&self) -> bool {
        !self.calls.is_empty() || self.items.iter().any(NestedItem::writes)
    }
}

impl NestedKind {
    /// Whether an item of this kind makes its block a scope record of the names library (an
    /// unnamed module of the item's module): an `impl` (it holds for its type in the whole
    /// program, and its header is read in the block), and a struct, enum, union or type alias
    /// (the Reference, "Statements", "Item declarations": "Declaring an item within a
    /// statement block restricts its scope to the block containing the statement": a type
    /// with a head of its own, named by the bodies and impls of that block), and a trait (the
    /// same: a trait of the block's module, with its head, its impls in the block filed under
    /// its one key, and its methods candidates at method calls in the block, where it is in
    /// scope: core's `Write::write_fmt` declares `trait SpecWriteFmt` and its impls in its
    /// body). A block's `fn`, `const`, `static` and `use` alone make none: the body's own
    /// lowering resolves them. A `mod` and an `extern crate` make one too (the same quote: a
    /// module declared in a block, `mod __tokio_select_util { .. }`, and a crate bound in a
    /// block, `extern crate alloc as __alloc;`, are names of that block, and a path through
    /// them continues into what they hold: only the name solver at a module reads that).
    pub fn makes_module(self) -> bool {
        matches!(
            self,
            NestedKind::Impl | NestedKind::Struct | NestedKind::Enum | NestedKind::Union | NestedKind::TypeAlias | NestedKind::Trait | NestedKind::Mod | NestedKind::ExternCrate
        )
    }
}

/// An open bracket of the body.
#[derive(Clone, Copy)]
enum Open {
    /// A brace group: its entry in [`Nested::blocks`].
    Brace(u32),
    /// A paren or square group; `stmt`: an attribute's or a visibility's, after which the
    /// statement is still at its start.
    Other { stmt: bool },
    /// A macro call's arguments.
    Call,
}

/// The item being skipped: nothing inside it is read.
#[derive(Clone, Copy)]
struct Skip {
    kind: NestedKind,
    start: u32,
    block: u32,
    /// Ends at the `}` of its first top-level `{` (else only at a `;`).
    body: bool,
    depth: u32,
    in_body: bool,
    /// The offset after the `}` that closed its first top-level `{`: the item ends there
    /// unless the next token says the group was a const argument (`Tr<{ N }>`).
    closed: Option<u32>,
}

/// The keywords read so far of a statement that may be an item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lead {
    None,
    /// `unsafe`, `default`, `async`, `auto`, `safe`: an item only if an item keyword follows.
    Qualifier,
    /// `extern`, then perhaps an ABI string: `crate`, `fn`, or a block.
    Extern,
    /// `const`: a name (an item), `fn`/`trait`/`impl` and their qualifiers, or `{` (an inline const).
    Const,
    /// `static`: an item unless a closure follows.
    Static,
    /// `union`: an item when a name follows.
    Union,
    /// `macro_rules`: an item when `!` follows.
    MacroRules,
}

fn has_body(kind: NestedKind) -> bool {
    !matches!(kind, NestedKind::Use | NestedKind::ExternCrate | NestedKind::Const | NestedKind::Static | NestedKind::TypeAlias)
}

/// The item a keyword starts on its own.
fn item_word(word: &str) -> Option<NestedKind> {
    Some(match word {
        "impl" => NestedKind::Impl,
        "use" => NestedKind::Use,
        "fn" => NestedKind::Fn,
        "struct" => NestedKind::Struct,
        "enum" => NestedKind::Enum,
        "trait" => NestedKind::Trait,
        "type" => NestedKind::TypeAlias,
        "mod" => NestedKind::Mod,
        // A keyword in every edition (the Reference, "Keywords", reserved): never a plain name.
        "macro" => NestedKind::Macro,
        _ => return None,
    })
}

fn lead_word(word: &str) -> Lead {
    match word {
        "unsafe" | "default" | "async" | "auto" | "safe" => Lead::Qualifier,
        "extern" => Lead::Extern,
        "const" => Lead::Const,
        "static" => Lead::Static,
        "union" => Lead::Union,
        "macro_rules" => Lead::MacroRules,
        _ => Lead::None,
    }
}

/// A keyword an expression's `!` may follow (`if !(a)`): never a macro's name.
pub(crate) fn not_a_macro(word: &str) -> bool {
    matches!(word, "if" | "while" | "match" | "return" | "in" | "break" | "yield" | "else" | "move" | "mut" | "ref")
}

/// Whether `rest`, the text right after a `$`, is the word `crate` (a `$crate` path's first
/// segment: [`scan`] lists a statement call whose path starts with it).
fn dollar_crate(rest: &str) -> bool {
    rest.strip_prefix("crate").is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("an item's text is counted in u32")
}

/// Whether the text can write a block module or contain a statement macro call.
pub fn may_write_block(text: &str) -> bool {
    const WORDS: [&str; 8] = ["impl", "struct", "enum", "union", "type", "trait", "mod", "extern"];
    text.contains('!') || WORDS.into_iter().any(|word| text.contains(word))
}

/// The words that begin an item (the Reference, "Items"), as a token of a text: an item a
/// macro writes into a block, or that its caller hands in, begins with one of them.
pub(crate) const ITEM_WORDS: [&str; 14] =
    ["mod", "type", "fn", "struct", "enum", "union", "impl", "use", "const", "static", "trait", "macro_rules", "macro", "extern"];

/// Whether a macro's text may write an item: a `macro_rules!`'s rules or a `macro`'s
/// parameters and body (its definition), or a call's input (what the caller hands in, which a
/// transcriber may place). True when a token of it is an item's first word ([`ITEM_WORDS`]) or
/// a word, `!` and a bracket: a macro call, whose expansion may write anything (tokio's
/// `select!` writes `$crate::select!(@{ .. })`, and only that call's expansion the `mod`). A
/// superset: a matcher's literal `fn`, a `const { .. }` block, a `!` of a call that writes no
/// item each make it true; never false for a text that writes one. So a call in a block whose
/// definition and input are both false declares nothing there (`vec![1]`: alloc's `vec!`
/// writes paths and calls of functions only), and the names library need not expand it
/// (blaze-scope resolve.rs `expand_call`). One read of the text's tokens; the names library
/// keeps a definition's answer by its key (blaze-scope prepares.rs `Prepares::may_declare`).
/// It is [`may_declare_through`] with every `$crate` call's callee taken to write an item.
pub fn may_declare(text: &str) -> bool {
    may_declare_through(text, &mut |_| true)
}

/// The most segments of a `$crate` path [`may_declare_through`] reads after `$crate`; a call
/// through a longer one is taken to write an item (rule 18: a bound, never a guess of "no").
const MAX_PATH_SEGMENTS: usize = 16;

/// Where [`may_declare_through`] is in a text's tokens.
#[derive(Clone, Copy)]
enum Through {
    Idle,
    /// After a word that may name a macro (`m`, `a::m`'s `m`, a metavariable's `m`).
    Word,
    /// After that word's `!`.
    Bang,
    /// After a `$`.
    Dollar,
    /// After `$crate` and this many segments of its path: a `::` or the call's `!` may follow.
    Path(usize),
    /// After the first `:` of a `::` in a `$crate` path.
    Colon(usize),
    /// After a `::` of a `$crate` path: a segment follows.
    Sep(usize),
    /// After a `$crate` path's `!`.
    PathBang(usize),
    /// After a `#` ([`effect_through`] alone).
    Pound,
    /// After a `)`: it may close a repetition (`$($m)+`) whose operator ends a macro's path.
    Close,
    /// After a `)` and the first `:` of a `::` separator (`$($seg)::+`).
    CloseColon,
    /// After a `)` and a `::` separator.
    CloseSep,
    /// After `#[`.
    AttrOpen,
    /// Reading an attribute's path: its first segment's range and how many segments so far.
    AttrPath { first: (usize, usize), segments: u32 },
}

/// What a macro's expansion may do beyond the body it is called in (D189, point 3): one
/// record per macro DEFINITION, keyed by the definition (blaze-scope `macro_effect`).
/// `emits_surface`: it may write something seen outside the body ([`NestedItem::surface`]: an
/// `impl`, an extern block, an item under an attribute that is not inert). `binds_at_top`: it
/// may bind, at the top of the block it is called in, a name a later call's path starts with
/// (a `use`, a `macro_rules!` or `macro`, a `mod`, an `extern crate`), so the calls after it
/// resolve only once it is expanded. Both are supersets: never false for an expansion that
/// does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacroEffect {
    pub emits_surface: bool,
    pub binds_at_top: bool,
}

impl MacroEffect {
    /// Writes nothing seen outside, binds nothing.
    pub const NONE: MacroEffect = MacroEffect { emits_surface: false, binds_at_top: false };
    /// Not known to do less: a proc macro, a call resolved where the macro is called.
    pub const ANY: MacroEffect = MacroEffect { emits_surface: true, binds_at_top: true };

    /// Either's effect.
    #[must_use]
    pub fn or(self, other: MacroEffect) -> MacroEffect {
        MacroEffect { emits_surface: self.emits_surface || other.emits_surface, binds_at_top: self.binds_at_top || other.binds_at_top }
    }
}

/// The words whose item [`MacroEffect::emits_surface`] (an `impl`; `extern`: an extern block,
/// or `extern "C" fn`, read as one: a superset).
const SURFACE_WORDS: [&str; 2] = ["impl", "extern"];

/// The words whose item may bind a name a later call's path starts with
/// ([`MacroEffect::binds_at_top`]): the Reference, "Macros By Example", "Scoping, exporting,
/// and importing" (`macro_rules!` textual scope), "Use declarations", "Modules", "Extern
/// crate declarations".
const BINDING_WORDS: [&str; 5] = ["use", "macro_rules", "macro", "mod", "extern"];

/// [`MacroEffect`] of a macro definition's text (a `macro_rules!`'s rules, a `macro`'s
/// parameters or body): its own words ([`SURFACE_WORDS`], [`BINDING_WORDS`], an attribute
/// outside [`INERT_ATTRS`], or one whose path is a metavariable: `#[$m]`), joined with each
/// call it makes. A call through a `$crate` path asks `callee` with the path's segments after
/// `$crate`: `$crate` "refers to the crate defining the macro" (the Reference, "Macros by
/// example", "Hygiene"), so the caller resolves it there and answers that definition's effect.
/// A call by any other path (`m!`, `a::m!`) or through a metavariable (`$m!`) resolves where
/// the macro is called: [`MacroEffect::ANY`]. One read of the tokens; it stops once the effect
/// is [`MacroEffect::ANY`].
pub fn effect_through<'t>(text: &'t str, callee: &mut dyn FnMut(&[&'t str]) -> MacroEffect) -> MacroEffect {
    let mut effect = MacroEffect::NONE;
    let mut segments: [&'t str; MAX_PATH_SEGMENTS] = [""; MAX_PATH_SEGMENTS];
    let mut state = Through::Idle;
    let mut at = 0usize;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        if effect == MacroEffect::ANY {
            return effect;
        }
        let start = at;
        at += token.len as usize;
        let kind = token.kind;
        if matches!(kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        let open = matches!(kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace);
        let word = matches!(kind, TokenKind::Ident | TokenKind::RawIdent);
        state = match (state, kind) {
            (Through::Pound, TokenKind::OpenBracket) => Through::AttrOpen,
            // `#!` (an inner attribute) or a `#` not starting an attribute: not read, so taken
            // to emit (rule 3).
            (Through::Pound, _) => {
                effect.emits_surface = true;
                Through::Idle
            }
            (Through::AttrOpen, _) if word => Through::AttrPath { first: (start, at), segments: 1 },
            // `#[$m]`: the caller's attribute.
            (Through::AttrOpen, _) => {
                effect.emits_surface = true;
                Through::Idle
            }
            (Through::AttrPath { first, segments }, TokenKind::Colon) => Through::AttrPath { first, segments },
            (Through::AttrPath { first, segments }, _) if word => Through::AttrPath { first, segments: segments + 1 },
            (Through::AttrPath { first, segments }, _) => {
                if !inert_attr(&text[first.0..first.1], segments) {
                    effect.emits_surface = true;
                }
                Through::Idle
            }
            (now, _) if word => {
                let w = &text[start..at];
                effect.emits_surface |= SURFACE_WORDS.contains(&w);
                effect.binds_at_top |= BINDING_WORDS.contains(&w);
                match now {
                    Through::Dollar if w == "crate" => Through::Path(0),
                    Through::Sep(n) if n < MAX_PATH_SEGMENTS => {
                        segments[n] = w;
                        Through::Path(n + 1)
                    }
                    Through::Sep(_) => return MacroEffect::ANY,
                    _ if not_a_macro(w) => Through::Idle,
                    _ => Through::Word,
                }
            }
            (Through::Path(n), TokenKind::Colon) => Through::Colon(n),
            (Through::Colon(n), TokenKind::Colon) => Through::Sep(n),
            (Through::Word | Through::Path(0), TokenKind::Bang) => Through::Bang,
            (Through::Path(n), TokenKind::Bang) => Through::PathBang(n),
            (Through::Bang, _) if open || kind == TokenKind::Dollar => return MacroEffect::ANY,
            (Through::PathBang(n), _) if open || kind == TokenKind::Dollar => {
                effect = effect.or(callee(&segments[..n]));
                if kind == TokenKind::Dollar { Through::Dollar } else { Through::Idle }
            }
            // `$($m)+! { .. }` (evm2's `for_each_spec!`): a repetition's `)`, an optional `::`
            // separator and its operator end a macro's path as a word does (a superset: `(a) *
            // !(b)` reads as a call too).
            (_, TokenKind::CloseParen) => Through::Close,
            (Through::Close, TokenKind::Colon) => Through::CloseColon,
            (Through::CloseColon, TokenKind::Colon) => Through::CloseSep,
            (Through::Close | Through::CloseSep, TokenKind::Star | TokenKind::Plus | TokenKind::Question) => Through::Word,
            (_, TokenKind::Dollar) => Through::Dollar,
            (_, TokenKind::Pound) => Through::Pound,
            _ => Through::Idle,
        };
    }
    effect
}

/// [`may_declare`] of a macro definition's text (a `macro_rules!`'s rules, a `macro`'s
/// parameters or body), with each call the text makes through a `$crate` path asked of
/// `callee`, given the path's segments after `$crate` (the last is the called macro's name).
/// The Reference, "Macros by example", "Hygiene": `$crate` "refers to the crate defining the
/// macro", so such a call names one definition wherever the macro is called, and it writes an
/// item exactly when that definition may: `callee` resolves the path in the defining crate and
/// answers for what it finds. Every other call (`m!`, `a::m!`, a metavariable's `$m!`) is
/// resolved where the macro is CALLED, so it may name anything: true, as [`may_declare`] says.
///
/// alloc's `format!` transcribes `$crate::__export::must_use({ let res =
/// $crate::fmt::format($crate::__export::format_args!($($arg)*)); res })`: no item word and one
/// call, through `$crate`, of a builtin that writes no item; so a `format!` writes none, where
/// [`may_declare`] alone answers true for the call. True at the first item word, call by
/// another path, or call `callee` answers true for; one read of the tokens, no recursion here
/// (a callee that reads another definition is the caller's step, bounded there).
pub fn may_declare_through<'t>(text: &'t str, callee: &mut dyn FnMut(&[&'t str]) -> bool) -> bool {
    let mut segments: [&'t str; MAX_PATH_SEGMENTS] = [""; MAX_PATH_SEGMENTS];
    let mut state = Through::Idle;
    let mut at = 0usize;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = at;
        at += token.len as usize;
        let kind = token.kind;
        if matches!(kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        let open = matches!(kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace);
        state = match (state, kind) {
            (now, TokenKind::Ident | TokenKind::RawIdent) => {
                let w = &text[start..at];
                if ITEM_WORDS.contains(&w) {
                    return true;
                }
                match now {
                    Through::Dollar if w == "crate" => Through::Path(0),
                    Through::Sep(n) if n < MAX_PATH_SEGMENTS => {
                        segments[n] = w;
                        Through::Path(n + 1)
                    }
                    Through::Sep(_) => return true,
                    _ if not_a_macro(w) => Through::Idle,
                    _ => Through::Word,
                }
            }
            (Through::Path(n), TokenKind::Colon) => Through::Colon(n),
            (Through::Colon(n), TokenKind::Colon) => Through::Sep(n),
            (Through::Word | Through::Path(0), TokenKind::Bang) => Through::Bang,
            (Through::Path(n), TokenKind::Bang) => Through::PathBang(n),
            // A call by a word, or `$m ! $args` (a transcriber calls a macro on a fragment that
            // holds its brackets): resolved at the call site, so it may write anything.
            (Through::Bang, _) if open || kind == TokenKind::Dollar => return true,
            (Through::PathBang(n), _) if open || kind == TokenKind::Dollar => {
                if callee(&segments[..n]) {
                    return true;
                }
                // `$crate::m! $args`: the `$` starts the fragment.
                if kind == TokenKind::Dollar { Through::Dollar } else { Through::Idle }
            }
            // `$($m)+! { .. }` (evm2's `for_each_spec!`, which calls the macro its caller names
            // by a repeated path): a repetition's `)`, an optional `::` separator and its
            // operator end a macro's path as a word does. A superset: `(a) * !(b)` reads as a
            // call too.
            (_, TokenKind::CloseParen) => Through::Close,
            (Through::Close, TokenKind::Colon) => Through::CloseColon,
            (Through::CloseColon, TokenKind::Colon) => Through::CloseSep,
            (Through::Close | Through::CloseSep, TokenKind::Star | TokenKind::Plus | TokenKind::Question) => Through::Word,
            (_, TokenKind::Dollar) => Through::Dollar,
            _ => Through::Idle,
        };
    }
    false
}

/// The blocks of `text` (one item's whole text) and the item statements in them.
pub fn scan<'a>(text: &str, alloc: Alloc<'a>) -> Nested<'a> {
    scan_at(text, 0, alloc)
}

/// [`scan`] of a text `depth` item bodies deep: then each `fn`, `const` and `static` found is
/// scanned in turn for [`NestedItem::body_writes`] (its own text is a body-like text too).
fn scan_at<'a>(text: &str, depth: u16, alloc: Alloc<'a>) -> Nested<'a> {
    struct Frame<'t, 'a> {
        text: &'t str,
        out: Nested<'a>,
        next: usize,
        depth: u16,
        writes_kind: bool,
        surface_kind: bool,
        own_surface: bool,
        braced: bool,
    }
    let mut root = scan_one(text, alloc);
    if root.end == NestedEnd::Unbalanced {
        root.surface_by_text = true;
        return root;
    }
    let mut stack: AVec<'a, Frame<'_, 'a>> = AVec::new_in(alloc);
    stack.push(Frame {
        text,
        out: root,
        next: 0,
        depth,
        writes_kind: false,
        surface_kind: false,
        own_surface: false,
        braced: false,
    });
    loop {
        let mut descend: Option<(&str, Nested<'a>, u16)> = None;
        if let Some(top) = stack.last_mut() {
            while let Some(item) = top.out.items.get_mut(top.next) {
                let Some(own) = top.text.get(item.range.start as usize..item.range.end as usize) else {
                    // Never: the scan's ranges are of `text`. Taken to be seen outside, never inside.
                    item.surface = true;
                    top.next += 1;
                    continue;
                };
                let writes_kind = matches!(item.kind, NestedKind::Fn | NestedKind::Const | NestedKind::Static);
                // The kinds whose own text may hold an item seen outside (a block `mod`'s, a trait's
                // default bodies, an array length's block); an `impl` and an extern block already are.
                let surface_kind = !matches!(
                    item.kind,
                    NestedKind::Impl | NestedKind::ExternBlock | NestedKind::MacroRules | NestedKind::Macro | NestedKind::Use | NestedKind::ExternCrate
                );
                let own_surface = matches!(item.kind, NestedKind::Impl | NestedKind::ExternBlock) || attrs_emit(own);
                // A body with no brace group has no block (`const K: u8 = 1;`).
                let braced = own.contains('{');
                let deep = top.depth >= MAX_BODY_DEPTH;
                if braced && !deep && (writes_kind || (surface_kind && !own_surface)) {
                    let mut inner = scan_one(own, alloc);
                    if inner.end == NestedEnd::Unbalanced {
                        inner.surface_by_text = true;
                        if writes_kind {
                            item.body_writes = inner.writes();
                        }
                        item.surface = own_surface || (surface_kind && inner.surface_by_text);
                        top.next += 1;
                        continue;
                    }
                    top.writes_kind = writes_kind;
                    top.surface_kind = surface_kind;
                    top.own_surface = own_surface;
                    top.braced = braced;
                    descend = Some((own, inner, top.depth + 1));
                    break;
                }
                if writes_kind && braced {
                    item.body_writes = deep;
                }
                item.surface = own_surface || (surface_kind && braced && deep);
                top.next += 1;
            }
        }
        if let Some((own, inner, next_depth)) = descend {
            stack.push(Frame {
                text: own,
                out: inner,
                next: 0,
                depth: next_depth,
                writes_kind: false,
                surface_kind: false,
                own_surface: false,
                braced: false,
            });
            continue;
        }
        let Some(mut done) = stack.pop() else {
            return scan_one(text, alloc);
        };
        done.out.surface_by_text = done.out.items.iter().any(|i| i.surface);
        if let Some(parent) = stack.last_mut() {
            if let Some(item) = parent.out.items.get_mut(parent.next) {
                if parent.writes_kind && parent.braced {
                    item.body_writes = done.out.writes();
                }
                item.surface = parent.own_surface || (parent.surface_kind && parent.braced && done.out.surface_by_text);
            }
            parent.next += 1;
        } else {
            return done.out;
        }
    }
}

/// The outer attributes that leave an item in a block seen only by that block (D189, point
/// 1): they check, document, lay out or tune the item, and write nothing (the Reference,
/// "Attributes", "Built-in attributes index": conditional compilation, diagnostics,
/// documentation, code generation, type system). Every other attribute makes the item a
/// surface item: `derive` and an attribute macro write items, `cfg_attr` may expand to one,
/// `macro_export`, `no_mangle`, `export_name`, `used`, `link_section`, `lang`,
/// `panic_handler` and `global_allocator` name the item outside the body, `unsafe(..)` wraps
/// one of those, and an unknown one is not known to do nothing.
const INERT_ATTRS: [&str; 18] = [
    "cfg",
    "doc",
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "inline",
    "cold",
    "must_use",
    "deprecated",
    "track_caller",
    "repr",
    "non_exhaustive",
    "automatically_derived",
    "target_feature",
    "optimize",
    "coverage",
];

/// The tools whose attributes ([`INERT_ATTRS`]' rule, a path of two or more segments) only
/// tune a tool's reading: the Reference, "Attributes", "Tool attributes" (`rustfmt::skip`,
/// `clippy::..`), and "Diagnostic attributes" (`diagnostic::on_unimplemented`).
const INERT_TOOLS: [&str; 3] = ["rustfmt", "clippy", "diagnostic"];

/// Whether an attribute whose path's first segment is `first`, of `segments` segments, is
/// inert ([`INERT_ATTRS`] for one segment, [`INERT_TOOLS`] for a tool's path).
pub(crate) fn inert_attr(first: &str, segments: u32) -> bool {
    if segments == 1 { INERT_ATTRS.contains(&first) } else { INERT_TOOLS.contains(&first) }
}

/// Where [`attrs_emit`] is in an item's leading outer attributes.
#[derive(Clone, Copy)]
enum AttrAt {
    /// Before an attribute, or after one's `]`.
    Between,
    /// After `#`.
    Pound,
    /// After `#[`.
    Open,
    /// Reading the attribute's path: its first segment's range and how many segments so far.
    Path { first: (usize, usize), segments: u32 },
    /// In the attribute's input, this many brackets deep.
    Input(u32),
}

/// Whether one of the outer attributes `item` (an item statement's text, attributes first)
/// starts with is outside [`INERT_ATTRS`] and [`INERT_TOOLS`]. Reads the attributes only: it
/// stops at the first token after them. A `#` not followed by `[` is taken to emit (rule 3: a
/// form not read is never taken to be inert).
fn attrs_emit(item: &str) -> bool {
    let mut state = AttrAt::Between;
    let mut at = 0usize;
    for token in tokenize(item, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = at;
        at += token.len as usize;
        let kind = token.kind;
        if matches!(kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        let open = matches!(kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace);
        let close = matches!(kind, TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace);
        let word = matches!(kind, TokenKind::Ident | TokenKind::RawIdent);
        state = match state {
            AttrAt::Between if kind == TokenKind::Pound => AttrAt::Pound,
            AttrAt::Between => return false,
            AttrAt::Pound if kind == TokenKind::OpenBracket => AttrAt::Open,
            AttrAt::Pound => return true,
            AttrAt::Open if word => AttrAt::Path { first: (start, at), segments: 1 },
            AttrAt::Open => return true,
            AttrAt::Path { first, segments } if kind == TokenKind::Colon => AttrAt::Path { first, segments },
            AttrAt::Path { first, segments } if word => AttrAt::Path { first, segments: segments + 1 },
            AttrAt::Path { first, segments } => {
                if !inert_attr(&item[first.0..first.1], segments) {
                    return true;
                }
                match kind {
                    TokenKind::CloseBracket => AttrAt::Between,
                    _ if open => AttrAt::Input(1),
                    // `#[doc = ".."]`: the input runs to the `]`.
                    _ => AttrAt::Input(0),
                }
            }
            AttrAt::Input(0) if kind == TokenKind::CloseBracket => AttrAt::Between,
            AttrAt::Input(d) if open => AttrAt::Input(d + 1),
            AttrAt::Input(d) if close => AttrAt::Input(d.saturating_sub(1)),
            AttrAt::Input(d) => AttrAt::Input(d),
        };
    }
    false
}

/// The outer attributes of the statement under way in a block: from its start `stmt_start`
/// through the `]` of its last outer attribute, `attrs_end`. `None` when it starts with none
/// (a visibility or a qualifier alone: `pub`, `unsafe`), or `attrs_end` is an earlier
/// statement's.
fn outer_attrs(text: &str, stmt_start: Option<u32>, attrs_end: u32) -> Option<Range> {
    stmt_start.filter(|&s| attrs_end > s && text[s as usize..].starts_with('#')).map(|start| Range { start, end: attrs_end })
}

/// The call start (first outer attribute at a statement's start, or the path's start) and
/// path range (without outer attributes) of the macro call whose `!` was just read.
#[derive(Clone, Copy)]
struct CallHead {
    start: u32,
    path: Range,
}

/// One text's scan, its items' own bodies not read ([`NestedItem::body_writes`] all `false`).
fn scan_one<'a>(text: &str, alloc: Alloc<'a>) -> Nested<'a> {
    let mut out = Nested {
        blocks: AVec::new_in(alloc),
        items: AVec::new_in(alloc),
        calls: AVec::new_in(alloc),
        end: NestedEnd::Whole,
        surface_by_text: false,
    };
    let mut stack: AVec<'a, Open> = AVec::new_in(alloc);
    // How many of the open brackets are a macro call's arguments.
    let mut in_call = 0u32;
    let mut skip: Option<Skip> = None;
    // At a statement's start (only ever directly in a brace group).
    let mut fresh = false;
    // Where the statement started, when attributes or a visibility came first.
    let mut stmt_start: Option<u32> = None;
    // Per block, by id: the outer attributes of the expression or `let` statement under way in
    // it ([`Block::attrs`] of the blocks written inside that statement).
    let mut stmt_attrs: AVec<'a, Option<Range>> = AVec::new_in(alloc);
    // The offset after the `]` of the last outer attribute read at a statement's start
    // ([`outer_attrs`]), and whether the attribute under way is an inner one (`#![..]`, the
    // block's own: the statement starts after it).
    let mut attrs_end = 0u32;
    let mut inner_attr = false;
    let mut lead = Lead::None;
    // The start of the path under way in a block: a call's path itself (`a::b`, `::a`,
    // `$crate::a`), never including outer attributes written before it.
    let mut path_start: Option<u32> = None;
    // Where the whole call starts: the statement's first outer attribute when `path_start`
    // began at a statement's start with outer attributes (`#[cfg(..)] a::m!(..)`), else
    // `path_start`.
    let mut call_start: Option<u32> = None;
    // Whether `path_start` began at a statement's start ([`NestedCall::expr`] is its negation),
    // and, for the `name!` read last, whether its path did.
    let mut path_stmt = false;
    let mut bang_stmt = false;
    // What the last tokens were, for `#[`, `#![`, `pub(` and `name!(`.
    let mut after_pound = false;
    let mut after_pub = false;
    let mut after_ident = false;
    let mut after_bang_of: Option<Option<CallHead>> = None;
    // After the first `:` of a `::` (1) or its second (2): a word after the second goes on
    // the path, a single `:` (`let x: T`) ends it.
    let mut sep = 0u8;
    // After a `$`: the word after it is no path's first (`$crate`'s `crate`, `$crater`'s).
    let mut after_dollar = false;
    // The listed call whose arguments are open: its range ends at their closer.
    let mut open_call: Option<usize> = None;
    let mut at = 0usize;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = at;
        at += token.len as usize;
        let kind = token.kind;
        if matches!(kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        let is_open = matches!(kind, TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket);
        let is_close = matches!(kind, TokenKind::CloseBrace | TokenKind::CloseParen | TokenKind::CloseBracket);
        // Inside an item: only its end is looked for.
        if let Some(s) = skip.as_mut() {
            if let Some(end) = s.closed {
                // The Reference, "Generic parameters", "Const generics": a const argument may
                // be "a block expression" (`Tr<{ N }>`, `A<{ N }, T>`). No statement starts
                // with `>` or `,`, so a brace group followed by one was such an argument and
                // the item goes on to its real body (or its `;`).
                if matches!(kind, TokenKind::Gt | TokenKind::Comma) {
                    s.closed = None;
                    s.in_body = false;
                    continue;
                }
                let done = *s;
                skip = None;
                out.items.push(NestedItem { block: done.block, kind: done.kind, range: Range { start: done.start, end }, body_writes: false, surface: false });
                fresh = true;
                // This token starts the next statement (or closes the block): read below.
            } else if is_open {
                s.in_body |= s.body && s.depth == 0 && kind == TokenKind::OpenBrace;
                s.depth += 1;
                continue;
            } else if is_close && s.depth > 0 {
                s.depth -= 1;
                if s.depth == 0 && s.in_body && kind == TokenKind::CloseBrace {
                    s.closed = Some(offset(at));
                }
                continue;
            } else if is_close {
                // Its block closes before it ends: not an item the parser accepts; dropped,
                // and the closer is read as the block's.
                skip = None;
            } else {
                if kind == TokenKind::Semi && s.depth == 0 {
                    let done = *s;
                    skip = None;
                    out.items.push(NestedItem { block: done.block, kind: done.kind, range: Range { start: done.start, end: offset(at) }, body_writes: false, surface: false });
                    fresh = true;
                }
                continue;
            }
        }
        // Inside a macro call's arguments: only brackets are counted.
        if in_call > 0 {
            if is_open {
                stack.push(Open::Call);
                in_call += 1;
            } else if is_close {
                if stack.pop().is_none() {
                    out.end = NestedEnd::Unbalanced;
                    return out;
                }
                in_call -= 1;
                if in_call == 0 {
                    if let Some(call) = open_call.take() {
                        out.calls[call].range.end = offset(at);
                    }
                    fresh = kind == TokenKind::CloseBrace && matches!(stack.last(), Some(Open::Brace(_)));
                }
            }
            continue;
        }
        let was_pound = core::mem::take(&mut after_pound);
        let was_pub = core::mem::take(&mut after_pub);
        let was_ident = core::mem::take(&mut after_ident);
        let bang_of = after_bang_of.take();
        let was_sep = core::mem::take(&mut sep);
        let was_dollar = core::mem::take(&mut after_dollar);
        let block = match stack.last() {
            Some(Open::Brace(b)) => Some(*b),
            _ => None,
        };
        // The innermost block around, through paren and square groups (`f(m!())`).
        let enclosing = block.or_else(|| stack.iter().rev().find_map(|o| if let Open::Brace(b) = o { Some(*b) } else { None }));
        if is_open {
            // The Reference, "Attributes": an attribute's input is any "delimited token
            // tree" (`#[allow(a, b)]`, `#[doc(hidden)]`, `#[cfg_attr(x, derive(Y))]`,
            // `#[doc = concat!("a", "b")]`). Every group inside an attribute's brackets is
            // the attribute's: it opens no block, is no call, and leaves the statement at its
            // start, so the item written after the attribute is still found.
            let in_attr = matches!(stack.last(), Some(Open::Other { stmt: true }));
            let open = match (kind, bang_of) {
                _ if in_attr => Open::Other { stmt: true },
                // `name!(..)`, `name![..]`, `name!{..}`: the arguments are not read. A call in
                // a block is listed under the innermost one, at a statement's start or in an
                // expression (the Reference, "Statements", "Item declarations": an item a
                // block expression declares is scoped to that block, wherever the block stands:
                // `let read = tokio::select! { .. }` writes `{ mod __tokio_select_util { .. }
                // .. }`, reth's `with_adapter!` writes `{ type $A = ..; $body }`).
                (_, Some(head)) => {
                    if let (Some(head), Some(block)) = (head, enclosing) {
                        open_call = Some(out.calls.len());
                        let none = Range { start: head.path.start, end: head.path.start };
                        let attrs = if bang_stmt { none } else { stmt_attrs.get(block as usize).copied().flatten().unwrap_or(none) };
                        out.calls.push(NestedCall {
                            block,
                            path: head.path,
                            range: Range { start: head.start, end: head.path.end },
                            expr: !bang_stmt,
                            attrs,
                        });
                    }
                    in_call += 1;
                    Open::Call
                }
                // `extern "C" { .. }` at a statement's start: an item whose body this is.
                (TokenKind::OpenBrace, None) if fresh && lead == Lead::Extern && block.is_some() => {
                    skip = Some(Skip { kind: NestedKind::ExternBlock, start: stmt_start.unwrap_or(offset(start)), block: block.unwrap_or(0), body: true, depth: 1, in_body: true, closed: None });
                    lead = Lead::None;
                    stmt_start = None;
                    path_start = None;
                    call_start = None;
                    continue;
                }
                (TokenKind::OpenBrace, None) => {
                    let id = offset(out.blocks.len());
                    let parent = stack.iter().rev().find_map(|o| if let Open::Brace(b) = o { Some(*b) } else { None });
                    let none = Range { start: offset(start), end: offset(start) };
                    let attrs = match (block, parent) {
                        // A block statement: its own attributes; a new statement of the parent.
                        (Some(b), _) if fresh => {
                            stmt_attrs[b as usize] = None;
                            outer_attrs(text, stmt_start, attrs_end).unwrap_or(none)
                        }
                        // Inside a statement of the parent, maybe within its parens.
                        (_, Some(p)) => stmt_attrs[p as usize].unwrap_or(none),
                        (_, None) => none,
                    };
                    stmt_attrs.push(None);
                    out.blocks.push(Block { parent, attrs, open: offset(start), close: 0 });
                    Open::Brace(id)
                }
                // `#[..]`, `#![..]` and `pub(..)` leave the statement at its start.
                (TokenKind::OpenBracket, None) => Open::Other { stmt: fresh && was_pound },
                _ => Open::Other { stmt: fresh && was_pub },
            };
            let keeps = matches!(open, Open::Other { stmt: true });
            // A statement starting with a group (`#[cfg(x)] (a, m!());`): under way, under the
            // outer attributes before it.
            if let (true, false, Some(b), Open::Other { .. }) = (fresh, keeps, block, open) {
                stmt_attrs[b as usize] = outer_attrs(text, stmt_start, attrs_end);
            }
            stack.push(open);
            if !keeps {
                fresh = matches!(open, Open::Brace(_));
                stmt_start = None;
            }
            lead = Lead::None;
            path_start = None;
            call_start = None;
            continue;
        }
        if is_close {
            match stack.pop() {
                None => {
                    out.end = NestedEnd::Unbalanced;
                    return out;
                }
                Some(Open::Brace(b)) => {
                    out.blocks[b as usize].close = offset(at);
                    fresh = matches!(stack.last(), Some(Open::Brace(_)));
                    stmt_start = None;
                }
                Some(Open::Other { stmt: true }) => {
                    // An attribute at a statement's start closed: an outer one ends the
                    // statement's attributes so far; an inner one is the block's, and the
                    // statement starts after it.
                    if kind == TokenKind::CloseBracket && matches!(stack.last(), Some(Open::Brace(_))) {
                        if core::mem::take(&mut inner_attr) {
                            stmt_start = None;
                        } else {
                            attrs_end = offset(at);
                        }
                    }
                }
                Some(_) => {
                    fresh = false;
                    stmt_start = None;
                }
            }
            lead = Lead::None;
            path_start = None;
            call_start = None;
            continue;
        }
        let Some(block) = block else {
            // Outside every block (the item's own header), or directly in a paren or square
            // group: no statement starts here. In a group inside a block, a call's path is
            // read as in an expression of that block (`f(m!())`, `[m!(), 1]`); outside every
            // block, a call is passed over and not listed.
            after_ident = matches!(kind, TokenKind::Ident | TokenKind::RawIdent) && !not_a_macro(&text[start..at]);
            match kind {
                TokenKind::Bang if was_ident => {
                    call_start = None;
                    after_bang_of = Some(path_start.take().filter(|_| enclosing.is_some()).map(|p| CallHead { start: p, path: Range { start: p, end: offset(start) } }));
                    bang_stmt = false;
                }
                _ if enclosing.is_none() => {
                    path_start = None;
                    call_start = None;
                }
                TokenKind::Ident | TokenKind::RawIdent if was_sep == 2 || was_dollar => {}
                TokenKind::Ident | TokenKind::RawIdent => {
                    path_start = after_ident.then_some(offset(start));
                    call_start = path_start;
                }
                TokenKind::Colon if was_sep == 1 => sep = 2,
                TokenKind::Colon if text[start..].starts_with("::") && (path_start.is_some() || !was_ident) => {
                    sep = 1;
                    path_start.get_or_insert(offset(start));
                    call_start = path_start;
                }
                TokenKind::Dollar if dollar_crate(&text[at..]) => {
                    after_dollar = true;
                    path_start = Some(offset(start));
                    call_start = path_start;
                }
                _ => {
                    after_dollar = kind == TokenKind::Dollar;
                    path_start = None;
                    call_start = None;
                }
            }
            continue;
        };
        match kind {
            TokenKind::Semi => {
                stmt_attrs[block as usize] = None;
                fresh = true;
                stmt_start = None;
                lead = Lead::None;
                path_start = None;
                call_start = None;
            }
            TokenKind::Pound if fresh && lead == Lead::None => {
                stmt_start.get_or_insert(offset(start));
                after_pound = true;
                inner_attr = false;
            }
            // `#!` of an inner attribute.
            TokenKind::Bang if fresh && was_pound => {
                after_pound = true;
                inner_attr = true;
            }
            TokenKind::Bang if fresh && lead == Lead::MacroRules => {
                skip = Some(Skip { kind: NestedKind::MacroRules, start: stmt_start.unwrap_or(offset(start)), block, body: true, depth: 0, in_body: false, closed: None });
                lead = Lead::None;
                stmt_start = None;
                path_start = None;
                call_start = None;
            }
            TokenKind::Bang if was_ident => {
                let start_at = call_start.take();
                after_bang_of = Some(path_start.take().map(|p| CallHead { start: start_at.unwrap_or(p), path: Range { start: p, end: offset(start) } }));
                bang_stmt = path_stmt;
                fresh = false;
                lead = Lead::None;
                stmt_start = None;
            }
            // An ABI string after `extern`.
            TokenKind::Literal { .. } if fresh && lead == Lead::Extern => {}
            TokenKind::Ident if fresh => {
                let word = &text[start..at];
                let begin = stmt_start.unwrap_or(offset(start));
                // What `word` makes of the statement: an item, more keywords, or neither.
                let item = match (lead, word) {
                    (Lead::Union, _) => Some(NestedKind::Union),
                    (Lead::MacroRules, _) => None,
                    (Lead::Static, "move") => None,
                    (Lead::Static, _) => Some(NestedKind::Static),
                    (Lead::Const, "fn") => Some(NestedKind::Fn),
                    (Lead::Const, "trait") => Some(NestedKind::Trait),
                    (Lead::Const, "impl") => Some(NestedKind::Impl),
                    (Lead::Const, "unsafe" | "async" | "extern" | "safe" | "auto") => None,
                    (Lead::Const, _) => Some(NestedKind::Const),
                    (Lead::Extern, "crate") => Some(NestedKind::ExternCrate),
                    (Lead::None, "pub") => None,
                    (_, w) => item_word(w),
                };
                if let Some(kind) = item {
                    skip = Some(Skip { kind, start: begin, block, body: has_body(kind), depth: 0, in_body: false, closed: None });
                    lead = Lead::None;
                    stmt_start = None;
                    path_start = None;
                    call_start = None;
                    continue;
                }
                let next = match (lead, word) {
                    (Lead::None, "pub") => {
                        after_pub = true;
                        stmt_start = Some(begin);
                        continue;
                    }
                    (Lead::Const, "unsafe" | "async" | "safe" | "auto") => Lead::Const,
                    (Lead::Const, "extern") => Lead::Extern,
                    (Lead::None | Lead::Qualifier, w) => lead_word(w),
                    _ => Lead::None,
                };
                if next == Lead::None {
                    // An expression or a `let`: the statement is under way, under the outer
                    // attributes written before it.
                    stmt_attrs[block as usize] = outer_attrs(text, stmt_start, attrs_end);
                    fresh = false;
                    stmt_start = None;
                    let is_path = lead == Lead::None && !not_a_macro(word);
                    path_start = is_path.then_some(offset(start));
                    call_start = is_path.then_some(begin);
                    after_ident = !not_a_macro(word);
                } else {
                    stmt_start = Some(begin);
                    // `union` and `default` may be plain names: `union!(..)`, `default.x`.
                    after_ident = true;
                    path_start = Some(offset(start));
                    call_start = Some(begin);
                }
                path_stmt = true;
                lead = next;
            }
            TokenKind::Ident | TokenKind::RawIdent => {
                if fresh {
                    stmt_attrs[block as usize] = outer_attrs(text, stmt_start, attrs_end);
                }
                after_ident = !not_a_macro(&text[start..at]);
                // A word after `::` or `$` goes on the path; any other starts one: a call in an
                // expression (`let read = tokio::select! { .. }`) is listed too.
                if was_sep != 2 && !was_dollar {
                    let stmt = fresh && lead == Lead::None && after_ident;
                    path_start = after_ident.then_some(offset(start));
                    call_start = if stmt { Some(stmt_start.unwrap_or(offset(start))) } else { path_start };
                    path_stmt = stmt;
                }
                fresh = false;
                lead = Lead::None;
                stmt_start = None;
            }
            // The second `:` of a `::`: the path goes on.
            TokenKind::Colon if was_sep == 1 => {
                sep = 2;
                fresh = false;
                lead = Lead::None;
            }
            // `::a::b!(..)`: a call's path may START with `::` (lane TG2: a derive's
            // `::dep::declare!(COUNT)` lost its declaration, and its methods the name), and a
            // `::` after a word goes on its path. A single `:` (`let x: T`) ends a path.
            TokenKind::Colon
                if text[start..].starts_with("::") && (path_start.is_some() || (fresh && lead == Lead::None) || (!fresh && !was_ident)) =>
            {
                sep = 1;
                if fresh {
                    stmt_attrs[block as usize] = outer_attrs(text, stmt_start, attrs_end);
                }
                if path_start.is_none() {
                    call_start = Some(stmt_start.take().unwrap_or(offset(start)));
                    path_start = Some(offset(start));
                    path_stmt = fresh;
                }
                fresh = false;
                lead = Lead::None;
            }
            // `$crate::a!(..)`: a statement call's path may START with `$crate` in a text a
            // macro wrote (the Reference, "Macros By Example", "Hygiene": `$crate` "refers to
            // the crate defining the macro"). The body lowering keeps it as written in an
            // expansion's text (blaze-expand body.rs `body_words`), where the names library
            // writes `crate` with a mark (blaze-scope resolve.rs `render_with`): both texts
            // must list the same calls, or the two layers number an expansion's blocks and
            // calls apart (front2 scope_table.rs). tokio's `select!` recurses this way,
            // `$crate::select!(@{ .. })` at its expansion's top level, before the expansion
            // that writes `{ mod __tokio_select_util { .. } .. }`. A `$` before any other
            // word starts no path: only `$crate` survives into an expansion's text. In an
            // expression too (`let x = $crate::m!()`), as the library's `crate::m!()` is.
            TokenKind::Dollar if (!fresh || lead == Lead::None) && dollar_crate(&text[at..]) => {
                if fresh {
                    stmt_attrs[block as usize] = outer_attrs(text, stmt_start, attrs_end);
                }
                call_start = Some(stmt_start.take().unwrap_or(offset(start)));
                path_start = Some(offset(start));
                path_stmt = fresh;
                after_dollar = true;
                fresh = false;
                lead = Lead::None;
            }
            _ => {
                // A statement starting with an operator or a literal (`#[cfg(x)] *p = m!();`):
                // under way, under the outer attributes before it.
                if fresh {
                    stmt_attrs[block as usize] = outer_attrs(text, stmt_start, attrs_end);
                }
                after_dollar = kind == TokenKind::Dollar;
                fresh = false;
                lead = Lead::None;
                stmt_start = None;
                path_start = None;
                call_start = None;
            }
        }
    }
    if !stack.is_empty() || skip.is_some() {
        out.end = NestedEnd::Unbalanced;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{NestedEnd, NestedKind, scan};

    /// (kind, block, text) of each item found.
    fn items(text: &str) -> std::vec::Vec<(NestedKind, u32, &str)> {
        let n = scan(text, blaze_own::testing::heap().alloc());
        assert_eq!(n.end, NestedEnd::Whole, "{text}");
        n.items.iter().map(|i| (i.kind, i.block, &text[i.range.start as usize..i.range.end as usize])).collect()
    }

    #[test]
    fn the_sol_shape() {
        let text = "const _: () = {\n    use ::alloy_sol_types as alloy_sol_types;\n    { fn conv() {} }\n    #[automatically_derived]\n    impl alloy_sol_types::SolCall for aCall { type Token<'a> = u8; fn f() { impl Hidden for X {} } }\n};";
        let got = items(text);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], (NestedKind::Use, 0, "use ::alloy_sol_types as alloy_sol_types;"));
        assert_eq!(got[1], (NestedKind::Fn, 1, "fn conv() {}"));
        assert_eq!(got[2].0, NestedKind::Impl);
        assert!(got[2].2.starts_with("#[automatically_derived]\n    impl alloy_sol_types::SolCall") && got[2].2.ends_with("} }"), "{}", got[2].2);
        let n = scan(text, blaze_own::testing::heap().alloc());
        assert_eq!(n.blocks.len(), 2);
        assert_eq!((n.blocks[0].parent, n.blocks[1].parent), (None, Some(0)));
        assert_eq!(&text[n.blocks[1].open as usize..n.blocks[1].close as usize], "{ fn conv() {} }");
        assert!(n.has_impl());
    }

    #[test]
    fn serde_and_a_fn_body() {
        let got = items("fn f<T>(x: T) -> u8 where T: Copy { let a = 1; extern crate serde as _serde; unsafe impl<T> Tr for S<T> where T: X {} if a > 0 { pub(crate) struct P(u8); } a }");
        let kinds: std::vec::Vec<_> = got.iter().map(|g| (g.0, g.1)).collect();
        assert_eq!(kinds, [(NestedKind::ExternCrate, 0), (NestedKind::Impl, 0), (NestedKind::Struct, 1)], "{got:?}");
        assert_eq!(got[1].2, "unsafe impl<T> Tr for S<T> where T: X {}");
        assert_eq!(got[2].2, "pub(crate) struct P(u8);");
    }

    #[test]
    fn a_word_is_not_an_item() {
        // A string, a comment, a macro call's arguments, a type position, a closure's block
        // in call arguments (which IS a block), an inline const, an unsafe block.
        let got = items("fn f() { let s = \"impl A for B {}\"; // impl C for D {}\n /* impl E for F {} */ m!{ impl G for H {} } v![impl I for J {}]; let x: &dyn Fn() -> Box<dyn K> = &|| todo!(); g(|| { impl L for M {} }); const { 1 }; unsafe { h() }; let y = S { a: 1 }; }");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].2, "impl L for M {}");
        let n = scan("fn f() { a::b!{ impl G for H {} } c!(); let x = d!(); if !(x) { e! { } } }", blaze_own::testing::heap().alloc());
        assert!(n.items.is_empty());
        let text = "fn f() { a::b!{ impl G for H {} } c!(); let x = d!(); if !(x) { e! { } } }";
        let calls: std::vec::Vec<&str> = n.calls.iter().map(|c| &text[c.path.start as usize..c.path.end as usize]).collect();
        assert_eq!(calls, ["a::b", "c", "d", "e"], "statement and expression calls; `if !(x)` is none");
    }

    #[test]
    fn every_keyword_form() {
        let got = items("const X: u8 = { const K: u8 = { 1 }; static S: u8 = 2; const fn c() {} async fn a() {} extern \"C\" fn e() {} extern \"C\" { fn x(); } union U { a: u8 } type T = u8; enum E { A } trait R {} mod m {} macro_rules! mr { () => {} } unsafe extern \"C\" fn u() {} let union = 1; let default = union; default };");
        let kinds: std::vec::Vec<_> = got.iter().map(|g| g.0).collect();
        use NestedKind::*;
        assert_eq!(kinds, [Const, Static, Fn, Fn, Fn, ExternBlock, Union, TypeAlias, Enum, Trait, Mod, MacroRules, Fn], "{got:?}");
        assert_eq!(got[0].2, "const K: u8 = { 1 };");
        assert_eq!(got[5].2, "extern \"C\" { fn x(); }");
    }

    /// serde_derive 1.0.229 src/dummy.rs `wrap_in_const` writes, around every derived impl:
    /// `const _: () = { #[allow(unused_extern_crates, clippy::useless_attribute)] extern crate
    /// serde as _serde; _serde::__require_serde_not_serde_core!(); #code };`. The Reference,
    /// "Attributes": an attribute's input is a delimited token tree, so the `(..)` inside
    /// `#[allow(..)]` belongs to the attribute and the `extern crate` after it is the item.
    /// (The tempo census of 2026-11-16, tempo-front-6b.txt: "9781 NEW container head: Resolve
    /// Path" and "7443 NEW gap UnreadImpl", every one `_serde` not found at segment 0.)
    #[test]
    fn an_attribute_with_arguments_keeps_its_item() {
        let text = "const _: () = {\n    #[allow(unused_extern_crates, clippy::useless_attribute)]\n    extern crate serde as _serde;\n    _serde::__require_serde_not_serde_core!();\n    #[automatically_derived]\n    impl _serde::Serialize for Burn {\n        fn serialize<__S>(&self, __serializer: __S) -> _serde::__private228::Result<__S::Ok, __S::Error> where __S: _serde::Serializer { loop {} }\n    }\n};";
        let got = items(text);
        let kinds: std::vec::Vec<_> = got.iter().map(|g| (g.0, g.1)).collect();
        assert_eq!(kinds, [(NestedKind::ExternCrate, 0), (NestedKind::Impl, 0)], "{got:?}");
        assert_eq!(got[0].2, "#[allow(unused_extern_crates, clippy::useless_attribute)]\n    extern crate serde as _serde;");
        let n = scan(text, blaze_own::testing::heap().alloc());
        assert_eq!(n.blocks.len(), 1, "the attribute's group and the impl's body are no blocks: {:?}", n.blocks);
        let calls: std::vec::Vec<&str> = n.calls.iter().map(|c| &text[c.range.start as usize..c.range.end as usize]).collect();
        assert_eq!(calls, ["_serde::__require_serde_not_serde_core!()"]);
    }

    /// The other attribute shapes real expansions write before a block's item: alloy's `sol!`
    /// (alloy-sol-macro-expander 1.7.3 src/expand/mod.rs:931) writes `#[doc(hidden)]
    /// #[allow(dead_code)] type UnderlyingSolTuple<'a> = ..;` and `#[automatically_derived]
    /// #[doc(hidden)] impl ..`; serde's Deserialize writes `#[allow(non_camel_case_types)]
    /// #[doc(hidden)] enum __Field { .. }` and `#[doc(hidden)] struct __FieldVisitor;` in the
    /// body of `fn deserialize`. Nested groups, a call and a brace group inside an attribute
    /// are the attribute's too. An inner attribute (`#![inner(z)]`) is the block's: the
    /// Reference, "Attributes", "Inner attributes" are written only at the start of a block
    /// (rustc: "an inner attribute is not permitted following an outer attribute"), so it
    /// stands first and the first item's text starts after it.
    #[test]
    fn every_attribute_shape_keeps_its_item() {
        let got = items("fn deserialize() { #![inner(z)] #[allow(non_camel_case_types)] #[doc(hidden)] enum __Field { __field0, __ignore } #[doc(hidden)] struct __FieldVisitor; #[automatically_derived] #[doc(hidden)] impl A for __Field {} #[doc(hidden)] #[allow(dead_code)] type UnderlyingSolTuple<'a> = (u8,); #[cfg_attr(feature = \"x\", derive(Clone, PartialEq), repr(C))] pub(crate) struct P(u8); #[doc = concat!(\"a\", \"b\")] #[x(y = { impl Not for Counted {} })] fn after() {} let v = 1; #[cfg(test)] { v } }");
        let kinds: std::vec::Vec<_> = got.iter().map(|g| g.0).collect();
        use NestedKind::*;
        assert_eq!(kinds, [Enum, Struct, Impl, TypeAlias, Struct, Fn], "{got:?}");
        assert!(got[0].2.starts_with("#[allow(non_camel_case_types)] #[doc(hidden)] enum __Field") && got[0].2.ends_with("__ignore }"), "{}", got[0].2);
        assert_eq!(got[1].2, "#[doc(hidden)] struct __FieldVisitor;");
        assert_eq!(got[2].2, "#[automatically_derived] #[doc(hidden)] impl A for __Field {}");
        assert_eq!(got[4].2, "#[cfg_attr(feature = \"x\", derive(Clone, PartialEq), repr(C))] pub(crate) struct P(u8);");
        assert!(got[5].2.starts_with("#[doc = concat!(") && got[5].2.ends_with("fn after() {}"), "{}", got[5].2);
        assert!(got.iter().all(|g| g.1 == 0), "every item is the body's own block's: {got:?}");
    }

    /// The Reference, "Generic parameters", "Const generics": a const argument may be "a block
    /// expression". Such a brace group before the item's body does not end the item.
    #[test]
    fn a_braced_const_argument_does_not_end_an_item() {
        let got = items("fn f() { impl Tr<{ N }> for X { fn m() {} } impl<const M: usize> Two<{ M }, u8> for A<{ M + 1 }> where B<{ M }>: Sized {} struct S<const K: usize = { 3 }>; fn g() -> A<{ 2 }> { loop {} } struct Last {} }");
        let texts: std::vec::Vec<&str> = got.iter().map(|g| g.2).collect();
        assert_eq!(
            texts,
            [
                "impl Tr<{ N }> for X { fn m() {} }",
                "impl<const M: usize> Two<{ M }, u8> for A<{ M + 1 }> where B<{ M }>: Sized {}",
                "struct S<const K: usize = { 3 }>;",
                "fn g() -> A<{ 2 }> { loop {} }",
                "struct Last {}",
            ]
        );
    }

    /// A call's whole range: through the bracket closing its arguments, never the `;` after it;
    /// a call in an expression is listed as one at a statement's start is.
    #[test]
    fn a_statement_call_has_its_whole_range() {
        let text = "fn f() { a::b!{ impl G for H {} } c!(1, (2)); let x = d!(); e![x]; }";
        let n = scan(text, blaze_own::testing::heap().alloc());
        let calls: std::vec::Vec<&str> = n.calls.iter().map(|c| &text[c.range.start as usize..c.range.end as usize]).collect();
        assert_eq!(calls, ["a::b!{ impl G for H {} }", "c!(1, (2))", "d!()", "e![x]"]);
        let expr: std::vec::Vec<bool> = n.calls.iter().map(|c| c.expr).collect();
        assert_eq!(expr, [false, false, true, false], "only `d!()` stands in an expression");
    }

    /// tempo's `let read = tokio::select! { .. };` (crates/zones/node/tests/it/utils/network.rs
    /// :516; the expansion writes `{ mod __tokio_select_util { .. } .. }`) and reth's
    /// `let n = with_adapter!(f, |A| { .. })` (crates/trie/db/src/lib.rs:44 writes `{ type $A =
    /// ..; $body }`): a call in an expression, in parens, after `::` or `$crate` is listed under
    /// the innermost block, with its path from its first word. The Reference, "Statements",
    /// "Item declarations": "Declaring an item within a statement block restricts its scope to
    /// the block containing the statement", wherever the block expression stands. The negative
    /// twin: a `!` after a keyword, `x != y`, a type's single `:`, a `$` before another word, a
    /// call inside another call's arguments, and a call in the item's header are not listed.
    /// Expected: the two vectors below; without the expression arms the first is `[]`.
    #[test]
    fn a_call_in_an_expression_is_listed_under_its_block() {
        let heap = blaze_own::testing::heap();
        let paths = |text: &str| -> std::vec::Vec<(u32, std::string::String)> {
            let n = scan(text, heap.alloc());
            assert_eq!(n.end, NestedEnd::Whole, "{text}");
            n.calls.iter().map(|c| (c.block, std::string::String::from(&text[c.path.start as usize..c.path.end as usize]))).collect()
        };
        let s = std::string::String::from;
        assert_eq!(
            paths("fn f() { let read = tokio::select! { _ = a => 1 }; let n = with_adapter!(f, |A| { 1 }); g(::dep::m!(), [$crate::k!()]); { let y: u8 = h!(); } }"),
            [(0, s("tokio::select")), (0, s("with_adapter")), (0, s("::dep::m")), (0, s("$crate::k")), (1, s("h"))]
        );
        assert_eq!(paths("fn f() -> [u8; n!()] { if !(x) { return !y; } let b = x != y; let t: T = $crater!(); a!(b!(c)); }"), [(0, s("a"))]);
    }

    /// tempo crates/consensus/src/follow/executor/actor.rs:325: `#[cfg(feature = "fuzz")] let
    /// details = serde_json::json!({ .. });`. The Reference, "Statements": a statement's outer
    /// attributes apply to the whole statement, so an expression call carries them
    /// ([`super::NestedCall::attrs`]), in a `let`, an expression statement and one starting
    /// with a group or an operator. The twin: a call in a statement with no attribute, a
    /// statement call (its path holds its own), and a call after an inner attribute of the
    /// block (`#![..]` is the block's, no statement's) carry none. Without the field's arms
    /// every call below has `""`. Prints nothing.
    #[test]
    fn an_expression_call_carries_its_statements_outer_attributes() {
        let heap = blaze_own::testing::heap();
        let attrs = |text: &str| -> std::vec::Vec<(std::string::String, std::string::String)> {
            let n = scan(text, heap.alloc());
            assert_eq!(n.end, NestedEnd::Whole, "{text}");
            n.calls
                .iter()
                .map(|c| (std::string::String::from(&text[c.path.start as usize..c.path.end as usize]), std::string::String::from(&text[c.attrs.start as usize..c.attrs.end as usize])))
                .collect()
        };
        let s = |a: &str, b: &str| (std::string::String::from(a), std::string::String::from(b));
        let cfg = "#[cfg(feature = \"fuzz\")]";
        assert_eq!(
            attrs("fn f() { #[cfg(feature = \"fuzz\")] let d = serde_json::json!({ \"h\": 1 }); #[cfg(feature = \"fuzz\")] #[allow(x)] g(a::m!()); #[cfg(feature = \"fuzz\")] (b!(), 1); #[cfg(feature = \"fuzz\")] *p = c!(); }"),
            [s("serde_json::json", cfg), s("a::m", "#[cfg(feature = \"fuzz\")] #[allow(x)]"), s("b", cfg), s("c", cfg)]
        );
        assert_eq!(
            attrs("fn f() { let d = serde_json::json!({}); #[cfg(x)] s!(); { #![allow(x)] let e = t!(); } }"),
            [s("serde_json::json", ""), s("s", ""), s("t", "")]
        );
        // A statement call's own outer attributes stay in `range` while `path` starts at the path.
        let stmt_attr = "fn f() { #[cfg(x)] s!(); }";
        let n = scan(stmt_attr, heap.alloc());
        assert_eq!(&stmt_attr[n.calls[0].path.start as usize..n.calls[0].path.end as usize], "s");
        assert_eq!(&stmt_attr[n.calls[0].range.start as usize..n.calls[0].range.end as usize], "#[cfg(x)] s!()");
    }

    /// tokio 1.53.3 macros/select.rs: the entry arm expands to `$crate::select!(@{ .. })`, and
    /// the body lowering keeps `$crate` as written in an expansion's text, where the names
    /// library writes `crate`. Both must list the call at the same place, with its path from
    /// the `$` (or from its outer attributes); a `$` before another word starts no listed call,
    /// and a `$crate` call in expression position is listed as an expression call (`expr`).
    /// Prints nothing; without the `$crate` arm
    /// the first assertion fails with `[]` for the lowered text.
    #[test]
    fn a_dollar_crate_statement_call_is_listed_as_its_crate_spelling_is() {
        let heap = blaze_own::testing::heap();
        let lowered = "{ $crate :: select ! (@ { start = 0 ; () } _ = f => {}) \n}";
        let rendered = "{ crate :: select ! (@ { start = 0 ; () } _ = f => {}) \n}";
        let paths = |text: &str| -> std::vec::Vec<(u32, u32, u32)> {
            let n = scan(text, heap.alloc());
            assert_eq!(n.end, NestedEnd::Whole, "{text}");
            n.calls.iter().map(|c| (c.block, c.range.start, c.range.end - c.range.start)).collect()
        };
        let got = paths(lowered);
        let start = lowered.find('$').expect("a `$`") as u32;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].0, got[0].1), (0, start));
        assert_eq!(&lowered[got[0].1 as usize..(got[0].1 + got[0].2) as usize], "$crate :: select ! (@ { start = 0 ; () } _ = f => {})");
        // The same call at the same block and ordinal in the library's spelling.
        assert_eq!(paths(rendered).len(), 1);
        let attr = "{ #[allow(unreachable_code)] $crate::count!(_ _) ; let x = $crate::m!(); $crater!(); }";
        let n = scan(attr, heap.alloc());
        let paths: std::vec::Vec<&str> = n.calls.iter().map(|c| &attr[c.path.start as usize..c.path.end as usize]).collect();
        assert_eq!(paths, ["$crate::count", "$crate::m"], "`$crater` is not listed; an attributed statement call's `path` starts after its attributes");
        assert!(
            n.calls.iter().map(|c| &attr[c.range.start as usize..c.range.end as usize]).eq(["#[allow(unreachable_code)] $crate::count!(_ _)", "$crate::m!()"]),
            "the statement call's `range` keeps its attributes"
        );
    }

    /// `may_declare` is a superset of "writes an item": true for an item word or any macro call
    /// a text makes (written `m!(..)`, a passed-in `$m!(..)`, `$m!$args`), false for a text of
    /// expressions alone (alloc's `vec!` rules; `!x`, `a != b`). Prints nothing.
    #[test]
    fn a_text_may_declare_only_by_an_item_word_or_a_call() {
        for yes in ["mod m {}", "($e:expr) => { impl A for B {} }", "() => { $crate::m!() }", "($m:path) => { $m!(1) }", "($m:path, $a:tt) => { $m ! $a }"] {
            assert!(super::may_declare(yes), "{yes}");
        }
        // A call through a repeated path: evm2's `for_each_spec!` (no item word; the `!` follows
        // the repetition), and a path repeated with a `::` separator. Both were false.
        for yes in ["([$($x:tt)*] $($m:tt)+) => { $($m)+! { [$($x)*] A a, B b, } }", "($($s:ident)::+) => { $($s)::+!() }"] {
            assert!(super::may_declare(yes), "a call through a repeated path: {yes}");
        }
        // Controls: a repetition that no `!` follows, and one with a `,` separator.
        for no in ["($($e:expr),*) => { 0 $(+ $e)* }", "($($e:expr),*) => { [$($e),*] }"] {
            assert!(!super::may_declare(no), "{no}");
        }
        for no in ["() => { $crate::vec::Vec::new() }", "($e:expr) => { if !$e { return x != y; } }", "1", "// mod m {}\n\"impl\""] {
            assert!(!super::may_declare(no), "{no}");
        }
    }

    /// The Reference, "Macros by example", "Hygiene": `$crate` names the defining crate, so a
    /// `$crate::a::m!` call names one definition, and the text declares what that one does.
    /// alloc's `format!` rules (as pinned): the one call asked, by its segments after `$crate`;
    /// answered "declares nothing", the text declares nothing. The twin: the same callee
    /// answered "may declare" makes the text declare, and so does `may_declare`, which asks no
    /// callee. A call by a plain name (resolved where the macro is called), through a
    /// metavariable, or beside an item word declares whatever the callee says. Prints nothing.
    #[test]
    fn a_crate_path_call_declares_what_its_callee_declares() {
        let format = "($($arg:tt)*) => { $crate::__export::must_use({ let res = $crate::fmt::format($crate::__export::format_args!($($arg)*)); res }) }";
        let mut asked: std::vec::Vec<std::vec::Vec<&str>> = std::vec::Vec::new();
        let declares = super::may_declare_through(format, &mut |path| {
            asked.push(path.to_vec());
            false
        });
        assert!(!declares, "a `format!` whose callee writes nothing writes nothing");
        assert_eq!(asked, [std::vec!["__export", "format_args"]], "only the call is asked, by its path after `$crate`");
        assert!(super::may_declare_through(format, &mut |_| true), "the twin: a callee that may write an item");
        assert!(super::may_declare(format), "unresolved, the call may write anything");
        let fragment = "($a:tt) => { $crate::m! $a }";
        assert!(!super::may_declare_through(fragment, &mut |path| path != ["m"]), "`$crate::m! $a` asks `m`");
        for anyway in ["() => { inner!() }", "($m:ident) => { $m!() }", "() => { $crate::a::b!(); struct S; }"] {
            assert!(super::may_declare_through(anyway, &mut |_| false), "{anyway}");
        }
    }

    #[test]
    fn an_unclosed_body_is_not_used() {
        assert_eq!(scan("fn f() { impl A for B {", blaze_own::testing::heap().alloc()).end, NestedEnd::Unbalanced);
        assert_eq!(scan("fn f() { } }", blaze_own::testing::heap().alloc()).end, NestedEnd::Unbalanced);
    }

    /// A trait declared in a block makes its block a module of the names library, alone (no
    /// impl beside it) as with its impl (core's `Write::write_fmt` shape): the Reference,
    /// "Statements", "Item declarations": "Declaring an item within a statement block
    /// restricts its scope to the block containing the statement." A `fn` alone makes none.
    /// Prints nothing; fails with "a block's trait makes no module" without `Trait` in
    /// `NestedKind::makes_module`.
    #[test]
    fn a_trait_in_a_block_makes_a_module() {
        let alone = "fn f() { trait T { fn m(&self) -> u8 { 1 } } 0u8.m(); }";
        let got = items(alone);
        assert_eq!(got, [(NestedKind::Trait, 0, "trait T { fn m(&self) -> u8 { 1 } }")]);
        assert!(scan(alone, blaze_own::testing::heap().alloc()).makes_module(), "a block's trait makes no module");
        let write_fmt = "fn write_fmt(&mut self, args: Arguments<'_>) -> Result { trait SpecWriteFmt { fn spec_write_fmt(self, args: Arguments<'_>) -> Result; } impl<W: Write + ?Sized> SpecWriteFmt for &mut W { fn spec_write_fmt(self, args: Arguments<'_>) -> Result { write(self, args) } } self.spec_write_fmt(args) }";
        let kinds: std::vec::Vec<NestedKind> = items(write_fmt).iter().map(|i| i.0).collect();
        assert_eq!(kinds, [NestedKind::Trait, NestedKind::Impl]);
        // Nightly core `Iterator::advance_by` shape: `const trait` + `const impl` inside a body.
        let advance_by = "fn advance_by(&mut self, n: usize) { const trait SpecAdvanceBy { fn spec_advance_by(&mut self, n: usize); } const impl<I> SpecAdvanceBy for I { default fn spec_advance_by(&mut self, n: usize) {} } const unsafe trait UnsafeConst {} unsafe const trait ConstUnsafe {} self.spec_advance_by(n); }";
        assert!(items(advance_by).iter().map(|i| i.0).eq([NestedKind::Trait, NestedKind::Impl, NestedKind::Trait, NestedKind::Trait]));
        assert!(scan(advance_by, blaze_own::testing::heap().alloc()).makes_module(), "a block's const trait makes no module");
        assert!(!scan("fn f() { fn g() {} g() }", blaze_own::testing::heap().alloc()).makes_module());
    }

    /// A local fn whose own body declares a `mod` (std's `init` > `reset_sigpipe` >
    /// `#[cfg(..)] { mod sigpipe { .. } }`) writes its block: the fn is then an item of that
    /// block's module. Two deep too; a local fn whose body writes nothing, and a `const`
    /// without a brace group, do not.
    #[test]
    fn a_local_fn_whose_body_writes_writes_its_block() {
        let heap = blaze_own::testing::heap();
        let init = "unsafe fn init(s: u8) { reset(s); unsafe fn reset(s: u8) { #[cfg(all())] { mod sigpipe { pub const DEFAULT: u8 = 0; } match s { sigpipe::DEFAULT => {} _ => {} } } } }";
        let n = scan(init, heap.alloc());
        assert_eq!(n.end, NestedEnd::Whole);
        assert!(!n.makes_module(), "the mod is the local fn's, not init's");
        assert_eq!(n.items.len(), 1);
        assert!(n.items[0].body_writes && n.items[0].writes(), "a local fn's block mod does not write its block");
        assert!(n.writes());
        let deep = "fn a() { fn b() { fn c() { m!(); } } }";
        assert!(scan(deep, heap.alloc()).writes(), "a statement call two bodies deep writes nothing");
        let plain = "fn a() { fn b() -> u8 { 1 } const K: u8 = 1; b() + K }";
        let n = scan(plain, heap.alloc());
        assert!(!n.writes() && n.items.iter().all(|i| !i.body_writes));
    }

    /// D189, point 3, the text's part of `MacroEffect`: alloc's `format!` (as pinned) asks its
    /// one `$crate` call and, answered NONE, is NONE; answered "emits", it emits. Its own words:
    /// `impl` emits (twin: `-> u8`), a `derive` attribute emits (twin: `#[allow(..)]`), a
    /// `#[$m]` emits; `use` and `macro_rules!` bind at top (twin: `let`); a plain-name call and
    /// a metavariable's call are ANY whatever the callee says. Prints nothing.
    #[test]
    fn a_definition_s_effect_is_its_words_and_its_crate_calls() {
        use super::{MacroEffect, effect_through};
        let format = "($($arg:tt)*) => { $crate::__export::must_use({ $crate::fmt::format($crate::__export::format_args!($($arg)*)) }) }";
        let mut asked: std::vec::Vec<std::vec::Vec<&str>> = std::vec::Vec::new();
        let none = effect_through(format, &mut |path| {
            asked.push(path.to_vec());
            MacroEffect::NONE
        });
        assert_eq!(none, MacroEffect::NONE);
        assert_eq!(asked, [std::vec!["__export", "format_args"]]);
        let emits = MacroEffect { emits_surface: true, binds_at_top: false };
        assert_eq!(effect_through(format, &mut |_| emits), emits, "the twin: a callee that emits");
        let words: [(&str, MacroEffect); 8] = [
            ("() => { impl Tr for X {} }", emits),
            ("() => { fn f() -> u8 { 0 } }", MacroEffect::NONE),
            ("() => { #[derive(Clone)] struct S; }", emits),
            ("() => { #[allow(dead_code)] struct S; }", MacroEffect::NONE),
            ("($(#[$m:meta])*) => { $(#[$m])* struct S; }", emits),
            ("($p:path) => { use $p as format; }", MacroEffect { emits_surface: false, binds_at_top: true }),
            ("() => { macro_rules! inner { () => {} } }", MacroEffect { emits_surface: false, binds_at_top: true }),
            ("() => { let x = 1; x }", MacroEffect::NONE),
        ];
        for (text, want) in words {
            assert_eq!(effect_through(text, &mut |_| MacroEffect::NONE), want, "{text}");
        }
        for any in ["() => { inner!() }", "($m:ident) => { $m!() }", "([$($x:tt)*] $($m:tt)+) => { $($m)+! { [$($x)*] } }"] {
            assert_eq!(effect_through(any, &mut |_| MacroEffect::NONE), MacroEffect::ANY, "{any}");
        }
    }

    /// D189, point 1: each trigger of `surface_by_text`, each beside its twin that differs by
    /// the trigger alone. An `impl` in item position (twin: the same words in a call's input,
    /// a string and a comment, which are the surface pass's to read through the call); an
    /// extern block (twin: an `extern "C" fn` and an `extern crate`, names of the block
    /// alone); every attribute named as emitting (twin: the inert ones, tool attributes
    /// included); a `cfg_attr`, which may expand to a derive; a local fn's, a block `mod`'s and
    /// a trait's own text holding one (twin: the same items holding a plain struct); a body
    /// that does not parse. A `macro_rules!` is not read inside. Prints nothing.
    #[test]
    fn a_body_is_surface_by_its_text_only_for_what_leaves_it() {
        let heap = blaze_own::testing::heap();
        let surface = |text: &str| {
            let n = scan(text, heap.alloc());
            n.surface_by_text
        };
        assert!(surface("fn f() { impl Tr for X {} }"), "an impl in item position");
        assert!(surface("fn f() { #[cfg(x)] unsafe impl<T> Tr for X<T> {} }"), "an attributed, qualified impl");
        for twin in ["fn f() { m!(impl Tr for X {}); }", "fn f() { let s = \"impl Tr for X {}\"; }", "fn f() { // impl Tr for X {}\n }"] {
            assert!(!surface(twin), "no impl in item position: {twin}");
        }
        assert!(surface("fn f() { extern \"C\" { fn g(); } }"), "an extern block");
        for twin in ["fn f() { extern \"C\" fn g() {} }", "fn f() { extern crate alloc; }"] {
            assert!(!surface(twin), "a name of the block alone: {twin}");
        }
        for attr in [
            "#[derive(Clone)] struct S;",
            "#[my_attr] fn g() {}",
            "#[tokio::main] async fn g() {}",
            "#[macro_export] macro_rules! m { () => {} }",
            "#[no_mangle] extern \"C\" fn g() {}",
            "#[unsafe(no_mangle)] extern \"C\" fn g() {}",
            "#[export_name = \"g\"] fn g() {}",
            "#[used] static S: u8 = 0;",
            "#[lang = \"sized\"] trait Sized {}",
            "#[panic_handler] fn p(_: &PanicInfo) -> ! { loop {} }",
            "#[global_allocator] static A: Alloc = Alloc;",
            "#[cfg_attr(feature = \"serde\", derive(Serialize))] struct S;",
            "#[allow(dead_code)] #[derive(Debug)] struct S;",
        ] {
            let text = std::format!("fn f() {{ {attr} }}");
            assert!(surface(&text), "an emitting attribute: {attr}");
        }
        for inert in [
            "#[allow(dead_code)] struct S;",
            "#[inline] fn g() {}",
            "#[doc = concat!(\"a\", \"b\")] #[must_use] fn g() -> u8 { 0 }",
            "#[rustfmt::skip] #[cfg(test)] fn g() {}",
            "#[repr(C)] #[derive_not_here] struct S;",
            "macro_rules! m { () => { impl Tr for X {} } }",
            "struct S; enum E { A } trait T { fn f(&self); }",
        ] {
            let text = std::format!("fn f() {{ {inert} }}");
            let emits = inert.contains("derive_not_here");
            assert_eq!(surface(&text), emits, "inert unless an attribute is not: {inert}");
        }
        assert!(surface("fn f() { fn g() { impl Tr for X {} } }"), "a local fn's body");
        assert!(surface("fn f() { mod m { #[derive(Clone)] struct S; } }"), "a block mod's items");
        assert!(surface("fn f() { trait T { fn d(&self) { impl Tr for X {} } } }"), "a trait's default body");
        assert!(surface("const _: () = { use serde as _serde; impl _serde::Serialize for S { } };"), "serde's derive block");
        for twin in ["fn f() { fn g() { struct S; } }", "fn f() { mod m { struct S; } }", "fn f() { trait T { fn d(&self) { struct S; } } }"] {
            assert!(!surface(twin), "a plain item inside: {twin}");
        }
        let n = scan("fn f() { impl Tr for X {} struct S; }", heap.alloc());
        assert_eq!(n.items.iter().map(|i| i.surface).collect::<std::vec::Vec<_>>(), [true, false], "the bit is the item's own");
        let open = scan("fn f() { struct S; ", heap.alloc());
        assert!(open.end == NestedEnd::Unbalanced && open.surface_by_text, "a body that does not parse");
    }
}
