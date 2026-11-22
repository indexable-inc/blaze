//! The checkout argument as a person writes it, made once into what the inside takes: a bstd
//! [`Path`] beneath the start directory, or the start directory itself. `./x`, `x/`, `x//y` and
//! `x/./y` name `x` and `x/y`; `.`, `./` and no argument name the start directory; a root or a
//! `..` is refused with the path as it was written.

use core::alloc::Allocator;

use blaze_own::Vec;
use bstd::path::{Path, PathError, PathWhy};

/// What the checkout argument names.
#[derive(Debug, PartialEq, Eq)]
pub enum Checkout {
    /// The directory blaze was started in.
    Here,
    /// A directory beneath it.
    Under(Path),
}

/// `raw` (the argument, if one was given) as a [`Checkout`]. Spellings of one directory give
/// one value; a refusal names `raw` itself, not the normalised form.
/// `alloc` holds the canonical spelling while it is checked.
pub fn checkout<A: Allocator>(raw: Option<&str>, alloc: A) -> Result<Checkout, PathError> {
    let Some(raw) = raw else { return Ok(Checkout::Here) };
    let refused = |why| PathError { path: raw.into(), why };
    if raw.starts_with('/') {
        return Err(refused(PathWhy::Rooted));
    }
    let mut canonical: Vec<u8, A> = Vec::with_capacity_in(raw.len(), alloc);
    for part in raw.split('/').filter(|part| !part.is_empty() && *part != ".") {
        if !canonical.is_empty() {
            canonical.push(b'/');
        }
        canonical.extend_from_slice(part.as_bytes());
    }
    if canonical.is_empty() {
        return Ok(Checkout::Here);
    }
    let text = core::str::from_utf8(&canonical).expect("whole parts of a str joined by `/` are UTF-8");
    Path::new(text).map(Checkout::Under).map_err(|error| refused(error.why))
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::alloc::Global;

    use super::*;

    fn under(raw: &str) -> Checkout {
        Checkout::Under(Path::new(raw).expect("a test's canonical path"))
    }

    fn of(raw: Option<&str>) -> Result<Checkout, PathError> {
        checkout(raw, Global)
    }

    #[test]
    fn no_argument_is_here() {
        assert_eq!(of(None), Ok(Checkout::Here));
    }

    #[test]
    fn dot_is_here() {
        assert_eq!(of(Some(".")), Ok(Checkout::Here));
        assert_eq!(of(Some("./")), Ok(Checkout::Here));
        assert_eq!(of(Some("./.")), Ok(Checkout::Here));
    }

    #[test]
    fn spellings_of_one_directory_are_one_path() {
        for raw in ["hello", "./hello", "hello/", "./hello/", "hello/."] {
            assert_eq!(of(Some(raw)), Ok(under("hello")), "{raw}");
        }
        for raw in ["a/b", "./a/b", "a//b", "a/./b/", "a/b/"] {
            assert_eq!(of(Some(raw)), Ok(under("a/b")), "{raw}");
        }
    }

    #[test]
    fn parent_is_refused_by_its_written_name() {
        for raw in ["..", "../x", "./a/../b", "a/.."] {
            let error = of(Some(raw)).expect_err(raw);
            assert_eq!((error.path.as_str(), error.why), (raw, PathWhy::Parent));
        }
    }

    #[test]
    fn root_is_refused_by_its_written_name() {
        let error = of(Some("/etc")).expect_err("/etc");
        assert_eq!((error.path.as_str(), error.why), ("/etc", PathWhy::Rooted));
    }
}
