//! Owned text in an allocator the caller names (AGENTS.md rule 11, bump-allocate): [`String`],
//! and on unix [`OsString`] and [`PathBuf`], so a task's text lives in its
//! [`Frame`](crate::Frame) (`String<&Frame>`) or its request's
//! [`ArenaLocal`](crate::ArenaLocal) rather than on the global heap. std's `String`,
//! `OsString` and `PathBuf` take no allocator. [`format_in!`](crate::format_in) is `format!`
//! into an allocator; [`frame::string`](crate::frame::string), [`frame::path`](crate::frame::path)
//! and [`frame::os_string`](crate::frame::os_string) make one in a frame.
//!
//! [`String`] is `blaze_own`'s, over `blaze_own::Vec<u8, A>`, re-exported here: text has one
//! home. Its `into_bytes` and `into_boxed_str` give `blaze_own`'s `Vec` and `Box`; code still on
//! std's `Vec` and `Box` takes the bytes with [`into_std_bytes`] and [`into_std_boxed_str`],
//! which move the block without a copy. [`OsString`] and [`PathBuf`] stay here, over std's
//! `Vec<u8, A>`, because they need std's `OsStr`.
//!
//! None of them has a default allocator: every place that makes one says where its bytes go.
//!
//! Each compares, orders and hashes as its borrowed form (`str`, `OsStr`, `Path`) does, and
//! borrows as it, so it is a map key looked up by `&str`, `&OsStr` or `&Path`.
//!
//! [`OsString`] and [`PathBuf`] are unix only. There an `OsStr` is any bytes
//! (`std::os::unix::ffi::OsStrExt`), so both are safe code over the bytes. On Windows an
//! `OsStr` is WTF-8, whose only public constructor from bytes,
//! `OsStr::from_encoded_bytes_unchecked`, accepts bytes `as_encoded_bytes` gave or split at a
//! UTF-8 boundary, but not two such strings joined (a lead surrogate at the end of one and a
//! trail surrogate at the start of the other must merge into one code point, which only std's
//! internal `Wtf8Buf::push` does). So on Windows the types are not there, and naming one is a
//! compile error ("cannot find type `PathBuf` in crate `blaze_mem`"), never a silent fallback to
//! the global heap. WASI is left out too: its `OsStrExt` is unstable (`wasi_ext`).

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::{OsString, PathBuf};

use core::alloc::Allocator;

#[doc(hidden)]
pub use blaze_own::text::{format, format_into};
pub use blaze_own::text::{FromUtf8Error, String};

/// The text's bytes as std's `Vec<u8, A>`, the same block moved, for code still on std's
/// collections.
pub fn into_std_bytes<A: Allocator>(text: String<A>) -> Vec<u8, A> {
    let (ptr, len, capacity, alloc) = text.into_bytes().into_raw_parts_with_alloc();
    // SAFETY: the parts of a `blaze_own::Vec<u8, A>` taken apart just above: `ptr` is dangling
    // when `capacity` is 0, else a block `alloc` returned for `Layout::array::<u8>(capacity)`
    // (`blaze_own::RawBuf`'s invariant), which is what std's `Vec` frees; `len <= capacity`
    // bytes are initialized.
    unsafe { Vec::from_raw_parts_in(ptr.as_ptr(), len, capacity, alloc) }
}

/// The text as std's `Box<str, A>`, its room shrunk to its length, for code still on std's
/// `Box`.
pub fn into_std_boxed_str<A: Allocator>(text: String<A>) -> Box<str, A> {
    let (raw, alloc) = blaze_own::Box::into_raw_with_allocator(text.into_boxed_str());
    // SAFETY: a `blaze_own::Box<str, A>` taken apart just above: `raw` is a block `alloc`
    // returned for `Layout::for_value(&*raw)` (dangling when the text is empty), which is what
    // std's `Box` frees; the text is UTF-8.
    unsafe { Box::from_raw_in(raw, alloc) }
}

/// `format!` into the allocator given first: `format_in!(frame, "{name}-{n}")` is a
/// [`String<&Frame>`](crate::String). The same macro as `blaze_own::format_in!`, named from
/// blaze-mem so its callers need no second dependency.
#[macro_export]
macro_rules! format_in {
    ($alloc:expr, $($arg:tt)*) => {
        $crate::text::format($alloc, ::core::format_args!($($arg)*))
    };
}

/// `format!` appended to the caller's buffer, in place of a new string: `format_into!(&mut
/// buf, "{name}-{n}")` writes into `buf` (a [`String<A>`](crate::String) or any
/// `fmt::Write`), so a loop that formats reuses one buffer and allocates only when it grows.
/// The first choice of the `implicit_allocator` lint's help where a `format!` or `to_string`
/// is the finding: no new allocation at all. As `blaze_own::format_into!`.
#[macro_export]
macro_rules! format_into {
    ($out:expr, $($arg:tt)*) => {
        $crate::text::format_into($out, ::core::format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::{String, into_std_boxed_str, into_std_bytes};

    /// Invariant: `format_into!` appends to the caller's buffer and allocates nothing
    /// while what it writes fits the capacity the buffer has.
    #[test]
    fn format_into_appends_to_the_callers_buffer_without_growing_it() {
        let mut buf = String::with_capacity_in(64, Global);
        let capacity = buf.capacity();
        for n in 0..3 {
            format_into!(&mut buf, "{n}-");
        }
        format_into!(&mut buf, "end");
        assert_eq!(&*buf, "0-1-2-end");
        assert_eq!(buf.capacity(), capacity, "it wrote into the buffer it was given");
    }

    /// The moves to std's types keep the bytes, empty text included (a dangling block).
    #[test]
    fn text_moves_to_std_types_whole() {
        for text in ["", "blaze", "aé日🦀"] {
            let bytes = into_std_bytes(String::from_str_in(text, Global));
            assert_eq!(bytes.as_slice(), text.as_bytes());
            let boxed = into_std_boxed_str(String::from_str_in(text, Global));
            assert_eq!(&*boxed, text);
        }
    }
}
