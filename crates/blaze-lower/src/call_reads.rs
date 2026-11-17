//! Which reads of a body's read list each body macro call made ([`crate::by_name::ByName`]):
//! the body host learns no call's place when a proc macro runs (blaze-expand's `ExpandCx`
//! is not told which call asks), so the expander, which holds the call, keeps the span its
//! expansion's reads take in the body's list. blaze-ladder front2 `body_left` turns each
//! span into decision D111's findings through blaze-scope's one filing
//! (`blaze_scope::left_reads::file_left_reads`), the one the item host files through.

use blaze_own::AString;
use blaze_surface::{CallShape, Range};

/// One body call whose expansion joined reads to the body's list.
#[derive(Debug)]
pub struct CallReads<'a> {
    /// The whole call's range in the body's text (past the item's length: in an expansion
    /// appended to it); `None` for a body's attribute or derive, which the lowering hands the
    /// expander with no range (placed at the body's item).
    pub at: Option<Range>,
    /// The macro's path as written at the call (the topmost call's: an eager input's nested
    /// call's reads join its caller's span).
    pub path: AString<'a>,
    pub shape: CallShape,
    /// The span of the body's read list ([`crate::by_name::ByName::reads`]) its reads take.
    pub reads: core::ops::Range<usize>,
}
