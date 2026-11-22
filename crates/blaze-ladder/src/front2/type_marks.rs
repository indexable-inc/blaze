//! A type macro's path read where its first token's MARK says: the macro names of a written
//! type's own text, with the marks of the item text it is a slice of.
//!
//! The case (tempo census, 35 fns, "Head: Item: TypeMacro(.. (a macro call its expansion
//! leaves))"): pin-project-lite's `pin_project!` writes, in the calling crate, `struct
//! Projection { f: $crate::__pin_project_make_proj_field_mut!(#[pin] F) }`; objc2's
//! `extern_class!` writes `$crate::__fallback_if_not_set! { .. }` in a type the same way. The
//! item's text spells that `$crate` as `crate`, its crate (pin-project-lite) only in the token's
//! mark (blaze-scope crate_roots.rs `root_mark`; the Reference, "Macros By Example", "Hygiene":
//! "`$crate` ... refers to the crate where the macro was defined"). A written type's macro call
//! is expanded from the type's own text (super::type_expand `expand_type_text`), lexed afresh:
//! no token there carries a mark, so `crate::__pin_project_make_proj_field_mut` was read in the
//! CALLING crate, named nothing, the call stayed unexpanded and the head was refused.
//!
//! [`Marked`] is the one change: the call resolver of the type's text (super::scoped's
//! [`ScopedBy`], unchanged) asked with the mark the ITEM's marks hold for the path's first
//! word, found by where that word is in the item's text (the type's text is a slice of it:
//! [`TextMarks::mark_of`], as super::item_site reads a written path's mark). A path the
//! lowering already marked (one an expansion of the type wrote) keeps its own mark; a path
//! that is no slice of the item's text (an expansion's words, appended past it) keeps what
//! the lowering gave; a word with no mark row is one written in a file, read at the call
//! site, as before. So nothing is guessed: each path is read where its own token says.

use blaze_expand::ProcKind;
use blaze_lower::by_name::{MacroNames, NoMacro, PathAt, Resolved};
use blaze_own::Alloc;
use blaze_scope::crate_roots::{MarkMiss, TextMarks};

use super::scoped::ScopedBy;
use super::source::NamesSource;

/// [`ScopedBy`] with the marks of the item text the expanded type is a slice of; `marks: None`
/// for a type written in a file (no token of it carries a mark): then exactly [`ScopedBy`].
pub(super) struct Marked<'d, 'n: 'd, 'm, S> {
    pub scoped: ScopedBy<'d, 'n, S>,
    pub marks: Option<TextMarks<'m>>,
}

/// The first word of a macro call's path as the call resolver splits it (super::scoped
/// `resolve`: segments split at `:` and white space): a slice of `path`.
fn first_word(path: &str) -> Option<&str> {
    path.split(|c: char| c == ':' || c.is_whitespace()).find(|s| !s.is_empty())
}

impl<'d, 'n: 'd, 'm, S: NamesSource<'n>> Marked<'d, 'n, 'm, S> {
    /// Where the call's path is read: the lowering's mark when it gave one; else the item's
    /// mark of the path's first word, when the word is in the item's text; else unmarked.
    fn at(&self, path: &str, at: PathAt) -> PathAt {
        if at.mark.is_some() {
            return at;
        }
        let (Some(marks), Some(first)) = (self.marks, first_word(path)) else { return at };
        match marks.mark_of(first) {
            Ok(Some(mark)) => PathAt { mark: Some(mark), ..at },
            // No mark row starts at the word: written in a file, read at the call site.
            Ok(None) => at,
            // Not a slice of the item's text: words the lowering appended (an expansion's),
            // whose marks are the lowering's own.
            Err(MarkMiss::NotInText { .. }) => at,
        }
    }
}

impl<'d, 'n: 'd, 'm, S: NamesSource<'n>> MacroNames<'d> for Marked<'d, 'n, 'm, S> {
    fn resolve<'a>(&self, path: &str, at: PathAt, kind: ProcKind, alloc: Alloc<'a>) -> Result<Resolved<'d, 'a>, NoMacro<'a>>
    where
        'd: 'a,
    {
        self.scoped.resolve(path, self.at(path, at), kind, alloc)
    }

    fn local_site(&self) -> Option<blaze_expand::DefSite> {
        self.scoped.local_site()
    }
}

#[cfg(test)]
mod tests {
    use super::first_word;

    /// The first word is the one the resolver reads as the path's head, and it is a slice of
    /// the path (so of the item's text when the path is): `crate:: __m !` gives `crate` at
    /// offset 0; a leading `::` or space is skipped. A first word that was a copy would lose
    /// its place, and its mark with it.
    #[test]
    fn the_first_word_is_a_slice_of_the_path() {
        let path = "crate:: __pin_project_make_proj_field_mut";
        let w = first_word(path).expect("a first word");
        assert_eq!(w, "crate");
        assert_eq!(w.as_ptr(), path.as_ptr());
        let abs = " ::dep::m";
        let w = first_word(abs).expect("a first word");
        assert_eq!(w, "dep");
        assert_eq!(w.as_ptr() as usize - abs.as_ptr() as usize, 3);
        assert_eq!(first_word(":: "), None);
        println!("type marks: the first word is the path's own slice (crate at 0, dep at 3, none in `:: `)");
    }
}
