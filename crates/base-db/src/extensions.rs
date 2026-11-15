//! A crate's syntax extensions ([`Extensions`], docs/v2/plan/design/edition.md): the crate-level
//! attributes that change how the crate's files parse: `#![blaze::uniq]`,
//! `#![blaze::error_unions]` and `#![blaze::records]` today.
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

use span::{BlazeEdition, Edition, Extensions};
use syntax::{LexedStr, SyntaxKind, T};

use crate::{Crate, Db};

/// What turns each syntax extension on: its crate attribute's path, as its tokens' text
/// joined. One row per bit of [`Extensions`] (`every_extension_has_its_attribute` checks).
const EXTENSION_ATTRS: [(&str, Extensions); Extensions::ALL.len()] =
    [
        ("blaze::uniq", Extensions::UNIQ),
        ("blaze::error_unions", Extensions::ERROR_UNIONS),
        ("blaze::records", Extensions::RECORDS),
    ];

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
    let mut extensions = Extensions::NONE;
    leading_attrs(text, edition, |attr| {
        if let Some(&(_, it)) =
            EXTENSION_ATTRS.iter().find(|(name, _)| attr.iter().copied().eq(split(name)))
        {
            extensions = extensions.with(it);
        }
    });
    extensions
}

/// Each leading inner attribute of `text` (a crate root, or a crate attribute's file,
/// `#![<attr>]`), in order, as the text of its tokens between `#![` and its `]`: each
/// `#![...]` before the first other token, past a shebang, a frontmatter, whitespace and
/// comments. One never closed ends the walk and is not given.
fn leading_attrs<'t>(text: &'t str, edition: Edition, mut each: impl FnMut(&[&str])) {
    // alloc: the text's tokens, once per change of the crate root
    let lexed = LexedStr::new(edition, text);
    let tokens = (0..lexed.len()).filter(|&at| {
        let kind = lexed.kind(at);
        // An inner doc comment (`//! ..`, `/*! .. */`) is a token of its own kind, not
        // trivia, and stands where an inner attribute does: a root that opens with its docs
        // still has its attributes read.
        !kind.is_trivia()
            && !matches!(
                kind,
                SyntaxKind::SHEBANG | SyntaxKind::FRONTMATTER | SyntaxKind::INNER_DOC_COMMENT
            )
    });
    // alloc: as above, the indices of the non-trivia tokens
    let tokens: Vec<usize> = tokens.collect();
    let kind = |n: usize| tokens.get(n).map(|&at| lexed.kind(at));
    // alloc: one attribute's tokens at a time, reused
    let mut attr: Vec<&str> = Vec::new();
    let mut n = 0;
    while kind(n) == Some(T![#]) && kind(n + 1) == Some(T![!]) && kind(n + 2) == Some(T!['[']) {
        let start = n + 3;
        let mut depth = 1usize;
        let mut end = start;
        while depth > 0 {
            match kind(end) {
                None => return,
                Some(T!['('] | T!['['] | T!['{']) => depth += 1,
                Some(T![')'] | T![']'] | T!['}']) => depth -= 1,
                Some(_) => {}
            }
            end += 1;
        }
        // `end` is past the `]` that closes the attribute.
        attr.clear();
        attr.extend(tokens[start..end - 1].iter().map(|&at| lexed.text(at)));
        each(&attr);
        n = end;
    }
}

/// What a crate's leading inner attributes say of its blaze edition
/// ([`Crate::blaze_edition`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SaidBlazeEdition {
    /// No `#![blaze::edition = ".."]`: a crate of Rust's own edition.
    None,
    /// The one edition every such attribute names.
    Is(BlazeEdition),
    /// An attribute naming a version that is no blaze edition's, as written (the string
    /// literal's text, quotes included), or one whose value is no string literal.
    Unknown(Box<str>),
    /// Two attributes that name two editions: the manifest's and the root file's own.
    Two(BlazeEdition, BlazeEdition),
}

impl Crate {
    /// The blaze edition the crate is of (docs/v2/plan/design/std.md, "The blaze edition:
    /// opting in"): what `#![blaze::edition = "2026.11"]` says, among its root's leading
    /// inner attributes or its command-line crate attributes, which is where the manifest's
    /// `edition = "blaze-2026.11"` puts it. `None` for a crate without one, which is Rust's,
    /// unchanged, and for one whose attribute is refused ([`Crate::said_blaze_edition`],
    /// reported by the crate's checks): a crate is never quietly of an edition it did not
    /// name.
    pub fn blaze_edition(self, db: &Db) -> Option<BlazeEdition> {
        match crate_blaze_edition(db, self) {
            SaidBlazeEdition::Is(it) => Some(*it),
            SaidBlazeEdition::None | SaidBlazeEdition::Unknown(_) | SaidBlazeEdition::Two(..) => None,
        }
    }

    /// What the crate's attributes say of its blaze edition, a refused one included.
    pub fn said_blaze_edition(self, db: &Db) -> &SaidBlazeEdition {
        crate_blaze_edition(db, self)
    }
}

/// [`Crate::said_blaze_edition`]: a firewall, as [`crate_extensions`] is, so an edit to the
/// root file that keeps its edition re-runs nothing that asks for it.
#[blaze_query::tracked(returns(ref))]
fn crate_blaze_edition(db: &Db, krate: Crate) -> SaidBlazeEdition {
    let data = krate.data(db);
    let edition = data.edition;
    let root: &str = db.file_text(data.root_file_id).text(db);
    let mut said = SaidBlazeEdition::None;
    leading_blaze_edition(root, edition, &mut said);
    for &file in krate.crate_attr_files(db) {
        leading_blaze_edition(db.file_text(file).text(db), edition, &mut said);
    }
    said
}

/// `said`, with what the leading inner attributes of `text` add: each
/// `#![blaze::edition = "<version>"]`, read off its tokens as an extension's attribute is
/// (written as itself: one a `cfg_attr` or a macro would make says nothing).
pub(crate) fn leading_blaze_edition(text: &str, edition: Edition, said: &mut SaidBlazeEdition) {
    leading_attrs(text, edition, |attr| {
        let path = split(BlazeEdition::ATTR);
        let n = path.clone().count();
        if attr.len() < n || !attr[..n].iter().copied().eq(path) {
            return;
        }
        let named = match &attr[n..] {
            ["=", literal] => literal
                .strip_prefix('"')
                .and_then(|it| it.strip_suffix('"'))
                .and_then(BlazeEdition::from_version)
                .ok_or(*literal),
            rest => Err(rest.first().copied().unwrap_or("")),
        };
        *said = match (&*said, named) {
            (SaidBlazeEdition::Unknown(_) | SaidBlazeEdition::Two(..), _) => return,
            // alloc: error path, the refused value
            (_, Err(written)) => SaidBlazeEdition::Unknown(Box::from(written)),
            (SaidBlazeEdition::None, Ok(it)) => SaidBlazeEdition::Is(it),
            (SaidBlazeEdition::Is(was), Ok(it)) if *was == it => return,
            (SaidBlazeEdition::Is(was), Ok(it)) => SaidBlazeEdition::Two(*was, it),
        };
    });
}

/// The tokens' text an extension's path lexes to: its names and each `::` as two `:`.
fn split(name: &str) -> impl Iterator<Item = &str> + Clone {
    name.split("::").enumerate().flat_map(|(n, part)| {
        let colons: &[&str] = if n == 0 { &[] } else { &[":", ":"] };
        colons.iter().copied().chain(std::iter::once(part))
    })
}

#[cfg(test)]
mod tests {
    use span::{BlazeEdition, Edition, Extensions};

    use super::{EXTENSION_ATTRS, SaidBlazeEdition, leading_blaze_edition, leading_extensions};

    /// The blaze edition is read off the leading inner attributes: the attribute written as
    /// itself with a known version, and nothing else. A crate with none is of no blaze
    /// edition (the control: a reader that answered `Is` for it would compile every Rust
    /// crate as the edition).
    #[test]
    fn the_blaze_edition_is_read_off_the_leading_inner_attributes() {
        let is = SaidBlazeEdition::Is(BlazeEdition::Blaze2026_11);
        let unknown = |written: &str| SaidBlazeEdition::Unknown(Box::from(written));
        let cases: &[(&str, SaidBlazeEdition)] = &[
            ("fn main() {}\n", SaidBlazeEdition::None),
            ("#![no_std]\n#![blaze::uniq]\nfn main() {}\n", SaidBlazeEdition::None),
            ("#![blaze::edition = \"2026.11\"]\nfn main() {}\n", is.clone()),
            // After another attribute, and with its path's tokens apart: two cases, so a
            // failure says which the reader does not take.
            ("#![allow(x)]\n#![blaze::edition = \"2026.11\"]", is.clone()),
            ("#![blaze :: edition = \"2026.11\"]\n", is.clone()),
            // After the crate's own docs, which most roots open with.
            ("//! docs\n/*! more */\n#![blaze::edition = \"2026.11\"]\nfn f() {}\n", is.clone()),
            // An outer doc comment belongs to the item after it: what follows is no leading
            // inner attribute.
            ("/// of f\n#![blaze::edition = \"2026.11\"]\nfn f() {}\n", SaidBlazeEdition::None),
            // After a comment that is not a doc comment.
            ("// a comment\n#![blaze::edition = \"2026.11\"]\nfn f() {}\n", is.clone()),
            ("#![blaze::edition = \"2026.11\"]\n#![blaze::edition = \"2026.11\"]", is.clone()),
            ("#![blaze::edition = \"2026.12\"]", unknown("\"2026.12\"")),
            ("#![blaze::edition = \"blaze-2026.11\"]", unknown("\"blaze-2026.11\"")),
            ("#![blaze::edition]", unknown("")),
            ("#![blaze::edition(2026)]", unknown("(")),
            ("#![cfg_attr(all(), blaze::edition = \"2026.11\")]", SaidBlazeEdition::None),
            ("fn f() {}\n#![blaze::edition = \"2026.11\"]\n", SaidBlazeEdition::None),
            ("#![blaze::editions = \"2026.11\"]", SaidBlazeEdition::None),
        ];
        for (text, expected) in cases {
            let mut said = SaidBlazeEdition::None;
            leading_blaze_edition(text, Edition::CURRENT, &mut said);
            assert_eq!(&said, expected, "{text:?}");
        }
        // The manifest's attribute is a file of its own, read after the root's: the same
        // edition twice is that edition.
        let mut said = SaidBlazeEdition::None;
        leading_blaze_edition("#![blaze::edition = \"2026.11\"]", Edition::CURRENT, &mut said);
        leading_blaze_edition(BlazeEdition::Blaze2026_11.crate_attr(), Edition::CURRENT, &mut said);
        assert_eq!(said, is, "the attribute `BlazeEdition::crate_attr` gives is the one read");
    }

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
