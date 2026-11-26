//! The bugs a control can plant in the screen and terminal (D165, rule 78: a strategy is a type
//! parameter at one seam). [`Terminal`](crate::terminal::Terminal) takes one as its type
//! parameter `P: Planted<VtPlant>`; shipped code uses `NoPlant`, whose every `has` is `false`,
//! so each check compiles away. The model check's controls (`sim_tests.rs`) pass a value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VtPlant {
    /// The code as it is.
    None,
    /// Scrolling up (SU) clears the pending wrap, which ghostty keeps (Terminal.zig:2207).
    PendingWrapClearedOnScrollUp,
    /// Reflow drops the last cell of a soft-wrapped row.
    ReflowDropsWrappedTail,
    /// Overwriting a cell with the style it already has releases the style without taking it
    /// again, so the id is freed while cells still name it.
    StyleFreedWhileNamed,
    /// A row moved across a page boundary is taken for a row of the page before it.
    PageBoundaryOffByOne,
    /// Snapshot omits SGR style transitions when encoding rows.
    SnapshotDropsStyle,
    /// Snapshot omits the final cursor position (CUP).
    SnapshotDropsCursor,
    /// Snapshot omits primary soft-wrap flags.
    SnapshotDropsSoftWrap,
}

impl Plants for VtPlant {}
