//! Interning of slices, potentially with a header.
//!
//! See [`crate::intern`] for how interned values live. [`InternedSlice`] and
//! [`InternedSliceRef`] are essentially [`Interned<(Header, Box<[SliceType]>)>`][crate::Interned]
//! and [`InternedRef`][crate::InternedRef] with the same types, but more optimized: the header,
//! the length and the slice are stored once, together, and the pointer is thin.
//!
//! Safe code: the typed store, the value's layout and the handle's pointer are
//! [`crate::typed`]'s.

use std::{
    fmt::{self, Debug},
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
};

use blaze_hash::FixedState;

use crate::{
    sync::OnceLock,
    typed::{SlicePtr, Slices},
};

/// A header followed by a slice, laid out as one value.
#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct HeaderSlice<H, S: ?Sized> {
    pub header: H,
    pub slice: S,
}

/// A header with the length of the slice after it.
#[derive(Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct HeaderWithLength<H> {
    pub header: H,
    pub length: usize,
}

type Pointee<T> =
    crate::typed::Pointee<<T as SliceInternable>::Header, <T as SliceInternable>::SliceType>;

type Ptr<T> = SlicePtr<<T as SliceInternable>::Header, <T as SliceInternable>::SliceType>;

/// `Send` and `Sync`, as its header and items are.
pub struct InternedSlice<T: SliceInternable> {
    /// The start of a value of `T::storage()`, never moved or freed.
    ptr: Ptr<T>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: SliceInternable> InternedSlice<T> {
    #[inline]
    pub fn from_header_and_slice<'a>(
        header: T::Header,
        slice: &[T::SliceType],
    ) -> InternedSliceRef<'a, T> {
        let hash = FixedState.hash_one((&header, slice));
        let ptr = T::storage().get().intern(hash, header, slice);
        InternedSliceRef { ptr, _marker: PhantomData }
    }

    #[inline]
    pub fn as_ref(&self) -> InternedSliceRef<'_, T> {
        InternedSliceRef { ptr: self.ptr, _marker: PhantomData }
    }
}

/// Compares interned `Ref`s using pointer equality.
impl<T: SliceInternable> PartialEq for InternedSlice<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: SliceInternable> Eq for InternedSlice<T> {}

impl<T: SliceInternable> Hash for InternedSlice<T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.ptr.addr())
    }
}

impl<T: SliceInternable> Deref for InternedSlice<T> {
    type Target = Pointee<T>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ptr.get()
    }
}

impl<T: SliceInternable> Clone for InternedSlice<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: SliceInternable> Copy for InternedSlice<T> {}

impl<T> Debug for InternedSlice<T>
where
    T: SliceInternable,
    T::SliceType: Debug,
    T::Header: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

/// `Send` and `Sync`, as its header and items are.
#[repr(transparent)]
pub struct InternedSliceRef<'a, T: SliceInternable> {
    /// The start of a value of `T::storage()`, never moved or freed.
    ptr: Ptr<T>,
    _marker: PhantomData<(fn() -> T, &'a ())>,
}

impl<'a, T: SliceInternable> InternedSliceRef<'a, T> {
    #[inline]
    pub fn to_owned(self) -> InternedSlice<T> {
        InternedSlice { ptr: self.ptr, _marker: PhantomData }
    }

    #[inline]
    pub fn get(self) -> &'a Pointee<T> {
        self.ptr.get()
    }

    /// Changes the attached lifetime: interned values live for the process, so the lifetime is
    /// only a lint against misuse, not a soundness check.
    #[inline]
    pub fn change_lifetime<'b>(self) -> InternedSliceRef<'b, T> {
        InternedSliceRef { ptr: self.ptr, _marker: PhantomData }
    }
}

impl<T: SliceInternable> Clone for InternedSliceRef<'_, T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: SliceInternable> Copy for InternedSliceRef<'_, T> {}

impl<T: SliceInternable> Hash for InternedSliceRef<'_, T> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.ptr.addr());
    }
}

impl<T: SliceInternable> PartialEq for InternedSliceRef<'_, T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr
    }
}

impl<T: SliceInternable> Eq for InternedSliceRef<'_, T> {}

impl<T: SliceInternable> Deref for InternedSliceRef<'_, T> {
    type Target = Pointee<T>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

impl<T> Debug for InternedSliceRef<'_, T>
where
    T: SliceInternable,
    T::SliceType: Debug,
    T::Header: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

pub struct InternSliceStorage<T: SliceInternable> {
    store: OnceLock<Slices<T::Header, T::SliceType>>,
}

#[allow(
    clippy::new_without_default,
    reason = "this a const fn, so it can't be default yet. See <https://github.com/rust-lang/rust/issues/63065>"
)]
impl<T: SliceInternable> InternSliceStorage<T> {
    pub const fn new() -> Self {
        Self { store: OnceLock::new() }
    }
}

impl<T: SliceInternable> InternSliceStorage<T> {
    pub(crate) fn get(&self) -> &Slices<T::Header, T::SliceType> {
        self.store.get_or_init(Slices::new)
    }
}

pub trait SliceInternable: Sized + 'static {
    type Header: Eq + Hash + Send + Sync;
    type SliceType: Eq + Hash + Send + Sync + Copy + 'static;
    fn storage() -> &'static InternSliceStorage<Self>;
}

/// Implements `SliceInternable` for a given list of types, making them usable with `InternedSlice`.
/// `gc;` is accepted and means the same: every interned slice lives for the process.
#[macro_export]
#[doc(hidden)]
macro_rules! _impl_slice_internable {
    ( gc; $tag:ident, $h:ty, $t:ty $(,)? ) => {
        $crate::_impl_slice_internable!($tag, $h, $t);
    };
    ( $tag:ident, $h:ty, $t:ty $(,)? ) => {
        #[allow(unreachable_pub)]
        pub struct $tag;
        impl $crate::SliceInternable for $tag {
            type Header = $h;
            type SliceType = $t;
            fn storage() -> &'static $crate::InternSliceStorage<Self> {
                static STORAGE: $crate::InternSliceStorage<$tag> =
                    $crate::InternSliceStorage::new();
                &STORAGE
            }
        }
    };
}
pub use crate::_impl_slice_internable as impl_slice_internable;

#[cfg(test)]
mod tests {
    use super::*;

    crate::impl_slice_internable!(gc; Numbers, String, u32);
    type NumbersSlice = InternedSlice<Numbers>;

    const _: () = assert!(size_of::<InternedSlice<Numbers>>() == size_of::<usize>());
    const _: () = assert!(size_of::<InternedSliceRef<'static, Numbers>>() == size_of::<usize>());

    #[test]
    fn simple_slices() {
        let d = NumbersSlice::from_header_and_slice("abc".to_owned(), &[123, 456]);
        let e = NumbersSlice::from_header_and_slice("abc".to_owned(), &[123, 456]);
        assert_eq!(d, e);
        assert_eq!(d.to_owned(), e.to_owned());
        assert_eq!(d.header.length, 2);
        assert_eq!(d.header.header, "abc");
        assert_eq!(d.slice, [123, 456]);
        let other_header = NumbersSlice::from_header_and_slice("abd".to_owned(), &[123, 456]);
        let longer = NumbersSlice::from_header_and_slice("abc".to_owned(), &[123, 456, 0]);
        let empty = NumbersSlice::from_header_and_slice("abc".to_owned(), &[]);
        assert_ne!(d, other_header);
        assert_ne!(d, longer);
        assert_ne!(d, empty);
        assert!(empty.slice.is_empty());
        assert_eq!(empty.header.length, 0);
        assert_eq!(empty, NumbersSlice::from_header_and_slice("abc".to_owned(), &[]));
        assert_eq!(d.to_owned().as_ref(), d);
        let longer_life: InternedSliceRef<'static, Numbers> = d.change_lifetime();
        assert_eq!(longer_life, d);
    }

    crate::impl_slice_internable!(Wide, u8, u128);

    /// A header smaller than the slice's alignment: the elements are still aligned.
    #[test]
    fn slices_are_aligned() {
        for len in 0..64u128 {
            let slice: Vec<u128> = (0..len).collect();
            let interned = InternedSlice::<Wide>::from_header_and_slice(len as u8, &slice);
            assert_eq!(interned.slice.as_ptr().addr() % align_of::<u128>(), 0);
            assert_eq!(interned.slice, slice[..]);
            assert_eq!(interned.header.header, len as u8);
        }
    }

    #[cfg(not(blaze_check))]
    crate::impl_slice_internable!(Racing, (), u64);

    /// Threads interning overlapping new slices in different orders get pointer-equal handles.
    #[cfg(not(blaze_check))]
    #[test]
    fn slices_race_to_one_handle() {
        const THREADS: u64 = 16;
        const VALUES: u64 = 5_000;
        let slice = |n: u64| -> Vec<u64> { (0..n % 17).map(|i| n * 31 + i).collect() };
        let seen: Vec<Vec<InternedSlice<Racing>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..THREADS)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..VALUES)
                            .map(|n| {
                                let n = (n + worker * 331) % VALUES;
                                InternedSlice::<Racing>::from_header_and_slice((), &slice(n))
                                    .to_owned()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        // Every `n` a multiple of 17 makes the one empty slice.
        let distinct: std::collections::HashSet<Vec<u64>> = (0..VALUES).map(slice).collect();
        assert_eq!(<Racing as SliceInternable>::storage().get().len(), distinct.len());
        for (worker, handles) in seen.iter().enumerate() {
            for (i, handle) in handles.iter().enumerate() {
                let n = (i as u64 + worker as u64 * 331) % VALUES;
                assert_eq!(handle.slice, slice(n)[..]);
                assert_eq!(
                    handle.as_ref(),
                    InternedSlice::<Racing>::from_header_and_slice((), &slice(n))
                );
            }
        }
    }

    #[cfg(not(blaze_check))]
    crate::impl_slice_internable!(Kept, String, u16);

    /// Enough slices to fill many chunks of every shard: the first still read their contents.
    #[cfg(not(blaze_check))]
    #[test]
    fn slice_handles_stay_valid() {
        let early: Vec<_> = (0..256u16)
            .map(|n| {
                InternedSlice::<Kept>::from_header_and_slice(format!("early {n}"), &[n, n + 1])
                    .to_owned()
            })
            .collect();
        for n in 0..100_000u32 {
            InternedSlice::<Kept>::from_header_and_slice(format!("later {n}"), &[n as u16; 5]);
        }
        for (n, handle) in (0..256u16).zip(&early) {
            assert_eq!(handle.header.header, format!("early {n}"));
            assert_eq!(handle.slice, [n, n + 1]);
            assert_eq!(
                handle.as_ref(),
                InternedSlice::<Kept>::from_header_and_slice(format!("early {n}"), &[n, n + 1])
            );
        }
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p intern --release
/// model_`): a slice type's first interns racing to make its store, against a set: the header
/// kept is the first intern's of each value, every other header is dropped once, equal slices
/// get one handle, and a handle reads its header, length and items whole.
#[cfg(all(test, blaze_check))]
#[allow(unsafe_code, reason = "each execution's storage is reached through a pointer the test sets and clears")]
mod model {
    use std::{
        ptr,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicPtr, Ordering::Relaxed},
        },
    };

    use blaze_check::{
        linear::{History, Spec},
        thread,
    };

    use super::*;

    /// A header that says when it is dropped. An interned header never is, so one dropped by
    /// the time `from_header_and_slice` returns was found there already.
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
            assert!(!self.dropped.swap(true, Relaxed), "a header dropped twice");
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

    struct Modeled;

    /// This execution's storage. A `static` one would keep the store the first execution made,
    /// whose atomics each later execution starts again from null while its values stay.
    static CURRENT: AtomicPtr<InternSliceStorage<Modeled>> = AtomicPtr::new(ptr::null_mut());

    impl SliceInternable for Modeled {
        type Header = Counted;
        type SliceType = u64;

        fn storage() -> &'static InternSliceStorage<Modeled> {
            // SAFETY: `model_first_slices_race` points `CURRENT` at a storage it frees only after
            // joining every thread that interns, and clears it first. The model's threads are
            // coroutines on its one OS thread, so this `Relaxed` load sees that store.
            unsafe { CURRENT.load(Relaxed).as_ref() }.expect("a model's storage")
        }
    }

    /// `value`'s items: one more than the value, so each value has its own layout.
    fn items(value: u64) -> Vec<u64> {
        (0..=value).map(|i| value * 100 + i).collect()
    }

    /// The sequential interner: bit `v` is set when value `v`'s slice is in it.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Set(u8);

    impl Spec for Set {
        /// The value whose header and items are interned.
        type Op = u64;
        /// Whether this call's header is the one kept: the slice was not there.
        type Ret = bool;

        fn apply(&mut self, &value: &u64) -> bool {
            let new = self.0 & (1 << value) == 0;
            self.0 |= 1 << value;
            new
        }
    }

    fn intern(history: &History<Set>, value: u64) -> InternedSlice<Modeled> {
        let mut handle = None;
        history.record(value, |&value| {
            let (header, dropped) = Counted::new(value);
            let items = items(value);
            let interned = InternedSlice::<Modeled>::from_header_and_slice(header, &items);
            assert!(
                interned.header.header.value == value
                    && interned.header.length == items.len()
                    && interned.slice == items[..],
                "a handle reads its header, length and items whole"
            );
            handle = Some(interned.to_owned());
            !dropped.load(Relaxed)
        });
        handle.expect("a recorded intern")
    }

    #[test]
    fn model_first_slices_race() {
        blaze_check::model(|| {
            let storage = Box::new(InternSliceStorage::<Modeled>::new());
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
            assert!(one == one_there && two == two_there, "equal slices, one handle");
            assert!(one != two, "two slices, two handles");
            assert_eq!(storage.get().len(), 2, "one store, made once");
            CURRENT.store(ptr::null_mut(), Relaxed);
            drop(storage);
        });
    }
}
