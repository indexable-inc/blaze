//! The byte sink every writer here fills: blaze-text's [`Put`] into a vector in a named
//! allocator, a refusal kept (and every later write dropped) rather than a panic, and reported
//! once by [`Bytes::finish`].

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::Put;

pub(crate) struct Bytes<'a> {
    buf: Vec<u8, Alloc<'a>>,
    failed: Option<AllocFailure>,
}

impl<'a> Bytes<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Bytes<'a> {
        Bytes { buf: Vec::new_in(alloc), failed: None }
    }

    /// `bytes` at the end, unless an earlier write was refused.
    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        if self.failed.is_none()
            && let Err(e) = self.buf.try_extend_from_slice(bytes)
        {
            self.failed = Some(e);
        }
    }

    /// The bytes, or the first refusal.
    pub(crate) fn finish(self) -> Result<Vec<u8, Alloc<'a>>, AllocFailure> {
        match self.failed {
            Some(e) => Err(e),
            None => Ok(self.buf),
        }
    }
}

impl Put for Bytes<'_> {
    #[inline]
    fn put_str(&mut self, text: &str) {
        self.raw(text.as_bytes());
    }
}
