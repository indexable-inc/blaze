//! Interning of single values.
//!
//! A value is interned once, into its type's [`InternStorage`], and lives there unmoved for the
//! life of the process (see [`crate::arena`]): an [`Interned`] is a pointer to it, `Copy`, with
//! no reference count, and two are equal when they point at the same value. An [`InternedRef`]
//! is the same pointer with a lifetime, which only marks where a handle is meant to be used
//! (the query engine refuses to store one, as it is no `blaze_query::db::Value`); [`InternedRef::to_owned`] and
//! [`InternedRef::change_lifetime`] move between the two freely.
//!
//! Interned values are never freed: memory is bounded by the database's reachability GC
//! rebuilding what it keeps (docs/v2/plan/lanes/db.md).
//!
//! Safe code: the typed store and the handle's pointer are [`crate::typed`]'s.

use std::{
    fmt::{self, Debug, Display},
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
    ptr,
};

use blaze_hash::FixedState;

use crate::{
    sync::OnceLock,
    typed::{Ptr, Values},
};

pub struct Interned<T: Internable> {
    /// Points into `T::storage()`, which never moves or frees a value.
    ptr: &'static T,
}

impl<T: Internable> Interned<T> {
    #[inline]
    pub fn new(obj: T) -> Self {
        Self { ptr: intern(obj) }
    }

    /// Interns `obj`, as [`Interned::new`] does, handing out the handle with a lifetime.
    #[inline]
    pub fn new_gc<'a>(obj: T) -> InternedRef<'a, T> {
        Self::new(obj).as_ref_static()
    }

    #[inline]
    pub fn as_ref(&self) -> InternedRef<'_, T> {
        self.as_ref_static()
    }

    #[inline]
    fn as_ref_static<'a>(self) -> InternedRef<'a, T> {
        InternedRef { ptr: Ptr::new(self.ptr), _marker: PhantomData }
    }
}

/// The one copy of `obj` in `T::storage()`, interning it if it is not there.
#[inline]
fn intern<T: Internable>(obj: T) -> &'static T {
    let hash = FixedState.hash_one(&obj);
    T::storage().get().intern(hash, obj)
}

impl<T: Internable> Clone for Interned<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Internable> Copy for Interned<T> {}

/// Compares interned `Ref`s using pointer equality.
impl<T: Internable> PartialEq for Interned<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr, other.ptr)
    }
}

impl<T: Internable> Eq for Interned<T> {}

impl<T: Internable> Hash for Interned<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(ptr::from_ref(self.ptr).addr())
    }
}

impl<T: Internable> AsRef<T> for Interned<T> {
    #[inline]
    fn as_ref(&self) -> &T {
        self.ptr
    }
}

impl<T: Internable> Deref for Interned<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ptr
    }
}

impl<T: Debug + Internable> Debug for Interned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <T as Debug>::fmt(self.ptr, f)
    }
}

impl<T: Display + Internable> Display for Interned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        <T as Display>::fmt(self.ptr, f)
    }
}

/// `Send` and `Sync` where `T: Sync`, as `&T` is.
#[repr(transparent)]
pub struct InternedRef<'a, T> {
    /// A value of an [`InternStorage`], never moved or freed.
    ptr: Ptr<T>,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: Internable> InternedRef<'a, T> {
    #[inline]
    pub fn as_raw(self) -> *const T {
        self.ptr.as_ptr()
    }

    #[inline]
    pub fn to_owned(self) -> Interned<T> {
        Interned { ptr: self.ptr.get() }
    }

    #[inline]
    pub fn get(self) -> &'a T {
        self.ptr.get()
    }

    /// Changes the attached lifetime: interned values live for the process, so the lifetime is
    /// only a lint against misuse, not a soundness check.
    #[inline]
    pub fn change_lifetime<'b>(self) -> InternedRef<'b, T> {
        InternedRef { ptr: self.ptr, _marker: PhantomData }
    }
}

impl<T> Clone for InternedRef<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for InternedRef<'_, T> {}

impl<T: Hash> Hash for InternedRef<'_, T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.ptr.as_ptr().addr());
    }
}

impl<T: PartialEq> PartialEq for InternedRef<'_, T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr.as_ptr(), other.ptr.as_ptr())
    }
}

impl<T: Eq> Eq for InternedRef<'_, T> {}

impl<T> Deref for InternedRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ptr.get()
    }
}

impl<T: Debug> Debug for InternedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: Display> Display for InternedRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

pub struct InternStorage<T: ?Sized> {
    store: OnceLock<Values<T>>,
}

#[allow(
    clippy::new_without_default,
    reason = "this a const fn, so it can't be default yet. See <https://github.com/rust-lang/rust/issues/63065>"
)]
impl<T: ?Sized> InternStorage<T> {
    pub const fn new() -> Self {
        Self { store: OnceLock::new() }
    }
}

impl<T: Internable + ?Sized> InternStorage<T> {
    pub(crate) fn get(&self) -> &Values<T> {
        self.store.get_or_init(Values::new)
    }
}

pub trait Internable: Hash + Eq + Send + Sync + 'static {
    fn storage() -> &'static InternStorage<Self>;
}

/// Implements `Internable` for a given list of types, making them usable with `Interned`.
/// `gc;` is accepted and means the same: every interned value lives for the process.
#[macro_export]
#[doc(hidden)]
macro_rules! _impl_internable {
    ( gc; $($t:ty),+ $(,)? ) => {
        $crate::_impl_internable!($($t),+);
    };
    ( $($t:ty),+ $(,)? ) => { $(
        impl $crate::Internable for $t {
            fn storage() -> &'static $crate::InternStorage<Self> {
                static STORAGE: $crate::InternStorage<$t> = $crate::InternStorage::new();
                &STORAGE
            }
        }
    )+ };
}
pub use crate::_impl_internable as impl_internable;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(PartialEq, Eq, Hash, Debug)]
    #[cfg_attr(blaze_check, allow(dead_code, reason = "made only by a normal build's threaded test"))]
    struct Name(u32);
    impl_internable!(Name);

    const _: () = assert!(size_of::<Interned<Name>>() == size_of::<usize>());
    const _: () = assert!(size_of::<InternedRef<'static, Name>>() == size_of::<usize>());

    /// A value interned from many threads at once is one value: every thread gets the handle
    /// interned before them, and values first interned by racing threads are one per value.
    #[cfg(not(blaze_check))]
    #[test]
    fn interned_from_many_threads() {
        let held: Vec<_> = (0..64).map(|i| Interned::new(Name(i))).collect();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    for round in 0..20_000u32 {
                        let found = Interned::new(Name(round % 64));
                        assert!(found == held[(round % 64) as usize]);
                        let churned = Interned::new(Name(1000 + round % 7));
                        let again = Interned::new(Name(1000 + round % 7));
                        assert!(churned == again);
                    }
                });
            }
        });
        let after: Vec<_> = (0..64).map(|i| Interned::new(Name(i))).collect();
        assert!(after == held);
    }

    #[cfg(not(blaze_check))]
    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Racing(u64);
    #[cfg(not(blaze_check))]
    impl_internable!(Racing);

    /// Threads interning overlapping new values in different orders get pointer-equal handles.
    #[cfg(not(blaze_check))]
    #[test]
    fn new_values_race_to_one_handle() {
        const THREADS: u64 = 16;
        const VALUES: u64 = 20_000;
        let seen: Vec<Vec<Interned<Racing>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..VALUES)
                            .map(|n| Interned::new(Racing((n + worker * 1_237) % VALUES)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert_eq!(<Racing as Internable>::storage().get().len(), VALUES as usize);
        for (worker, handles) in seen.iter().enumerate() {
            for (i, handle) in handles.iter().enumerate() {
                let n = (i as u64 + worker as u64 * 1_237) % VALUES;
                assert!(ptr::eq(&**handle, &*Interned::new(Racing(n))));
                assert_eq!(handle.0, n);
            }
        }
    }

    #[cfg(not(blaze_check))]
    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Text(String);
    #[cfg(not(blaze_check))]
    impl_internable!(Text);

    /// Enough values to fill many chunks of every shard: the first handles still read their
    /// values and are still what interning them again gives.
    #[cfg(not(blaze_check))]
    #[test]
    fn handles_stay_valid() {
        let early: Vec<_> = (0..256).map(|n| Interned::new(Text(format!("early {n}")))).collect();
        let addresses: Vec<_> = early.iter().map(|it| ptr::from_ref(&**it)).collect();
        for n in 0..200_000 {
            Interned::new(Text(format!("later {n}")));
        }
        for (n, (handle, &at)) in early.iter().zip(&addresses).enumerate() {
            assert_eq!(handle.0, format!("early {n}"));
            assert!(ptr::eq(&**handle, at));
            assert_eq!(Interned::new(Text(format!("early {n}"))), *handle);
        }
    }

    crate::impl_internable!(String);

    #[test]
    fn simple_interned() {
        let a = Interned::new("abc".to_owned());
        let b = Interned::new("abc".to_owned());
        assert_eq!(a, b);
        assert_eq!(a.as_ref(), b.as_ref());
        assert_eq!(a.as_ref(), a.as_ref());
        #[allow(clippy::clone_on_copy)]
        let cloned = a.clone();
        assert_eq!(a, cloned);
        assert_eq!(*a, "abc");
        assert_eq!(*b, "abc");
        assert_eq!(b.as_ref().to_owned(), a);
        let c = Interned::new("def".to_owned());
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_ne!(b.as_ref(), c.as_ref());
        assert_eq!(*c.as_ref(), "def");
        assert_eq!(format!("{a} {c:?}"), "abc \"def\"");
    }

    #[derive(Debug, PartialEq, Eq, Hash)]
    struct GcString(String);
    crate::impl_internable!(gc; GcString);

    #[test]
    fn new_gc_is_new() {
        let r = Interned::new_gc(GcString("abc".to_owned()));
        let owned = Interned::new(GcString("abc".to_owned()));
        assert_eq!(r.to_owned(), owned);
        assert_eq!(r, owned.as_ref());
        assert_eq!(r.get().0, "abc");
        assert!(ptr::eq(r.as_raw(), &*owned));
        let longer: InternedRef<'static, GcString> = r.change_lifetime();
        assert_eq!(longer, r);
    }

    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Keyed(Vec<u8>);
    impl_internable!(Keyed);

    /// Interning against an `HashMap` from value to the address of its first handle, over
    /// seeded values that collide often: a value seen before gets that handle, a new one a
    /// handle of its own, and every handle reads its value.
    #[test]
    fn interning_agrees_with_a_map_model() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x1a7e);
        let mut model: blaze_hash::HashMap<Vec<u8>, *const Keyed> = blaze_hash::HashMap::default();
        for _ in 0..20_000 {
            let len = rng.index(24);
            let bytes: Vec<u8> = (0..len).map(|_| b"ab\0"[rng.index(3)]).collect();
            let handle = Interned::new(Keyed(bytes.clone()));
            assert_eq!(handle.0, bytes);
            let first = *model.entry(bytes).or_insert(ptr::from_ref(&*handle));
            assert!(ptr::eq(first, &*handle), "one handle a value");
        }
        let mut addresses: Vec<_> = model.values().map(|&at| at.addr()).collect();
        addresses.sort_unstable();
        addresses.dedup();
        assert_eq!(addresses.len(), model.len(), "a handle of its own for each value");
    }

    #[cfg(not(blaze_check))]
    #[derive(PartialEq, Eq, Hash, Debug)]
    struct Counted(u64);
    #[cfg(not(blaze_check))]
    impl_internable!(Counted);

    /// A hit, a read and, once the type's store exists, a miss all allocate nothing: values,
    /// chunks and tables come from `crate::space`'s region.
    #[cfg(not(blaze_check))]
    #[test]
    fn interning_allocates_nothing() {
        let held = Interned::new(Counted(u64::MAX));
        blaze_alloc_count::assert_at_most(0, "an interned hit and a read", || {
            assert!(Interned::new(Counted(u64::MAX)) == held);
            assert_eq!(held.0, u64::MAX);
        });
        blaze_alloc_count::assert_at_most(0, "interning new values, tables growing", || {
            for n in 0..100_000 {
                assert_eq!(Interned::new(Counted(n)).0, n);
            }
        });
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p intern --release
/// model_`): a type's first interns racing to make its store, against a set: the copy kept is
/// the first intern's of each value, every other copy is dropped once, and equal values get one
/// handle.
#[cfg(all(test, blaze_check))]
#[allow(unsafe_code, reason = "each execution's storage is reached through a pointer the test sets and clears")]
mod model {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicPtr, Ordering::Relaxed},
    };

    use blaze_check::{
        linear::{History, Spec},
        thread,
    };

    use super::*;

    /// A value that says when it is dropped. An interned value never is, so a copy dropped by
    /// the time `Interned::new` returns was found there already.
    struct Counted {
        value: u64,
        dropped: Arc<AtomicBool>,
    }

    impl Counted {
        fn new(value: u64) -> (Counted, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            (Counted { value, dropped: dropped.clone() }, dropped)
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            assert!(!self.dropped.swap(true, Relaxed), "a copy dropped twice");
        }
    }

    impl PartialEq for Counted {
        fn eq(&self, other: &Counted) -> bool {
            self.value == other.value
        }
    }

    impl Eq for Counted {}

    impl Hash for Counted {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.value.hash(state);
        }
    }

    /// This execution's storage. A `static` one would keep the store the first execution made,
    /// whose atomics each later execution starts again from null while its values stay.
    static CURRENT: AtomicPtr<InternStorage<Counted>> = AtomicPtr::new(ptr::null_mut());

    impl Internable for Counted {
        fn storage() -> &'static InternStorage<Counted> {
            // SAFETY: `model_first_interns_race` points `CURRENT` at a storage it frees only after
            // joining every thread that interns, and clears it first. The model's threads are
            // coroutines on its one OS thread, so this `Relaxed` load sees that store.
            unsafe { CURRENT.load(Relaxed).as_ref() }.expect("a model's storage")
        }
    }

    /// The sequential interner: bit `v` is set when value `v` is in it.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Set(u8);

    impl Spec for Set {
        /// The value interned.
        type Op = u64;
        /// Whether this call's copy is the one kept: the value was not there.
        type Ret = bool;

        fn apply(&mut self, &value: &u64) -> bool {
            let new = self.0 & (1 << value) == 0;
            self.0 |= 1 << value;
            new
        }
    }

    fn intern(history: &History<Set>, value: u64) -> Interned<Counted> {
        let mut handle = None;
        history.record(value, |&value| {
            let (copy, dropped) = Counted::new(value);
            let interned = Interned::new(copy);
            assert_eq!(interned.value, value, "a handle reads its value whole");
            handle = Some(interned);
            !dropped.load(Relaxed)
        });
        handle.expect("a recorded intern")
    }

    #[test]
    fn model_first_interns_race() {
        blaze_check::model(|| {
            let storage = Box::new(InternStorage::<Counted>::new());
            CURRENT.store(ptr::from_ref(&*storage).cast_mut(), Relaxed);
            let history = Arc::new(History::<Set>::new());
            let other = {
                let history = history.clone();
                thread::spawn(move || [1, 2].map(|value| intern(&history, value)))
            };
            let two = intern(&history, 2);
            let one = intern(&history, 1);
            let [one_there, two_there] = other.join().unwrap();
            history.check(Set::default());
            assert!(one == one_there && two == two_there, "equal values, one handle");
            assert!(one != two, "two values, two handles");
            assert_eq!(storage.get().len(), 2, "one store, made once");
            CURRENT.store(ptr::null_mut(), Relaxed);
            drop(storage);
        });
    }
}
