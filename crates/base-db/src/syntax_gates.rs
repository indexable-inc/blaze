//! A crate's syntax gates ([`SyntaxGates`], docs/v2/plan/design/edition.md): the crate-level
//! attributes that change how the crate's files parse, `#![blaze::uniq]` alone today.
//!
//! A file's parse depends on them, so they are read before any parse: off the tokens of the
//! crate root's leading inner attributes and of its command-line crate attributes
//! ([`crate::crate_attrs`]), never off a tree. Every file id made for the crate's files carries
//! them ([`EditionedFileId::new_gated`](crate::EditionedFileId::new_gated)), which puts them in
//! the key of every parse, and of every query keyed by the file, beside the edition.
//!
//! Only the attribute written as itself is read: one that only a `cfg_attr` or a macro would
//! make turns no gate on, so the crate's `&uniq` is then the syntax error it is without the
//! gate, never quietly another program.

use span::{Edition, SyntaxGates};
use syntax::{LexedStr, SyntaxKind, T};

use crate::{Crate, Db};

/// The attribute paths of the gates, as their tokens' text joined: `blaze::uniq`.
const GATES: &[(&str, SyntaxGates)] = &[("blaze::uniq", SyntaxGates::UNIQ)];

impl Crate {
    /// The syntax gates the crate's attributes turn on: see the module.
    pub fn syntax_gates(self, db: &Db) -> SyntaxGates {
        crate_syntax_gates(db, self)
    }
}

/// [`Crate::syntax_gates`]: a firewall, so an edit to the root file that keeps the gates
/// re-parses none of the crate's files for them.
#[blaze_query::tracked(returns(copy))]
fn crate_syntax_gates(db: &Db, krate: Crate) -> SyntaxGates {
    let data = krate.data(db);
    let edition = data.edition;
    let root: &str = db.file_text(data.root_file_id).text(db);
    let mut gates = leading_gates(root, edition);
    for &file in krate.crate_attr_files(db) {
        gates = gates.with(leading_gates(db.file_text(file).text(db), edition));
    }
    gates
}

/// The gates the leading inner attributes of `text` (a crate root, or a crate attribute's
/// file, `#![<attr>]`) turn on: each `#![...]` before the first other token, past a shebang,
/// a frontmatter, whitespace and comments, whose path is a gate's and that has no arguments.
pub(crate) fn leading_gates(text: &str, edition: Edition) -> SyntaxGates {
    // alloc: the text's tokens, once per change of the crate root
    let lexed = LexedStr::new(edition, text);
    let tokens = (0..lexed.len()).filter(|&at| {
        let kind = lexed.kind(at);
        !kind.is_trivia() && !matches!(kind, SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER)
    });
    // alloc: as above, the indices of the non-trivia tokens
    let tokens: Vec<usize> = tokens.collect();
    let kind = |n: usize| tokens.get(n).map(|&at| lexed.kind(at));
    let mut gates = SyntaxGates::NONE;
    let mut n = 0;
    while kind(n) == Some(T![#]) && kind(n + 1) == Some(T![!]) && kind(n + 2) == Some(T!['[']) {
        let start = n + 3;
        let mut depth = 1usize;
        let mut end = start;
        while depth > 0 {
            match kind(end) {
                None => return gates,
                Some(T!['('] | T!['['] | T!['{']) => depth += 1,
                Some(T![')'] | T![']'] | T!['}']) => depth -= 1,
                Some(_) => {}
            }
            end += 1;
        }
        // `end` is past the `]` that closes the attribute.
        let path = || tokens[start..end - 1].iter().map(|&at| lexed.text(at));
        if let Some(&(_, gate)) = GATES.iter().find(|(name, _)| path().eq(split(name))) {
            gates = gates.with(gate);
        }
        n = end;
    }
    gates
}

/// The tokens' text a gate's path lexes to: its names and each `::` as two `:`.
fn split(name: &str) -> impl Iterator<Item = &str> {
    name.split("::").enumerate().flat_map(|(n, part)| {
        let colons: &[&str] = if n == 0 { &[] } else { &[":", ":"] };
        colons.iter().copied().chain(std::iter::once(part))
    })
}

#[cfg(test)]
mod tests {
    use span::{Edition, SyntaxGates};

    use super::leading_gates;

    #[test]
    fn the_gate_is_read_off_the_leading_inner_attributes() {
        let cases: &[(&str, SyntaxGates)] = &[
            ("#![blaze::uniq]\nfn f() {}\n", SyntaxGates::UNIQ),
            ("#!/usr/bin/env run\n//! docs\n#![allow(x)]\n#![blaze :: uniq]\n", SyntaxGates::UNIQ),
            ("#![cfg_attr(all(), deny(x))] #![blaze::uniq]", SyntaxGates::UNIQ),
            ("", SyntaxGates::NONE),
            ("fn f() {}\n#![blaze::uniq]\n", SyntaxGates::NONE),
            ("#![blaze::uniq(x)]", SyntaxGates::NONE),
            ("#![cfg_attr(all(), blaze::uniq)]", SyntaxGates::NONE),
            ("#![blaze::arbitrary_ints]", SyntaxGates::NONE),
            ("#[blaze::uniq]\nfn f() {}\n", SyntaxGates::NONE),
            ("#![blaze::uniq", SyntaxGates::NONE),
        ];
        for &(text, want) in cases {
            assert_eq!(leading_gates(text, Edition::Edition2021), want, "{text:?}");
        }
    }
}
