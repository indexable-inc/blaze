//! Other crates' names, as the cell core reads them. A dependency is resolved before its
//! dependents (the ready queue's order), so each of its cells is already decided: a read of
//! one is a lookup in its `exports` record, never a wait. The core asks a module of another
//! crate for one name in one namespace at a time; a glob of such a module (`use dep::m::*`) is
//! the same per-name read through that module, its exported names as one layer.

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;

use crate::{BindingState, Def, Namespace, Visibility};

/// What a dependency's module holds under one name in one namespace: its own binding or one its
/// globs bring (the record's answer, final).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DepAnswer {
    NotFound,
    Found(Def, Visibility),
    /// Final and ambiguous in that crate: naming it is an error.
    Ambiguous(Def, Visibility),
    /// That crate could not decide it either (its own path into a crate it could not read).
    Unknown,
}

/// The decided names of the crates one crate depends on.
pub(crate) trait DepNames<N> {
    /// Whether `module` is a module of a dependency read here (its record present).
    fn has_module(&self, module: ItemKey) -> bool;

    /// `module`'s answer for `name` in `ns`; `module` is one [`DepNames::has_module`] holds.
    fn name_in(&self, module: ItemKey, ns: Namespace, name: N) -> DepAnswer;

    /// The root module of `krate`, a crate read here (what a `$crate` of its macros names).
    fn crate_root(&self, krate: CrateItemKey) -> Option<ItemKey>;

    /// Whether `key` is an enum of a dependency read here: [`DepNames::name_in`] of it answers
    /// with its variants.
    fn has_enum(&self, key: ItemKey) -> bool;
}

/// The variants of the enums of the crate being solved (its text's and its expansions'), as a
/// path through an enum reads them. Reference `names.namespaces`: the type namespace holds
/// "Struct, union, enum, enum variant declarations", the value namespace "Enum variant
/// constructors" (a variant with fields in braces has none).
pub(crate) trait EnumNames<N> {
    /// Whether enum `of` is held here. One an expansion wrote is held once that expansion is
    /// read, which is before any path can reach its name.
    fn holds(&self, of: ItemKey) -> bool;

    /// Enum `of`'s variant `name` in `ns`; `of` is one [`EnumNames::holds`] holds.
    fn variant(&self, of: ItemKey, ns: Namespace, name: N) -> DepAnswer;
}

/// The seam's reader: the names library's built crates (each a `CrateNames` finished in this
/// process or a `CrateScope` record read in place), through the one lookup the old core's
/// answers use (`Built::lookup`: the module's own bindings, then its glob edges); `alloc` holds
/// a glob walk's frontier.
pub(crate) struct BuiltDeps<'q, 'r, 'a> {
    pub(crate) built: &'q crate::resolve::Built<'r, 'a>,
    pub(crate) alloc: Alloc<'q>,
    /// The cell core writes the crate (the switch): its reads are the crate's read list. The
    /// seam's shadow run asks nothing, so the old core's read list stays its own.
    pub(crate) asks: bool,
    /// The glob walk's scratch, kept across every name read here (taken out for one
    /// [`crate::resolve::Built::lookup_in`] and put back): made once in `alloc`.
    pub(crate) through: core::cell::Cell<Option<blaze_surface::ThroughScratch<'q>>>,
}

impl<'a> DepNames<&'a str> for BuiltDeps<'_, '_, 'a> {
    fn has_module(&self, module: ItemKey) -> bool {
        self.built.of(module).is_some_and(|d| d.scope(module).is_some())
    }

    fn crate_root(&self, krate: CrateItemKey) -> Option<ItemKey> {
        let at = *self.built.by_prefix.get(&krate.prefix())?;
        let dep = self.built.crates.get(at).copied().flatten().filter(|d| d.key() == krate)?;
        // A question of the crate's read list (crate::asked): the crate `$crate` names.
        if self.asks {
            self.built.ask(|| crate::asked::Question::krate(dep, self.alloc));
        }
        Some(dep.root())
    }

    fn has_enum(&self, key: ItemKey) -> bool {
        self.built.of(key).is_some_and(|d| d.enum_of(key).is_some())
    }
    fn name_in(&self, module: ItemKey, ns: Namespace, name: &'a str) -> DepAnswer {
        let mut scratch = self.through.take().unwrap_or_else(|| blaze_surface::ThroughScratch::new_in(self.alloc));
        let found = self.built.lookup_in(module, ns, name, &mut scratch);
        self.through.set(Some(scratch));
        // Every read of another crate is a question of this crate's read list (crate::asked):
        // the name's whole answer there (its own binding or its globs'), as `Kind::Lookup`.
        #[cfg(test)]
        let noted = !forgets(found);
        #[cfg(not(test))]
        let noted = true;
        if noted && self.asks {
            self.built.ask(|| crate::asked::Question::lookup(module, ns, name, found, self.alloc));
        }
        match found {
            None => DepAnswer::NotFound,
            Some(b) => match b.state {
                BindingState::Determined => DepAnswer::Found(b.def, b.vis),
                BindingState::Ambiguous(_) => DepAnswer::Ambiguous(b.def, b.vis),
            },
        }
    }
}

/// The read list's planted bugs (tests/asked.rs, `crate::asked::plant`) as the cell core asks:
/// one question per name read, its whole answer. `ForgetGlobs` forgets an answer a glob brought.
#[cfg(test)]
fn forgets(found: Option<crate::Binding>) -> bool {
    use crate::asked::plant::{PLANT, Plant};
    match (PLANT.with(core::cell::Cell::get), found) {
        (Plant::ForgetFound, Some(_)) => true,
        (Plant::ForgetFailed, None) => true,
        (Plant::ForgetGlobs, Some(b)) => b.origin == crate::Origin::Glob,
        _ => false,
    }
}

impl DepAnswer {
    /// The answer as a decided cell.
    pub(super) fn value(self) -> super::Value {
        match self {
            DepAnswer::NotFound => super::Value::NotFound,
            DepAnswer::Found(d, v) => super::Value::Found(d, v),
            DepAnswer::Ambiguous(d, v) => super::Value::Ambiguous(d, v),
            DepAnswer::Unknown => super::Value::Outside,
        }
    }
}
