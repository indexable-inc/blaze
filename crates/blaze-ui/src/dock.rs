//! Generic, zero-recursion, ECS-backed Binary-Tree Modular Docking & Tiling Manager.
//!
//! Models splits, leaves, and tabs as real entities in `blaze_ecs::World` connected via
//! `ChildOf` and [`DockedIn`] relations, while supporting:
//! - Arbitrary nested Horizontal & Vertical splits with live draggable splitters.
//! - Dragging any tab onto any leaf's `Left` / `Right` / `Top` / `Bottom` / `Center` drop zone.
//! - Maximizing / restoring any pane to full viewport.
//! - Splitting side-by-side or closing tabs with automatic empty-leaf pruning.

use core::alloc::Allocator;

use blaze_ecs::{ChildOf, Entity, World};
use blaze_own::Vec as OwnVec;

use crate::components::{ChildOrder, DockedIn, WidgetKind};
use crate::error::UiError;
use crate::types::Rect;

pub const DOCK_HEADER_H: f32 = 28.0;
pub const DOCK_SPLIT_GAP: f32 = 5.0;

/// Orientation of a binary dock split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    /// Side-by-side (`a` left, `b` right).
    Horizontal,
    /// Stacked (`a` top, `b` bottom).
    Vertical,
}

/// Drop zone when dragging a dock tab over a leaf pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

/// ECS component stored on a dock split entity (`WidgetKind::DockSplit`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockSplit {
    pub dir: SplitDir,
    pub ratio: f32,
    pub first: Entity,
    pub second: Entity,
}

/// ECS component stored on a dock leaf container (`WidgetKind::DockLeaf`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockLeaf {
    pub active_tab: usize,
}

/// ECS component stored on a dock tab entity (`WidgetKind::DockTab`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockTab {
    /// Stable application pane identifier.
    pub pane_id: u32,
    /// Content root entity rendered inside the leaf body when this tab is active.
    pub content: Entity,
}

/// Computed layout for one dock leaf pane.
#[derive(Clone, Copy, Debug)]
pub struct DockLeafRect {
    pub leaf: Entity,
    pub rect: Rect,
    pub header: Rect,
    pub body: Rect,
    pub active_tab: usize,
}

/// Computed layout for one draggable split divider.
#[derive(Clone, Copy, Debug)]
pub struct DockSplitterRect {
    pub split: Entity,
    pub dir: SplitDir,
    pub rect: Rect,
    pub parent_rect: Rect,
}

/// Active tab-drag state for live drop-zone previews.
#[derive(Clone, Copy, Debug)]
pub struct DockDrag {
    pub tab: Entity,
    pub source_leaf: Entity,
    pub pointer_x: f32,
    pub pointer_y: f32,
    pub target: Option<(Entity, DropZone, Rect)>,
}

/// ECS-backed binary-tree docking workspace manager.
pub struct DockWorkspace<A: Allocator + Copy> {
    pub root: Option<Entity>,
    pub maximized_tab: Option<Entity>,
    pub leaves: OwnVec<DockLeafRect, A>,
    pub splitters: OwnVec<DockSplitterRect, A>,
    pub dragging_splitter: Option<Entity>,
    pub dragging_tab: Option<DockDrag>,
    next_order: u32,
}

impl<A: Allocator + Copy + 'static> DockWorkspace<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            root: None,
            maximized_tab: None,
            leaves: OwnVec::new_in(alloc),
            splitters: OwnVec::new_in(alloc),
            dragging_splitter: None,
            dragging_tab: None,
            next_order: 1,
        }
    }

    fn spawn_unique(&mut self, world: &mut World<A>, name: &str) -> Result<Entity, UiError> {
        let ord = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        if world.entity_by_name(name).is_none() {
            Ok(world.spawn_named(name)?)
        } else {
            let mut uniq = blaze_own::String::new_in(world.alloc());
            let _ = core::fmt::write(&mut uniq, format_args!("{name}#{ord}"));
            Ok(world.spawn_named(uniq.as_str())?)
        }
    }

    /// Spawn a new `DockLeaf` entity.
    pub fn spawn_leaf(&mut self, world: &mut World<A>, name: &str) -> Result<Entity, UiError> {
        let leaf = self.spawn_unique(world, name)?;
        world.set_copy(leaf, WidgetKind::DockLeaf)?;
        world.set_copy(leaf, DockLeaf { active_tab: 0 })?;
        Ok(leaf)
    }

    /// Spawn a `DockTab` entity docked inside `leaf` via `(DockedIn, leaf)` and `(ChildOf, leaf)`.
    pub fn add_tab(
        &mut self,
        world: &mut World<A>,
        leaf: Entity,
        name: &str,
        pane_id: u32,
        content: Entity,
    ) -> Result<Entity, UiError> {
        let tab = self.spawn_unique(world, name)?;
        world.set_copy(tab, WidgetKind::DockTab)?;
        world.set_copy(tab, DockTab { pane_id, content })?;
        let ord = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        world.set_copy(tab, ChildOrder(ord))?;
        world.relate::<DockedIn>(tab, leaf)?;
        world.relate::<ChildOf>(tab, leaf)?;
        Ok(tab)
    }

    /// Spawn a `DockSplit` combining `first` and `second` with orientation `dir` and split `ratio`.
    pub fn spawn_split(
        &mut self,
        world: &mut World<A>,
        name: &str,
        dir: SplitDir,
        ratio: f32,
        first: Entity,
        second: Entity,
    ) -> Result<Entity, UiError> {
        let split = self.spawn_unique(world, name)?;
        world.set_copy(split, WidgetKind::DockSplit)?;
        world.set_copy(split, DockSplit { dir, ratio: ratio.clamp(0.12, 0.88), first, second })?;
        world.relate::<ChildOf>(first, split)?;
        world.relate::<ChildOf>(second, split)?;
        Ok(split)
    }

    /// Return deterministically ordered tabs currently docked in `leaf`.
    pub fn tabs_of(&self, world: &World<A>, leaf: Entity) -> Result<OwnVec<Entity, A>, UiError> {
        let children = world.children_of(leaf)?;
        let mut pairs: OwnVec<(u32, Entity), A> = OwnVec::new_in(world.alloc());
        for &e in children.as_slice() {
            if world.get::<DockTab>(e)?.is_some() {
                let ord = world.get::<ChildOrder>(e)?.map_or(u32::MAX, |o| o.0);
                pairs.try_push((ord, e)).map_err(|r| UiError::Alloc(r.why))?;
            }
        }
        sort_pairs(pairs.as_mut_slice());
        let mut out = OwnVec::new_in(world.alloc());
        for &(_, e) in pairs.as_slice() {
            out.try_push(e).map_err(|r| UiError::Alloc(r.why))?;
        }
        Ok(out)
    }

    /// Return the currently active tab entity in `leaf`.
    pub fn active_tab_of(&self, world: &World<A>, leaf: Entity) -> Result<Option<Entity>, UiError> {
        let tabs = self.tabs_of(world, leaf)?;
        if tabs.is_empty() {
            return Ok(None);
        }
        let active = world
            .get::<DockLeaf>(leaf)?
            .map_or(0, |l| l.active_tab)
            .min(tabs.len() - 1);
        Ok(tabs.get(active).copied())
    }

    /// Select tab index `tab_idx` in `leaf`.
    pub fn select_tab(
        &mut self,
        world: &mut World<A>,
        leaf: Entity,
        tab_idx: usize,
    ) -> Result<(), UiError> {
        let tabs = self.tabs_of(world, leaf)?;
        if tabs.is_empty() {
            return Ok(());
        }
        let clamped = tab_idx.min(tabs.len() - 1);
        world.set_copy(leaf, DockLeaf { active_tab: clamped })?;
        Ok(())
    }

    /// Toggle maximize on `tab` (or restore if already maximized).
    pub fn toggle_maximize(&mut self, tab: Entity) {
        if self.maximized_tab == Some(tab) {
            self.maximized_tab = None;
        } else {
            self.maximized_tab = Some(tab);
        }
    }

    /// Recompute all leaf & splitter rects iteratively inside `bounds` (ZERO recursion).
    pub fn compute_layout(&mut self, world: &World<A>, bounds: Rect) -> Result<(), UiError> {
        self.leaves.clear();
        self.splitters.clear();

        if let Some(max_tab) = self.maximized_tab {
            if world.is_alive(max_tab) {
                if let Some(leaf) = world.parent_of(max_tab)? {
                    let header = Rect::new(bounds.x, bounds.y, bounds.w, DOCK_HEADER_H.min(bounds.h));
                    let body = Rect::new(
                        bounds.x,
                        bounds.y + header.h,
                        bounds.w,
                        (bounds.h - header.h).max(0.0),
                    );
                    let tabs = self.tabs_of(world, leaf)?;
                    let active = tabs.iter().position(|&t| t == max_tab).unwrap_or(0);
                    self.leaves
                        .try_push(DockLeafRect { leaf, rect: bounds, header, body, active_tab: active })
                        .map_err(|r| UiError::Alloc(r.why))?;
                    return Ok(());
                }
            }
            self.maximized_tab = None;
        }

        let Some(root) = self.root else {
            return Ok(());
        };
        if !world.is_alive(root) {
            return Ok(());
        }

        let mut stack: OwnVec<(Entity, Rect), A> = OwnVec::new_in(world.alloc());
        stack.try_push((root, bounds)).map_err(|r| UiError::Alloc(r.why))?;

        while let Some((node, r)) = stack.pop() {
            if let Some(split) = world.get::<DockSplit>(node)?.copied() {
                let ratio = split.ratio.clamp(0.12, 0.88);
                let (ra, rb, srect) = match split.dir {
                    SplitDir::Horizontal => {
                        let avail = (r.w - DOCK_SPLIT_GAP).max(0.0);
                        let wa = avail * ratio;
                        let wb = (avail - wa).max(0.0);
                        (
                            Rect::new(r.x, r.y, wa, r.h),
                            Rect::new(r.x + wa + DOCK_SPLIT_GAP, r.y, wb, r.h),
                            Rect::new(r.x + wa, r.y, DOCK_SPLIT_GAP, r.h),
                        )
                    }
                    SplitDir::Vertical => {
                        let avail = (r.h - DOCK_SPLIT_GAP).max(0.0);
                        let ha = avail * ratio;
                        let hb = (avail - ha).max(0.0);
                        (
                            Rect::new(r.x, r.y, r.w, ha),
                            Rect::new(r.x, r.y + ha + DOCK_SPLIT_GAP, r.w, hb),
                            Rect::new(r.x, r.y + ha, r.w, DOCK_SPLIT_GAP),
                        )
                    }
                };
                self.splitters
                    .try_push(DockSplitterRect { split: node, dir: split.dir, rect: srect, parent_rect: r })
                    .map_err(|r| UiError::Alloc(r.why))?;
                // Push second first so first pops first (left-to-right / top-to-bottom).
                stack.try_push((split.second, rb)).map_err(|r| UiError::Alloc(r.why))?;
                stack.try_push((split.first, ra)).map_err(|r| UiError::Alloc(r.why))?;
            } else if let Some(leaf_comp) = world.get::<DockLeaf>(node)?.copied() {
                let header = Rect::new(r.x, r.y, r.w, DOCK_HEADER_H.min(r.h));
                let body = Rect::new(r.x, r.y + header.h, r.w, (r.h - header.h).max(0.0));
                self.leaves
                    .try_push(DockLeafRect {
                        leaf: node,
                        rect: r,
                        header,
                        body,
                        active_tab: leaf_comp.active_tab,
                    })
                    .map_err(|r| UiError::Alloc(r.why))?;
            }
        }
        Ok(())
    }

    /// Update a dragged splitter's ratio from pointer coordinates `(px, py)`.
    pub fn drag_splitter(&self, world: &mut World<A>, px: f32, py: f32) -> Result<bool, UiError> {
        let Some(split_ent) = self.dragging_splitter else {
            return Ok(false);
        };
        let Some(sp_layout) = self.splitters.iter().find(|s| s.split == split_ent).copied() else {
            return Ok(false);
        };
        let Some(mut split) = world.get::<DockSplit>(split_ent)?.copied() else {
            return Ok(false);
        };
        let new_ratio = match sp_layout.dir {
            SplitDir::Horizontal => {
                if sp_layout.parent_rect.w > 1.0 {
                    (px - sp_layout.parent_rect.x) / sp_layout.parent_rect.w
                } else {
                    split.ratio
                }
            }
            SplitDir::Vertical => {
                if sp_layout.parent_rect.h > 1.0 {
                    (py - sp_layout.parent_rect.y) / sp_layout.parent_rect.h
                } else {
                    split.ratio
                }
            }
        }
        .clamp(0.14, 0.86);

        if (new_ratio - split.ratio).abs() > 0.001 {
            split.ratio = new_ratio;
            world.set_copy(split_ent, split)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Hit-test a splitter divider with generous 3px grab margin.
    pub fn hit_splitter(&self, px: f32, py: f32) -> Option<DockSplitterRect> {
        for &s in self.splitters.as_slice() {
            let expanded = Rect::new(s.rect.x - 3.0, s.rect.y - 3.0, s.rect.w + 6.0, s.rect.h + 6.0);
            if expanded.contains(px, py) {
                return Some(s);
            }
        }
        None
    }

    /// Compute the live drop zone & preview rect when hovering `(px, py)` during a tab drag.
    pub fn compute_drop_target(&self, px: f32, py: f32) -> Option<(Entity, DropZone, Rect)> {
        let leaf_r = self.leaves.iter().find(|l| l.rect.contains(px, py)).copied()?;
        let r = leaf_r.rect;
        if r.w <= 1.0 || r.h <= 1.0 {
            return None;
        }
        let fx = (px - r.x) / r.w;
        let fy = (py - r.y) / r.h;
        let zone = if fy < 0.22 {
            DropZone::Top
        } else if fy > 0.78 {
            DropZone::Bottom
        } else if fx < 0.25 {
            DropZone::Left
        } else if fx > 0.75 {
            DropZone::Right
        } else {
            DropZone::Center
        };
        let preview = match zone {
            DropZone::Left => Rect::new(r.x, r.y, r.w * 0.5, r.h),
            DropZone::Right => Rect::new(r.x + r.w * 0.5, r.y, r.w * 0.5, r.h),
            DropZone::Top => Rect::new(r.x, r.y, r.w, r.h * 0.5),
            DropZone::Bottom => Rect::new(r.x, r.y + r.h * 0.5, r.w, r.h * 0.5),
            DropZone::Center => r,
        };
        Some((leaf_r.leaf, zone, preview))
    }

    /// Move `tab` from `source_leaf` onto `target_leaf` at `zone`, creating a split if needed
    /// and pruning empty leaves automatically.
    pub fn drop_tab(
        &mut self,
        world: &mut World<A>,
        tab: Entity,
        source_leaf: Entity,
        target_leaf: Entity,
        zone: DropZone,
    ) -> Result<bool, UiError> {
        if !world.is_alive(tab) || !world.is_alive(target_leaf) {
            return Ok(false);
        }
        let src_tabs = self.tabs_of(world, source_leaf)?;
        if source_leaf == target_leaf && (zone == DropZone::Center || src_tabs.len() <= 1) {
            return Ok(false);
        }

        match zone {
            DropZone::Center => {
                let ord = self.next_order;
                self.next_order = self.next_order.saturating_add(1);
                world.set_copy(tab, ChildOrder(ord))?;
                world.relate::<DockedIn>(tab, target_leaf)?;
                world.relate::<ChildOf>(tab, target_leaf)?;
                let new_tabs = self.tabs_of(world, target_leaf)?;
                let active = new_tabs.len().saturating_sub(1);
                world.set_copy(target_leaf, DockLeaf { active_tab: active })?;
            }
            DropZone::Left | DropZone::Right | DropZone::Top | DropZone::Bottom => {
                let new_leaf = self.spawn_leaf(world, "DockLeaf")?;
                let ord = self.next_order;
                self.next_order = self.next_order.saturating_add(1);
                world.set_copy(tab, ChildOrder(ord))?;
                world.relate::<DockedIn>(tab, new_leaf)?;
                world.relate::<ChildOf>(tab, new_leaf)?;

                let parent = world.parent_of(target_leaf)?;
                let (dir, first, second) = match zone {
                    DropZone::Left => (SplitDir::Horizontal, new_leaf, target_leaf),
                    DropZone::Right => (SplitDir::Horizontal, target_leaf, new_leaf),
                    DropZone::Top => (SplitDir::Vertical, new_leaf, target_leaf),
                    DropZone::Bottom => (SplitDir::Vertical, target_leaf, new_leaf),
                    DropZone::Center => (SplitDir::Horizontal, target_leaf, new_leaf),
                };
                let split = self.spawn_split(world, "DockSplit", dir, 0.5, first, second)?;
                if let Some(p) = parent {
                    if let Some(mut psplit) = world.get::<DockSplit>(p)?.copied() {
                        if psplit.first == target_leaf {
                            psplit.first = split;
                        } else if psplit.second == target_leaf {
                            psplit.second = split;
                        }
                        world.set_copy(p, psplit)?;
                        world.relate::<ChildOf>(split, p)?;
                    }
                } else {
                    self.root = Some(split);
                }
            }
        }

        self.prune_empty_leaf(world, source_leaf)?;
        Ok(true)
    }

    fn prune_empty_leaf(&mut self, world: &mut World<A>, leaf: Entity) -> Result<(), UiError> {
        if !world.is_alive(leaf) {
            return Ok(());
        }
        let remaining = self.tabs_of(world, leaf)?;
        if !remaining.is_empty() {
            let cur = world.get::<DockLeaf>(leaf)?.map_or(0, |l| l.active_tab);
            if cur >= remaining.len() {
                world.set_copy(leaf, DockLeaf { active_tab: remaining.len() - 1 })?;
            }
            return Ok(());
        }
        // Empty leaf: promote sibling into grandparent and despawn redundant split + leaf.
        let Some(parent_split) = world.parent_of(leaf)? else {
            return Ok(());
        };
        let Some(ps) = world.get::<DockSplit>(parent_split)?.copied() else {
            return Ok(());
        };
        let sibling = if ps.first == leaf { ps.second } else { ps.first };
        let grandparent = world.parent_of(parent_split)?;

        // Detach sibling before despawning `parent_split` (since `ChildOf` is `OnDeleteCascade`).
        if let Some(gp) = grandparent {
            world.relate::<ChildOf>(sibling, gp)?;
            if let Some(mut gps) = world.get::<DockSplit>(gp)?.copied() {
                if gps.first == parent_split {
                    gps.first = sibling;
                } else if gps.second == parent_split {
                    gps.second = sibling;
                }
                world.set_copy(gp, gps)?;
            }
        } else {
            let _ = world.remove_pair(sibling, world.child_of(), parent_split)?;
            self.root = Some(sibling);
        }
        world.despawn(parent_split)?;
        Ok(())
    }
}

fn sort_pairs(items: &mut [(u32, Entity)]) {
    let mut i = 1usize;
    while i < items.len() {
        let mut j = i;
        while j > 0 {
            let (a, b) = match (items.get(j - 1), items.get(j)) {
                (Some(x), Some(y)) => (x.0, y.0),
                _ => break,
            };
            if a <= b {
                break;
            }
            items.swap(j - 1, j);
            j -= 1;
        }
        i += 1;
    }
}
