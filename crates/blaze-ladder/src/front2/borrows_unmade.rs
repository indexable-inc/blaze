//! Why an ADT a body's types name was not made for the move and borrow check (piece
//! pm-flow-adt, 2026-11-16). Before, [`super::borrows::gather_facts`] asked the store for the
//! ADT's row and dropped the answer when it was an error (`ensure_one`: `Err(_) => false`), so
//! the flow lowering met the ADT with no facts and refused the body as `UnknownAdt` at the
//! place it moved, naming neither the ADT nor why (pm-small, tree c61a1c3861: "body `Stre`: the
//! flow lowering refused a form: Lower(Refusal { form: UnknownAdt, range: 61..74 })", the
//! macro side's `proc_macro::TokenStream`). AGENTS.md rule 3 (nothing quietly ignored) and
//! rule 19 (an error names the item and the rule): the store's answer is kept per ADT, and a
//! body the lowering refuses for an unknown ADT is refused as [`AdtUnmade`] of that ADT.
//!
//! Where the facts come from (the question the piece asked): there is NO second ADT table. The
//! lowering's `Facts::adt` (blaze-flow-lower lib.rs `adt_facts`, `self.facts.adt(k)`) reads
//! [`super::borrows::BorrowFacts`], which `gather_facts` fills from `r.store.adts`, the SAME
//! store over the SAME `Records` the body's type check read: the owner's readers, whose crates
//! are the check scope's (blaze-world check_scope.rs, taken by blaze-cli body_types.rs
//! `prepare` once records-side-scope-1 is in). A crate the type check could name a type from
//! is a crate the flow facts can ask: the gap is the row not being MADE, and this says why.

use blaze_surface::Range;

use super::heads::HeadError;
use super::types::Why;

/// Why an ADT's row was not made, as a value a `Copy` refusal can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdtUnmade {
    /// The checker key begins no item of any crate of the owner's records (`Keys::library_in`
    /// gave `None`): the type check's key is no library item's.
    NoLibraryKey,
    /// The store refused the ADT's head ([`HeadError`]), by its class.
    Head(HeadClass),
    /// The store made no row and gave no error: a want that does not make what it is asked
    /// (a bug of the store, named).
    NoRow,
}

/// A [`HeadError`] by its variant; an item's own refusal ([`HeadError::Item`]) with its
/// [`Why`]'s variant and the item-relative range it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadClass {
    NoText,
    Cfg,
    NotAHead(&'static str),
    Cycle,
    NeedMade,
    KeyClash,
    Item { why: WhyClass, at: Option<(u32, u32)> },
    /// A need's frozen head was not filed (heads.rs `HeadError::Seed`).
    Seed,
    /// A frozen head's record could not be filed (heads.rs `HeadError::Filed`).
    Filed(&'static str),
}

/// A [`Why`] by its variant (the boxed words stay with the error; the variant and the range
/// name the class and the place).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhyClass {
    Need,
    Unsupported(&'static str),
    Program,
    Resolve,
    NoCheckerType,
    AssocNotFound,
    AssocAmbiguous,
    TwoPrincipals,
    TypeMacro,
    PatternType,
    ConstPath,
    NeedConst,
    Regions,
}

fn range_of(r: &Range) -> Option<(u32, u32)> {
    Some((r.start, r.end))
}

impl HeadClass {
    /// The class of `e`: every variant named, no catch-all (a new variant is a compile error
    /// here, so it gets its own name).
    pub fn of(e: &HeadError) -> HeadClass {
        match e {
            HeadError::NoText(_) => HeadClass::NoText,
            HeadError::Cfg(_) => HeadClass::Cfg,
            HeadError::NotAHead(_, what) => HeadClass::NotAHead(what),
            HeadError::Cycle(_) => HeadClass::Cycle,
            HeadError::NeedMade { .. } => HeadClass::NeedMade,
            HeadError::Key(_) => HeadClass::KeyClash,
            HeadError::Seed { .. } => HeadClass::Seed,
            HeadError::Filed { what, .. } => HeadClass::Filed(what),
            HeadError::Item(_, why) => {
                let (why, at) = match why {
                    Why::Need(_) => (WhyClass::Need, None),
                    Why::Unsupported(what, r) | Why::UnsupportedIn(what, _, r) => (WhyClass::Unsupported(what), range_of(r)),
                    Why::Program(_, r) => (WhyClass::Program, range_of(r)),
                    Why::Resolve(_, r) => (WhyClass::Resolve, range_of(r)),
                    Why::NoCheckerType(_, r) => (WhyClass::NoCheckerType, range_of(r)),
                    Why::AssocNotFound(_, r) => (WhyClass::AssocNotFound, range_of(r)),
                    Why::AssocAmbiguous(_, _, r) => (WhyClass::AssocAmbiguous, range_of(r)),
                    Why::TwoPrincipals(_, _, r) => (WhyClass::TwoPrincipals, range_of(r)),
                    Why::TypeMacro(_, r) => (WhyClass::TypeMacro, range_of(r)),
                    Why::PatternType(_, r) => (WhyClass::PatternType, range_of(r)),
                    Why::ConstPath(_, r) => (WhyClass::ConstPath, range_of(r)),
                    Why::NeedConst(_, r) => (WhyClass::NeedConst, range_of(r)),
                    Why::Regions(_) => (WhyClass::Regions, None),
                };
                HeadClass::Item { why, at }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An item's refusal keeps its class and range; the store's own refusals keep theirs. A
    /// `HeadClass::of` that collapsed every error to one class (what `ensure_one`'s
    /// `Err(_) => false` did) fails the first assert. Prints nothing; passes as
    /// "test front2::borrows_unmade::tests::head_class_keeps_the_why ... ok".
    #[test]
    fn head_class_keeps_the_why() {
        let key = blaze_id::ItemKey(blaze_id::ContentId([7; 32]));
        let e = HeadError::Item(key, Why::Unsupported("a field type", Range::new(3, 9)));
        assert_eq!(HeadClass::of(&e), HeadClass::Item { why: WhyClass::Unsupported("a field type"), at: Some((3, 9)) });
        assert_eq!(HeadClass::of(&HeadError::NoText(key)), HeadClass::NoText);
        assert_ne!(HeadClass::of(&HeadError::Cfg(key)), HeadClass::of(&HeadError::NoText(key)));
    }
}
