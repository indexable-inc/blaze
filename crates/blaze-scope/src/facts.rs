//! What the machine reads of the module it resolves: [`ScopeFacts`], and [`RecordFacts`], its
//! implementation over the item layer's records read through [`Items`].
//!
//! The values it hands back are blaze-surface's working forms ([`DeclsBuilder`],
//! [`CrateRootBuilder`], [`ModuleInfo`]), the one home of each: a record decodes losslessly
//! into its working form, and the machine reads only that.
//!
//! Reads of another module's scope are not here: the machine yields them as asks
//! ([`crate::Read`]) and takes their answers, so it never computes another module's scope.

use core::alloc::Allocator;
use core::fmt;
use core::hash::Hash;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_surface::{
    CrateRootBuilder, DeclsBuilder, ExpandedDecls, Items, ModuleInfo, NotBuilt, SurfaceError,
};

/// What a name is to the machine: a small value that compares, orders and hashes. Its order
/// must be the order of the names' text, since a scope is sorted by it: `&str` (what
/// [`RecordFacts`] gives) orders by its text, and so do the tests' `&'static str`.
pub trait Name: Copy + Ord + Hash + fmt::Debug {}

impl<T: Copy + Ord + Hash + fmt::Debug> Name for T {}

/// Everything the machine reads of the module it resolves and of its crate, each a record
/// decoded into its working form in the facts' allocator. A missing record is the
/// [`SurfaceError`] naming it, which the machine stops on; [`ScopeFacts::expansion`]'s
/// `Ok(None)` means the call has not been expanded yet.
pub trait ScopeFacts {
    type Name: Name;
    type Alloc: Allocator + Clone;

    /// The module's place in its crate: the head of its own `ModuleDecls` record.
    fn module(&self, module: ItemKey) -> Result<ModuleInfo<Self::Name>, SurfaceError>;

    /// What the module's own text declares: its `ModuleDecls` record.
    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<Self::Name, Self::Alloc>, SurfaceError>;

    /// The crate's `CrateRoot` record.
    fn crate_root(
        &self,
        krate: CrateItemKey,
    ) -> Result<CrateRootBuilder<Self::Name, Self::Alloc>, SurfaceError>;

    /// What the call's expansion adds to its module, or `None` while it is not expanded.
    fn expansion(
        &self,
        call: ItemKey,
    ) -> Result<Option<ExpandedDecls<Self::Name, Self::Alloc>>, SurfaceError>;
}

/// [`ScopeFacts`] over the records an [`Items`] reads: names are the records' string-table
/// text, borrowed for `'a`, and each record is decoded into `alloc`.
///
/// Where each fact comes from: [`ScopeFacts::module`] is the head of the module's own
/// `ModuleDecls` record (its crate, parent, name in the parent and `#[no_implicit_prelude]`,
/// written by the parent's `mod` line's task), [`ScopeFacts::decls`] that record's lists, and
/// [`ScopeFacts::crate_root`] the crate's `CrateRoot` record. An expansion has no record yet,
/// so [`ScopeFacts::expansion`] is [`NotBuilt`]: a module whose macro call resolves stops there
/// with that error, never on a guess that the call adds nothing.
pub struct RecordFacts<'a, I: Items, A: Allocator + Clone> {
    pub items: &'a I,
    pub alloc: A,
}

impl<'a, I: Items, A: Allocator + Clone> RecordFacts<'a, I, A> {
    pub fn new_in(items: &'a I, alloc: A) -> RecordFacts<'a, I, A> {
        RecordFacts { items, alloc }
    }
}

impl<'a, I: Items, A: Allocator + Clone> ScopeFacts for RecordFacts<'a, I, A> {
    type Name = &'a str;
    type Alloc = A;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<&'a str>, SurfaceError> {
        Ok(self.items.decls(module)?.info())
    }

    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<&'a str, A>, SurfaceError> {
        Ok(DeclsBuilder::from(&self.items.decls(module)?, self.alloc.clone()))
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<&'a str, A>, SurfaceError> {
        Ok(CrateRootBuilder::from(&self.items.crate_root(krate)?, self.alloc.clone()))
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<ExpandedDecls<&'a str, A>>, SurfaceError> {
        Err(NotBuilt { method: "expansion", item: call }.into())
    }
}
