//! Pure `#![no_std]`, zero-unsafe, sans-IO binary-space-partitioning (BSP) tiling window manager
//! built on `blaze-own` explicit allocators (`Alloc<'a>`, `AVec`, `ABox`).
//!
//! Manages workspace tabs (`Cmd+T`, `Cmd+1..9`), arbitrary nested vertical/horizontal splits
//! (`Cmd+D`, `Cmd+Shift+D`), directional spatial focus (`Cmd+Opt+Arrow`/`HJKL`), pane zoom
//! (`Cmd+Shift+Enter`), split ratio resizing (mouse divider drag & keyboard nudge), and
//! safe layout computation that never panics at any viewport dimension.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::alloc::Allocator;

use blaze_own::{ABox, AVec, Alloc, Box, Vec};

/// Axis-aligned rectangle in logical screen coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TileRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl TileRect {
    #[inline]
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    #[inline]
    pub fn width(self) -> f32 {
        (self.x1 - self.x0).max(0.0)
    }

    #[inline]
    pub fn height(self) -> f32 {
        (self.y1 - self.y0).max(0.0)
    }

    #[inline]
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// Unique identifier for a tiled terminal/shell pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u32);

/// Unique identifier for a workspace tab inside a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u32);

/// Unique identifier for an internal BSP split node (used for divider hit-testing & drag resize).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SplitId(pub u32);

/// Orientation of a binary split container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    /// Side-by-side split: `first` on left, `second` on right (`Cmd+D`).
    Vertical,
    /// Stacked split: `first` on top, `second` on bottom (`Cmd+Shift+D`).
    Horizontal,
}

/// Cardinal direction for spatial focus navigation and split resizing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// Binary space partitioning tree node.
#[derive(Clone, Debug, PartialEq)]
pub enum TileNode<'a> {
    Leaf(PaneId),
    Split {
        id: SplitId,
        axis: SplitAxis,
        /// Fraction `[0.15, 0.85]` allocated to `first`.
        ratio: f32,
        first: ABox<'a, TileNode<'a>>,
        second: ABox<'a, TileNode<'a>>,
    },
}

/// One workspace tab containing a BSP tree of panes.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceTab<'a> {
    pub id: TabId,
    pub root: TileNode<'a>,
    pub focused: PaneId,
    pub zoomed: bool,
}

/// Computed screen rectangle for one visible pane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneLayout {
    pub pane: PaneId,
    pub rect: TileRect,
    pub focused: bool,
}

/// Interactive divider handle between two sibling subtrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitHandle {
    pub split: SplitId,
    pub axis: SplitAxis,
    /// Visual 1px/gap divider rect.
    pub rect: TileRect,
    /// Expanded hit target rect for effortless pointer grabbing.
    pub hit_rect: TileRect,
    /// Bounding rect of the parent split container (maps drag coordinates -> ratio).
    pub parent_rect: TileRect,
}

/// Complete layout result for the active workspace tab.
#[derive(Clone, Debug, PartialEq)]
pub struct TileLayout<'a> {
    pub panes: AVec<'a, PaneLayout>,
    pub handles: AVec<'a, SplitHandle>,
    pub pane_count_in_tab: usize,
    pub is_zoomed: bool,
}

impl<'a> TileLayout<'a> {
    /// Create an empty layout backed by `alloc`.
    pub fn empty(alloc: Alloc<'a>) -> Self {
        Self {
            panes: Vec::new_in(alloc),
            handles: Vec::new_in(alloc),
            pane_count_in_tab: 0,
            is_zoomed: false,
        }
    }
}

/// Outcome of closing a pane or workspace tab (`Cmd+W`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseOutcome<'a> {
    PaneClosed {
        closed: PaneId,
        next_focus: PaneId,
    },
    TabClosed {
        closed_tab: TabId,
        closed_panes: AVec<'a, PaneId>,
        next_focus: PaneId,
    },
    LastPaneInLastTab,
}

const MIN_RATIO: f32 = 0.15;
const MAX_RATIO: f32 = 0.85;

/// Complete in-window workspace & BSP tiling window manager.
#[derive(Clone, Debug)]
pub struct TileManager<'a> {
    tabs: AVec<'a, WorkspaceTab<'a>>,
    active_tab: usize,
    next_pane: u32,
    next_tab: u32,
    next_split: u32,
    alloc: Alloc<'a>,
}

impl<'a> TileManager<'a> {
    pub fn new(alloc: Alloc<'a>) -> (Self, TabId, PaneId) {
        let tab_id = TabId(1);
        let pane_id = PaneId(1);
        let tab = WorkspaceTab {
            id: tab_id,
            root: TileNode::Leaf(pane_id),
            focused: pane_id,
            zoomed: false,
        };
        let mut tabs = Vec::with_capacity_in(1, alloc);
        tabs.push(tab);
        let mgr = Self {
            tabs,
            active_tab: 0,
            next_pane: 2,
            next_tab: 2,
            next_split: 1,
            alloc,
        };
        (mgr, tab_id, pane_id)
    }

    pub fn allocator(&self) -> Alloc<'a> {
        self.alloc
    }

    pub fn tabs(&self) -> &[WorkspaceTab<'a>] {
        &self.tabs
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_tab(&self) -> &WorkspaceTab<'a> {
        &self.tabs[self.active_tab]
    }

    pub fn focused_pane(&self) -> PaneId {
        self.tabs[self.active_tab].focused
    }

    /// All panes belonging to the currently active workspace tab (in tree order).
    pub fn active_tab_panes(&self) -> AVec<'a, PaneId> {
        let mut out = Vec::new_in(self.alloc);
        self.tabs[self.active_tab].root.collect_leaves(&mut out);
        out
    }

    /// Create a new workspace tab (`Cmd+T`) and switch to it.
    pub fn new_tab(&mut self) -> (TabId, PaneId) {
        let tab_id = TabId(self.next_tab);
        self.next_tab += 1;
        let pane_id = PaneId(self.next_pane);
        self.next_pane += 1;
        self.tabs.push(WorkspaceTab {
            id: tab_id,
            root: TileNode::Leaf(pane_id),
            focused: pane_id,
            zoomed: false,
        });
        self.active_tab = self.tabs.len() - 1;
        (tab_id, pane_id)
    }

    /// Switch to 1-based workspace tab index (`Cmd+1..8`, and `Cmd+9` jumps to last tab).
    pub fn select_tab_slot(&mut self, slot_1_based: usize) -> Option<PaneId> {
        if self.tabs.is_empty() || slot_1_based == 0 {
            return None;
        }
        let idx = if slot_1_based == 9 {
            self.tabs.len() - 1
        } else {
            let i = slot_1_based - 1;
            if i >= self.tabs.len() {
                return None;
            }
            i
        };
        self.active_tab = idx;
        Some(self.tabs[self.active_tab].focused)
    }

    pub fn select_tab_by_id(&mut self, id: TabId) -> Option<PaneId> {
        let idx = self.tabs.iter().position(|t| t.id == id)?;
        self.active_tab = idx;
        Some(self.tabs[self.active_tab].focused)
    }

    pub fn cycle_tab(&mut self, delta: isize) -> Option<PaneId> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let n = self.tabs.len() as isize;
        self.active_tab = (self.active_tab as isize + delta).rem_euclid(n) as usize;
        Some(self.tabs[self.active_tab].focused)
    }

    /// Split the currently focused pane along `axis` (`Cmd+D` vertical, `Cmd+Shift+D` horizontal).
    pub fn split_focused(&mut self, axis: SplitAxis) -> PaneId {
        let new_pane = PaneId(self.next_pane);
        self.next_pane += 1;
        let split_id = SplitId(self.next_split);
        self.next_split += 1;
        let alloc = self.alloc;
        let tab = &mut self.tabs[self.active_tab];
        tab.zoomed = false;
        let target = tab.focused;
        if tab.root.split_leaf(target, new_pane, split_id, axis, alloc) {
            tab.focused = new_pane;
        }
        new_pane
    }

    /// Close the currently focused pane (`Cmd+W`).
    pub fn close_focused(&mut self) -> CloseOutcome<'a> {
        let target = self.focused_pane();
        self.close_pane(target)
    }

    /// Close a specific pane in the active tab (collapsing its parent split or closing the tab).
    pub fn close_pane(&mut self, target: PaneId) -> CloseOutcome<'a> {
        let tab_idx = self.active_tab;
        let alloc = self.alloc;
        let leaves = {
            let mut v = Vec::new_in(alloc);
            self.tabs[tab_idx].root.collect_leaves(&mut v);
            v
        };
        if leaves.scan_for(&target).is_none() {
            return CloseOutcome::LastPaneInLastTab;
        }
        if leaves.len() <= 1 {
            if self.tabs.len() <= 1 {
                return CloseOutcome::LastPaneInLastTab;
            }
            let removed = self.tabs.remove_shift(tab_idx);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            }
            let next_focus = self.tabs[self.active_tab].focused;
            return CloseOutcome::TabClosed {
                closed_tab: removed.id,
                closed_panes: leaves,
                next_focus,
            };
        }

        let pos = leaves.iter().position(|&p| p == target).unwrap_or(0);
        let fallback_focus = if pos + 1 < leaves.len() {
            leaves[pos + 1]
        } else {
            leaves[pos.saturating_sub(1)]
        };

        let tab = &mut self.tabs[tab_idx];
        let old_root = core::mem::replace(&mut tab.root, TileNode::Leaf(fallback_focus));
        if let Some(new_root) = old_root.remove_leaf(target, alloc) {
            tab.root = new_root;
        }
        if tab.focused == target {
            tab.focused = fallback_focus;
        }
        if tab.root.leaf_count() <= 1 {
            tab.zoomed = false;
        }
        CloseOutcome::PaneClosed {
            closed: target,
            next_focus: tab.focused,
        }
    }

    /// Close an entire workspace tab by id (e.g. clicking a tab's close button).
    pub fn close_tab(&mut self, id: TabId) -> CloseOutcome<'a> {
        if self.tabs.len() <= 1 {
            return CloseOutcome::LastPaneInLastTab;
        }
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return CloseOutcome::LastPaneInLastTab;
        };
        let removed = self.tabs.remove_shift(idx);
        let mut closed_panes = Vec::new_in(self.alloc);
        removed.root.collect_leaves(&mut closed_panes);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if idx < self.active_tab {
            self.active_tab -= 1;
        }
        CloseOutcome::TabClosed {
            closed_tab: removed.id,
            closed_panes,
            next_focus: self.tabs[self.active_tab].focused,
        }
    }

    pub fn focus_pane(&mut self, pane: PaneId) -> bool {
        let tab = &mut self.tabs[self.active_tab];
        if tab.root.contains(pane) {
            let changed = tab.focused != pane;
            tab.focused = pane;
            changed
        } else {
            false
        }
    }

    /// Focus `pane` even if it lives in a different workspace tab, switching tabs and unzooming as needed.
    pub fn focus_pane_anywhere(&mut self, pane: PaneId) -> bool {
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            if tab.root.contains(pane) {
                self.active_tab = idx;
                if tab.focused != pane {
                    tab.zoomed = false;
                }
                tab.focused = pane;
                return true;
            }
        }
        false
    }

    /// Cycle focus to next/previous pane in tree order (`Cmd+]` / `Cmd+[`).
    pub fn cycle_pane(&mut self, delta: isize) -> Option<PaneId> {
        let alloc = self.alloc;
        let tab = &mut self.tabs[self.active_tab];
        let mut leaves = Vec::new_in(alloc);
        tab.root.collect_leaves(&mut leaves);
        if leaves.len() <= 1 {
            return None;
        }
        tab.zoomed = false;
        let cur = leaves.iter().position(|&p| p == tab.focused).unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(leaves.len() as isize) as usize;
        tab.focused = leaves[next];
        Some(tab.focused)
    }

    /// Move focus spatially to the nearest neighbor pane in `dir` (`Cmd+Opt+Arrow` / `HJKL`).
    pub fn focus_direction(&mut self, dir: FocusDir, area: TileRect, gap: f32) -> Option<PaneId> {
        let tab_idx = self.active_tab;
        if self.tabs[tab_idx].root.leaf_count() <= 1 {
            return None;
        }
        self.tabs[tab_idx].zoomed = false;
        let layout = self.layout(area, gap);
        let cur = layout.panes.iter().find(|p| p.focused)?;
        let cx = (cur.rect.x0 + cur.rect.x1) * 0.5;
        let cy = (cur.rect.y0 + cur.rect.y1) * 0.5;

        let mut best: Option<(PaneId, f32)> = None;
        for cand in &layout.panes {
            if cand.pane == cur.pane {
                continue;
            }
            let nx = (cand.rect.x0 + cand.rect.x1) * 0.5;
            let ny = (cand.rect.y0 + cand.rect.y1) * 0.5;
            let (primary, overlap, perp) = match dir {
                FocusDir::Left => {
                    if cand.rect.x1 > cur.rect.x0 + gap + 2.0 {
                        continue;
                    }
                    let ov =
                        (cur.rect.y1.min(cand.rect.y1) - cur.rect.y0.max(cand.rect.y0)).max(0.0);
                    (cur.rect.x0 - cand.rect.x1, ov, (ny - cy).abs())
                }
                FocusDir::Right => {
                    if cand.rect.x0 < cur.rect.x1 - gap - 2.0 {
                        continue;
                    }
                    let ov =
                        (cur.rect.y1.min(cand.rect.y1) - cur.rect.y0.max(cand.rect.y0)).max(0.0);
                    (cand.rect.x0 - cur.rect.x1, ov, (ny - cy).abs())
                }
                FocusDir::Up => {
                    if cand.rect.y1 > cur.rect.y0 + gap + 2.0 {
                        continue;
                    }
                    let ov =
                        (cur.rect.x1.min(cand.rect.x1) - cur.rect.x0.max(cand.rect.x0)).max(0.0);
                    (cur.rect.y0 - cand.rect.y1, ov, (nx - cx).abs())
                }
                FocusDir::Down => {
                    if cand.rect.y0 < cur.rect.y1 - gap - 2.0 {
                        continue;
                    }
                    let ov =
                        (cur.rect.x1.min(cand.rect.x1) - cur.rect.x0.max(cand.rect.x0)).max(0.0);
                    (cand.rect.y0 - cur.rect.y1, ov, (nx - cx).abs())
                }
            };
            // Strongly prefer panes sharing an edge (positive perpendicular overlap), then closest distance.
            let penalty = if overlap > 0.0 { 0.0 } else { 10_000.0 };
            let score = penalty + primary.max(0.0) * 8.0 + perp;
            if best.is_none_or(|(_, bs)| score < bs) {
                best = Some((cand.pane, score));
            }
        }
        let (target, _) = best?;
        self.tabs[tab_idx].focused = target;
        Some(target)
    }

    /// Toggle zooming the focused pane to fill the workspace (`Cmd+Shift+Enter`).
    pub fn toggle_zoom(&mut self) -> bool {
        let tab = &mut self.tabs[self.active_tab];
        if tab.root.leaf_count() <= 1 {
            tab.zoomed = false;
            return false;
        }
        tab.zoomed = !tab.zoomed;
        true
    }

    /// Set exact ratio of a split divider during live mouse drag.
    pub fn set_split_ratio(&mut self, split: SplitId, ratio: f32) -> bool {
        let clamped = ratio.clamp(MIN_RATIO, MAX_RATIO);
        self.tabs[self.active_tab].root.set_ratio(split, clamped)
    }

    /// Nudge the nearest enclosing split of the focused pane along `dir` (`Cmd+Ctrl+Arrow`).
    pub fn nudge_focused_split(&mut self, dir: FocusDir, delta: f32) -> bool {
        let axis = match dir {
            FocusDir::Left | FocusDir::Right => SplitAxis::Vertical,
            FocusDir::Up | FocusDir::Down => SplitAxis::Horizontal,
        };
        let signed = match dir {
            FocusDir::Left | FocusDir::Up => -delta.abs(),
            FocusDir::Right | FocusDir::Down => delta.abs(),
        };
        let tab = &mut self.tabs[self.active_tab];
        let focused = tab.focused;
        tab.root.nudge_enclosing_split(focused, axis, signed)
    }

    /// Grow (`delta > 0`) or shrink (`delta < 0`) the focused pane relative to its immediate
    /// enclosing split (`Cmd+` / `Cmd-` on split panes), always expanding/shrinking the focused
    /// pane regardless of whether it sits in the first or second child of the split.
    pub fn resize_focused_pane(&mut self, delta: f32) -> bool {
        let tab = &mut self.tabs[self.active_tab];
        if tab.zoomed || tab.root.leaf_count() <= 1 {
            return false;
        }
        let focused = tab.focused;
        tab.root.resize_pane_enclosing(focused, delta)
    }

    /// Reset all split ratios in the active tab to `0.5` (`Cmd+Shift+=`).
    pub fn equalize_splits(&mut self) {
        self.tabs[self.active_tab].root.equalize();
    }

    /// Compute exact non-overlapping pane rects and draggable split handles for the active tab,
    /// allocating into the manager's allocator.
    pub fn layout(&self, area: TileRect, gap: f32) -> TileLayout<'a> {
        self.layout_in(area, gap, self.alloc)
    }

    /// Compute exact non-overlapping pane rects and draggable split handles for the active tab
    /// into caller-provided `alloc` (e.g. a per-frame bump arena).
    pub fn layout_in<'b>(&self, area: TileRect, gap: f32, alloc: Alloc<'b>) -> TileLayout<'b> {
        let tab = &self.tabs[self.active_tab];
        let pane_count = tab.root.leaf_count();
        let safe_area = TileRect::new(
            area.x0,
            area.y0,
            area.x1.max(area.x0 + 40.0),
            area.y1.max(area.y0 + 40.0),
        );
        if tab.zoomed && pane_count > 1 {
            let mut panes = Vec::with_capacity_in(1, alloc);
            panes.push(PaneLayout {
                pane: tab.focused,
                rect: safe_area,
                focused: true,
            });
            return TileLayout {
                panes,
                handles: Vec::new_in(alloc),
                pane_count_in_tab: pane_count,
                is_zoomed: true,
            };
        }
        let mut panes = Vec::with_capacity_in(pane_count, alloc);
        let mut handles = Vec::with_capacity_in(pane_count.saturating_sub(1), alloc);
        tab.root
            .layout_into(safe_area, gap.max(0.0), tab.focused, &mut panes, &mut handles);
        TileLayout {
            panes,
            handles,
            pane_count_in_tab: pane_count,
            is_zoomed: false,
        }
    }
}

impl<'a> TileNode<'a> {
    pub fn leaf_count(&self) -> usize {
        match self {
            TileNode::Leaf(_) => 1,
            TileNode::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            TileNode::Leaf(id) => *id == pane,
            TileNode::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    pub fn collect_leaves<A: Allocator>(&self, out: &mut Vec<PaneId, A>) {
        match self {
            TileNode::Leaf(id) => out.push(*id),
            TileNode::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    fn split_leaf(
        &mut self,
        target: PaneId,
        new_pane: PaneId,
        split_id: SplitId,
        axis: SplitAxis,
        alloc: Alloc<'a>,
    ) -> bool {
        match self {
            TileNode::Leaf(id) if *id == target => {
                let old = *id;
                *self = TileNode::Split {
                    id: split_id,
                    axis,
                    ratio: 0.5,
                    first: Box::new_in(TileNode::Leaf(old), alloc),
                    second: Box::new_in(TileNode::Leaf(new_pane), alloc),
                };
                true
            }
            TileNode::Leaf(_) => false,
            TileNode::Split { first, second, .. } => {
                first.split_leaf(target, new_pane, split_id, axis, alloc)
                    || second.split_leaf(target, new_pane, split_id, axis, alloc)
            }
        }
    }

    fn remove_leaf(self, target: PaneId, alloc: Alloc<'a>) -> Option<Self> {
        match self {
            TileNode::Leaf(id) if id == target => None,
            TileNode::Leaf(id) => Some(TileNode::Leaf(id)),
            TileNode::Split { id, axis, ratio, first, second } => {
                if !first.contains(target) && !second.contains(target) {
                    return Some(TileNode::Split { id, axis, ratio, first, second });
                }
                let f = Box::into_inner(first).remove_leaf(target, alloc);
                let s = Box::into_inner(second).remove_leaf(target, alloc);
                match (f, s) {
                    (Some(a), Some(b)) => Some(TileNode::Split {
                        id,
                        axis,
                        ratio,
                        first: Box::new_in(a, alloc),
                        second: Box::new_in(b, alloc),
                    }),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                }
            }
        }
    }

    fn set_ratio(&mut self, target: SplitId, new_ratio: f32) -> bool {
        match self {
            TileNode::Leaf(_) => false,
            TileNode::Split { id, ratio, first, second, .. } => {
                if *id == target {
                    *ratio = new_ratio;
                    true
                } else {
                    first.set_ratio(target, new_ratio) || second.set_ratio(target, new_ratio)
                }
            }
        }
    }

    fn nudge_enclosing_split(&mut self, focused: PaneId, want_axis: SplitAxis, delta: f32) -> bool {
        match self {
            TileNode::Leaf(_) => false,
            TileNode::Split { axis, ratio, first, second, .. } => {
                let in_first = first.contains(focused);
                let in_second = second.contains(focused);
                if !in_first && !in_second {
                    return false;
                }
                let deeper = if in_first {
                    first.nudge_enclosing_split(focused, want_axis, delta)
                } else {
                    second.nudge_enclosing_split(focused, want_axis, delta)
                };
                if deeper {
                    return true;
                }
                if *axis == want_axis {
                    *ratio = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
                    return true;
                }
                false
            }
        }
    }

    fn resize_pane_enclosing(&mut self, focused: PaneId, delta: f32) -> bool {
        match self {
            TileNode::Leaf(_) => false,
            TileNode::Split { ratio, first, second, .. } => {
                let in_first = first.contains(focused);
                let in_second = second.contains(focused);
                if !in_first && !in_second {
                    return false;
                }
                let deeper = if in_first {
                    first.resize_pane_enclosing(focused, delta)
                } else {
                    second.resize_pane_enclosing(focused, delta)
                };
                if deeper {
                    return true;
                }
                let signed = if in_first { delta } else { -delta };
                let next = (*ratio + signed).clamp(MIN_RATIO, MAX_RATIO);
                if (next - *ratio).abs() > 0.0001 {
                    *ratio = next;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn equalize(&mut self) {
        if let TileNode::Split { ratio, first, second, .. } = self {
            *ratio = 0.5;
            first.equalize();
            second.equalize();
        }
    }

    fn layout_into<A: Allocator>(
        &self,
        rect: TileRect,
        gap: f32,
        focused: PaneId,
        panes: &mut Vec<PaneLayout, A>,
        handles: &mut Vec<SplitHandle, A>,
    ) {
        match self {
            TileNode::Leaf(id) => {
                panes.push(PaneLayout {
                    pane: *id,
                    rect,
                    focused: *id == focused,
                });
            }
            TileNode::Split { id, axis, ratio, first, second } => {
                let r = ratio.clamp(MIN_RATIO, MAX_RATIO);
                let (r1, r2, div_rect, hit_rect) = match axis {
                    SplitAxis::Vertical => {
                        let w = (rect.x1 - rect.x0).max(2.0);
                        let g = gap.min(w * 0.25);
                        let avail = (w - g).max(2.0);
                        let w1 = round_f32(avail * r).clamp(1.0, (avail - 1.0).max(1.0));
                        let split_x0 = rect.x0 + w1;
                        let split_x1 = (split_x0 + g).min(rect.x1);
                        let first_r = TileRect::new(rect.x0, rect.y0, split_x0, rect.y1);
                        let second_r = TileRect::new(split_x1, rect.y0, rect.x1, rect.y1);
                        let mid = (split_x0 + split_x1) * 0.5;
                        let div = TileRect::new(split_x0, rect.y0, split_x1, rect.y1);
                        let hit = TileRect::new(mid - 4.0, rect.y0, mid + 4.0, rect.y1);
                        (first_r, second_r, div, hit)
                    }
                    SplitAxis::Horizontal => {
                        let h = (rect.y1 - rect.y0).max(2.0);
                        let g = gap.min(h * 0.25);
                        let avail = (h - g).max(2.0);
                        let h1 = round_f32(avail * r).clamp(1.0, (avail - 1.0).max(1.0));
                        let split_y0 = rect.y0 + h1;
                        let split_y1 = (split_y0 + g).min(rect.y1);
                        let first_r = TileRect::new(rect.x0, rect.y0, rect.x1, split_y0);
                        let second_r = TileRect::new(rect.x0, split_y1, rect.x1, rect.y1);
                        let mid = (split_y0 + split_y1) * 0.5;
                        let div = TileRect::new(rect.x0, split_y0, rect.x1, split_y1);
                        let hit = TileRect::new(rect.x0, mid - 4.0, rect.x1, mid + 4.0);
                        (first_r, second_r, div, hit)
                    }
                };
                handles.push(SplitHandle {
                    split: *id,
                    axis: *axis,
                    rect: div_rect,
                    hit_rect,
                    parent_rect: rect,
                });
                first.layout_into(r1, gap, focused, panes, handles);
                second.layout_into(r2, gap, focused, panes, handles);
            }
        }
    }
}

/// `#![no_std]` IEEE-754 half-away-from-zero rounding (equivalent to `f32::round`).
#[inline]
fn round_f32(x: f32) -> f32 {
    if !x.is_finite() || x.abs() >= 8_388_608.0 {
        return x;
    }
    let ax = x.abs();
    let i = ax as u32;
    let rounded = if ax - (i as f32) >= 0.5 {
        (i + 1) as f32
    } else {
        i as f32
    };
    if x.is_sign_negative() { -rounded } else { rounded }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_rect_geometry() {
        let r = TileRect::new(10.0, 20.0, 110.0, 70.0);
        assert_eq!(r.width(), 100.0);
        assert_eq!(r.height(), 50.0);
        assert!(r.contains(10.0, 20.0));
        assert!(r.contains(60.0, 45.0));
        assert!(r.contains(110.0, 70.0));
        assert!(!r.contains(9.9, 45.0));
        assert!(!r.contains(60.0, 70.1));

        let inverted = TileRect::new(50.0, 50.0, 10.0, 20.0);
        assert_eq!(inverted.width(), 0.0);
        assert_eq!(inverted.height(), 0.0);
    }

    #[test]
    fn resize_focused_pane_grows_and_shrinks_both_sides() {
        let alloc = Alloc::global();
        let (mut wm, _tab, p0) = TileManager::new(alloc);
        let p1 = wm.split_focused(SplitAxis::Vertical);
        let bounds = TileRect::new(0.0, 0.0, 1000.0, 600.0);

        let width_of = |wm: &TileManager<'_>, pid: PaneId| -> f32 {
            let l = wm.layout(bounds, 4.0);
            let r = l.panes.iter().find(|p| p.pane == pid).unwrap().rect;
            r.x1 - r.x0
        };

        // Focused on p1 (second child): +delta grows p1, -delta shrinks p1.
        assert_eq!(wm.focused_pane(), p1);
        let base_p1 = width_of(&wm, p1);
        assert!(wm.resize_focused_pane(0.10));
        let grown_p1 = width_of(&wm, p1);
        assert!(grown_p1 > base_p1, "p1 should grow: {grown_p1} > {base_p1}");
        assert!(wm.resize_focused_pane(-0.20));
        let shrunk_p1 = width_of(&wm, p1);
        assert!(shrunk_p1 < base_p1, "p1 should shrink: {shrunk_p1} < {base_p1}");

        // Equalize then focus p0 (first child): +delta grows p0, -delta shrinks p0.
        wm.equalize_splits();
        assert!(wm.focus_pane(p0));
        let base_p0 = width_of(&wm, p0);
        assert!(wm.resize_focused_pane(0.10));
        let grown_p0 = width_of(&wm, p0);
        assert!(grown_p0 > base_p0, "p0 should grow: {grown_p0} > {base_p0}");
        assert!(wm.resize_focused_pane(-0.20));
        let shrunk_p0 = width_of(&wm, p0);
        assert!(shrunk_p0 < base_p0, "p0 should shrink: {shrunk_p0} < {base_p0}");
    }

    #[test]
    fn tabs_splits_focus_zoom_and_close_lifecycle() {
        let alloc = Alloc::global();
        let (mut wm, t1, p1) = TileManager::new(alloc);
        let bounds = TileRect::new(0.0, 0.0, 1200.0, 800.0);

        assert_eq!(wm.tabs().len(), 1);
        assert_eq!(wm.active_tab_index(), 0);
        assert_eq!(wm.active_tab().id, t1);
        assert_eq!(wm.focused_pane(), p1);
        assert!(!wm.toggle_zoom());
        assert_eq!(wm.close_focused(), CloseOutcome::LastPaneInLastTab);

        let p2 = wm.split_focused(SplitAxis::Vertical);
        let p3 = wm.split_focused(SplitAxis::Horizontal);
        assert_eq!(wm.active_tab_panes().as_slice(), &[p1, p2, p3]);
        assert_eq!(wm.focused_pane(), p3);

        // Layout has 3 panes and 2 split handles.
        let lay = wm.layout(bounds, 4.0);
        assert_eq!(lay.panes.len(), 3);
        assert_eq!(lay.handles.len(), 2);
        assert_eq!(lay.pane_count_in_tab, 3);
        assert!(!lay.is_zoomed);

        // Directional navigation: p3 (bottom-right) -> Up -> p2 (top-right) -> Left -> p1 (left).
        assert_eq!(wm.focus_direction(FocusDir::Up, bounds, 4.0), Some(p2));
        assert_eq!(wm.focus_direction(FocusDir::Left, bounds, 4.0), Some(p1));
        assert_eq!(wm.focus_direction(FocusDir::Right, bounds, 4.0), Some(p2));

        // Zoom focused pane p2.
        assert!(wm.toggle_zoom());
        let zoomed = wm.layout(bounds, 4.0);
        assert!(zoomed.is_zoomed);
        assert_eq!(zoomed.panes.len(), 1);
        assert_eq!(zoomed.panes[0].pane, p2);
        assert!(zoomed.handles.is_empty());

        // Cycle pane unzooms and wraps around.
        assert_eq!(wm.cycle_pane(1), Some(p3));
        assert!(!wm.active_tab().zoomed);

        // New tab + tab navigation + cross-tab focus.
        let (t2, p4) = wm.new_tab();
        assert_eq!(wm.tabs().len(), 2);
        assert_eq!(wm.active_tab().id, t2);
        assert_eq!(wm.focused_pane(), p4);
        assert_eq!(wm.select_tab_slot(1), Some(p3));
        assert_eq!(wm.select_tab_slot(9), Some(p4));
        assert_eq!(wm.cycle_tab(-1), Some(p3));
        assert!(wm.focus_pane_anywhere(p4));
        assert_eq!(wm.active_tab().id, t2);

        // Close tab t2 -> returns to t1.
        match wm.close_tab(t2) {
            CloseOutcome::TabClosed { closed_tab, closed_panes, next_focus } => {
                assert_eq!(closed_tab, t2);
                assert_eq!(closed_panes.as_slice(), &[p4]);
                assert_eq!(next_focus, p3);
            }
            other => panic!("expected TabClosed, got {other:?}"),
        }

        // Close p3 and p2 -> leaves only p1.
        assert_eq!(
            wm.close_pane(p3),
            CloseOutcome::PaneClosed { closed: p3, next_focus: p2 }
        );
        assert_eq!(
            wm.close_focused(),
            CloseOutcome::PaneClosed { closed: p2, next_focus: p1 }
        );
        assert_eq!(wm.active_tab_panes().as_slice(), &[p1]);
        assert_eq!(wm.close_focused(), CloseOutcome::LastPaneInLastTab);
    }

    #[test]
    fn split_ratio_drag_nudge_and_tiny_viewport_never_panic() {
        let alloc = Alloc::global();
        let (mut wm, _t1, _p1) = TileManager::new(alloc);
        let _p2 = wm.split_focused(SplitAxis::Vertical);
        let lay = wm.layout(TileRect::new(0.0, 0.0, 1000.0, 500.0), 4.0);
        let split_id = lay.handles[0].split;

        assert!(wm.set_split_ratio(split_id, 0.01));
        let clamped_low = wm.layout(TileRect::new(0.0, 0.0, 1000.0, 500.0), 4.0);
        assert!(clamped_low.panes[0].rect.width() >= 140.0);

        assert!(wm.nudge_focused_split(FocusDir::Right, 0.25));
        let nudged = wm.layout(TileRect::new(0.0, 0.0, 1000.0, 500.0), 4.0);
        assert!(nudged.panes[0].rect.width() > clamped_low.panes[0].rect.width());

        // Degenerate / tiny viewport must not panic.
        let tiny = wm.layout(TileRect::new(0.0, 0.0, 0.0, 0.0), 100.0);
        assert_eq!(tiny.panes.len(), 2);
        assert!(tiny.panes[0].rect.width() >= 1.0);
        assert!(tiny.panes[1].rect.width() >= 1.0);
    }
}
