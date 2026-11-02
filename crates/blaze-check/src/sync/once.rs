//! `std::sync::OnceLock`. The value is in `std`'s cell; the model keeps the cell's state in
//! this execution (empty, being initialized by a thread, done) and the clock its initializer
//! released. A `static` cell an earlier execution set stays set, as one set before the model:
//! its initializer must do no operation the model sees.

use std::{fmt, panic::Location, sync::atomic::AtomicU64 as Id};

use crate::rt::{self, Kind, Op, Runtime, locks::Found};

pub struct OnceLock<T> {
    id: Id,
    cell: std::sync::OnceLock<T>,
}

impl<T> OnceLock<T> {
    pub const fn new() -> OnceLock<T> {
        OnceLock { id: Id::new(0), cell: std::sync::OnceLock::new() }
    }

    fn object(&self, rt: &mut Runtime) -> u32 {
        rt.once(&self.id, self.cell.get().is_some())
    }

    fn done(&self) -> &T {
        self.cell.get().expect("blaze-check: a once cell the model says is done holds a value")
    }

    #[track_caller]
    pub fn get(&self) -> Option<&T> {
        let found = rt::op(
            Location::caller(),
            "OnceLock::get",
            |rt| Ok(Op { obj: self.object(rt), kind: Kind::Read }),
            |rt| {
                let obj = self.object(rt);
                Ok(rt.once_look(obj, false))
            },
        );
        match found {
            None => self.cell.get(),
            Some(Found::Done) => Some(self.done()),
            Some(Found::Run | Found::Empty) => None,
        }
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        // `&mut` orders this with every other access; the model starts the cell again from
        // what it holds after.
        *self.id.get_mut() = 0;
        self.cell.get_mut()
    }

    #[track_caller]
    pub fn set(&self, value: T) -> Result<(), T> {
        let mut value = Some(value);
        self.get_or_init(|| value.take().expect("an initializer runs once"));
        match value {
            None => Ok(()),
            Some(value) => Err(value),
        }
    }

    #[track_caller]
    pub fn get_or_init<F: FnOnce() -> T>(&self, f: F) -> &T {
        let at = Location::caller();
        let mut f = Some(f);
        loop {
            let found = rt::op(
                at,
                "OnceLock::get_or_init",
                |rt| {
                    let obj = self.object(rt);
                    rt.once_wait(obj)?;
                    Ok(Op { obj, kind: Kind::Write })
                },
                |rt| {
                    let obj = self.object(rt);
                    Ok(rt.once_look(obj, true))
                },
            );
            match found {
                None => {
                    let f = f.take().expect("an initializer runs once");
                    return self.cell.get_or_init(f);
                }
                Some(Found::Done) => return self.done(),
                Some(Found::Run) => {
                    let f = f.take().expect("an initializer runs once");
                    if self.cell.set(f()).is_err() {
                        panic!("blaze-check: a once cell was set while the model initialized it");
                    }
                    rt::with(|rt| {
                        let obj = self.object(rt);
                        rt.once_done(obj);
                    });
                    return self.done();
                }
                // Another thread initializes it: this one waited, and looks again.
                Some(Found::Empty) => {}
            }
        }
    }

    pub fn into_inner(self) -> Option<T> {
        self.cell.into_inner()
    }

    pub fn take(&mut self) -> Option<T> {
        *self.id.get_mut() = 0;
        self.cell.take()
    }
}

impl<T> Default for OnceLock<T> {
    fn default() -> OnceLock<T> {
        OnceLock::new()
    }
}

impl<T> From<T> for OnceLock<T> {
    fn from(value: T) -> OnceLock<T> {
        OnceLock { id: Id::new(0), cell: std::sync::OnceLock::from(value) }
    }
}

impl<T: Clone> Clone for OnceLock<T> {
    fn clone(&self) -> OnceLock<T> {
        // clone: `OnceLock::clone` clones the value, as std's does
        OnceLock { id: Id::new(0), cell: self.cell.clone() }
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.cell, f)
    }
}

impl<T: PartialEq> PartialEq for OnceLock<T> {
    fn eq(&self, other: &OnceLock<T>) -> bool {
        self.cell == other.cell
    }
}

impl<T: Eq> Eq for OnceLock<T> {}
