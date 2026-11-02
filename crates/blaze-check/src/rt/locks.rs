//! Locks, condition variables and once cells, as loom's `rt/mutex.rs`, `rt/rwlock.rs` and
//! `rt/condvar.rs`: a thread that cannot take a lock is blocked at its operation until the lock
//! is released; taking one acquires what its last release released.

use super::{Kind, Object, Runtime, Wait};
use crate::clock::{Clock, MAX_THREADS};

pub(crate) struct Mutex {
    holder: Option<u8>,
    sync: Clock,
}

pub(crate) struct RwLock {
    /// A bit per reading thread.
    readers: u8,
    writer: Option<u8>,
    sync: Clock,
}

pub(crate) struct Condvar {
    /// Waiting threads, first come first notified: `waiters[..len]`. A thread waits at one
    /// condvar at a time and leaves when notified, so there are at most `MAX_THREADS`.
    waiters: [u8; MAX_THREADS],
    len: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnceState {
    Empty,
    Running(u8),
    Done,
}

/// A once cell's state in this execution; its value is in the cell itself.
pub(crate) struct Once {
    pub(crate) state: OnceState,
    sync: Clock,
}

/// What a once cell's operation found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Found {
    /// Initialized, and what initialized it happens before the caller's next event.
    Done,
    /// Not initialized, and the caller now initializes it.
    Run,
    /// Not initialized (a `get`).
    Empty,
}

impl Runtime {
    pub(crate) fn mutex(&mut self, id: &std::sync::atomic::AtomicU64) -> u32 {
        self.object(id, |_| Object::Mutex(Mutex { holder: None, sync: Clock::ZERO }))
    }

    pub(crate) fn rwlock(&mut self, id: &std::sync::atomic::AtomicU64) -> u32 {
        self.object(id, |_| {
            Object::RwLock(RwLock { readers: 0, writer: None, sync: Clock::ZERO })
        })
    }

    pub(crate) fn condvar(&mut self, id: &std::sync::atomic::AtomicU64) -> u32 {
        self.object(id, |_| Object::Condvar(Condvar { waiters: [0; MAX_THREADS], len: 0 }))
    }

    pub(crate) fn once(&mut self, id: &std::sync::atomic::AtomicU64, set: bool) -> u32 {
        let state = if set { OnceState::Done } else { OnceState::Empty };
        self.object(id, |_| Object::Once(Once { state, sync: Clock::ZERO }))
    }

    fn lock_mut(&mut self, obj: u32) -> &mut Mutex {
        match &mut self.objects[obj as usize].obj {
            Object::Mutex(it) => it,
            _ => unreachable!("blaze-check: a lock operation on another object"),
        }
    }

    fn rw_mut(&mut self, obj: u32) -> &mut RwLock {
        match &mut self.objects[obj as usize].obj {
            Object::RwLock(it) => it,
            _ => unreachable!("blaze-check: a lock operation on another object"),
        }
    }

    pub(crate) fn once_mut(&mut self, obj: u32) -> &mut Once {
        match &mut self.objects[obj as usize].obj {
            Object::Once(it) => it,
            _ => unreachable!("blaze-check: a once operation on another object"),
        }
    }

    fn block_me(&mut self, obj: u32) {
        let me = self.current();
        self.threads[me].state = super::State::Blocked(Wait::Object(obj));
    }

    /// Before `lock`: blocks the thread while another holds it.
    pub(crate) fn mutex_wait(&mut self, obj: u32) -> Result<(), String> {
        let me = self.current();
        match self.lock_mut(obj).holder {
            Some(holder) if holder as usize == me => {
                // alloc: a finding's or an error's text, made once, when the model stops
                Err(format!("{} locks a mutex it holds: a deadlock", self.describe(me)))
            }
            Some(_) => {
                self.block_me(obj);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Takes the mutex if it is free.
    pub(crate) fn mutex_take(&mut self, obj: u32) -> bool {
        let me = self.current();
        let lock = self.lock_mut(obj);
        if lock.holder.is_some() {
            return false;
        }
        lock.holder = Some(me as u8);
        let sync = lock.sync;
        self.threads[me].clock.join(&sync);
        self.block_others(obj, |kind| kind == Kind::Write);
        true
    }

    pub(crate) fn mutex_release(&mut self, obj: u32) {
        let me = self.tick();
        let clock = self.threads[me].clock;
        let lock = self.lock_mut(obj);
        lock.holder = None;
        lock.sync.join(&clock);
        self.wake(Wait::Object(obj));
    }

    /// Before `read` (`write: false`) or `write`: blocks the thread while it cannot take it.
    pub(crate) fn rw_wait(&mut self, obj: u32, write: bool) -> Result<(), String> {
        let me = self.current();
        let lock = self.rw_mut(obj);
        if lock.writer == Some(me as u8) || (write && lock.readers & (1 << me) != 0) {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!("{} takes a lock it holds: a deadlock", self.describe(me)));
        }
        if lock.writer.is_some() || (write && lock.readers != 0) {
            self.block_me(obj);
        }
        Ok(())
    }

    pub(crate) fn rw_take(&mut self, obj: u32, write: bool) -> bool {
        let me = self.current();
        let lock = self.rw_mut(obj);
        if lock.writer.is_some() || (write && lock.readers != 0) {
            return false;
        }
        match write {
            true => lock.writer = Some(me as u8),
            false => lock.readers |= 1 << me,
        }
        let sync = lock.sync;
        self.threads[me].clock.join(&sync);
        match write {
            true => self.block_others(obj, |kind| matches!(kind, Kind::Read | Kind::Write)),
            false => self.block_others(obj, |kind| kind == Kind::Write),
        }
        true
    }

    pub(crate) fn rw_release(&mut self, obj: u32, write: bool) {
        let me = self.tick();
        let clock = self.threads[me].clock;
        let lock = self.rw_mut(obj);
        lock.sync.join(&clock);
        match write {
            true => lock.writer = None,
            false => lock.readers &= !(1 << me),
        }
        if lock.writer.is_none() && lock.readers == 0 {
            self.wake(Wait::Object(obj));
        }
    }

    pub(crate) fn condvar_enqueue(&mut self, obj: u32) {
        let me = self.current() as u8;
        let Object::Condvar(condvar) = &mut self.objects[obj as usize].obj else {
            unreachable!("blaze-check: a condvar operation on another object")
        };
        let len = usize::from(condvar.len);
        assert!(len < MAX_THREADS, "blaze-check: a condvar with a waiter per thread already");
        condvar.waiters[len] = me;
        condvar.len += 1;
    }

    pub(crate) fn condvar_notify(&mut self, obj: u32, all: bool) {
        let Object::Condvar(condvar) = &mut self.objects[obj as usize].obj else {
            unreachable!("blaze-check: a condvar operation on another object")
        };
        let len = usize::from(condvar.len);
        let woken = if all { len } else { len.min(1) };
        let waiters = condvar.waiters;
        condvar.waiters.copy_within(woken..len, 0);
        condvar.len = (len - woken) as u8;
        for &thread in &waiters[..woken] {
            let th = &mut self.threads[thread as usize];
            if th.state == super::State::Blocked(Wait::Notify(obj)) {
                th.state = super::State::Runnable;
            }
        }
    }

    /// Before an initialization of a once cell: blocks the thread while another initializes it.
    pub(crate) fn once_wait(&mut self, obj: u32) -> Result<(), String> {
        let me = self.current();
        match self.once_mut(obj).state {
            // alloc: a finding's or an error's text, made once, when the model stops
            OnceState::Running(by) if by as usize == me => Err(format!(
                "{} initializes a once cell from inside its own initialization: a deadlock",
                self.describe(me)
            )),
            OnceState::Running(_) => {
                self.block_me(obj);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A look at a once cell: initialized, or, with `init`, the caller's to initialize.
    pub(crate) fn once_look(&mut self, obj: u32, init: bool) -> Found {
        let me = self.current();
        let once = self.once_mut(obj);
        match once.state {
            OnceState::Done => {
                let sync = once.sync;
                self.threads[me].clock.join(&sync);
                Found::Done
            }
            OnceState::Empty if init => {
                once.state = OnceState::Running(me as u8);
                self.block_others(obj, |kind| kind == Kind::Write);
                Found::Run
            }
            OnceState::Empty | OnceState::Running(_) => Found::Empty,
        }
    }

    /// The caller's initialization ended: the cell holds its value.
    pub(crate) fn once_done(&mut self, obj: u32) {
        let me = self.tick();
        let clock = self.threads[me].clock;
        let once = self.once_mut(obj);
        once.state = OnceState::Done;
        once.sync = clock;
        self.wake(Wait::Object(obj));
    }
}
