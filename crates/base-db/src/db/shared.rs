//! The database when tasks that outlive a borrow read it: a detached build, a tiering thread.
//!
//! Scoped work borrows `&Db` and the borrow checker keeps writes out until it ends. A task on a
//! `'static` thread can't borrow, so it holds a [`Reader`], and the owner's write first stops
//! every reader ([`blaze_query::db::Storage::stop_readers`]), then waits for each to let go:
//! no task forks the database here, they share the one `Db`.

use std::{ops::Deref, sync::Arc};

use blaze_query::db::Database as _;
use parking_lot::{Condvar, Mutex};

use super::Db;

#[cfg(test)]
mod tests;

/// The owner of the one [`Db`] while [`Reader`]s of it may be running. `&Shared` reads as
/// `&Db`. A write says which kind it is: [`Shared::write`] stops the readers and waits for
/// them, [`Shared::idle`] stops none. There is no `DerefMut`: an `&mut` taken to read would
/// stop every reader.
pub struct Shared {
    db: Arc<Db>,
    gone: Arc<Gone>,
}

/// One task's hold on a [`Shared`] database, read as `&Db`. Dropping it lets a waiting write go
/// on.
pub struct Reader {
    // Declared before `left`: fields drop in order, so the count has gone down by the time
    // `left` wakes the writer, who checks it under the same lock.
    db: Arc<Db>,
    left: Left,
}

#[derive(Default)]
struct Gone {
    lock: Mutex<()>,
    left: Condvar,
}

struct Left(Arc<Gone>);

impl Drop for Left {
    fn drop(&mut self) {
        let _held = self.0.lock.lock();
        self.0.left.notify_all();
    }
}

impl Shared {
    pub fn new(db: Db) -> Shared {
        // alloc: setup, once per database
        Shared { db: Arc::new(db), gone: Arc::default() }
    }

    /// A hold on the database for a task that outlives the caller's borrow.
    pub fn reader(&self) -> Reader {
        Reader { db: Arc::clone(&self.db), left: Left(Arc::clone(&self.gone)) }
    }

    /// Has every reader, held or borrowed, unwind with `Cancelled::PendingWrite` at its next
    /// query call, until the next [`Shared::write`].
    pub fn stop_readers(&self) {
        self.db.storage().stop_readers();
    }

    /// The database to write to: stops every [`Reader`] still held, waits for each to let go,
    /// and clears the stop if one was made since the last write, so what runs after the write
    /// isn't stopped too.
    pub fn write(&mut self) -> &mut Db {
        if Arc::strong_count(&self.db) > 1 {
            self.db.storage().stop_readers();
            let held = blaze_graph::wait::lock(&self.gone.lock, "db readers left");
            let db = &self.db;
            drop(blaze_graph::wait::wait_until(&self.gone.left, held, "db readers gone", |_| {
                Arc::strong_count(db) == 1
            }));
        }
        let db = Arc::get_mut(&mut self.db)
            .expect("a reader of the database is held past its last `Reader`: only `Reader`s clone it");
        if db.storage().readers_stopped() {
            db.storage_mut().resume_readers();
        }
        db
    }

    /// The database to write to if no [`Reader`] holds it, stopping none: for a write that
    /// may as well wait for the next time nothing reads it.
    pub fn idle(&mut self) -> Option<&mut Db> {
        let db = Arc::get_mut(&mut self.db)?;
        if db.storage().readers_stopped() {
            db.storage_mut().resume_readers();
        }
        Some(db)
    }

    /// The database, once no reader holds it.
    pub fn into_inner(mut self) -> Db {
        self.write();
        match Arc::try_unwrap(self.db) {
            Ok(db) => db,
            Err(_) => unreachable!("`write` waited for every reader to let go"),
        }
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
        Reader { db: Arc::clone(&self.db), left: Left(Arc::clone(&self.left.0)) }
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("base_db::Shared").field("readers", &(Arc::strong_count(&self.db) - 1)).finish()
    }
}
