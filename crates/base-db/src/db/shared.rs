//! The database when tasks that outlive a borrow read it: a detached build, a tiering thread.
//!
//! Scoped work borrows `&Db` and the borrow checker keeps writes out until it ends. A task on a
//! `'static` thread can't borrow, so it holds a [`Reader`]: a copy of the database at the
//! snapshot it was made at, with no world. The owner's write never waits for it: a commit
//! makes the next snapshot beside the one the reader holds (`Memo::next`'s spill table), and
//! the reader answers from its own until it lets go.
//!
//! Cost (rule 67): a reader is two reference counts; a commit while one is held copies the
//! snapshot's table once ([`crate::world`]).

use std::ops::Deref;

use super::Db;

#[cfg(test)]
mod tests;

/// The owner of the one [`Db`] while [`Reader`]s of it may be running. `&Shared` reads as
/// `&Db`; [`Shared::write`] is the database to edit. There is no `DerefMut`.
pub struct Shared {
    db: Db,
}

/// One task's hold on a snapshot of a [`Shared`] database, read as `&Db`.
pub struct Reader {
    db: Db,
}

impl Shared {
    pub fn new(db: Db) -> Shared {
        Shared { db }
    }

    /// A hold on the database's current snapshot for a task that outlives the caller's borrow.
    pub fn reader(&self) -> Reader {
        Reader { db: self.db.reader_copy() }
    }

    /// The database to write to. Readers keep the snapshot they hold: nothing waits.
    pub fn write(&mut self) -> &mut Db {
        &mut self.db
    }

    /// The database.
    pub fn into_inner(self) -> Db {
        self.db
    }
}

impl Deref for Shared {
    type Target = Db;

    #[inline]
    fn deref(&self) -> &Db {
        &self.db
    }
}

impl Deref for Reader {
    type Target = Db;

    #[inline]
    fn deref(&self) -> &Db {
        &self.db
    }
}

impl Clone for Reader {
    fn clone(&self) -> Reader {
        Reader { db: self.db.reader_copy() }
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("base_db::Shared").field("db", &self.db).finish()
    }
}
