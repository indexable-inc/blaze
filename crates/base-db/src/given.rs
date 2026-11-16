//! What the driver gives a crate for one task, outside the records: a value per crate and slot
//! type (a proc-macro crate's loaded expanders, `hir_expand::proc_macro::ProcMacros`), set before
//! the task runs ([`Gifts`], passed to [`crate::Db::new`]) and read in place ([`Crate::given`]).
//!
//! A gift is an input of the task: the driver keys the task by what it gives (for proc macros,
//! the expanders' code ids), so no read of one is recorded here. Nothing is given after the
//! database is made: a command answers and exits (rule 70).

use std::alloc::Global;
use std::any::{Any, TypeId};
use std::sync::Arc;

use blaze_surface::CrateItemKey;

use crate::{Crate, Db};

/// One gift: a value of any type, shared.
pub struct Given(Arc<dyn Any + Send + Sync, Global>);

impl Given {
    pub fn new<T: Any + Send + Sync>(value: T) -> Given {
        Given(Arc::new_in(value, Global))
    }

    fn get<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for Given {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Given(..)")
    }
}

/// Two gifts of one slot to one crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GivenTwice {
    pub krate: CrateItemKey,
    pub slot: &'static str,
}

impl std::fmt::Display for GivenTwice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "crate {:x} was given `{}` twice", self.krate, self.slot)
    }
}

impl std::error::Error for GivenTwice {}

/// The gifts of one task, sorted by (crate, slot) when the database is made.
#[derive(Debug, Default)]
pub struct Gifts {
    rows: Vec<(CrateItemKey, TypeId, &'static str, Given), Global>,
}

impl Gifts {
    pub fn new() -> Gifts {
        Gifts { rows: Vec::new_in(Global) }
    }

    /// Gives `krate` `given` in slot `S`.
    pub fn give<S: 'static>(&mut self, krate: CrateItemKey, given: Given) {
        self.rows.push((krate, TypeId::of::<S>(), std::any::type_name::<S>(), given));
    }

    pub(crate) fn seal(&mut self) -> Result<(), GivenTwice> {
        self.rows.sort_unstable_by_key(|row| (row.0, row.1));
        match self.rows.windows(2).find(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1)) {
            Some(pair) => Err(GivenTwice { krate: pair[1].0, slot: pair[1].2 }),
            None => Ok(()),
        }
    }

    fn get<S: 'static, T: Any>(&self, krate: CrateItemKey) -> Option<&T> {
        let slot = TypeId::of::<S>();
        let at = self.rows.binary_search_by_key(&(krate, slot), |row| (row.0, row.1)).ok()?;
        self.rows[at].3.get::<T>()
    }
}

impl Crate {
    /// What the driver gave the crate in slot `S`, as a `T`; `None` where it gave nothing (or a
    /// value of another type).
    pub fn given<S: 'static, T: Any>(self, db: &Db) -> Option<&T> {
        db.gifts().get::<S, T>(self.key(db))
    }
}
