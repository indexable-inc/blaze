//! Whether a trait or impl declares an associated item of a name, read from its member rows'
//! names alone, before its head is made.
//!
//! Method resolution asks every trait in scope for an item of the called name (blaze-typeck
//! rules/method.rs `probe_method`: `for &tr in traits_in_scope { assoc_item(tr, name) }`; and
//! rules/lit_path.rs for `T::name` paths). Before this, [`BodyWorld::assoc_item`] made the head of
//! each such trait to read its members, so ONE trait in scope whose head is refused stopped every
//! method call in its module, whatever the method's name. The hello world's run met it: alloc's
//! vec module imports `core::mem::TransmuteFrom` (library/alloc/src/vec/mod.rs:108, for the
//! `Recyclable` blanket impl's where-clauses at 3506-3511), whose head is refused
//! (`pub unsafe trait TransmuteFrom<Src, const ASSUME: Assume = ..>`, core/src/mem/
//! transmutability.rs:91: "a const parameter whose type is not an integer, `char` or `bool`
//! (`adt_const_params`)"), and so `Vec::as_slice`, `len`, `push`, `pop`, `capacity`, .. were
//! left out with that text ("check: Head: Item: Unsupported(..)", stops-3.txt lines 3-20;
//! port27/front2/walk-AJ-briefs.md P12 names the item).
//!
//! The rule is the language's: a method found through a trait in scope is one of that trait's
//! OWN associated items of that name (the Reference, "Method call expressions": "Then, for each
//! candidate type T, search for a visible method with a receiver of that type in the following
//! places: T's inherent methods .. Any of the methods provided by a visible trait implemented by
//! T"; a trait provides the items its body declares). So a trait none of whose members is named
//! `name` is no candidate, and its head is never needed to say so.
//!
//! EXACT, not a guess: the head's member list is made from this same list ([`super::heads`]
//! `members`: `each_member`, the macro call itself skipped because its expansion's members are
//! listed beside it, each member's name as `m.name.unwrap_or("")`), and [`BodyWorld::assoc_item`]
//! over a made head answers only from those members (`fns`, `consts`, `assoc`). So where this says
//! "declares none", the made head would answer `None` too: nothing that could be the answer is
//! filtered. Where it says "may declare", the head is made as before, and a refusal of it is the
//! body's refusal by name (it could have been the answer: never dropped).
//!
//! [`BodyWorld::assoc_item`]: super::world_items::BodyWorld

use blaze_id::ItemKey as LibKey;
use blaze_surface::ItemKind;
use blaze_typeck::Name;

use super::source::NamesSource;

/// Whether the trait or impl `owner` (its library key) declares an associated item named
/// `name`, by its member rows' names: `false` only when none of the members its head would be
/// made from has that name. No text is read, no head made, nothing allocated.
///
/// A container with no member row has no members (`NamesSource::each_member`'s `false`; the
/// head's `members` reads it as the empty list too), so it declares none.
pub(super) fn may_declare<'n, S: NamesSource<'n>>(names: S, owner: LibKey, name: Name) -> bool {
    let mut found = false;
    names.each_member(owner, &mut |m| {
        // The call itself: its expansion's members are listed beside it (`written_by`), as the
        // head's `members` reads them.
        if m.kind == ItemKind::MacroCall {
            return;
        }
        if Name::of(m.name.unwrap_or("")) == name {
            found = true;
        }
    });
    found
}
