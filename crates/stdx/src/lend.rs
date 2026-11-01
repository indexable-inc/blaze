//! A value lent to a thread-local for the length of a call.

use std::{
    cell::{Cell, RefCell},
    thread::LocalKey,
};

/// Runs `f` with `*value` moved into `slot`, and moves what `slot` holds back into `*value` when
/// `f` returns or unwinds: state that the code `f` calls reads and adds to through the
/// thread-local rather than a parameter. Lends of one slot do not nest.
pub fn lend<T: Default + 'static, R>(
    slot: &'static LocalKey<RefCell<Option<T>>>,
    value: &mut T,
    f: impl FnOnce() -> R,
) -> R {
    struct Return<'v, T: 'static> {
        slot: &'static LocalKey<RefCell<Option<T>>>,
        value: &'v mut T,
    }

    impl<T: 'static> Drop for Return<'_, T> {
        fn drop(&mut self) {
            if let Some(lent) = give_back(self.slot) {
                *self.value = lent;
            }
        }
    }

    lend_to(slot, std::mem::take(value));
    let _return = Return { slot, value };
    f()
}

// `f` may wait and resume on another thread: each access to the slot is its own call, never
// inlined into `lend`, whose inlined access could reuse the first thread's slot address
// (docs/v2/plan/lanes/may-suspend.md).

#[inline(never)]
fn lend_to<T: 'static>(slot: &'static LocalKey<RefCell<Option<T>>>, value: T) {
    slot.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "scopes lending one thread-local do not nest");
        *slot = Some(value);
    });
}

#[inline(never)]
fn give_back<T: 'static>(slot: &'static LocalKey<RefCell<Option<T>>>) -> Option<T> {
    slot.with(|slot| slot.borrow_mut().take())
}

/// The `take` of a pair that carries a thread-local with a job that switches out (a job on a
/// fiber waiting on a query, which may resume on another thread; `blaze_query::carry`): a
/// value the job set or lent the thread-local and reads again after the wait. What the slot
/// holds is moved off the thread, which is left with the default: boxed, or 0 when it is the
/// default (`is_default`, and then left in place), so a thread-local no job set costs nothing.
pub fn take_carried<T: Default + 'static>(slot: &'static LocalKey<RefCell<T>>, is_default: fn(&T) -> bool) -> usize {
    let taken = slot.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        (!is_default(&slot)).then(|| std::mem::take(&mut *slot))
    });
    // alloc: only for a value a job set, once a switch of the job holding it
    taken.ok().flatten().map_or(0, |value| Box::into_raw(Box::new(value)).expose_provenance())
}

/// Puts back what [`take_carried`] took of `slot`, on the thread the job resumes on.
///
/// # Panics
///
/// Where that thread's slot holds a value: another job's would be lost.
#[allow(unsafe_code)]
pub fn put_carried<T: Default + 'static>(slot: &'static LocalKey<RefCell<T>>, is_default: fn(&T) -> bool, taken: usize) {
    slot.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(is_default(&*slot), "a job's thread-local put back on a thread that holds another's");
        if taken != 0 {
            // SAFETY: `taken` is what `take_carried` of this slot, so of a `T`, boxed and
            // exposed, handed to this pair's `put` once.
            *slot = *unsafe { Box::from_raw(std::ptr::with_exposed_provenance_mut::<T>(taken)) };
        }
    });
}

/// [`take_carried`] for a thread-local `Cell`.
pub fn take_carried_cell<T: Default + 'static>(slot: &'static LocalKey<Cell<T>>, is_default: fn(&T) -> bool) -> usize {
    let taken = slot.try_with(|slot| {
        let value = slot.take();
        if is_default(&value) {
            slot.set(value);
            None
        } else {
            Some(value)
        }
    });
    // alloc: as in `take_carried`
    taken.ok().flatten().map_or(0, |value| Box::into_raw(Box::new(value)).expose_provenance())
}

/// [`put_carried`] for a thread-local `Cell`.
///
/// # Panics
///
/// As [`put_carried`].
#[allow(unsafe_code)]
pub fn put_carried_cell<T: Default + 'static>(slot: &'static LocalKey<Cell<T>>, is_default: fn(&T) -> bool, taken: usize) {
    slot.with(|slot| {
        let here = slot.take();
        assert!(is_default(&here), "a job's thread-local put back on a thread that holds another's");
        slot.set(here);
        if taken != 0 {
            // SAFETY: as in `put_carried`.
            slot.set(*unsafe { Box::from_raw(std::ptr::with_exposed_provenance_mut::<T>(taken)) });
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, panic};

    use super::lend;

    thread_local! {
        static SLOT: RefCell<Option<Vec<u32>>> = const { RefCell::new(None) };
    }

    fn push(it: u32) {
        SLOT.with(|slot| slot.borrow_mut().as_mut().expect("in a lend").push(it));
    }

    #[test]
    fn what_is_added_in_the_lend_comes_back() {
        let mut value = vec![1];
        assert_eq!(
            lend(&SLOT, &mut value, || {
                push(2);
                7
            }),
            7
        );
        assert_eq!(value, [1, 2]);
        assert!(SLOT.with(|slot| slot.borrow().is_none()));
    }

    #[test]
    fn what_is_added_before_an_unwind_comes_back() {
        let mut value = Vec::new();
        let unwound = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            lend(&SLOT, &mut value, || -> () {
                push(3);
                panic!("unwinds")
            })
        }));
        assert!(unwound.is_err());
        assert_eq!(value, [3]);
        assert!(SLOT.with(|slot| slot.borrow().is_none()));
    }

    /// Runs `f` on `worker`'s thread and gives back what it made.
    fn on<R: Send + 'static>(
        worker: &crossbeam_channel::Sender<Box<dyn FnOnce() + Send>>,
        f: impl FnOnce() -> R + Send + 'static,
    ) -> R {
        let (tx, rx) = crossbeam_channel::unbounded();
        worker.send(Box::new(move || tx.send(f()).unwrap())).unwrap();
        rx.recv().unwrap()
    }

    fn take() -> usize {
        super::take_carried(&SLOT, Option::is_none)
    }

    fn put(taken: usize) {
        super::put_carried(&SLOT, Option::is_none, taken);
    }

    /// Two jobs each holding a lend run crosswise on two threads whose own slots hold a value,
    /// as the scheduler hosts them: as a job comes on, the thread's value is taken aside and the
    /// job's put on the empty slot; as it goes off, the job's is taken and the thread's put back.
    /// Each job sees its own value on either thread, and each thread keeps its own.
    #[test]
    fn jobs_swapped_crosswise_onto_threads_whose_slots_are_held_each_see_their_own() {
        let workers: Vec<_> = (0..2u32)
            .map(|at| {
                let (tx, rx) = crossbeam_channel::unbounded::<Box<dyn FnOnce() + Send>>();
                let thread = std::thread::spawn(move || {
                    SLOT.set(Some(vec![900 + at]));
                    for f in rx {
                        f();
                    }
                    SLOT.take()
                });
                (tx, thread)
            })
            .collect();
        let job = |mine: u32| {
            on(&workers[mine as usize].0, move || {
                let worker = take();
                let mut value = vec![100 + mine];
                SLOT.set(Some(std::mem::take(&mut value)));
                push(mine);
                let job = take();
                put(worker);
                job
            })
        };
        let mut jobs = [job(0), job(1)];
        for round in 0..3u32 {
            for (at, held) in jobs.iter_mut().enumerate() {
                let (taken, mine) = (*held, at as u32);
                *held = on(&workers[(at + 1 + round as usize) % 2].0, move || {
                    let worker = take();
                    put(taken);
                    assert_eq!(SLOT.with_borrow(|it| it.as_ref().map(|it| it[0])), Some(100 + mine), "a job came on to another's value");
                    push(round);
                    let job = take();
                    put(worker);
                    job
                });
            }
        }
        for (at, held) in jobs.into_iter().enumerate() {
            let back = on(&workers[at].0, move || {
                let worker = take();
                put(held);
                let back = SLOT.take();
                put(worker);
                back
            });
            assert_eq!(back, Some(vec![100 + at as u32, at as u32, 0, 1, 2]));
        }
        for (at, (tx, thread)) in workers.into_iter().enumerate() {
            drop(tx);
            assert_eq!(thread.join().unwrap(), Some(vec![900 + at as u32]), "a thread lost its own value");
        }
    }

    #[test]
    #[should_panic(expected = "do not nest")]
    fn lends_do_not_nest() {
        let (mut outer, mut inner) = (Vec::new(), Vec::new());
        lend(&SLOT, &mut outer, || lend(&SLOT, &mut inner, || ()));
    }
}
