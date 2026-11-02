//! `std::sync::atomic`'s types. In a model each operation is a schedule point and a load reads
//! any store the C11 rules leave it (`rt/memory.rs`); outside one each is `std`'s. A shim holds
//! `std`'s atomic (the value outside a model, and the newest store inside one), the value it
//! was made with (every execution starts from it), and its object id. Memory zeroed by an
//! allocator is a valid shim holding 0, as it is a valid `std` atomic.

use std::{
    cell::Cell,
    fmt,
    panic::Location,
    sync::atomic::{self as imp, AtomicU64 as Id, Ordering::Relaxed},
};

pub use std::sync::atomic::{Ordering, compiler_fence};

use crate::rt::{self, At, FENCE, Kind, Op, Runtime};

/// A `std` atomic as the model's `u64` values.
trait Plain {
    fn get(&self) -> u64;
    fn put(&self, value: u64);
}

/// Where a shim's state is, for the operations every type shares.
struct Loc<'a, P: Plain> {
    id: &'a Id,
    real: &'a P,
    init: u64,
}

impl<P: Plain> Loc<'_, P> {
    fn object(&self, rt: &mut Runtime) -> u32 {
        // Each execution starts it from the value it was made with: a `static` touched by an
        // earlier execution, or outside any model (another test in the process), brings none of
        // that in, so a model's outcomes never depend on what ran before it.
        rt.atomic(self.id, self.init)
    }

    fn run<R>(
        &self,
        at: At,
        what: &'static str,
        kind: Kind,
        act: impl FnOnce(&mut Runtime, u32, u64) -> Result<R, String>,
    ) -> Option<R> {
        let obj = Cell::new(0);
        rt::op(
            at,
            what,
            |rt| {
                obj.set(self.object(rt));
                Ok(Op { obj: obj.get(), kind })
            },
            |rt| act(rt, obj.get(), self.real.get()),
        )
    }

    fn load(&self, at: At, order: Ordering) -> Option<u64> {
        check_load(order);
        self.run(at, "load", Kind::Load, |rt, obj, real| rt.load(obj, order, at, real))
    }

    fn store(&self, at: At, value: u64, order: Ordering) -> Option<()> {
        check_store(order);
        self.run(at, "store", Kind::Store, |rt, obj, real| {
            rt.store(obj, value, order, at, real)?;
            self.real.put(value);
            Ok(())
        })
    }

    /// A read-modify-write: `f` gives the new value from the old, or `None` to fail (a
    /// compare-exchange's load with `failure`).
    fn rmw(
        &self,
        at: At,
        what: &'static str,
        success: Ordering,
        failure: Ordering,
        f: impl FnOnce(u64) -> Option<u64>,
    ) -> Option<Result<u64, u64>> {
        self.run(at, what, Kind::Rmw, |rt, obj, real| {
            let new = Cell::new(None);
            let out = rt.rmw(obj, success, failure, at, real, |old| {
                new.set(f(old));
                new.get()
            })?;
            if let Some(new) = new.get() {
                self.real.put(new);
            }
            Ok(out)
        })
    }

    fn swap(&self, at: At, value: u64, order: Ordering) -> Option<u64> {
        self.rmw(at, "swap", order, Ordering::Relaxed, |_| Some(value)).map(unwrap_either)
    }

    fn compare_exchange(
        &self,
        at: At,
        current: u64,
        new: u64,
        success: Ordering,
        failure: Ordering,
    ) -> Option<Result<u64, u64>> {
        check_failure(failure);
        self.rmw(at, "compare_exchange", success, failure, |old| (old == current).then_some(new))
    }

    fn fetch(&self, at: At, what: &'static str, order: Ordering, f: impl FnOnce(u64) -> u64) -> Option<u64> {
        self.rmw(at, what, order, Ordering::Relaxed, |old| Some(f(old))).map(unwrap_either)
    }

    /// `get_mut`: a non-atomic write every other access must be ordered with; puts the newest
    /// value in `real` for the caller to hand out.
    fn plain_write(&self, at: At) {
        let value = rt::with(|rt| {
            let obj = self.object(rt);
            rt.plain_write(obj, at)
        });
        match value {
            Some(Ok(value)) => self.real.put(value),
            Some(Err(found)) => rt::stop(found),
            None => {}
        }
    }

    /// `into_inner`: a non-atomic read of the newest value.
    fn plain_read(&self, at: At) -> Option<u64> {
        let value = rt::with(|rt| {
            let obj = self.object(rt);
            rt.plain_read(obj, at, self.real.get())
        })?;
        match value {
            Ok(value) => Some(value),
            Err(found) => rt::stop(found),
        }
    }
}

fn unwrap_either(out: Result<u64, u64>) -> u64 {
    match out {
        Ok(it) | Err(it) => it,
    }
}

fn check_load(order: Ordering) {
    match order {
        Ordering::Release => panic!("there is no such thing as a release load"),
        Ordering::AcqRel => panic!("there is no such thing as an acquire-release load"),
        _ => {}
    }
}

fn check_store(order: Ordering) {
    match order {
        Ordering::Acquire => panic!("there is no such thing as an acquire store"),
        Ordering::AcqRel => panic!("there is no such thing as an acquire-release store"),
        _ => {}
    }
}

fn check_failure(order: Ordering) {
    match order {
        Ordering::Release => panic!("there is no such thing as a release failure ordering"),
        Ordering::AcqRel => panic!("there is no such thing as an acquire-release failure ordering"),
        _ => {}
    }
}

/// `std::sync::atomic::fence`. An SC fence is a schedule point (an RMW of one global clock);
/// the others act on the thread's own clocks alone, so where they run among other threads'
/// operations changes nothing and they are not.
#[track_caller]
pub fn fence(order: Ordering) {
    if order == Ordering::Relaxed {
        panic!("there is no such thing as a relaxed fence");
    }
    let at = Location::caller();
    let done = if order == Ordering::SeqCst {
        rt::op(at, "fence", |_| Ok(Op { obj: FENCE, kind: Kind::Rmw }), |rt| {
            rt.fence(order);
            Ok(())
        })
    } else {
        rt::with(|rt| {
            rt.note(at, "fence");
            rt.fence(order);
        })
    };
    if done.is_none() {
        imp::fence(order);
    }
}

macro_rules! atomic_int {
    ($($(#[$attr:meta])* $name:ident($int:ty);)*) => {$(
        impl Plain for imp::$name {
            #[inline]
            fn get(&self) -> u64 {
                self.load(Relaxed) as u64
            }

            #[inline]
            fn put(&self, value: u64) {
                self.store(value as $int, Relaxed);
            }
        }

        $(#[$attr])*
        pub struct $name {
            real: imp::$name,
            init: $int,
            id: Id,
        }

        impl $name {
            pub const fn new(value: $int) -> $name {
                $name { real: imp::$name::new(value), init: value, id: Id::new(0) }
            }

            fn loc(&self) -> Loc<'_, imp::$name> {
                Loc { id: &self.id, real: &self.real, init: self.init as u64 }
            }

            #[track_caller]
            pub fn load(&self, order: Ordering) -> $int {
                match self.loc().load(Location::caller(), order) {
                    Some(value) => value as $int,
                    None => self.real.load(order),
                }
            }

            #[track_caller]
            pub fn store(&self, value: $int, order: Ordering) {
                if self.loc().store(Location::caller(), value as u64, order).is_none() {
                    self.real.store(value, order);
                }
            }

            #[track_caller]
            pub fn swap(&self, value: $int, order: Ordering) -> $int {
                match self.loc().swap(Location::caller(), value as u64, order) {
                    Some(old) => old as $int,
                    None => self.real.swap(value, order),
                }
            }

            #[track_caller]
            pub fn compare_exchange(
                &self,
                current: $int,
                new: $int,
                success: Ordering,
                failure: Ordering,
            ) -> Result<$int, $int> {
                let at = Location::caller();
                match self.loc().compare_exchange(at, current as u64, new as u64, success, failure) {
                    Some(out) => out.map(|it| it as $int).map_err(|it| it as $int),
                    None => self.real.compare_exchange(current, new, success, failure),
                }
            }

            /// As [`Self::compare_exchange`]: like loom, the model explores no spurious failure.
            #[track_caller]
            pub fn compare_exchange_weak(
                &self,
                current: $int,
                new: $int,
                success: Ordering,
                failure: Ordering,
            ) -> Result<$int, $int> {
                let at = Location::caller();
                match self.loc().compare_exchange(at, current as u64, new as u64, success, failure) {
                    Some(out) => out.map(|it| it as $int).map_err(|it| it as $int),
                    None => self.real.compare_exchange_weak(current, new, success, failure),
                }
            }

            #[track_caller]
            pub fn fetch_add(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| (old as $int).wrapping_add(value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_add", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_add(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_sub(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| (old as $int).wrapping_sub(value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_sub", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_sub(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_and(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| ((old as $int) & value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_and", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_and(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_nand(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| (!((old as $int) & value)) as u64;
                match self.loc().fetch(Location::caller(), "fetch_nand", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_nand(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_or(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| ((old as $int) | value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_or", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_or(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_xor(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| ((old as $int) ^ value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_xor", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_xor(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_max(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| (old as $int).max(value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_max", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_max(value, order),
                }
            }

            #[track_caller]
            pub fn fetch_min(&self, value: $int, order: Ordering) -> $int {
                let f = |old: u64| (old as $int).min(value) as u64;
                match self.loc().fetch(Location::caller(), "fetch_min", order, f) {
                    Some(old) => old as $int,
                    None => self.real.fetch_min(value, order),
                }
            }

            /// `std`'s loop: a load, then compare-exchanges until one succeeds or `f` gives up.
            #[track_caller]
            pub fn fetch_update<F>(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: F,
            ) -> Result<$int, $int>
            where
                F: FnMut($int) -> Option<$int>,
            {
                let mut prev = self.load(fetch_order);
                while let Some(next) = f(prev) {
                    match self.compare_exchange_weak(prev, next, set_order, fetch_order) {
                        Ok(it) => return Ok(it),
                        Err(next_prev) => prev = next_prev,
                    }
                }
                Err(prev)
            }

            #[track_caller]
            pub fn get_mut(&mut self) -> &mut $int {
                self.loc().plain_write(Location::caller());
                self.real.get_mut()
            }

            #[track_caller]
            pub fn into_inner(self) -> $int {
                let value = self.loc().plain_read(Location::caller());
                match value {
                    Some(value) => value as $int,
                    None => self.real.into_inner(),
                }
            }
        }

        impl Default for $name {
            fn default() -> $name {
                $name::new(Default::default())
            }
        }

        impl From<$int> for $name {
            fn from(value: $int) -> $name {
                $name::new(value)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.real, f)
            }
        }
    )*};
}

atomic_int! {
    /// `std::sync::atomic::AtomicU8`.
    AtomicU8(u8);
    /// `std::sync::atomic::AtomicU16`.
    AtomicU16(u16);
    /// `std::sync::atomic::AtomicU32`.
    AtomicU32(u32);
    /// `std::sync::atomic::AtomicU64`.
    AtomicU64(u64);
    /// `std::sync::atomic::AtomicUsize`.
    AtomicUsize(usize);
    /// `std::sync::atomic::AtomicI8`.
    AtomicI8(i8);
    /// `std::sync::atomic::AtomicI16`.
    AtomicI16(i16);
    /// `std::sync::atomic::AtomicI32`.
    AtomicI32(i32);
    /// `std::sync::atomic::AtomicI64`.
    AtomicI64(i64);
    /// `std::sync::atomic::AtomicIsize`.
    AtomicIsize(isize);
}

impl Plain for imp::AtomicBool {
    #[inline]
    fn get(&self) -> u64 {
        u64::from(self.load(Relaxed))
    }

    #[inline]
    fn put(&self, value: u64) {
        self.store(value != 0, Relaxed);
    }
}

/// `std::sync::atomic::AtomicBool`.
pub struct AtomicBool {
    real: imp::AtomicBool,
    init: bool,
    id: Id,
}

impl AtomicBool {
    pub const fn new(value: bool) -> AtomicBool {
        AtomicBool { real: imp::AtomicBool::new(value), init: value, id: Id::new(0) }
    }

    fn loc(&self) -> Loc<'_, imp::AtomicBool> {
        Loc { id: &self.id, real: &self.real, init: u64::from(self.init) }
    }

    #[track_caller]
    pub fn load(&self, order: Ordering) -> bool {
        match self.loc().load(Location::caller(), order) {
            Some(value) => value != 0,
            None => self.real.load(order),
        }
    }

    #[track_caller]
    pub fn store(&self, value: bool, order: Ordering) {
        if self.loc().store(Location::caller(), u64::from(value), order).is_none() {
            self.real.store(value, order);
        }
    }

    #[track_caller]
    pub fn swap(&self, value: bool, order: Ordering) -> bool {
        match self.loc().swap(Location::caller(), u64::from(value), order) {
            Some(old) => old != 0,
            None => self.real.swap(value, order),
        }
    }

    #[track_caller]
    pub fn compare_exchange(
        &self,
        current: bool,
        new: bool,
        success: Ordering,
        failure: Ordering,
    ) -> Result<bool, bool> {
        let at = Location::caller();
        let (current64, new64) = (u64::from(current), u64::from(new));
        match self.loc().compare_exchange(at, current64, new64, success, failure) {
            Some(out) => out.map(|it| it != 0).map_err(|it| it != 0),
            None => self.real.compare_exchange(current, new, success, failure),
        }
    }

    /// As [`Self::compare_exchange`]: like loom, the model explores no spurious failure.
    #[track_caller]
    pub fn compare_exchange_weak(
        &self,
        current: bool,
        new: bool,
        success: Ordering,
        failure: Ordering,
    ) -> Result<bool, bool> {
        let at = Location::caller();
        let (current64, new64) = (u64::from(current), u64::from(new));
        match self.loc().compare_exchange(at, current64, new64, success, failure) {
            Some(out) => out.map(|it| it != 0).map_err(|it| it != 0),
            None => self.real.compare_exchange_weak(current, new, success, failure),
        }
    }

    #[track_caller]
    pub fn fetch_and(&self, value: bool, order: Ordering) -> bool {
        let f = |old: u64| old & u64::from(value);
        match self.loc().fetch(Location::caller(), "fetch_and", order, f) {
            Some(old) => old != 0,
            None => self.real.fetch_and(value, order),
        }
    }

    #[track_caller]
    pub fn fetch_nand(&self, value: bool, order: Ordering) -> bool {
        let f = |old: u64| u64::from(!(old != 0 && value));
        match self.loc().fetch(Location::caller(), "fetch_nand", order, f) {
            Some(old) => old != 0,
            None => self.real.fetch_nand(value, order),
        }
    }

    #[track_caller]
    pub fn fetch_or(&self, value: bool, order: Ordering) -> bool {
        let f = |old: u64| old | u64::from(value);
        match self.loc().fetch(Location::caller(), "fetch_or", order, f) {
            Some(old) => old != 0,
            None => self.real.fetch_or(value, order),
        }
    }

    #[track_caller]
    pub fn fetch_xor(&self, value: bool, order: Ordering) -> bool {
        let f = |old: u64| old ^ u64::from(value);
        match self.loc().fetch(Location::caller(), "fetch_xor", order, f) {
            Some(old) => old != 0,
            None => self.real.fetch_xor(value, order),
        }
    }

    #[track_caller]
    pub fn fetch_not(&self, order: Ordering) -> bool {
        self.fetch_xor(true, order)
    }

    #[track_caller]
    pub fn fetch_update<F>(&self, set_order: Ordering, fetch_order: Ordering, mut f: F) -> Result<bool, bool>
    where
        F: FnMut(bool) -> Option<bool>,
    {
        let mut prev = self.load(fetch_order);
        while let Some(next) = f(prev) {
            match self.compare_exchange_weak(prev, next, set_order, fetch_order) {
                Ok(it) => return Ok(it),
                Err(next_prev) => prev = next_prev,
            }
        }
        Err(prev)
    }

    #[track_caller]
    pub fn get_mut(&mut self) -> &mut bool {
        self.loc().plain_write(Location::caller());
        self.real.get_mut()
    }

    #[track_caller]
    pub fn into_inner(self) -> bool {
        let value = self.loc().plain_read(Location::caller());
        match value {
            Some(value) => value != 0,
            None => self.real.into_inner(),
        }
    }
}

impl Default for AtomicBool {
    fn default() -> AtomicBool {
        AtomicBool::new(false)
    }
}

impl From<bool> for AtomicBool {
    fn from(value: bool) -> AtomicBool {
        AtomicBool::new(value)
    }
}

impl fmt::Debug for AtomicBool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.real, f)
    }
}

impl<T> Plain for imp::AtomicPtr<T> {
    #[inline]
    fn get(&self) -> u64 {
        self.load(Relaxed).expose_provenance() as u64
    }

    #[inline]
    fn put(&self, value: u64) {
        self.store(std::ptr::with_exposed_provenance_mut(value as usize), Relaxed);
    }
}

fn to_ptr<T>(value: u64) -> *mut T {
    std::ptr::with_exposed_provenance_mut(value as usize)
}

fn from_ptr<T>(ptr: *mut T) -> u64 {
    ptr.expose_provenance() as u64
}

/// `std::sync::atomic::AtomicPtr`. A pointer goes through the model as its address with its
/// provenance exposed, and comes back with it.
pub struct AtomicPtr<T> {
    real: imp::AtomicPtr<T>,
    /// The pointer it was made with (in an atomic: a raw pointer is neither `Send` nor `Sync`).
    init: imp::AtomicPtr<T>,
    id: Id,
}

impl<T> AtomicPtr<T> {
    pub const fn new(ptr: *mut T) -> AtomicPtr<T> {
        AtomicPtr { real: imp::AtomicPtr::new(ptr), init: imp::AtomicPtr::new(ptr), id: Id::new(0) }
    }

    fn loc(&self) -> Loc<'_, imp::AtomicPtr<T>> {
        Loc { id: &self.id, real: &self.real, init: from_ptr(self.init.load(Relaxed)) }
    }

    #[track_caller]
    pub fn load(&self, order: Ordering) -> *mut T {
        match self.loc().load(Location::caller(), order) {
            Some(value) => to_ptr(value),
            None => self.real.load(order),
        }
    }

    #[track_caller]
    pub fn store(&self, ptr: *mut T, order: Ordering) {
        if self.loc().store(Location::caller(), from_ptr(ptr), order).is_none() {
            self.real.store(ptr, order);
        }
    }

    #[track_caller]
    pub fn swap(&self, ptr: *mut T, order: Ordering) -> *mut T {
        match self.loc().swap(Location::caller(), from_ptr(ptr), order) {
            Some(old) => to_ptr(old),
            None => self.real.swap(ptr, order),
        }
    }

    #[track_caller]
    pub fn compare_exchange(
        &self,
        current: *mut T,
        new: *mut T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*mut T, *mut T> {
        let at = Location::caller();
        match self.loc().compare_exchange(at, from_ptr(current), from_ptr(new), success, failure) {
            Some(out) => out.map(to_ptr).map_err(to_ptr),
            None => self.real.compare_exchange(current, new, success, failure),
        }
    }

    /// As [`Self::compare_exchange`]: like loom, the model explores no spurious failure.
    #[track_caller]
    pub fn compare_exchange_weak(
        &self,
        current: *mut T,
        new: *mut T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*mut T, *mut T> {
        let at = Location::caller();
        match self.loc().compare_exchange(at, from_ptr(current), from_ptr(new), success, failure) {
            Some(out) => out.map(to_ptr).map_err(to_ptr),
            None => self.real.compare_exchange_weak(current, new, success, failure),
        }
    }

    #[track_caller]
    pub fn fetch_update<F>(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        mut f: F,
    ) -> Result<*mut T, *mut T>
    where
        F: FnMut(*mut T) -> Option<*mut T>,
    {
        let mut prev = self.load(fetch_order);
        while let Some(next) = f(prev) {
            match self.compare_exchange_weak(prev, next, set_order, fetch_order) {
                Ok(it) => return Ok(it),
                Err(next_prev) => prev = next_prev,
            }
        }
        Err(prev)
    }

    #[track_caller]
    pub fn get_mut(&mut self) -> &mut *mut T {
        self.loc().plain_write(Location::caller());
        self.real.get_mut()
    }

    #[track_caller]
    pub fn into_inner(self) -> *mut T {
        let value = self.loc().plain_read(Location::caller());
        match value {
            Some(value) => to_ptr(value),
            None => self.real.into_inner(),
        }
    }
}

impl<T> Default for AtomicPtr<T> {
    fn default() -> AtomicPtr<T> {
        AtomicPtr::new(std::ptr::null_mut())
    }
}

impl<T> From<*mut T> for AtomicPtr<T> {
    fn from(ptr: *mut T) -> AtomicPtr<T> {
        AtomicPtr::new(ptr)
    }
}

impl<T> fmt::Debug for AtomicPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.real, f)
    }
}
