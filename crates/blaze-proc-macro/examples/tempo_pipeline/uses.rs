//! The names a file's `use` items bind, read from its tokens: the stand-in resolver's first step
//! (blaze-scope's `Names::macro_path` is the real one), so a call's first segment is read
//! through the `use` that names it before any search by name. The Reference, "Use
//! declarations": `use a::b::c;` binds `c` to `a::b::c`, `use a::b as d;` binds `d`,
//! `use a::{self, b};` binds `a` and `b`, `use a::*;` and `use a as _;` bind no name of the file.
//!
//! The stand-in reads the file whole, not module by module: a name two `use`s of the file bind to
//! two different paths is [`Bound::Twice`], never guessed. A `use` whose tree holds anything but
//! names, `::`, `as`, `*`, `,` and braces (a `macro_rules!` body's `use $crate::..`) binds
//! nothing. No recursion: the groups open are a stack.

use std::collections::BTreeMap;

use blaze_expand::{Delim, LexError, Raw, TokenKind, TokenStream, lex};
use blaze_own::Alloc;
use blaze_surface::Edition;

/// What a name of the file is bound to by its `use` items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    /// The whole path, its segments in order (`tokio::select`).
    Path(Vec<String>),
    /// Two `use`s of the file bind the name to different paths.
    Twice,
}

/// The names the file's `use` items bind.
#[derive(Default, Debug)]
pub struct Uses {
    by_name: BTreeMap<String, Bound>,
}

impl Uses {
    /// The `use` items of `text`.
    pub fn of(text: &str, edition: Edition, alloc: Alloc<'_>) -> Result<Uses, LexError> {
        let stream = lex(text, edition, alloc)?;
        let mut uses = Uses::default();
        let list = stream.tokens();
        let mut at = 0;
        while at < list.len() {
            let is_use = matches!(list[at].kind, TokenKind::Ident { raw: Raw::No, .. }) && stream.text_at(at) == "use";
            at += 1;
            if is_use {
                at = uses.read_tree(&stream, at);
            }
        }
        Ok(uses)
    }

    /// What `name` is bound to, when a `use` of the file binds it.
    pub fn get(&self, name: &str) -> Option<&Bound> {
        self.by_name.get(name)
    }

    fn bind(&mut self, name: &str, path: &[String]) {
        if name == "_" || path.is_empty() {
            return;
        }
        let path = path.to_vec();
        match self.by_name.get(name) {
            None => {
                self.by_name.insert(name.to_owned(), Bound::Path(path));
            }
            Some(Bound::Path(was)) if *was == path => {}
            Some(_) => {
                self.by_name.insert(name.to_owned(), Bound::Twice);
            }
        }
    }

    /// The tree after a `use` keyword at `from`, up to its `;`: each name it binds. Where the
    /// reading stopped (past the `;`, or at the token it does not read).
    fn read_tree(&mut self, stream: &TokenStream<'_>, from: usize) -> usize {
        let list = stream.tokens();
        // The path so far, and for each open brace the path's length at it.
        let mut path: Vec<String> = Vec::new();
        let mut bases: Vec<usize> = Vec::new();
        // The tree being read has bound its name already (an `as`, a `*`, a closed group).
        let mut closed = false;
        let mut at = from;
        while at < list.len() {
            let base = bases.last().copied().unwrap_or(0);
            match list[at].kind {
                TokenKind::Punct { char: ';', .. } if bases.is_empty() => {
                    self.finish(&path, base, closed);
                    return at + 1;
                }
                TokenKind::Punct { char: ':', .. } => {}
                TokenKind::Punct { char: '*', .. } => closed = true,
                TokenKind::Punct { char: ',', .. } if !bases.is_empty() => {
                    self.finish(&path, base, closed);
                    path.truncate(base);
                    closed = false;
                }
                TokenKind::Open(Delim::Brace) => {
                    bases.push(path.len());
                    closed = false;
                }
                TokenKind::Close(Delim::Brace) if !bases.is_empty() => {
                    self.finish(&path, base, closed);
                    bases.pop();
                    path.truncate(base);
                    closed = true;
                }
                TokenKind::Ident { .. } if stream.text_at(at) == "as" => {
                    let alias = match list.get(at + 1).map(|t| t.kind) {
                        Some(TokenKind::Ident { .. }) => stream.text_at(at + 1),
                        // `as _` lexes as an identifier or as its own punctuation: no name either way.
                        _ => "_",
                    };
                    if !closed && path.len() > base {
                        self.bind(alias, without_self(&path));
                    }
                    closed = true;
                    at += 1;
                }
                TokenKind::Ident { .. } if !stream.text_at(at).starts_with('$') => path.push(stream.text_at(at).to_owned()),
                // Anything else (`$crate`, a stray `;` inside braces): not a tree this reads.
                _ => return at,
            }
            at += 1;
        }
        at
    }

    /// The end of one tree (at `,`, `}` or `;`): its last name bound to its path, unless the tree
    /// bound already or holds no name past its group's prefix.
    fn finish(&mut self, path: &[String], base: usize, closed: bool) {
        if closed || path.len() <= base {
            return;
        }
        let target = without_self(path);
        if let Some(name) = target.last() {
            let name = name.clone();
            self.bind(&name, target);
        }
    }
}

/// `a::self` is `a` (`use a::{self, ..}`).
fn without_self(path: &[String]) -> &[String] {
    match path.split_last() {
        Some((last, rest)) if last == "self" && !rest.is_empty() => rest,
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::{Bound, Uses};

    fn path(of: &str) -> Option<Bound> {
        Some(Bound::Path(of.split("::").map(str::to_owned).collect()))
    }

    /// The forms tempo writes (consensus dkg/manager/actor/mod.rs:54 `use tokio::select;`,
    /// follow/driver/actor/mod.rs:15 `use tokio::{select, sync::mpsc};`) and the Reference's.
    #[test]
    fn a_files_use_items_bind_their_names() {
        let text = "
            use tokio::{select, sync::mpsc};
            pub(crate) use ::commonware_macros::select as cselect;
            use a::{self, b::{c, d as e}, f::*};
            use g as _;
            macro_rules! m { () => { use $crate::x; } }
            mod inner { use other::select; }
        ";
        let uses = Uses::of(text, blaze_surface::Edition::E2021, blaze_own::testing::heap().alloc()).expect("the text lexes");
        assert_eq!(uses.get("mpsc").cloned(), path("tokio::sync::mpsc"));
        assert_eq!(uses.get("cselect").cloned(), path("commonware_macros::select"));
        assert_eq!(uses.get("a").cloned(), path("a"));
        assert_eq!(uses.get("c").cloned(), path("a::b::c"));
        assert_eq!(uses.get("e").cloned(), path("a::b::d"));
        assert_eq!(uses.get("d"), None);
        assert_eq!(uses.get("g"), None);
        assert_eq!(uses.get("x"), None);
        // Bound to `tokio::select` and to `other::select` in one file: never guessed.
        assert_eq!(uses.get("select").cloned(), Some(Bound::Twice));
    }
}
