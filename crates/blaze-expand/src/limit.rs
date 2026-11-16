//! The recursion limit of expansion. An expansion lists its nested calls and expands none, so
//! the depth is the caller's to count; these are the types it counts with.

use core::fmt;

/// `#![recursion_limit = "n"]`: how deep calls made by calls may go.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RecursionLimit {
    pub most: u32,
}

impl RecursionLimit {
    /// rustc's default.
    pub const DEFAULT: RecursionLimit = RecursionLimit { most: 128 };
}

/// How many expansions a call is inside: 0 for a call written in a source file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Depth(pub u32);

impl Depth {
    pub const SOURCE: Depth = Depth(0);

    /// The depth of a call the expansion of a call at this depth made, if `limit` admits it.
    pub fn deeper(self, limit: RecursionLimit) -> Result<Depth, LimitReached> {
        match self.0.checked_add(1) {
            Some(next) if next <= limit.most => Ok(Depth(next)),
            _ => Err(LimitReached { limit }),
        }
    }
}

/// A call past the recursion limit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LimitReached {
    pub limit: RecursionLimit,
}

impl fmt::Display for LimitReached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "recursion limit reached while expanding: the limit is {} (`#![recursion_limit]`)",
            self.limit.most
        )
    }
}

impl core::error::Error for LimitReached {}
