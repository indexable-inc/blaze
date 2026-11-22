//! Pure-Rust Binary-Tree Modular Docking & Tiling Window Manager (Rerun / Tracy / egui_dock style).
//!
//! Features:
//!   - Arbitrary nested Horizontal & Vertical splits with live draggable splitters.
//!   - Every pane (`Timeline`, `FlameGraph`, `CodeMap`, `CpuTopology`, `HotSpots`, `HeapDiff`,
//!     `Builds`, `Advisor`, `IoLocks`, `Inspector`) can be:
//!       * Dragged by its tab onto ANY other pane's Left / Right / Top / Bottom / Center drop zone.
//!       * Maximized / Expanded to full viewport (`⤢` button or double-click tab bar) and restored.
//!       * Split side-by-side (`+` button) or closed (`×`), with empty leaves pruned automatically.
//!   - Resized freely in both X and Y via any split divider.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PaneId {
    Overview,
    Timeline,
    FlameGraph,
    CodeMap,
    CpuTopology,
    HotSpots,
    HeapDiff,
    Builds,
    Advisor,
    IoLocks,
    Inspector,
}

impl PaneId {
    pub const ALL: [PaneId; 11] = [
        PaneId::Overview,
        PaneId::Timeline,
        PaneId::CodeMap,
        PaneId::FlameGraph,
        PaneId::CpuTopology,
        PaneId::HotSpots,
        PaneId::HeapDiff,
        PaneId::Builds,
        PaneId::Advisor,
        PaneId::IoLocks,
        PaneId::Inspector,
    ];

    pub fn title(self) -> &'static str {
        match self {
            PaneId::Overview => "Vitals",
            PaneId::Timeline => "Trace",
            PaneId::FlameGraph => "Flame",
            PaneId::CodeMap => "Map",
            PaneId::CpuTopology => "CPU",
            PaneId::HotSpots => "Hot",
            PaneId::HeapDiff => "Heap",
            PaneId::Builds => "Builds",
            PaneId::Advisor => "Alerts",
            PaneId::IoLocks => "Locks",
            PaneId::Inspector => "Code",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDir {
    /// Side-by-side: `a` on left, `b` on right.
    Horizontal,
    /// Stacked: `a` on top, `b` on bottom.
    Vertical,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    #[inline]
    pub fn contains(self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

pub const PANE_HEADER_H: f32 = 28.0;
pub const SPLIT_GAP: f32 = 5.0;

#[derive(Clone)]
pub enum DockNode {
    Leaf {
        id: u32,
        tabs: Vec<PaneId>,
        active: usize,
    },
    Split {
        id: u32,
        dir: SplitDir,
        ratio: f32,
        a: Box<DockNode>,
        b: Box<DockNode>,
    },
}

#[derive(Clone)]
pub struct LeafLayout {
    pub id: u32,
    pub rect: Rect,
    pub header: Rect,
    pub body: Rect,
    pub tabs: Vec<PaneId>,
    pub active: usize,
}

impl LeafLayout {
    #[inline]
    pub fn active_pane(&self) -> Option<PaneId> {
        self.tabs.get(self.active).copied()
    }
}

#[derive(Clone, Copy)]
pub struct SplitterLayout {
    pub split_id: u32,
    pub dir: SplitDir,
    pub rect: Rect,
    pub parent: Rect,
}

pub struct DockTree {
    pub root: DockNode,
    pub maximized: Option<PaneId>,
    next_id: u32,
}

impl Default for DockTree {
    fn default() -> Self {
        Self::default_layout()
    }
}

impl DockTree {
    pub fn default_layout() -> Self {
        let root = DockNode::Leaf {
            id: 1,
            tabs: vec![
                PaneId::Overview,
                PaneId::HotSpots,
                PaneId::HeapDiff,
                PaneId::FlameGraph,
                PaneId::Timeline,
                PaneId::CpuTopology,
                PaneId::IoLocks,
                PaneId::CodeMap,
                PaneId::Builds,
                PaneId::Advisor,
            ],
            active: 0,
        };
        Self {
            root,
            maximized: None,
            next_id: 10,
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Computes screen rectangles for every visible leaf & draggable splitter.
    pub fn layout(&self, area: Rect) -> (Vec<LeafLayout>, Vec<SplitterLayout>) {
        if let Some(max_pane) = self.maximized {
            let leaf = LeafLayout {
                id: 0,
                rect: area,
                header: Rect {
                    x: area.x,
                    y: area.y,
                    w: area.w,
                    h: PANE_HEADER_H,
                },
                body: Rect {
                    x: area.x,
                    y: area.y + PANE_HEADER_H,
                    w: area.w,
                    h: (area.h - PANE_HEADER_H).max(1.0),
                },
                tabs: vec![max_pane],
                active: 0,
            };
            return (vec![leaf], Vec::new());
        }
        let mut leaves = Vec::with_capacity(8);
        let mut splits = Vec::with_capacity(8);
        layout_node(&self.root, area, &mut leaves, &mut splits);
        (leaves, splits)
    }

    pub fn contains_pane(&self, pane: PaneId) -> bool {
        node_contains(&self.root, pane)
    }

    /// Focuses `pane` if already docked; if not open yet, docks it on the right at 30% width.
    pub fn focus_or_open(&mut self, pane: PaneId) {
        if self.maximized.is_some() && self.maximized != Some(pane) {
            self.maximized = None;
        }
        if focus_pane(&mut self.root, pane) {
            return;
        }
        let split_id = self.alloc_id();
        let leaf_id = self.alloc_id();
        let old_root = core::mem::replace(
            &mut self.root,
            DockNode::Leaf {
                id: 0,
                tabs: Vec::new(),
                active: 0,
            },
        );
        self.root = DockNode::Split {
            id: split_id,
            dir: SplitDir::Horizontal,
            ratio: if pane == PaneId::Inspector { 0.70 } else { 0.58 },
            a: Box::new(old_root),
            b: Box::new(DockNode::Leaf {
                id: leaf_id,
                tabs: vec![pane],
                active: 0,
            }),
        };
    }

    /// Ensures `pane` is visible in its own split alongside `Overview` (e.g. dragging `HEAP` opens `Heap` below).
    pub fn open_split(&mut self, pane: PaneId, dir: SplitDir, ratio: f32) {
        if self.maximized.is_some() && self.maximized != Some(pane) {
            self.maximized = None;
        }
        if let Some(leaf_id) = leaf_containing(&self.root, pane) {
            let overview_leaf = leaf_containing(&self.root, PaneId::Overview);
            if Some(leaf_id) != overview_leaf {
                focus_pane(&mut self.root, pane);
                return;
            }
            if leaf_tab_count(&self.root, leaf_id) > 1 {
                remove_pane_from_leaf(&mut self.root, leaf_id, pane);
                prune_empty(&mut self.root);
            } else {
                return;
            }
        }
        let split_id = self.alloc_id();
        let leaf_id = self.alloc_id();
        let old_root = core::mem::replace(
            &mut self.root,
            DockNode::Leaf {
                id: 0,
                tabs: Vec::new(),
                active: 0,
            },
        );
        self.root = DockNode::Split {
            id: split_id,
            dir,
            ratio,
            a: Box::new(old_root),
            b: Box::new(DockNode::Leaf {
                id: leaf_id,
                tabs: vec![pane],
                active: 0,
            }),
        };
    }

    pub fn select_tab(&mut self, leaf_id: u32, tab_idx: usize) {
        select_tab_node(&mut self.root, leaf_id, tab_idx);
    }

    pub fn set_ratio(&mut self, split_id: u32, ratio: f32) {
        set_ratio_node(&mut self.root, split_id, ratio.clamp(0.14, 0.86));
    }

    pub fn toggle_maximize(&mut self, pane: PaneId) {
        if self.maximized == Some(pane) {
            self.maximized = None;
        } else {
            self.maximized = Some(pane);
        }
    }

    /// Pop the active tab of a multi-tab leaf into a side-by-side split right next to it.
    pub fn pop_out_pane(&mut self, leaf_id: u32, pane: PaneId) {
        if self.maximized.is_some() {
            self.maximized = None;
        }
        // Only split if leaf has > 1 tab
        if leaf_tab_count(&self.root, leaf_id) <= 1 {
            return;
        }
        self.dock_pane(leaf_id, pane, leaf_id, DropZone::Right);
    }

    pub fn close_pane(&mut self, pane: PaneId) {
        if self.maximized == Some(pane) {
            self.maximized = None;
        }
        // Keep at least 1 pane in the entire workspace
        if total_pane_count(&self.root) <= 1 {
            return;
        }
        remove_pane_from_tree(&mut self.root, pane);
        prune_empty(&mut self.root);
    }

    /// Move `pane` from `from_leaf` and dock it into `target_leaf` at `zone` (Left/Right/Top/Bottom/Center).
    pub fn dock_pane(&mut self, from_leaf: u32, pane: PaneId, target_leaf: u32, zone: DropZone) {
        if self.maximized.is_some() {
            self.maximized = None;
        }
        if from_leaf == target_leaf && zone == DropZone::Center {
            return;
        }
        // Don't split a single-tab leaf against itself
        if from_leaf == target_leaf && leaf_tab_count(&self.root, from_leaf) <= 1 {
            return;
        }

        // Remove from source first (do NOT prune yet if target == source, prune after insert!)
        if !remove_pane_from_leaf(&mut self.root, from_leaf, pane) {
            return;
        }

        let new_split_id = self.alloc_id();
        let new_leaf_id = self.alloc_id();
        insert_into_target(&mut self.root, target_leaf, pane, zone, new_split_id, new_leaf_id);
        prune_empty(&mut self.root);
    }
}

/// Computes which of the 5 docking zones (`Left`, `Right`, `Top`, `Bottom`, `Center`) `(px, py)` falls in.
pub fn compute_drop_zone(r: Rect, px: f32, py: f32) -> DropZone {
    let nx = ((px - r.x) / r.w.max(1.0)).clamp(0.0, 1.0);
    let ny = ((py - r.y) / r.h.max(1.0)).clamp(0.0, 1.0);
    if nx < 0.24 {
        DropZone::Left
    } else if nx > 0.76 {
        DropZone::Right
    } else if ny < 0.28 {
        DropZone::Top
    } else if ny > 0.72 {
        DropZone::Bottom
    } else {
        DropZone::Center
    }
}

pub fn drop_preview_rect(r: Rect, zone: DropZone) -> Rect {
    match zone {
        DropZone::Left => Rect {
            x: r.x,
            y: r.y,
            w: r.w * 0.5,
            h: r.h,
        },
        DropZone::Right => Rect {
            x: r.x + r.w * 0.5,
            y: r.y,
            w: r.w * 0.5,
            h: r.h,
        },
        DropZone::Top => Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h * 0.5,
        },
        DropZone::Bottom => Rect {
            x: r.x,
            y: r.y + r.h * 0.5,
            w: r.w,
            h: r.h * 0.5,
        },
        DropZone::Center => r,
    }
}

fn layout_node(node: &DockNode, area: Rect, leaves: &mut Vec<LeafLayout>, splits: &mut Vec<SplitterLayout>) {
    match node {
        DockNode::Leaf { id, tabs, active } => {
            let act = (*active).min(tabs.len().saturating_sub(1));
            leaves.push(LeafLayout {
                id: *id,
                rect: area,
                header: Rect {
                    x: area.x,
                    y: area.y,
                    w: area.w,
                    h: PANE_HEADER_H.min(area.h),
                },
                body: Rect {
                    x: area.x,
                    y: area.y + PANE_HEADER_H,
                    w: area.w,
                    h: (area.h - PANE_HEADER_H).max(1.0),
                },
                tabs: tabs.clone(),
                active: act,
            });
        }
        DockNode::Split { id, dir, ratio, a, b } => match dir {
            SplitDir::Horizontal => {
                let avail = (area.w - SPLIT_GAP).max(40.0);
                let aw = (avail * ratio).round().clamp(120.0, (avail - 120.0).max(120.0));
                let bw = (avail - aw).max(40.0);
                let ra = Rect {
                    x: area.x,
                    y: area.y,
                    w: aw,
                    h: area.h,
                };
                let sp = Rect {
                    x: area.x + aw,
                    y: area.y,
                    w: SPLIT_GAP,
                    h: area.h,
                };
                let rb = Rect {
                    x: area.x + aw + SPLIT_GAP,
                    y: area.y,
                    w: bw,
                    h: area.h,
                };
                splits.push(SplitterLayout {
                    split_id: *id,
                    dir: *dir,
                    rect: sp,
                    parent: area,
                });
                layout_node(a, ra, leaves, splits);
                layout_node(b, rb, leaves, splits);
            }
            SplitDir::Vertical => {
                let avail = (area.h - SPLIT_GAP).max(40.0);
                let ah = (avail * ratio).round().clamp(90.0, (avail - 90.0).max(90.0));
                let bh = (avail - ah).max(40.0);
                let ra = Rect {
                    x: area.x,
                    y: area.y,
                    w: area.w,
                    h: ah,
                };
                let sp = Rect {
                    x: area.x,
                    y: area.y + ah,
                    w: area.w,
                    h: SPLIT_GAP,
                };
                let rb = Rect {
                    x: area.x,
                    y: area.y + ah + SPLIT_GAP,
                    w: area.w,
                    h: bh,
                };
                splits.push(SplitterLayout {
                    split_id: *id,
                    dir: *dir,
                    rect: sp,
                    parent: area,
                });
                layout_node(a, ra, leaves, splits);
                layout_node(b, rb, leaves, splits);
            }
        },
    }
}

fn node_contains(node: &DockNode, pane: PaneId) -> bool {
    match node {
        DockNode::Leaf { tabs, .. } => tabs.contains(&pane),
        DockNode::Split { a, b, .. } => node_contains(a, pane) || node_contains(b, pane),
    }
}

fn leaf_containing(node: &DockNode, pane: PaneId) -> Option<u32> {
    match node {
        DockNode::Leaf { id, tabs, .. } if tabs.contains(&pane) => Some(*id),
        DockNode::Leaf { .. } => None,
        DockNode::Split { a, b, .. } => leaf_containing(a, pane).or_else(|| leaf_containing(b, pane)),
    }
}

fn total_pane_count(node: &DockNode) -> usize {
    match node {
        DockNode::Leaf { tabs, .. } => tabs.len(),
        DockNode::Split { a, b, .. } => total_pane_count(a) + total_pane_count(b),
    }
}

fn leaf_tab_count(node: &DockNode, target: u32) -> usize {
    match node {
        DockNode::Leaf { id, tabs, .. } if *id == target => tabs.len(),
        DockNode::Leaf { .. } => 0,
        DockNode::Split { a, b, .. } => leaf_tab_count(a, target) + leaf_tab_count(b, target),
    }
}

fn focus_pane(node: &mut DockNode, pane: PaneId) -> bool {
    match node {
        DockNode::Leaf { tabs, active, .. } => {
            if let Some(pos) = tabs.iter().position(|p| *p == pane) {
                *active = pos;
                true
            } else {
                false
            }
        }
        DockNode::Split { a, b, .. } => focus_pane(a, pane) || focus_pane(b, pane),
    }
}

fn select_tab_node(node: &mut DockNode, leaf_id: u32, tab_idx: usize) {
    match node {
        DockNode::Leaf { id, tabs, active } if *id == leaf_id => {
            if tab_idx < tabs.len() {
                *active = tab_idx;
            }
        }
        DockNode::Leaf { .. } => {}
        DockNode::Split { a, b, .. } => {
            select_tab_node(a, leaf_id, tab_idx);
            select_tab_node(b, leaf_id, tab_idx);
        }
    }
}

fn set_ratio_node(node: &mut DockNode, split_id: u32, new_ratio: f32) {
    if let DockNode::Split { id, ratio, a, b, .. } = node {
        if *id == split_id {
            *ratio = new_ratio;
            return;
        }
        set_ratio_node(a, split_id, new_ratio);
        set_ratio_node(b, split_id, new_ratio);
    }
}

fn remove_pane_from_leaf(node: &mut DockNode, leaf_id: u32, pane: PaneId) -> bool {
    match node {
        DockNode::Leaf { id, tabs, active } if *id == leaf_id => {
            if let Some(pos) = tabs.iter().position(|p| *p == pane) {
                tabs.remove(pos);
                if *active >= tabs.len() && !tabs.is_empty() {
                    *active = tabs.len() - 1;
                }
                true
            } else {
                false
            }
        }
        DockNode::Leaf { .. } => false,
        DockNode::Split { a, b, .. } => {
            remove_pane_from_leaf(a, leaf_id, pane) || remove_pane_from_leaf(b, leaf_id, pane)
        }
    }
}

fn remove_pane_from_tree(node: &mut DockNode, pane: PaneId) -> bool {
    match node {
        DockNode::Leaf { tabs, active, .. } => {
            if let Some(pos) = tabs.iter().position(|p| *p == pane) {
                tabs.remove(pos);
                if *active >= tabs.len() && !tabs.is_empty() {
                    *active = tabs.len() - 1;
                }
                true
            } else {
                false
            }
        }
        DockNode::Split { a, b, .. } => {
            remove_pane_from_tree(a, pane) || remove_pane_from_tree(b, pane)
        }
    }
}

fn insert_into_target(
    node: &mut DockNode,
    target_leaf: u32,
    pane: PaneId,
    zone: DropZone,
    split_id: u32,
    new_leaf_id: u32,
) -> bool {
    match node {
        DockNode::Leaf { id, tabs, active } if *id == target_leaf => {
            if zone == DropZone::Center {
                tabs.push(pane);
                *active = tabs.len() - 1;
                return true;
            }
            let new_leaf = DockNode::Leaf {
                id: new_leaf_id,
                tabs: vec![pane],
                active: 0,
            };
            let existing = core::mem::replace(
                node,
                DockNode::Leaf {
                    id: 0,
                    tabs: Vec::new(),
                    active: 0,
                },
            );
            let (dir, first, second) = match zone {
                DropZone::Left => (SplitDir::Horizontal, new_leaf, existing),
                DropZone::Right => (SplitDir::Horizontal, existing, new_leaf),
                DropZone::Top => (SplitDir::Vertical, new_leaf, existing),
                DropZone::Bottom => (SplitDir::Vertical, existing, new_leaf),
                DropZone::Center => unreachable!(),
            };
            *node = DockNode::Split {
                id: split_id,
                dir,
                ratio: 0.5,
                a: Box::new(first),
                b: Box::new(second),
            };
            true
        }
        DockNode::Leaf { .. } => false,
        DockNode::Split { a, b, .. } => {
            insert_into_target(a, target_leaf, pane, zone, split_id, new_leaf_id)
                || insert_into_target(b, target_leaf, pane, zone, split_id, new_leaf_id)
        }
    }
}

/// Collapses any Split whose child Leaf became empty after a drag or close.
fn prune_empty(node: &mut DockNode) -> bool {
    match node {
        DockNode::Leaf { tabs, .. } => tabs.is_empty(),
        DockNode::Split { a, b, .. } => {
            let empty_a = prune_empty(a);
            let empty_b = prune_empty(b);
            if empty_a && empty_b {
                return true;
            }
            if empty_a {
                let keep = core::mem::replace(
                    b.as_mut(),
                    DockNode::Leaf {
                        id: 0,
                        tabs: Vec::new(),
                        active: 0,
                    },
                );
                *node = keep;
                return false;
            }
            if empty_b {
                let keep = core::mem::replace(
                    a.as_mut(),
                    DockNode::Leaf {
                        id: 0,
                        tabs: Vec::new(),
                        active: 0,
                    },
                );
                *node = keep;
                return false;
            }
            false
        }
    }
}
