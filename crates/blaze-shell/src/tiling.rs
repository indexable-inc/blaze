//! Thin re-export of the pure `#![no_std]` sans-IO BSP tiling & workspace manager in
//! [`blaze_tiling`], keeping `blaze-shell` free of duplicated layout-tree logic.

pub use blaze_tiling::{
    CloseOutcome, FocusDir, PaneId, PaneLayout, SplitAxis, SplitHandle, SplitId, TabId,
    TileLayout, TileManager, TileNode, TileRect, WorkspaceTab,
};
