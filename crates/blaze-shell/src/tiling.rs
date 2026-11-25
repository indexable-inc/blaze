//! Pure sans-IO binary-space-partitioning (BSP) tiling window manager for `blaze-shell`.
//!
//! Manages workspace tabs (`Cmd+T`, `Cmd+1..9`), arbitrary nested vertical/horizontal splits
//! (`Cmd+D`, `Cmd+Shift+D`), directional spatial focus (`Cmd+Opt+Arrow`/`HJKL`), pane zoom
//! (`Cmd+Shift+Enter`), split ratio resizing (mouse divider drag & keyboard nudge), and
//! safe layout computation that never panics at any viewport dimension.

use crate::view::HitRect;

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
pub enum TileNode {
    Leaf(PaneId),
    Split {
        id: SplitId,
        axis: SplitAxis,
        /// Fraction `[0.15, 0.85]` allocated to `first`.
        ratio: f32,
        first: Box<TileNode>,
        second: Box<TileNode>,
    },
}

/// One workspace tab containing a BSP tree of panes.
#[derive(Clone, Debug)]
pub struct WorkspaceTab {
    pub id: TabId,
    pub root: TileNode,
    pub focused: PaneId,
    pub zoomed: bool,
}

/// Computed screen rectangle for one visible pane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneLayout {
    pub pane: PaneId,
    pub rect: HitRect,
    pub focused: bool,
}

/// Interactive divider handle between two sibling subtrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitHandle {
    pub split: SplitId,
    pub axis: SplitAxis,
    /// Visual 1px/gap divider rect.
    pub rect: HitRect,
    /// Expanded hit target rect for effortless pointer grabbing.
    pub hit_rect: HitRect,
    /// Bounding rect of the parent split container (maps drag coordinates -> ratio).
    pub parent_rect: HitRect,
}

/// Complete layout result for the active workspace tab.
#[derive(Clone, Debug, Default)]
pub struct TileLayout {
    pub panes: Vec<PaneLayout>,
    pub handles: Vec<SplitHandle>,
    pub pane_count_in_tab: usize,
    pub is_zoomed: bool,
}

/// Outcome of closing a pane or workspace tab (`Cmd+W`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    PaneClosed {
        closed: PaneId,
        next_focus: PaneId,
    },
    TabClosed {
        closed_tab: TabId,
        closed_panes: Vec<PaneId>,
        next_focus: PaneId,
    },
    LastPaneInLastTab,
}

const MIN_RATIO: f32 = 0.15;
const MAX_RATIO: f32 = 0.85;

/// Complete in-window workspace & BSP tiling window manager.
#[derive(Clone, Debug)]
pub struct TileManager {
    tabs: Vec<WorkspaceTab>,
    active_tab: usize,
    next_pane: u32,
    next_tab: u32,
    next_split: u32,
}

impl TileManager {
    pub fn new() -> (Self, TabId, PaneId) {
        let tab_id = TabId(1);
        let pane_id = PaneId(1);
        let tab = WorkspaceTab {
            id: tab_id,
            root: TileNode::Leaf(pane_id),
            focused: pane_id,
            zoomed: false,
        };
        let mgr = Self {
            tabs: vec![tab],
            active_tab: 0,
            next_pane: 2,
            next_tab: 2,
            next_split: 1,
        };
        (mgr, tab_id, pane_id)
    }

    pub fn tabs(&self) -> &[WorkspaceTab] {
        &self.tabs
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_tab(&self) -> &WorkspaceTab {
        &self.tabs[self.active_tab]
    }

    pub fn focused_pane(&self) -> PaneId {
        self.tabs[self.active_tab].focused
    }

    /// All panes belonging to the currently active workspace tab (in tree order).
    pub fn active_tab_panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
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

    /// Switch to 0-based workspace tab index (`Cmd+1..8`, and `Cmd+9` jumps to last tab).
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
        let tab = &mut self.tabs[self.active_tab];
        tab.zoomed = false;
        let target = tab.focused;
        if tab.root.split_leaf(target, new_pane, split_id, axis) {
            tab.focused = new_pane;
        }
        new_pane
    }

    /// Close the currently focused pane (`Cmd+W`).
    pub fn close_focused(&mut self) -> CloseOutcome {
        let target = self.focused_pane();
        self.close_pane(target)
    }

    /// Close a specific pane in the active tab (collapsing its parent split or closing the tab).
    pub fn close_pane(&mut self, target: PaneId) -> CloseOutcome {
        let tab_idx = self.active_tab;
        let leaves = {
            let mut v = Vec::new();
            self.tabs[tab_idx].root.collect_leaves(&mut v);
            v
        };
        if !leaves.contains(&target) {
            return CloseOutcome::LastPaneInLastTab;
        }
        if leaves.len() <= 1 {
            if self.tabs.len() <= 1 {
                return CloseOutcome::LastPaneInLastTab;
            }
            let removed = self.tabs.remove(tab_idx);
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
        if let Some(new_root) = tab.root.clone().remove_leaf(target) {
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
    pub fn close_tab(&mut self, id: TabId) -> CloseOutcome {
        if self.tabs.len() <= 1 {
            return CloseOutcome::LastPaneInLastTab;
        }
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return CloseOutcome::LastPaneInLastTab;
        };
        let removed = self.tabs.remove(idx);
        let mut closed_panes = Vec::new();
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

    /// Cycle focus to next/previous pane in tree order (`Cmd+]` / `Cmd+[`).
    pub fn cycle_pane(&mut self, delta: isize) -> Option<PaneId> {
        let tab = &mut self.tabs[self.active_tab];
        let mut leaves = Vec::new();
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
    pub fn focus_direction(&mut self, dir: FocusDir, area: HitRect, gap: f32) -> Option<PaneId> {
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
                    let ov = (cur.rect.y1.min(cand.rect.y1) - cur.rect.y0.max(cand.rect.y0)).max(0.0);
                    (cur.rect.x0 - cand.rect.x1, ov, (ny - cy).abs())
                }
                FocusDir::Right => {
                    if cand.rect.x0 < cur.rect.x1 - gap - 2.0 {
                        continue;
                    }
                    let ov = (cur.rect.y1.min(cand.rect.y1) - cur.rect.y0.max(cand.rect.y0)).max(0.0);
                    (cand.rect.x0 - cur.rect.x1, ov, (ny - cy).abs())
                }
                FocusDir::Up => {
                    if cand.rect.y1 > cur.rect.y0 + gap + 2.0 {
                        continue;
                    }
                    let ov = (cur.rect.x1.min(cand.rect.x1) - cur.rect.x0.max(cand.rect.x0)).max(0.0);
                    (cur.rect.y0 - cand.rect.y1, ov, (nx - cx).abs())
                }
                FocusDir::Down => {
                    if cand.rect.y0 < cur.rect.y1 - gap - 2.0 {
                        continue;
                    }
                    let ov = (cur.rect.x1.min(cand.rect.x1) - cur.rect.x0.max(cand.rect.x0)).max(0.0);
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

    /// Reset all split ratios in the active tab to `0.5` (`Cmd+Shift+=`).
    pub fn equalize_splits(&mut self) {
        self.tabs[self.active_tab].root.equalize();
    }

    /// Compute exact non-overlapping pane rects and draggable split handles for the active tab.
    pub fn layout(&self, area: HitRect, gap: f32) -> TileLayout {
        let tab = &self.tabs[self.active_tab];
        let pane_count = tab.root.leaf_count();
        let safe_area = HitRect::new(
            area.x0,
            area.y0,
            area.x1.max(area.x0 + 40.0),
            area.y1.max(area.y0 + 40.0),
        );
        if tab.zoomed && pane_count > 1 {
            return TileLayout {
                panes: vec![PaneLayout {
                    pane: tab.focused,
                    rect: safe_area,
                    focused: true,
                }],
                handles: Vec::new(),
                pane_count_in_tab: pane_count,
                is_zoomed: true,
            };
        }
        let mut panes = Vec::with_capacity(pane_count);
        let mut handles = Vec::with_capacity(pane_count.saturating_sub(1));
        tab.root.layout_into(safe_area, gap.max(0.0), tab.focused, &mut panes, &mut handles);
        TileLayout {
            panes,
            handles,
            pane_count_in_tab: pane_count,
            is_zoomed: false,
        }
    }
}

impl TileNode {
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

    pub fn collect_leaves(&self, out: &mut Vec<PaneId>) {
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
    ) -> bool {
        match self {
            TileNode::Leaf(id) if *id == target => {
                let old = *id;
                *self = TileNode::Split {
                    id: split_id,
                    axis,
                    ratio: 0.5,
                    first: Box::new(TileNode::Leaf(old)),
                    second: Box::new(TileNode::Leaf(new_pane)),
                };
                true
            }
            TileNode::Leaf(_) => false,
            TileNode::Split { first, second, .. } => {
                first.split_leaf(target, new_pane, split_id, axis)
                    || second.split_leaf(target, new_pane, split_id, axis)
            }
        }
    }

    fn remove_leaf(self, target: PaneId) -> Option<Self> {
        match self {
            TileNode::Leaf(id) if id == target => None,
            TileNode::Leaf(id) => Some(TileNode::Leaf(id)),
            TileNode::Split { id, axis, ratio, first, second } => {
                if !first.contains(target) && !second.contains(target) {
                    return Some(TileNode::Split { id, axis, ratio, first, second });
                }
                let f = first.remove_leaf(target);
                let s = second.remove_leaf(target);
                match (f, s) {
                    (Some(a), Some(b)) => Some(TileNode::Split {
                        id,
                        axis,
                        ratio,
                        first: Box::new(a),
                        second: Box::new(b),
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

    fn equalize(&mut self) {
        if let TileNode::Split { ratio, first, second, .. } = self {
            *ratio = 0.5;
            first.equalize();
            second.equalize();
        }
    }

    fn layout_into(
        &self,
        rect: HitRect,
        gap: f32,
        focused: PaneId,
        panes: &mut Vec<PaneLayout>,
        handles: &mut Vec<SplitHandle>,
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
                        let w1 = (avail * r).round().clamp(1.0, (avail - 1.0).max(1.0));
                        let split_x0 = rect.x0 + w1;
                        let split_x1 = (split_x0 + g).min(rect.x1);
                        let first_r = HitRect::new(rect.x0, rect.y0, split_x0, rect.y1);
                        let second_r = HitRect::new(split_x1, rect.y0, rect.x1, rect.y1);
                        let mid = (split_x0 + split_x1) * 0.5;
                        let div = HitRect::new(split_x0, rect.y0, split_x1, rect.y1);
                        let hit = HitRect::new(mid - 4.0, rect.y0, mid + 4.0, rect.y1);
                        (first_r, second_r, div, hit)
                    }
                    SplitAxis::Horizontal => {
                        let h = (rect.y1 - rect.y0).max(2.0);
                        let g = gap.min(h * 0.25);
                        let avail = (h - g).max(2.0);
                        let h1 = (avail * r).round().clamp(1.0, (avail - 1.0).max(1.0));
                        let split_y0 = rect.y0 + h1;
                        let split_y1 = (split_y0 + g).min(rect.y1);
                        let first_r = HitRect::new(rect.x0, rect.y0, rect.x1, split_y0);
                        let second_r = HitRect::new(rect.x0, split_y1, rect.x1, rect.y1);
                        let mid = (split_y0 + split_y1) * 0.5;
                        let div = HitRect::new(rect.x0, split_y0, rect.x1, split_y1);
                        let hit = HitRect::new(rect.x0, mid - 4.0, rect.x1, mid + 4.0);
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
