//! The impl text a member fn's signature is read with (piece sig-assoc-projection), and its row
//! in the body's read list (rule 21).
//!
//! An impl's fn names `Self::Name` and the impl's lifetimes; blaze-flow-lower's
//! `read_impl_signature` (sig/impl_life.rs) reads them through the impl's header (its self
//! type's lifetimes by name) and its `type Name = ..;` member texts. Those come from the
//! source here: the impl's item text and each associated type member's text, through the same
//! `NamesSource` the heads are made from (heads.rs `members`: macro-written members included).
//!
//! RULE 21: what the reading took of the impl's text is a `FactKind::BorrowHead` row of the
//! body, `detail` [`DETAIL`] (borrow_reads.rs files detail 0, call_generics.rs detail 1): the
//! digest of the header (to its first `{`, as the reader cuts it) and of every associated type
//! member's name and text, sorted by name; or that the fn is no impl's member; or that its
//! impl's text is not given. No fn body is in it, so an edit inside a body moves no row; an
//! edit of `type IntoFuture = Pending<'a, T>;` or of the impl's header does, though the head's
//! erased types (which hold no lifetimes) do not change.

use blaze_flow_lower::{ImplText, impl_header_end};
use blaze_own::{AVec, Alloc};
use blaze_surface::ItemKind;
use blaze_typeck as tc;
use blaze_typeck::StableHasher;

use super::heads::FnHead;
use super::keys::{Keys, Origin};
use super::reads::{Fact, FactKind, Read};
use super::source::NamesSource;
use super::types::SelfTy;

/// The `detail` of this module's `BorrowHead` rows.
pub const DETAIL: u64 = 2;

/// The domain of this module's row answers.
const DOMAIN: &str = "blaze-ladder front2 borrow impl text read v1";

/// The impl a fn's signature is read with.
pub enum ImplRead<'n, 'a> {
    /// A free fn, or a trait's: its signature names no impl.
    NotMember,
    /// A member of the impl whose item text is `text`.
    Member {
        /// The impl's checker key: the read row is keyed by it, so its `Signature` (the
        /// impl's header and every non-fn member's text) is what the body reads.
        container: tc::ItemKey,
        text: &'n str,
        self_ty: tc::PTy,
        /// `(name, text)` of each associated type member, sorted.
        assoc: AVec<'a, (&'n str, &'n str)>,
    },
    /// A member of an impl whose item text the source does not give: read without it, so
    /// its `Self::Name` is refused by name (`AssocWhy::NoImpl`) and its impl's lifetimes are
    /// not mapped (each a fresh region at a call, as before this piece).
    NoText,
}

impl<'n, 'a> ImplRead<'n, 'a> {
    /// The text `read_impl_signature` takes; `None`: read with `read_signature`.
    pub fn text(&self) -> Option<ImplText<'_>> {
        match self {
            ImplRead::Member { text, self_ty, assoc, .. } => Some(ImplText { text: *text, self_ty: *self_ty, assoc: &assoc[..] }),
            ImplRead::NotMember | ImplRead::NoText => None,
        }
    }
}

/// The impl the fn `key` (its head `f`) is a member of, with its texts.
pub fn impl_of<'n, 'a, S: NamesSource<'n>>(names: S, keys: &Keys, key: tc::ItemKey, f: &FnHead<'_>, alloc: Alloc<'a>) -> ImplRead<'n, 'a> {
    let &SelfTy::Ty(self_ty, _) = &f.self_ else { return ImplRead::NotMember };
    let Some(Origin::Member { container, .. }) = keys.origin(key) else { return ImplRead::NotMember };
    let Some(lib) = keys.library_in(names, *container) else { return ImplRead::NoText };
    let Some(text) = names.text_of(lib).map(|t| t.text) else { return ImplRead::NoText };
    let mut assoc: AVec<'a, (&'n str, &'n str)> = AVec::new_in(alloc);
    // A container with no member has no row: no associated type, which the list says.
    names.each_member(lib, &mut |m| {
        if m.kind == ItemKind::TypeAlias
            && let Some(name) = m.name
        {
            assoc.push((name, m.text));
        }
    });
    assoc.sort_unstable();
    ImplRead::Member { container: *container, text, self_ty, assoc }
}

fn text(h: &mut StableHasher, s: &str) {
    h.u32(s.len() as u32);
    *h = h.bytes(s.as_bytes());
}

/// The row of what the borrow check took of the impl text of the fn `item` (the module doc).
pub fn impl_read(item: u128, read: &ImplRead<'_, '_>, alloc: Alloc<'_>) -> Read {
    let mut h = StableHasher::new(DOMAIN);
    // RULE 21: a read row is re-checked by its key's want (blaze-cli body_types/heads.rs,
    // `want_of_fact`): the impl's want covers its header and its `type` members' texts; the
    // fn's would cover only the fn's own signature.
    let key = match read {
        ImplRead::Member { container, .. } => container.0,
        ImplRead::NotMember | ImplRead::NoText => item,
    };
    match read {
        ImplRead::NotMember => h.tag(0),
        ImplRead::NoText => h.tag(1),
        ImplRead::Member { text: t, self_ty: _, assoc, .. } => {
            // The self type's erased form is the head's own (a row of the type check's list).
            match impl_header_end(t, alloc) {
                Some(end) => {
                    h.tag(2);
                    text(&mut h, t.get(..end).unwrap_or(t));
                }
                // The header does not lex: the reader refuses it; the whole text is what it saw.
                None => {
                    h.tag(3);
                    text(&mut h, t);
                }
            }
            h.u32(assoc.len() as u32);
            for &(name, body) in assoc.iter() {
                text(&mut h, name);
                text(&mut h, body);
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key, detail: DETAIL }, answer: Some(h.finish()) }
}
