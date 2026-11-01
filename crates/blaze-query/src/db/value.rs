//! What generated code relies on besides the engine: the [`Value`] proof that a type may stay
//! in a row across views, the borrowed lookups of interned constructors ([`Lookup`],
//! [`HashEqLike`]), `ids_of`'s field dispatch, the return modes, and the stable
//! value hash.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    ops::Deref,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use super::{AsId, Id, interned::NamesIds};

/// A value that may stay in a row across views though it names `'db`: interned handles and
/// values of them, never a `&'db` into a memo.
///
/// # Safety
///
/// An instance made in an earlier view stays safe to keep and use (its safe methods, `==`
/// included) in a later one: it holds no reference into storage a later view may change or
/// free. `#[derive(blaze_query::Value)]` checks it field by field; `'static` types need nothing.
#[diagnostic::on_unimplemented(
    message = "`{Self}` doesn't implement `blaze_query::Value`",
    note = "add `#[derive(blaze_query::Value)]` to `{Self}`"
)]
pub unsafe trait Value {}

macro_rules! values {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: values of this type hold no database-lifetime reference.
            unsafe impl Value for $ty {}
        )*
    };
}

values! {
    (), bool, char, f32, f64, i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize,
    String, PathBuf, std::collections::hash_map::RandomState, std::convert::Infallible,
    std::num::NonZeroI8, std::num::NonZeroI16, std::num::NonZeroI32, std::num::NonZeroI64,
    std::num::NonZeroI128, std::num::NonZeroIsize, std::num::NonZeroU8, std::num::NonZeroU16,
    std::num::NonZeroU32, std::num::NonZeroU64, std::num::NonZeroU128, std::num::NonZeroUsize,
    blaze_hash::FixedState, Id,
}

// SAFETY: a `str` is bytes.
unsafe impl Value for str {}
// SAFETY: a `Path` is bytes.
unsafe impl Value for Path {}
// SAFETY: every element is a `Value`.
unsafe impl<T: Value> Value for [T] {}
// SAFETY: a `'static` reference stays valid across views.
unsafe impl<T: ?Sized> Value for &'static T {}
// SAFETY: `PhantomData` holds nothing.
unsafe impl<T: ?Sized> Value for PhantomData<T> {}
// SAFETY: `BuildHasherDefault` holds no `T`.
unsafe impl<T> Value for std::hash::BuildHasherDefault<T> {}

macro_rules! container_values {
    ($($container:ident)::+ < $($parameter:ident),+ >) => {
        // SAFETY: every kept parameter is a `Value`.
        unsafe impl<$($parameter: Value),+> Value for $($container)::+<$($parameter),+> {}
    };
}

container_values!(Vec<T>);
container_values!(Option<T>);
container_values!(Result<T, E>);
container_values!(either::Either<L, R>);
container_values!(thin_vec::ThinVec<T>);
container_values!(std::ops::Range<T>);
container_values!(std::ops::RangeInclusive<T>);
container_values!(HashMap<K, V, S>);
container_values!(HashSet<K, S>);
container_values!(BTreeMap<K, V>);
container_values!(BTreeSet<K>);
container_values!(hashbrown::HashMap<K, V, S>);
container_values!(hashbrown::HashSet<K, S>);
container_values!(indexmap::IndexMap<K, V, S>);
container_values!(indexmap::IndexSet<K, S>);

// SAFETY: the owned pointee is a `Value`.
unsafe impl<T: ?Sized + Value> Value for Box<T> {}
// SAFETY: the shared pointee is a `Value`, kept alive by this count.
unsafe impl<T: ?Sized + Value> Value for Arc<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Value> Value for Rc<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Value> Value for triomphe::Arc<T> {}
// SAFETY: every element is a `Value`.
unsafe impl<T: Value, const N: usize> Value for [T; N] {}
// SAFETY: every kept element is an `A::Item`, a `Value`; the inline buffer is `A`'s items.
unsafe impl<A> Value for smallvec::SmallVec<A>
where
    A: smallvec::Array,
    A::Item: Value,
{
}

macro_rules! tuple_values {
    ($($t:ident),+) => {
        // SAFETY: every element is a `Value`.
        unsafe impl<$($t: Value),+> Value for ($($t,)*) {}
    };
}

tuple_values!(A);
tuple_values!(A, B);
tuple_values!(A, B, C);
tuple_values!(A, B, C, D);
tuple_values!(A, B, C, D, E);
tuple_values!(A, B, C, D, E, F);
tuple_values!(A, B, C, D, E, F, G);
tuple_values!(A, B, C, D, E, F, G, H);
tuple_values!(A, B, C, D, E, F, G, H, I);
tuple_values!(A, B, C, D, E, F, G, H, I, J);
tuple_values!(A, B, C, D, E, F, G, H, I, J, K);
tuple_values!(A, B, C, D, E, F, G, H, I, J, K, L);

/// A type that hashes and compares like `O`: an interned constructor's borrowed argument.
pub trait HashEqLike<O> {
    fn eq(&self, data: &O) -> bool;
}

/// What an interned constructor's argument becomes when its value is new (a
/// `Borrow`/`ToOwned` that works field by field).
pub trait Lookup<O> {
    fn into_owned(self) -> O;
}

impl<T> Lookup<T> for T {
    fn into_owned(self) -> T {
        self
    }
}

impl<T: Hash + Eq> HashEqLike<T> for T {
    fn eq(&self, data: &T) -> bool {
        self == data
    }
}

impl<T: Hash + Eq> HashEqLike<T> for &T {
    fn eq(&self, data: &T) -> bool {
        **self == *data
    }
}

impl<T: Hash + Eq> HashEqLike<&T> for T {
    fn eq(&self, data: &&T) -> bool {
        *self == **data
    }
}

impl<T: Clone> Lookup<T> for &T {
    fn into_owned(self) -> T {
        // clone: an intern miss keeps an owned copy of the key, once per distinct value
        Clone::clone(self)
    }
}

impl<'a, T: ?Sized + Hash + Eq> HashEqLike<&'a T> for Box<T>
where
    Box<T>: From<&'a T>,
{
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T: ?Sized + Hash + Eq> Lookup<Box<T>> for &'a T
where
    Box<T>: From<&'a T>,
{
    fn into_owned(self) -> Box<T> {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        Box::from(self)
    }
}

impl<'a, T: ?Sized + Hash + Eq> HashEqLike<&'a T> for Arc<T>
where
    Arc<T>: From<&'a T>,
{
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T: ?Sized + Hash + Eq> Lookup<Arc<T>> for &'a T
where
    Arc<T>: From<&'a T>,
{
    fn into_owned(self) -> Arc<T> {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        Arc::from(self)
    }
}

impl<'a, T: ?Sized + Hash + Eq> HashEqLike<&'a T> for triomphe::Arc<T>
where
    triomphe::Arc<T>: From<&'a T>,
{
    fn eq(&self, data: &&T) -> bool {
        **self == **data
    }
}

impl<'a, T: ?Sized + Hash + Eq> Lookup<triomphe::Arc<T>> for &'a T
where
    triomphe::Arc<T>: From<&'a T>,
{
    fn into_owned(self) -> triomphe::Arc<T> {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        triomphe::Arc::from(self)
    }
}

impl Lookup<String> for &str {
    fn into_owned(self) -> String {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        self.to_owned()
    }
}

impl HashEqLike<&str> for String {
    fn eq(&self, data: &&str) -> bool {
        self == *data
    }
}

impl<A, T: Hash + Eq + PartialEq<A>> HashEqLike<&[A]> for Vec<T> {
    fn eq(&self, data: &&[A]) -> bool {
        self.len() == data.len() && self.iter().zip(data.iter()).all(|(it, a)| it == a)
    }
}

impl<A: Hash + Eq + PartialEq<T> + Clone + Lookup<T>, T> Lookup<Vec<T>> for &[A] {
    fn into_owned(self) -> Vec<T> {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        self.iter().map(|a| Lookup::into_owned(a.clone())).collect()
    }
}

impl<const N: usize, A, T: Hash + Eq + PartialEq<A>> HashEqLike<[A; N]> for Vec<T> {
    fn eq(&self, data: &[A; N]) -> bool {
        self.len() == data.len() && self.iter().zip(data.iter()).all(|(it, a)| it == a)
    }
}

impl<const N: usize, A: Hash + Eq + PartialEq<T> + Clone + Lookup<T>, T> Lookup<Vec<T>> for [A; N] {
    fn into_owned(self) -> Vec<T> {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        self.into_iter().map(Lookup::into_owned).collect()
    }
}

impl HashEqLike<&Path> for PathBuf {
    fn eq(&self, data: &&Path) -> bool {
        self == data
    }
}

impl Lookup<PathBuf> for &Path {
    fn into_owned(self) -> PathBuf {
        // alloc: an intern miss keeps an owned copy of the key, once per distinct value
        self.to_owned()
    }
}

impl<T: Hash + Eq + Clone> HashEqLike<Cow<'_, T>> for T {
    fn eq(&self, data: &Cow<'_, T>) -> bool {
        self == data.as_ref()
    }
}

impl<T: Clone> Lookup<T> for Cow<'_, T> {
    fn into_owned(self) -> T {
        Cow::into_owned(self)
    }
}

impl HashEqLike<Cow<'_, str>> for String {
    fn eq(&self, data: &Cow<'_, str>) -> bool {
        self.as_str() == data.as_ref()
    }
}

impl Lookup<String> for Cow<'_, str> {
    fn into_owned(self) -> String {
        Cow::into_owned(self)
    }
}

impl HashEqLike<Cow<'_, Path>> for PathBuf {
    fn eq(&self, data: &Cow<'_, Path>) -> bool {
        self.as_path() == data.as_ref()
    }
}

impl Lookup<PathBuf> for Cow<'_, Path> {
    fn into_owned(self) -> PathBuf {
        Cow::into_owned(self)
    }
}

impl<T: Hash + Eq + Clone> HashEqLike<Cow<'_, [T]>> for Box<[T]> {
    fn eq(&self, data: &Cow<'_, [T]>) -> bool {
        self.as_ref() == data.as_ref()
    }
}

impl<T: Clone> Lookup<Box<[T]>> for Cow<'_, [T]> {
    fn into_owned(self) -> Box<[T]> {
        Cow::into_owned(self).into_boxed_slice()
    }
}

impl<T: Hash + Eq + Clone> HashEqLike<Cow<'_, [T]>> for Vec<T> {
    fn eq(&self, data: &Cow<'_, [T]>) -> bool {
        self.as_slice() == data.as_ref()
    }
}

impl<T: Clone> Lookup<Vec<T>> for Cow<'_, [T]> {
    fn into_owned(self) -> Vec<T> {
        Cow::into_owned(self)
    }
}

/// One field of an interned row for its `ids_of`: generated code calls
/// `(&&&IdsOf(&field)).push_ids(out)` with [`IdOfAsId`], [`IdOfNamed`] and [`IdOfOther`] in
/// scope, so method lookup picks the first that applies: the field's ID, what it names, or
/// nothing.
pub struct IdsOf<'a, T: ?Sized>(pub &'a T);

pub trait IdOfAsId {
    fn push_ids(&self, out: &mut Vec<Id>);
}

impl<T: AsId> IdOfAsId for &&IdsOf<'_, T> {
    #[inline]
    fn push_ids(&self, out: &mut Vec<Id>) {
        // alloc: the caller's list of a key's IDs, grown once
        out.push(self.0.as_id());
    }
}

pub trait IdOfNamed {
    fn push_ids(&self, out: &mut Vec<Id>);
}

impl<T: NamesIds + ?Sized> IdOfNamed for &IdsOf<'_, T> {
    #[inline]
    fn push_ids(&self, out: &mut Vec<Id>) {
        self.0.names_ids(out);
    }
}

pub trait IdOfOther {
    fn push_ids(&self, out: &mut Vec<Id>);
}

impl<T: ?Sized> IdOfOther for IdsOf<'_, T> {
    #[inline]
    fn push_ids(&self, _: &mut Vec<Id>) {}
}

/// `returns(as_ref)`: `&Option<T>` as `Option<&T>`, `&Result<T, E>` as `Result<&T, &E>`.
pub trait ReturnsAsRef {
    type AsRef<'a>
    where
        Self: 'a;
    fn as_ref(&self) -> Self::AsRef<'_>;
}

impl<T> ReturnsAsRef for Option<T> {
    type AsRef<'a>
        = Option<&'a T>
    where
        T: 'a;

    fn as_ref(&self) -> Option<&T> {
        Option::as_ref(self)
    }
}

impl<T, E> ReturnsAsRef for Result<T, E> {
    type AsRef<'a>
        = Result<&'a T, &'a E>
    where
        T: 'a,
        E: 'a;

    fn as_ref(&self) -> Result<&T, &E> {
        Result::as_ref(self)
    }
}

/// `returns(as_deref)`: `&Option<T>` as `Option<&T::Target>`, `&Result<T, E>` as
/// `Result<&T::Target, &E>`.
pub trait ReturnsAsDeref {
    type AsDeref<'a>
    where
        Self: 'a;
    fn as_deref(&self) -> Self::AsDeref<'_>;
}

impl<T: Deref> ReturnsAsDeref for Option<T> {
    type AsDeref<'a>
        = Option<&'a T::Target>
    where
        T: 'a;

    fn as_deref(&self) -> Option<&T::Target> {
        Option::as_deref(self)
    }
}

impl<T: Deref, E> ReturnsAsDeref for Result<T, E> {
    type AsDeref<'a>
        = Result<&'a T::Target, &'a E>
    where
        T: 'a,
        E: 'a;

    fn as_deref(&self) -> Result<&T::Target, &E> {
        Result::as_deref(self)
    }
}

/// The stable 64-bit hash of a value: what an evictable function's `hash` calls, compared
/// after eviction in place of `==`.
pub fn value_hash<V: Hash + ?Sized>(value: &V) -> u64 {
    let id = blaze_ids::Hasher::new("blaze-query.db.value").std(value).finish();
    u64::from_le_bytes(id.0[..8].try_into().expect("a content ID has 32 bytes"))
}
