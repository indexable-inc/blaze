//! A crate's syntax extensions ([`Extensions`], docs/v2/plan/design/edition.md): the crate-level
//! attributes that change how the crate's files parse, `#![blaze::uniq]` and
//! `#![blaze::error_unions]` today.
//!
//! A file's parse depends on them, so they are read before any parse: off the tokens of the
//! crate root's leading inner attributes and of its command-line crate attributes
//! ([`crate::crate_attrs`]; every one, since which of them rustc applies is read off their
//! parses), never off a tree. Every file id made for the crate's files carries them
//! ([`EditionedFileId::new_with_extensions`](crate::EditionedFileId::new_with_extensions)),
//! which puts them in the key of every parse, and of every query keyed by the file, beside the
//! edition.
//!
//! Only the attribute written as itself is read: one that only a `cfg_attr` or a macro would
//! make turns no extension on, so the crate's `&uniq` or `A | B` type is then the syntax error
//! it is without the extension, never quietly another program (hir-def refuses an extension's
//! attribute under `cfg_attr`, naming the rule).

use span::{Edition, Extensions};
use syntax::{LexedStr, SyntaxKind, T};

use crate::{Crate, Db};

/// What turns each syntax extension on: its crate attribute's path, as its tokens' text
/// joined. One row per bit of [`Extensions`] (`every_extension_has_its_attribute` checks).
const EXTENSION_ATTRS: [(&str, Extensions); Extensions::ALL.len()] =
    [("blaze::uniq", Extensions::UNIQ), ("blaze::error_unions", Extensions::ERROR_UNIONS)];

impl Crate {
    /// The syntax extensions the crate's attributes turn on: see the module.
    pub fn extensions(self, db: &Db) -> Extensions {
        crate_extensions(db, self)
    }
}

/// [`Crate::extensions`]: a firewall, so an edit to the root file that keeps the extensions
/// re-parses none of the crate's files for them.
#[blaze_query::tracked(returns(copy))]
fn crate_extensions(db: &Db, krate: Crate) -> Extensions {
    let data = krate.data(db);
    let edition = data.edition;
    let root: &str = db.file_text(data.root_file_id).text(db);
    let mut extensions = leading_extensions(root, edition);
    for &file in krate.crate_attr_files(db) {
        extensions = extensions.with(leading_extensions(db.file_text(file).text(db), edition));
    }
    extensions
}

/// The extensions the leading inner attributes of `text` (a crate root, or a crate attribute's
/// file, `#![<attr>]`) turn on: each `#![...]` before the first other token, past a shebang,
/// a frontmatter, whitespace and comments, whose tokens are exactly an extension's path (so
/// one with arguments turns nothing on).
pub(crate) fn leading_extensions(text: &str, edition: Edition) -> Extensions {
    // alloc: the text's tokens, once per change of the crate root
    let lexed = LexedStr::new(edition, text);
    let tokens = (0..lexed.len()).filter(|&at| {
        let kind = lexed.kind(at);
        !kind.is_trivia() && !matches!(kind, SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER)
    });
    // alloc: as above, the indices of the non-trivia tokens
    let tokens: Vec<usize> = tokens.collect();
    let kind = |n: usize| tokens.get(n).map(|&at| lexed.kind(at));
    let mut extensions = Extensions::NONE;
    let mut n = 0;
    while kind(n) == Some(T![#]) && kind(n + 1) == Some(T![!]) && kind(n + 2) == Some(T!['[']) {
        let start = n + 3;
        let mut depth = 1usize;
        let mut end = start;
        while depth > 0 {
            match kind(end) {
                None => return extensions,
                Some(T!['('] | T!['['] | T!['{']) => depth += 1,
                Some(T![')'] | T![']'] | T!['}']) => depth -= 1,
                Some(_) => {}
            }
            end += 1;
        }
        // `end` is past the `]` that closes the attribute.
        let path = || tokens[start..end - 1].iter().map(|&at| lexed.text(at));
        if let Some(&(_, it)) = EXTENSION_ATTRS.iter().find(|(name, _)| path().eq(split(name))) {
            extensions = extensions.with(it);
        }
        n = end;
    }
    extensions
}

/// The tokens' text an extension's path lexes to: its names and each `::` as two `:`.
fn split(name: &str) -> impl Iterator<Item = &str> {
    name.split("::").enumerate().flat_map(|(n, part)| {
        let colons: &[&str] = if n == 0 { &[] } else { &[":", ":"] };
        colons.iter().copied().chain(std::iter::once(part))
    })
}

#[cfg(test)]
mod tests {
    use span::{Edition, Extensions};

    use super::{EXTENSION_ATTRS, leading_extensions};

    #[test]
    fn the_extensions_are_read_off_the_leading_inner_attributes() {
        let both = Extensions::UNIQ.with(Extensions::ERROR_UNIONS);
        let cases: &[(&str, Extensions)] = &[
            ("#![blaze::uniq]\nfn f() {}\n", Extensions::UNIQ),
            ("#!/usr/bin/env run\n//! docs\n#![allow(x)]\n#![blaze :: uniq]\n", Extensions::UNIQ),
            ("#![cfg_attr(all(), deny(x))] #![blaze::uniq]", Extensions::UNIQ),
            ("", Extensions::NONE),
            ("fn f() {}\n#![blaze::uniq]\n", Extensions::NONE),
            ("#![blaze::uniq(x)]", Extensions::NONE),
            ("#![cfg_attr(all(), blaze::uniq)]", Extensions::NONE),
            ("#![blaze::arbitrary_ints]", Extensions::NONE),
            ("#[blaze::uniq]\nfn f() {}\n", Extensions::NONE),
            ("#![blaze::uniq", Extensions::NONE),
            ("#![blaze::error_unions]\nfn f() {}\n", Extensions::ERROR_UNIONS),
            ("//! docs\n#![blaze::error_unions]\n#![blaze::uniq]\n", both),
            ("#![blaze::uniq] #![allow(x)] #![blaze :: error_unions]", both),
            ("#![blaze::error_unions(x)]", Extensions::NONE),
            ("#![cfg_attr(all(), blaze::error_unions)]", Extensions::NONE),
            ("#![bla ze::error_unions]", Extensions::NONE),
            ("#![blaze::uniq]\nfn f() {}\n#![blaze::error_unions]\n", Extensions::UNIQ),
        ];
        for &(text, want) in cases {
            assert_eq!(leading_extensions(text, Edition::Edition2021), want, "{text:?}");
        }
    }

    #[test]
    fn every_extension_has_its_attribute() {
        for (n, extension) in Extensions::ALL.into_iter().enumerate() {
            assert_eq!(EXTENSION_ATTRS[n].1, extension);
            let text = format!("#![{}]", EXTENSION_ATTRS[n].0);
            assert_eq!(leading_extensions(&text, Edition::Edition2021), extension, "{text}");
        }
    }
}
