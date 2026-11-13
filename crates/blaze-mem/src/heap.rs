//! The global heap, fallibly (docs/v2/plan/design/fallible-bstd.md, decision D29): the few
//! values an interface makes blaze keep there (an effect's own bytes, a shared handle, a
//! name another crate's type owns as a std `String`) are made through these, which return
//! [`OutOfMemory`] naming "the global heap" where `Vec::from`, `to_owned`, `Box::new` and
//! `Arc::new` would end the process. Each use still says why it is on the global heap
//! (`// alloc:`, rule bump-allocate).

use std::{alloc::Global, fmt, mem, rc::Rc, sync::Arc};

use crate::{FormatError, Named, OutOfMemory, String, Vec};

/// What a refused allocation of a `T` on the global heap says.
#[cold]
fn refused<T>() -> OutOfMemory {
    OutOfMemory { arena: Global.name(), asked: mem::size_of::<T>() }
}

/// A copy of `from` as std's `Vec`.
pub fn bytes(from: &[u8]) -> Result<std::vec::Vec<u8>, OutOfMemory> {
    Ok(Vec::from_slice_in(from, Global)?.into_std())
}

/// A copy of `from` as std's `String`.
pub fn string(from: &str) -> Result<std::string::String, OutOfMemory> {
    Ok(String::from_str_in(from, Global)?.into_std())
}

/// `args` formatted into std's `String`: `heap::format(format_args!(..))`.
pub fn format(args: fmt::Arguments<'_>) -> Result<std::string::String, FormatError<OutOfMemory>> {
    Ok(crate::text::format(Global, args)?.into_std())
}

/// `value` in a `Box`.
pub fn boxed<T>(value: T) -> Result<Box<T>, OutOfMemory> {
    Box::try_new(value).map_err(|_| refused::<T>())
}

/// `value` behind an `Arc`.
pub fn shared<T>(value: T) -> Result<Arc<T>, OutOfMemory> {
    Arc::try_new(value).map_err(|_| refused::<T>())
}

/// `value` behind an `Rc`.
pub fn shared_local<T>(value: T) -> Result<Rc<T>, OutOfMemory> {
    Rc::try_new(value).map_err(|_| refused::<T>())
}
