//! [`SmolStr`]: an immutable string that is cheap to clone, in place of the smol_str crate. A
//! text of up to [`INLINE`] bytes is stored in the value itself, a `&'static str` is borrowed,
//! and a longer text is one shared `Arc<str>`. The tree uses it for the names attribute helpers
//! return (`simple_name`, `as_one_segment`), which are short.

use std::{borrow::Borrow, fmt, hash, ops::Deref, sync::Arc};

/// The most bytes stored inline.
pub(crate) const INLINE: usize = 23;

#[derive(Clone)]
enum Repr {
    Inline { len: u8, bytes: [u8; INLINE] },
    Static(&'static str),
    Heap(Arc<str>),
}

/// An immutable string; see the module's text.
#[derive(Clone)]
pub struct SmolStr(Repr);

impl SmolStr {
    /// A copy of `text`: inline when it fits, else shared on the heap.
    pub fn new(text: impl AsRef<str>) -> SmolStr {
        let text = text.as_ref();
        if text.len() <= INLINE {
            let mut bytes = [0; INLINE];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            // `text.len() <= INLINE`, which is under `u8::MAX`.
            SmolStr(Repr::Inline { len: text.len() as u8, bytes })
        } else {
            SmolStr(Repr::Heap(Arc::from(text)))
        }
    }

    /// `text` itself, borrowed for good: no copy.
    pub const fn new_static(text: &'static str) -> SmolStr {
        SmolStr(Repr::Static(text))
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Inline { len, bytes } => {
                let bytes = &bytes[..usize::from(*len)];
                // SAFETY: `new` copied these bytes from a `str` whole, so they are UTF-8.
                unsafe { std::str::from_utf8_unchecked(bytes) }
            }
            Repr::Static(text) => text,
            Repr::Heap(text) => text,
        }
    }
}

impl Default for SmolStr {
    fn default() -> SmolStr {
        SmolStr::new_static("")
    }
}

impl Deref for SmolStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SmolStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for SmolStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for SmolStr {
    fn from(text: &str) -> SmolStr {
        SmolStr::new(text)
    }
}

impl From<String> for SmolStr {
    fn from(text: String) -> SmolStr {
        SmolStr::new(text)
    }
}

impl PartialEq for SmolStr {
    fn eq(&self, other: &SmolStr) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SmolStr {}

impl PartialEq<str> for SmolStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SmolStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<SmolStr> for str {
    fn eq(&self, other: &SmolStr) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<SmolStr> for &str {
    fn eq(&self, other: &SmolStr) -> bool {
        *self == other.as_str()
    }
}

impl PartialOrd for SmolStr {
    fn partial_cmp(&self, other: &SmolStr) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmolStr {
    fn cmp(&self, other: &SmolStr) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl hash::Hash for SmolStr {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl fmt::Debug for SmolStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for SmolStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}
