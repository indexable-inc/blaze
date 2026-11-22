//! The `mod NAME;` items written LITERALLY in the tokens of a macro call: what the file walk
//! plans for a module a macro's expansion declares, before anything is expanded.
//!
//! The Reference, "Modules": "a module without a body is loaded from an external file", at
//! `name.rs` or `name/mod.rs` relative to the module the item is in, or at its `#[path]`. An
//! item an expansion writes is in the module of the CALL: rustc's expander resolves the file of
//! a `mod name;` it meets in an expansion against `ecx.current_expansion.module.dir_path` and
//! its `DirOwnership` (rustc_expand expand.rs, `ModKind::Unloaded` -> module.rs
//! `parse_external_mod` -> `mod_file_path` -> `default_submod_path`, "If we're in a foo.rs
//! file instead of a mod.rs file, we need to look for submodules in `./foo/<ident>.rs`"), the
//! directory of the module the call stands in, never the macro's definition. blaze's names
//! step does the same (blaze-scope resolve.rs `make_modules`, from `place_of(parent)`).
//!
//! The planner cannot know what an arbitrary macro writes before it is expanded, so this is a
//! SUPERSET for planning, by tokens alone: every `mod IDENT ;` of the call's input, inside any
//! group (`cfg_if! { if #[cfg(unix)] { mod unix; } else { mod other; } }`, turso's
//! `cfg_block! { #[cfg(all(target_family = "unix", not(miri)))] { mod unix; .. } }`), with the
//! inline `mod NAME { .. }` written around it in the tokens and a `#[path = ".."]` among the
//! outer attributes right before it. Both arms of a `cfg` are taken: cfg is not known here. A
//! module a macro writes from a COMPUTED name (`mod $name;`, a proc macro's output) has no such
//! tokens: it is not found here (blaze-cli engine_names.rs `finish` names it when it is read).
//!
//! What an extra one costs: the file walk lists it as a part of the calling file, so the sweep
//! probes its slot (a stat; `name.rs` and `name/mod.rs`), and only when a file is there is it
//! read and outlined once; a slot with no file is an absent answer and no read. A planned
//! module the expansion does not write (a cfg arm that is off, a `mod x;` inside a matcher or a
//! nested `macro_rules!`) is read by no crate's names: its outline is a record nobody reads, and
//! its bodies are those of a module no crate holds (the same as a `#[cfg(windows)] mod sys;`
//! written plainly). The list is at most one entry per three tokens of the input.

use blaze_lex::{Cpu, FrontmatterAllowed, LiteralKind, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};
use blaze_surface::Range;

/// One module the tokens declare: the range of its name (a raw `r#name` without its `r#`) and of
/// the string literal of its `#[path = ".."]`, quotes and escapes as written; both relative to
/// the text [`token_mods`] was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenSegment {
    pub name: Range,
    pub path: Option<Range>,
}

/// A `mod NAME;` of the tokens: itself, and the inline modules around it in the tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenMod {
    pub leaf: TokenSegment,
    /// Its inline modules, outermost first: a range of [`TokenMods::inline`].
    pub inline: Range,
}

/// What [`token_mods`] found: each `mod NAME;` in text order, and the inline modules around
/// them (each one's own copy of its chain, so an entry is read alone).
#[derive(Debug)]
pub struct TokenMods<'a> {
    pub mods: AVec<'a, TokenMod>,
    pub inline: AVec<'a, TokenSegment>,
}

impl TokenMods<'_> {
    /// The inline modules around `one`, outermost first.
    pub fn chain(&self, one: &TokenMod) -> &[TokenSegment] {
        &self.inline[one.inline.start as usize..one.inline.end as usize]
    }
}

/// Whether `text` (a macro call's input) may hold a `mod NAME;` [`token_mods`] finds: a filter
/// before the lex, never false for a text that holds one.
pub fn may_hold_mods(text: &str) -> bool {
    text.contains("mod")
}

fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("a macro call's input is counted in u32")
}

/// A significant token of the input: its kind and its range.
#[derive(Clone, Copy)]
struct Tok {
    kind: TokenKind,
    start: u32,
    end: u32,
}

/// The `mod NAME;` items written in `text`, a macro call's input (its token tree, brackets
/// included), with the inline modules around each in the tokens and each one's `#[path]`
/// (module docs). One lex of the text; a superset for planning, never an answer of what the
/// expansion writes.
pub fn token_mods<'a>(text: &str, alloc: Alloc<'a>) -> TokenMods<'a> {
    let mut out = TokenMods { mods: AVec::new_in(alloc), inline: AVec::new_in(alloc) };
    let mut toks: AVec<'a, Tok> = AVec::new_in(alloc);
    let mut at = 0usize;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = at;
        at += token.len as usize;
        if matches!(token.kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        toks.push(Tok { kind: token.kind, start: offset(start), end: offset(at) });
    }
    let word = |t: &Tok| &text[t.start as usize..t.end as usize];
    let is_word = |t: &Tok, w: &str| t.kind == TokenKind::Ident && word(t) == w;
    let is_name = |t: &Tok| matches!(t.kind, TokenKind::Ident | TokenKind::RawIdent);
    // The groups open: `true` for an inline module's body, `false` for any other.
    let mut stack: AVec<'a, bool> = AVec::new_in(alloc);
    // The inline modules open, outermost first: indexes into `segments`.
    let mut open: AVec<'a, u32> = AVec::new_in(alloc);
    let mut segments: AVec<'a, TokenSegment> = AVec::new_in(alloc);
    // The `#[path = ".."]` literal among the outer attributes read since the last token that was
    // none of an attribute, a visibility or `mod`.
    let mut path: Option<Range> = None;
    let mut i = 0usize;
    while i < toks.len() {
        let t = toks[i];
        match t.kind {
            // `#[..]` or `#![..]`: skipped whole; a `path = "lit"` one is kept for the `mod`.
            TokenKind::Pound => {
                let mut j = i + 1;
                let inner = toks.get(j).is_some_and(|n| n.kind == TokenKind::Bang);
                if inner {
                    j += 1;
                }
                if toks.get(j).is_none_or(|n| n.kind != TokenKind::OpenBracket) {
                    path = None;
                    i += 1;
                    continue;
                }
                let Some(close) = matching(&toks, j) else { break };
                let lit = toks.get(j + 3).filter(|l| matches!(l.kind, TokenKind::Literal { kind: LiteralKind::Str { terminated: true } | LiteralKind::RawStr { n_hashes: Some(_) }, .. }));
                if !inner && close == j + 4 && is_word(&toks[j + 1], "path") && toks[j + 2].kind == TokenKind::Eq && let Some(lit) = lit {
                    path = Some(Range { start: lit.start, end: lit.end });
                }
                i = close + 1;
            }
            // A visibility before the `mod`: `pub`, `pub(crate)`, `pub(in a::b)`.
            TokenKind::Ident if word(&t) == "pub" => {
                i += 1;
                if toks.get(i).is_some_and(|n| n.kind == TokenKind::OpenParen) {
                    let Some(close) = matching(&toks, i) else { break };
                    i = close + 1;
                }
            }
            TokenKind::Ident if word(&t) == "mod" && toks.get(i + 1).is_some_and(is_name) => {
                let n = toks[i + 1];
                // `r#name`: its file is `name.rs` (the names step reads the name unraw).
                let raw = if n.kind == TokenKind::RawIdent { 2 } else { 0 };
                let segment = TokenSegment { name: Range { start: n.start + raw, end: n.end }, path: path.take() };
                match toks.get(i + 2).map(|t| t.kind) {
                    Some(TokenKind::Semi) => {
                        let first = offset(out.inline.len());
                        out.inline.extend(open.iter().map(|&s| segments[s as usize]));
                        out.mods.push(TokenMod { leaf: segment, inline: Range { start: first, end: offset(out.inline.len()) } });
                        i += 3;
                    }
                    Some(TokenKind::OpenBrace) => {
                        let s = offset(segments.len());
                        segments.push(segment);
                        open.push(s);
                        stack.push(true);
                        i += 3;
                    }
                    _ => i += 2,
                }
            }
            TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket => {
                stack.push(false);
                path = None;
                i += 1;
            }
            TokenKind::CloseBrace | TokenKind::CloseParen | TokenKind::CloseBracket => {
                // An unbalanced closer is the outline's syntax error to report; here it closes
                // nothing.
                if stack.pop() == Some(true) {
                    open.pop();
                }
                path = None;
                i += 1;
            }
            _ => {
                path = None;
                i += 1;
            }
        }
    }
    out
}

/// The index of the closer of the group `toks[open]` opens, or `None` when the text ends first.
fn matching(toks: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0u32;
    for (at, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokenKind::OpenBrace | TokenKind::OpenParen | TokenKind::OpenBracket => depth += 1,
            TokenKind::CloseBrace | TokenKind::CloseParen | TokenKind::CloseBracket => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::borrow::ToOwned;
    use std::{format, string::String, vec::Vec};

    use super::*;

    /// One `mod NAME;` found: its inline chain (`name`, or `name@literal` for a `#[path]` one),
    /// its name and its `#[path]` literal, by their text.
    type Found = (Vec<String>, String, Option<String>);

    /// Each `mod NAME;` of `text`, in order.
    fn found(text: &str) -> Vec<Found> {
        let got = token_mods(text, blaze_own::testing::heap().alloc());
        let of = |r: Range| text[r.start as usize..r.end as usize].to_owned();
        got.mods
            .iter()
            .map(|m| {
                let chain = got.chain(m).iter().map(|s| match s.path {
                    Some(p) => format!("{}@{}", of(s.name), of(p)),
                    None => of(s.name),
                });
                (chain.collect(), of(m.leaf.name), m.leaf.path.map(of))
            })
            .collect()
    }

    /// A `mod NAME;` with no inline module around it in the tokens and no `#[path]`.
    fn plain(name: &str) -> Found {
        (Vec::new(), name.to_owned(), None)
    }

    /// turso's `core/io/mod.rs` (2026-11-21): `cfg_block!`'s three arms, each `mod` in a brace
    /// group under an attribute; every arm is found (cfg is not read here), each `pub use` is
    /// no module.
    #[test]
    fn cfg_block_arms_are_all_found() {
        let text = "{\n    #[cfg(all(target_os = \"linux\", feature = \"io_uring\", not(miri)))] {\n        mod io_uring;\n        #[cfg(feature = \"fs\")]\n        pub use io_uring::UringIO;\n    }\n\n    #[cfg(all(target_family = \"unix\", not(miri)))] {\n        mod unix;\n        pub use unix::UnixIO as PlatformIO;\n    }\n\n    #[cfg(all(target_os = \"windows\", not(miri)))] {\n        mod windows_lock;\n        mod windows;\n    }\n}";
        assert_eq!(found(text), [plain("io_uring"), plain("unix"), plain("windows_lock"), plain("windows")]);
    }

    /// cfg-if's shape: `if #[cfg(..)] { .. } else { .. }`, both arms.
    #[test]
    fn cfg_if_arms_are_both_found() {
        assert_eq!(found("{ if #[cfg(unix)] { mod unix; pub use self::unix::*; } else if #[cfg(windows)] { pub(crate) mod windows; } else { mod other; } }"), [plain("unix"), plain("windows"), plain("other")]);
    }

    /// An inline module in the tokens is a directory for the `mod` inside it; a `#[path]`
    /// right before a `mod` (after other attributes and a visibility) is its file's; a raw name
    /// is its file's name without `r#`.
    #[test]
    fn inline_modules_paths_and_raw_names() {
        let got = found("{ mod outer { #[cfg(x)] pub mod c; #[path = \"q.rs\"] mod d; } #[doc = \"z\"] #[path = r\"p.rs\"] pub(crate) mod e; mod r#type; #[path = \"dir\"] mod pd { mod f; } }");
        let outer = || Vec::from(["outer".to_owned()]);
        assert_eq!(
            got,
            [
                (outer(), "c".to_owned(), None),
                (outer(), "d".to_owned(), Some("\"q.rs\"".to_owned())),
                (Vec::new(), "e".to_owned(), Some("r\"p.rs\"".to_owned())),
                plain("type"),
                (Vec::from(["pd@\"dir\"".to_owned()]), "f".to_owned(), None),
            ]
        );
    }

    /// No module: a name a macro computes (`mod $name;`), a `mod` keyword with no `;` or body
    /// after its name, and an attribute's `path` that is not the whole attribute; a `#[path]`
    /// does not reach past another token to a later `mod`.
    #[test]
    fn what_is_no_planned_module() {
        assert!(found("{ mod $name; macro_rules! m { ($n:ident) => { mod $n; } } }").is_empty());
        assert!(found("{ mod a = 1; }").is_empty());
        assert_eq!(found("{ #[cfg_attr(x, path = \"y.rs\")] mod a; }"), [plain("a")]);
        assert_eq!(found("{ #[path = \"y.rs\"] struct S; mod a; }"), [plain("a")]);
        assert!(found("( a, b )").is_empty());
    }

    /// An inline module closed in the tokens no longer encloses what follows it.
    #[test]
    fn a_closed_inline_module_encloses_nothing_after() {
        assert_eq!(found("{ mod m { fn f() { } } mod after; }"), [plain("after")]);
    }
}
