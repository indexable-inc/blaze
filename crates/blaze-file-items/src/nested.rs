//! The items declared in the blocks of ONE body-like text (a `fn`, `const` or `static` item's
//! whole text), found by tokens: no tree, no lowering, no recursion.
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
//! at the `}` closing its first `{` outside brackets. Nothing inside an item is read: its own
//! bodies are other texts, scanned when they are lowered. A word inside a string or a comment
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
    ExternBlock,
}

/// An item statement: its block and its text's range (outer attributes included).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedItem {
    pub block: u32,
    pub kind: NestedKind,
    pub range: Range,
}

/// A macro call in statement position: its block, the range of its path (`a::b` of
/// `a::b!(..)`, with the outer attributes written before it) and the range of the whole call
/// (from the path's start through the bracket closing its arguments; a `;` after it is not in
/// it). Not expanded here: the items it may write are not in [`Nested::items`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NestedCall {
    pub block: u32,
    pub path: Range,
    pub range: Range,
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
}

impl Nested<'_> {
    /// Whether any block holds an `impl`.
    pub fn has_impl(&self) -> bool {
        self.items.iter().any(|i| i.kind == NestedKind::Impl)
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
    /// `const`: a name (an item), `fn` and its qualifiers, or `{` (an inline const).
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
fn not_a_macro(word: &str) -> bool {
    matches!(word, "if" | "while" | "match" | "return" | "in" | "break" | "yield" | "else" | "move" | "mut" | "ref")
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("an item's text is counted in u32")
}

/// The blocks of `text` (one item's whole text) and the item statements in them.
pub fn scan<'a>(text: &str, alloc: Alloc<'a>) -> Nested<'a> {
    let mut out = Nested { blocks: AVec::new_in(alloc), items: AVec::new_in(alloc), calls: AVec::new_in(alloc), end: NestedEnd::Whole };
    let mut stack: AVec<'a, Open> = AVec::new_in(alloc);
    // How many of the open brackets are a macro call's arguments.
    let mut in_call = 0u32;
    let mut skip: Option<Skip> = None;
    // At a statement's start (only ever directly in a brace group).
    let mut fresh = false;
    // Where the statement started, when attributes or a visibility came first.
    let mut stmt_start: Option<u32> = None;
    let mut lead = Lead::None;
    // The start of a path that began the statement (a statement call's name).
    let mut path_start: Option<u32> = None;
    // What the last tokens were, for `#[`, `#![`, `pub(` and `name!(`.
    let mut after_pound = false;
    let mut after_pub = false;
    let mut after_ident = false;
    let mut after_bang_of: Option<Option<Range>> = None;
    // The listed statement call whose arguments are open: its range ends at their closer.
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
                out.items.push(NestedItem { block: done.block, kind: done.kind, range: Range { start: done.start, end } });
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
                    out.items.push(NestedItem { block: done.block, kind: done.kind, range: Range { start: done.start, end: offset(at) } });
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
        let block = match stack.last() {
            Some(Open::Brace(b)) => Some(*b),
            _ => None,
        };
        if is_open {
            // The Reference, "Attributes": an attribute's input is any "delimited token
            // tree" (`#[allow(a, b)]`, `#[doc(hidden)]`, `#[cfg_attr(x, derive(Y))]`,
            // `#[doc = concat!("a", "b")]`). Every group inside an attribute's brackets is
            // the attribute's: it opens no block, is no call, and leaves the statement at its
            // start, so the item written after the attribute is still found.
            let in_attr = matches!(stack.last(), Some(Open::Other { stmt: true }));
            let open = match (kind, bang_of) {
                _ if in_attr => Open::Other { stmt: true },
                // `name!(..)`, `name![..]`, `name!{..}`: the arguments are not read.
                (_, Some(path)) => {
                    if let (Some(path), Some(block)) = (path, block) {
                        open_call = Some(out.calls.len());
                        out.calls.push(NestedCall { block, path, range: Range { start: path.start, end: path.end } });
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
                    continue;
                }
                (TokenKind::OpenBrace, None) => {
                    let id = offset(out.blocks.len());
                    out.blocks.push(Block { parent: stack.iter().rev().find_map(|o| if let Open::Brace(b) = o { Some(*b) } else { None }), open: offset(start), close: 0 });
                    Open::Brace(id)
                }
                // `#[..]`, `#![..]` and `pub(..)` leave the statement at its start.
                (TokenKind::OpenBracket, None) => Open::Other { stmt: fresh && was_pound },
                _ => Open::Other { stmt: fresh && was_pub },
            };
            let keeps = matches!(open, Open::Other { stmt: true });
            stack.push(open);
            if !keeps {
                fresh = matches!(open, Open::Brace(_));
                stmt_start = None;
            }
            lead = Lead::None;
            path_start = None;
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
                Some(Open::Other { stmt: true }) => {}
                Some(_) => {
                    fresh = false;
                    stmt_start = None;
                }
            }
            lead = Lead::None;
            path_start = None;
            continue;
        }
        let Some(block) = block else {
            // Outside every block (the item's own header), or directly in a paren or square
            // group: no statement starts here.
            after_ident = matches!(kind, TokenKind::Ident | TokenKind::RawIdent) && !not_a_macro(&text[start..at]);
            if kind == TokenKind::Bang && was_ident {
                after_bang_of = Some(None);
            }
            continue;
        };
        match kind {
            TokenKind::Semi => {
                fresh = true;
                stmt_start = None;
                lead = Lead::None;
                path_start = None;
            }
            TokenKind::Pound if fresh && lead == Lead::None => {
                stmt_start.get_or_insert(offset(start));
                after_pound = true;
            }
            // `#!` of an inner attribute.
            TokenKind::Bang if fresh && was_pound => after_pound = true,
            TokenKind::Bang if fresh && lead == Lead::MacroRules => {
                skip = Some(Skip { kind: NestedKind::MacroRules, start: stmt_start.unwrap_or(offset(start)), block, body: true, depth: 0, in_body: false, closed: None });
                lead = Lead::None;
                stmt_start = None;
            }
            TokenKind::Bang if was_ident => {
                after_bang_of = Some(path_start.take().map(|p| Range { start: p, end: offset(start) }));
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
                    (Lead::Const, "unsafe" | "async" | "extern" | "safe") => None,
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
                    continue;
                }
                let next = match (lead, word) {
                    (Lead::None, "pub") => {
                        after_pub = true;
                        stmt_start = Some(begin);
                        continue;
                    }
                    (Lead::Const, "unsafe" | "async" | "safe") => Lead::Const,
                    (Lead::Const, "extern") => Lead::Extern,
                    (Lead::None | Lead::Qualifier, w) => lead_word(w),
                    _ => Lead::None,
                };
                if next == Lead::None {
                    // An expression or a `let`: the statement is under way.
                    fresh = false;
                    stmt_start = None;
                    path_start = (lead == Lead::None && !not_a_macro(word)).then_some(begin);
                    after_ident = !not_a_macro(word);
                } else {
                    stmt_start = Some(begin);
                    // `union` and `default` may be plain names: `union!(..)`, `default.x`.
                    after_ident = true;
                    path_start = Some(begin);
                }
                lead = next;
            }
            TokenKind::Ident | TokenKind::RawIdent => {
                after_ident = !not_a_macro(&text[start..at]);
                fresh = false;
                lead = Lead::None;
                stmt_start = None;
            }
            // `a::b!(..)`: the path of a statement call goes on.
            TokenKind::Colon if path_start.is_some() => {
                fresh = false;
                lead = Lead::None;
            }
            _ => {
                fresh = false;
                lead = Lead::None;
                stmt_start = None;
                path_start = None;
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
    use blaze_own::Alloc;

    /// (kind, block, text) of each item found.
    fn items(text: &str) -> std::vec::Vec<(NestedKind, u32, &str)> {
        let n = scan(text, Alloc::global());
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
        let n = scan(text, Alloc::global());
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
        let n = scan("fn f() { a::b!{ impl G for H {} } c!(); let x = d!(); if !(x) { e! { } } }", Alloc::global());
        assert!(n.items.is_empty());
        let text = "fn f() { a::b!{ impl G for H {} } c!(); let x = d!(); if !(x) { e! { } } }";
        let calls: std::vec::Vec<&str> = n.calls.iter().map(|c| &text[c.path.start as usize..c.path.end as usize]).collect();
        assert_eq!(calls, ["a::b", "c", "e"], "statement calls only");
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
        let n = scan(text, Alloc::global());
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
    /// are the attribute's too.
    #[test]
    fn every_attribute_shape_keeps_its_item() {
        let got = items("fn deserialize() { #[allow(non_camel_case_types)] #[doc(hidden)] enum __Field { __field0, __ignore } #[doc(hidden)] struct __FieldVisitor; #[automatically_derived] #[doc(hidden)] impl A for __Field {} #[doc(hidden)] #[allow(dead_code)] type UnderlyingSolTuple<'a> = (u8,); #[cfg_attr(feature = \"x\", derive(Clone, PartialEq), repr(C))] pub(crate) struct P(u8); #[doc = concat!(\"a\", \"b\")] #[x(y = { impl Not for Counted {} })] #![inner(z)] fn after() {} let v = 1; #[cfg(test)] { v } }");
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

    /// A statement call's whole range: through the bracket closing its arguments, never the
    /// `;` after it; a call that is not in statement position is not listed.
    #[test]
    fn a_statement_call_has_its_whole_range() {
        let text = "fn f() { a::b!{ impl G for H {} } c!(1, (2)); let x = d!(); e![x]; }";
        let n = scan(text, Alloc::global());
        let calls: std::vec::Vec<&str> = n.calls.iter().map(|c| &text[c.range.start as usize..c.range.end as usize]).collect();
        assert_eq!(calls, ["a::b!{ impl G for H {} }", "c!(1, (2))", "e![x]"]);
    }

    #[test]
    fn an_unclosed_body_is_not_used() {
        assert_eq!(scan("fn f() { impl A for B {", Alloc::global()).end, NestedEnd::Unbalanced);
        assert_eq!(scan("fn f() { } }", Alloc::global()).end, NestedEnd::Unbalanced);
    }
}
