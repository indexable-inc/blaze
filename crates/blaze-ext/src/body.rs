//! A body as an extension sees it: blaze's real MIR ([`MirBody`]) and, beside it, the answers
//! of the front end that a body's lints ask most (which item it is, what each call calls),
//! taken before the job began so that reading them asks nothing.

use hir_def::DefWithBodyId;

use crate::{
    host::Prefetched,
    mir::{BasicBlockId, MirBody},
};

/// Which item a body is of: a function, a constant, a static. An identity to compare and to
/// ask facts of; it says nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Item(DefWithBodyId);

impl Item {
    pub(crate) fn new(def: DefWithBodyId) -> Item {
        Item(def)
    }

    pub(crate) fn def(self) -> DefWithBodyId {
        self.0
    }
}

/// One body of the crate being checked.
pub struct Body<'a, 'db> {
    item: Item,
    mir: &'db MirBody<'db>,
    prefetched: &'a Prefetched,
}

impl<'a, 'db> Body<'a, 'db> {
    pub(crate) fn new(
        item: Item,
        mir: &'db MirBody<'db>,
        prefetched: &'a Prefetched,
    ) -> Body<'a, 'db> {
        Body { item, mir, prefetched }
    }

    /// Which item the body is of.
    pub fn item(&self) -> Item {
        self.item
    }

    /// The item's name, as written (`drop`, `main`).
    pub fn name(&self) -> &'a str {
        &self.prefetched.name
    }

    /// The body's MIR as lowering built it and the borrow checker read it: before drop
    /// elaboration and every optimization, so each terminator is one thing the source wrote.
    pub fn mir(&self) -> &'db MirBody<'db> {
        self.mir
    }

    /// The impl the body's function is a method of, if it is one.
    pub fn impl_of(&self) -> Option<&'a ImplOf> {
        self.prefetched.impl_of.as_ref()
    }

    /// What the call that ends `block` calls, when it names a function (not a function
    /// pointer, a closure or a constructor).
    pub fn callee(&self, block: BasicBlockId) -> Option<&'a Callee> {
        let index = block.into_raw().into_u32();
        let callees = &self.prefetched.callees;
        let at = callees.binary_search_by_key(&index, |(block, _)| *block).ok()?;
        Some(&callees[at].1)
    }
}

/// The impl a method is of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImplOf {
    self_ty: String,
    trait_: Option<(String, Option<String>)>,
}

impl ImplOf {
    /// The host's: an impl of `self_ty` (as a message writes it), of the trait `trait_` (its
    /// name and, for one of `core`, `alloc` or `std`, the first module under its crate's
    /// root).
    pub fn new(self_ty: String, trait_: Option<(String, Option<String>)>) -> ImplOf {
        ImplOf { self_ty, trait_ }
    }

    /// The impl's `Self` type, as a message writes it (`Guard`, `Vec<T>`).
    pub fn self_ty(&self) -> &str {
        &self.self_ty
    }

    /// Whether the impl is of the trait `name` of the module `top` of `core`, `alloc` or
    /// `std`: `is_std_trait("ops", "Drop")`.
    pub fn is_std_trait(&self, top: &str, name: &str) -> bool {
        matches!(&self.trait_, Some((it, Some(of))) if it == name && of == top)
    }
}

/// A function a call names. Its text is read through `&str`: nothing here hands out an owned
/// string, whose copies would be the global heap's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callee {
    item: Option<Item>,
    name: String,
    std_top: Option<String>,
    self_adt: Option<(String, Option<String>)>,
    assoc: bool,
}

impl Callee {
    /// The host's: the function `name`, with its own body's item when it has one; for a
    /// function of `core`, `alloc` or `std`, `std_top` is the first module under its crate's
    /// root; for a method of an impl, `self_adt` is the ADT the impl is of, references
    /// peeled, by its name and its own such module; `assoc` says it is of an impl or a trait.
    pub fn new(
        item: Option<Item>,
        name: String,
        std_top: Option<String>,
        self_adt: Option<(String, Option<String>)>,
        assoc: bool,
    ) -> Callee {
        Callee { item, name, std_top, self_adt, assoc }
    }

    /// The function's own body, when it has one blaze can hold facts of.
    pub fn item(&self) -> Option<Item> {
        self.item
    }

    /// Its name (`unwrap`, `panicking`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether it is the free function `name` of the module `top` of `core`, `alloc` or `std`.
    pub fn is_std_free(&self, top: &str, name: &str) -> bool {
        !self.assoc && self.name == name && self.std_top.as_deref() == Some(top)
    }

    /// Whether it is defined in the module `top` of `core`, `alloc` or `std`
    /// (`in_std("option")` of `core::option::Option::unwrap`).
    pub fn in_std(&self, top: &str) -> bool {
        self.std_top.as_deref() == Some(top)
    }

    /// Whether it is a method of the ADT `adt` of the module `top` of `core`, `alloc` or
    /// `std`: `is_std_method("option", "Option")`.
    pub fn is_std_method(&self, top: &str, adt: &str) -> bool {
        matches!(&self.self_adt, Some((name, Some(of))) if name == adt && of == top)
    }
}
