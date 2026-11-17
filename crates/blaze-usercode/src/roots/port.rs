//! A build's directories as the front end writes paths against them ([`base_db::Port`]): the
//! one port of every value the front end stores, here, inside the front end's code identity
//! (`blaze_codegen_code::FRONT_END`), because what it writes is in the bytes of every record
//! the front end keeps. blaze-cargo's `portable::Places` is this, with what a unit's record key
//! adds (how the roots sit, the key a record tied to them is kept under).

use std::borrow::Cow;

use super::{Role, Roots};

/// What starts a path in a package id (`path+file:///w/p#0.1.0`): the root after it starts
/// no path as [`Roots::port`] reads one (a `/` is before it), so a string is ported in the
/// pieces these end.
const URL: &[u8] = b"file://";

/// The directories of one build, each by what it is, owned: what its paths are written
/// against.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dirs {
    roots: Vec<(Role, String)>,
}

impl Dirs {
    /// The directories `given` names; one that is none, not absolute or the file system's
    /// root is no root: what is under it stays as it is.
    pub fn new<'a>(given: impl IntoIterator<Item = (Role, Option<&'a str>)>) -> Dirs {
        // alloc: once per build's directories, a root each
        let roots = given
            .into_iter()
            .filter_map(|(role, path)| Some((role, path?.trim_end_matches('/').to_owned())))
            .filter(|(_, path)| path.starts_with('/') && path.len() > 1)
            .collect();
        Dirs { roots }
    }

    /// The roots, lent.
    pub fn view(&self) -> Roots<'_> {
        Roots::new(self.roots.iter().map(|(role, path)| (*role, path.as_str())))
    }

    /// Each root, by what it is, in the order given.
    pub fn roots(&self) -> impl Iterator<Item = (Role, &str)> {
        self.roots.iter().map(|(role, path)| (*role, path.as_str()))
    }

    /// `text` with each root that starts a path in it as its placeholder ([`Roots::port`]), a
    /// root after `file://` among them: lent when it holds none.
    pub fn text<'t>(&self, text: &'t [u8]) -> Cow<'t, [u8]> {
        let roots = self.view();
        if !text.windows(URL.len()).any(|it| it == URL) {
            return roots.ported(text);
        }
        // alloc: a string that names a file by URL (a path package's id), once as it is keyed
        let mut out = Vec::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.windows(URL.len()).position(|it| it == URL) {
            let (head, tail) = rest.split_at(at + URL.len());
            roots.port(head, |piece| out.extend_from_slice(piece));
            rest = tail;
        }
        roots.port(rest, |piece| out.extend_from_slice(piece));
        Cow::Owned(out)
    }

    /// What `text`, written by [`Dirs::text`] in some build, is in this one: each placeholder
    /// this build's root. An error for a placeholder of a root this build has none of.
    pub fn real<'t>(&self, text: &'t [u8]) -> Result<Cow<'t, [u8]>, String> {
        self.view().unport(text).map_err(|why| why.to_string())
    }
}

impl base_db::Port for Dirs {
    fn ported<'b>(&self, bytes: &'b [u8]) -> Cow<'b, [u8]> {
        self.text(bytes)
    }

    fn placed<'b>(&self, bytes: &'b [u8]) -> Result<Cow<'b, [u8]>, String> {
        self.real(bytes)
    }

    /// The one searcher of a root left in bytes (`Roots::embedded`).
    fn held(&self, bytes: &[u8]) -> Option<&'static str> {
        self.view().embedded(bytes).iter().next().map(Role::name)
    }
}
