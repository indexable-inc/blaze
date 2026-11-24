//! Deterministic zero-recursion two-pass ECS Layout Engine.
//!
//! Traverses the `ChildOf` hierarchy ordered by [`ChildOrder`] using an explicit stack:
//! - **Pass 1 (bottom-up):** measures intrinsic `(min_w, min_h)` for `Auto`-sized widgets.
//! - **Pass 2 (top-down):** distributes `Fixed`, `Auto`, and `Flex(weight)` space along
//!   `Row` / `Column` / `Stack` containers, applies `padding`, `gap`, `align`, `justify`, and
//!   [`ScrollOffset`], registers rounded GPU clip regions in [`DrawList`], and writes
//!   [`ComputedRect`] back onto every visible UI entity.

use core::alloc::Allocator;

use blaze_ecs::{Entity, World};
use blaze_own::Vec as OwnVec;

use crate::components::{
    ChildOrder, ComputedRect, Hidden, IntrinsicSize, LabelText, ScrollOffset, SubText,
    TextInputState, WidgetKind,
};
use crate::draw::DrawList;
use crate::error::UiError;
use crate::style::{Align, Direction, Justify, LayoutStyle, PaintStyle, Sizing};
use crate::types::{Rect, Vec2};

/// Flattened, deterministically ordered node in the current frame's layout pass.
#[derive(Clone, Copy, Debug)]
pub struct LayoutNode {
    pub entity: Entity,
    pub parent_idx: Option<usize>,
    pub first_child: usize,
    pub child_count: usize,
    pub style: LayoutStyle,
    pub intrinsic: Vec2,
    pub rect: Rect,
    pub content_rect: Rect,
    pub clip_id: u32,
    pub child_clip_id: u32,
    pub layer: u8,
}

/// Reusable scratch buffers for zero-churn per-frame layout computation.
pub struct LayoutEngine<A: Allocator + Copy> {
    pub nodes: OwnVec<LayoutNode, A>,
    children_buf: OwnVec<usize, A>,
    /// Entities in deterministic paint order (base layer first, then overlay layer).
    pub paint_order: OwnVec<Entity, A>,
    /// Map from node index -> slice range in `children_buf` populated during BFS build.
    order_scratch: OwnVec<(u32, Entity), A>,
}

impl<A: Allocator + Copy> LayoutEngine<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            nodes: OwnVec::new_in(alloc),
            children_buf: OwnVec::new_in(alloc),
            paint_order: OwnVec::new_in(alloc),
            order_scratch: OwnVec::new_in(alloc),
        }
    }

    /// Run two-pass layout rooted at `root` inside `viewport`, populating GPU clips in `draw`,
    /// writing [`ComputedRect`] components back into `world`, and returning the number of visited
    /// layout nodes.
    pub fn compute(
        &mut self,
        world: &mut World<A>,
        root: Entity,
        viewport: Rect,
        draw: &mut DrawList<A>,
    ) -> Result<u32, UiError> {
        self.compute_with_clip(world, root, viewport, 0, draw)
    }

    /// Run localized two-pass layout on the subtree rooted at layout-boundary `root` inside
    /// `bounds` inheriting `root_clip_id`. Leaves all outside entities untouched!
    pub fn compute_with_clip(
        &mut self,
        world: &mut World<A>,
        root: Entity,
        bounds: Rect,
        root_clip_id: u32,
        draw: &mut DrawList<A>,
    ) -> Result<u32, UiError> {
        self.nodes.clear();
        self.children_buf.clear();
        self.paint_order.clear();

        if !world.is_alive(root) || is_hidden(world, root) {
            return Ok(0);
        }

        // Phase 0: BFS-style iterative tree flattening using O(1) `world.children_slice`.
        let root_style = get_layout_style(world, root);
        self.nodes
            .try_push(LayoutNode {
                entity: root,
                parent_idx: None,
                first_child: 0,
                child_count: 0,
                style: root_style,
                intrinsic: Vec2::ZERO,
                rect: bounds,
                content_rect: bounds.inset(root_style.padding),
                clip_id: root_clip_id,
                child_clip_id: root_clip_id,
                layer: if root_style.overlay { 1 } else { 0 },
            })
            .map_err(|r| UiError::Alloc(r.why))?;

        let mut cursor: usize = 0;
        while cursor < self.nodes.len() {
            let (parent_entity, parent_layer) = match self.nodes.get(cursor) {
                Some(n) => (n.entity, n.layer),
                None => break,
            };

            self.order_scratch.clear();
            let mut needs_sort = false;
            let mut prev_ord: u32 = 0;
            for &ch in world.children_slice(parent_entity) {
                if !world.is_alive(ch) || is_hidden(world, ch) {
                    continue;
                }
                let ord = world.get::<ChildOrder>(ch)?.map_or(u32::MAX, |o| o.0);
                if ord < prev_ord {
                    needs_sort = true;
                }
                prev_ord = ord;
                self.order_scratch.try_push((ord, ch)).map_err(|r| UiError::Alloc(r.why))?;
            }
            if needs_sort {
                sort_order_pairs(self.order_scratch.as_mut_slice());
            }

            let first_child = self.children_buf.len();
            let count = self.order_scratch.len();
            for &( _, ch ) in self.order_scratch.as_slice() {
                let ch_idx = self.nodes.len();
                let st = get_layout_style(world, ch);
                let layer = if st.overlay || parent_layer > 0 { 1 } else { 0 };
                self.nodes
                    .try_push(LayoutNode {
                        entity: ch,
                        parent_idx: Some(cursor),
                        first_child: 0,
                        child_count: 0,
                        style: st,
                        intrinsic: Vec2::ZERO,
                        rect: Rect::ZERO,
                        content_rect: Rect::ZERO,
                        clip_id: 0,
                        child_clip_id: 0,
                        layer,
                    })
                    .map_err(|r| UiError::Alloc(r.why))?;
                self.children_buf.try_push(ch_idx).map_err(|r| UiError::Alloc(r.why))?;
            }
            if let Some(p) = self.nodes.get_mut(cursor) {
                p.first_child = first_child;
                p.child_count = count;
            }
            cursor += 1;
        }

        // Phase 1: Bottom-up intrinsic measurement (reverse BFS guarantees children before parent).
        let mut idx = self.nodes.len();
        while idx > 0 {
            idx -= 1;
            let (entity, style, first_child, child_count) = match self.nodes.get(idx) {
                Some(n) => (n.entity, n.style, n.first_child, n.child_count),
                None => continue,
            };

            let leaf_size = measure_leaf_intrinsic(world, entity, draw);
            let mut content_w = leaf_size.x;
            let mut content_h = leaf_size.y;

            if child_count > 0 {
                let mut sum_w: f32 = 0.0;
                let mut max_w: f32 = 0.0;
                let mut sum_h: f32 = 0.0;
                let mut max_h: f32 = 0.0;
                let mut flow_count: usize = 0;

                for ci in 0..child_count {
                    let Some(&c_idx) = self.children_buf.get(first_child + ci) else { continue };
                    let Some(cn) = self.nodes.get(c_idx) else { continue };
                    if cn.style.abs_pos.is_some() {
                        continue;
                    }
                    flow_count += 1;
                    sum_w += cn.intrinsic.x;
                    max_w = max_w.max(cn.intrinsic.x);
                    sum_h += cn.intrinsic.y;
                    max_h = max_h.max(cn.intrinsic.y);
                }

                let gaps = if flow_count > 1 {
                    let g_cnt = u16::try_from(flow_count - 1).map_or(0.0, f32::from);
                    style.gap * g_cnt
                } else {
                    0.0
                };

                match style.dir {
                    Direction::Row => {
                        content_w = content_w.max(sum_w + gaps);
                        content_h = content_h.max(max_h);
                    }
                    Direction::Column => {
                        content_w = content_w.max(max_w);
                        content_h = content_h.max(sum_h + gaps);
                    }
                    Direction::Stack => {
                        content_w = content_w.max(max_w);
                        content_h = content_h.max(max_h);
                    }
                }
            }

            let raw_w = match style.width {
                Sizing::Fixed(v) => v,
                Sizing::Auto | Sizing::Flex(_) => content_w + style.padding.horizontal(),
            };
            let raw_h = match style.height {
                Sizing::Fixed(v) => v,
                Sizing::Auto | Sizing::Flex(_) => content_h + style.padding.vertical(),
            };
            let clamped = Vec2::new(
                raw_w.clamp(style.min_w, style.max_w),
                raw_h.clamp(style.min_h, style.max_h),
            );
            if let Some(n) = self.nodes.get_mut(idx) {
                n.intrinsic = clamped;
            }
        }

        // Phase 2: Top-down rect, scroll, and rounded GPU clip assignment.
        for i in 0..self.nodes.len() {
            let (entity, style, rect, clip_id, first_child, child_count) = match self.nodes.get(i) {
                Some(n) => (n.entity, n.style, n.rect, n.clip_id, n.first_child, n.child_count),
                None => continue,
            };

            let content_rect = rect.inset(style.padding);
            let child_clip_id = if style.clip {
                let rad = world
                    .get_inherited::<PaintStyle>(entity)?
                    .map_or(0.0, |p| p.radius);
                draw.add_clip(clip_id, rect, rad)?
            } else {
                clip_id
            };

            if let Some(n) = self.nodes.get_mut(i) {
                n.content_rect = content_rect;
                n.child_clip_id = child_clip_id;
            }

            if child_count == 0 {
                continue;
            }

            let scroll = world
                .get::<ScrollOffset>(entity)?
                .copied()
                .unwrap_or_default();

            let mut fixed_main: f32 = 0.0;
            let mut total_flex: f32 = 0.0;
            let mut flow_count: usize = 0;

            for ci in 0..child_count {
                let Some(&c_idx) = self.children_buf.get(first_child + ci) else { continue };
                let Some(cn) = self.nodes.get(c_idx) else { continue };
                if cn.style.abs_pos.is_some() {
                    continue;
                }
                flow_count += 1;
                let main_sizing = match style.dir {
                    Direction::Row => cn.style.width,
                    Direction::Column => cn.style.height,
                    Direction::Stack => Sizing::Auto,
                };
                match main_sizing {
                    Sizing::Flex(w) => total_flex += w.max(0.0),
                    Sizing::Fixed(v) => fixed_main += v,
                    Sizing::Auto => {
                        fixed_main += match style.dir {
                            Direction::Row => cn.intrinsic.x,
                            Direction::Column => cn.intrinsic.y,
                            Direction::Stack => 0.0,
                        };
                    }
                }
            }

            let gaps = if flow_count > 1 {
                let g_cnt = u16::try_from(flow_count - 1).map_or(0.0, f32::from);
                style.gap * g_cnt
            } else {
                0.0
            };

            let avail_main = match style.dir {
                Direction::Row => content_rect.w,
                Direction::Column => content_rect.h,
                Direction::Stack => content_rect.h,
            };
            let remain_main = (avail_main - fixed_main - gaps).max(0.0);

            // Compute justification offset / extra spacing when no flex absorbs remaining space.
            let extra_space = if total_flex <= 0.0001 { remain_main } else { 0.0 };
            let (mut cursor_main, step_gap) = match style.justify {
                Justify::Start => (0.0, style.gap),
                Justify::Center => (extra_space * 0.5, style.gap),
                Justify::End => (extra_space, style.gap),
                Justify::SpaceBetween => {
                    if flow_count > 1 {
                        let denom = u16::try_from(flow_count - 1).map_or(1.0, f32::from);
                        (0.0, style.gap + extra_space / denom)
                    } else {
                        (0.0, style.gap)
                    }
                }
            };

            let mut total_content_w: f32 = 0.0;
            let mut total_content_h: f32 = 0.0;

            for ci in 0..child_count {
                let Some(&c_idx) = self.children_buf.get(first_child + ci) else { continue };
                let (c_style, c_intr) = match self.nodes.get(c_idx) {
                    Some(cn) => (cn.style, cn.intrinsic),
                    None => continue,
                };

                if let Some(abs) = c_style.abs_pos {
                    let w = match c_style.width {
                        Sizing::Fixed(v) => v,
                        Sizing::Flex(_) => content_rect.w,
                        Sizing::Auto => c_intr.x,
                    }
                    .clamp(c_style.min_w, c_style.max_w);
                    let h = match c_style.height {
                        Sizing::Fixed(v) => v,
                        Sizing::Flex(_) => content_rect.h,
                        Sizing::Auto => c_intr.y,
                    }
                    .clamp(c_style.min_h, c_style.max_h);
                    if let Some(cn) = self.nodes.get_mut(c_idx) {
                        cn.rect = Rect::new(rect.x + abs.x, rect.y + abs.y, w, h);
                        cn.clip_id = if c_style.overlay { 0 } else { child_clip_id };
                    }
                    continue;
                }

                let child_rect = match style.dir {
                    Direction::Row => {
                        let w = match c_style.width {
                            Sizing::Fixed(v) => v,
                            Sizing::Auto => c_intr.x,
                            Sizing::Flex(weight) => {
                                if total_flex > 0.0001 {
                                    remain_main * (weight / total_flex)
                                } else {
                                    0.0
                                }
                            }
                        }
                        .clamp(c_style.min_w, c_style.max_w);

                        let h = match c_style.height {
                            Sizing::Fixed(v) => v,
                            Sizing::Flex(_) => content_rect.h,
                            Sizing::Auto => {
                                if matches!(style.align, Align::Stretch) {
                                    content_rect.h
                                } else {
                                    c_intr.y
                                }
                            }
                        }
                        .clamp(c_style.min_h, c_style.max_h);

                        let y_off = match style.align {
                            Align::Start | Align::Stretch => 0.0,
                            Align::Center => (content_rect.h - h) * 0.5,
                            Align::End => content_rect.h - h,
                        };
                        let r = Rect::new(
                            content_rect.x + cursor_main - scroll.x,
                            content_rect.y + y_off - scroll.y,
                            w,
                            h,
                        );
                        cursor_main += w + step_gap;
                        total_content_w = cursor_main;
                        total_content_h = total_content_h.max(h);
                        r
                    }
                    Direction::Column => {
                        let h = match c_style.height {
                            Sizing::Fixed(v) => v,
                            Sizing::Auto => c_intr.y,
                            Sizing::Flex(weight) => {
                                if total_flex > 0.0001 {
                                    remain_main * (weight / total_flex)
                                } else {
                                    0.0
                                }
                            }
                        }
                        .clamp(c_style.min_h, c_style.max_h);

                        let w = match c_style.width {
                            Sizing::Fixed(v) => v,
                            Sizing::Flex(_) => content_rect.w,
                            Sizing::Auto => {
                                if matches!(style.align, Align::Stretch) {
                                    content_rect.w
                                } else {
                                    c_intr.x
                                }
                            }
                        }
                        .clamp(c_style.min_w, c_style.max_w);

                        let x_off = match style.align {
                            Align::Start | Align::Stretch => 0.0,
                            Align::Center => (content_rect.w - w) * 0.5,
                            Align::End => content_rect.w - w,
                        };
                        let r = Rect::new(
                            content_rect.x + x_off - scroll.x,
                            content_rect.y + cursor_main - scroll.y,
                            w,
                            h,
                        );
                        cursor_main += h + step_gap;
                        total_content_w = total_content_w.max(w);
                        total_content_h = cursor_main;
                        r
                    }
                    Direction::Stack => {
                        let w = match c_style.width {
                            Sizing::Fixed(v) => v,
                            Sizing::Flex(_) => content_rect.w,
                            Sizing::Auto => {
                                if matches!(style.align, Align::Stretch) {
                                    content_rect.w
                                } else {
                                    c_intr.x
                                }
                            }
                        }
                        .clamp(c_style.min_w, c_style.max_w);
                        let h = match c_style.height {
                            Sizing::Fixed(v) => v,
                            Sizing::Flex(_) => content_rect.h,
                            Sizing::Auto => {
                                if matches!(style.align, Align::Stretch) {
                                    content_rect.h
                                } else {
                                    c_intr.y
                                }
                            }
                        }
                        .clamp(c_style.min_h, c_style.max_h);
                        total_content_w = total_content_w.max(w);
                        total_content_h = total_content_h.max(h);
                        let x_off = match style.align {
                            Align::Start | Align::Stretch => 0.0,
                            Align::Center => (content_rect.w - w) * 0.5,
                            Align::End => content_rect.w - w,
                        };
                        let y_off = match style.justify {
                            Justify::Start | Justify::SpaceBetween => 0.0,
                            Justify::Center => (content_rect.h - h) * 0.5,
                            Justify::End => content_rect.h - h,
                        };
                        Rect::new(content_rect.x + x_off, content_rect.y + y_off, w, h)
                    }
                };

                if let Some(cn) = self.nodes.get_mut(c_idx) {
                    cn.rect = child_rect;
                    cn.clip_id = if c_style.overlay { 0 } else { child_clip_id };
                }
            }

            if style.scrollable {
                let mut updated = scroll;
                updated.content_w = total_content_w;
                updated.content_h = total_content_h;
                let max_y = (updated.content_h - content_rect.h).max(0.0);
                let max_x = (updated.content_w - content_rect.w).max(0.0);
                updated.y = updated.y.clamp(0.0, max_y);
                updated.x = updated.x.clamp(0.0, max_x);
                world.set_copy(entity, updated)?;
            }
        }

        // Phase 3: Build DFS pre-order paint list (base layer then overlay) and write `ComputedRect`.
        let mut dfs_stack: OwnVec<usize, A> = OwnVec::new_in(world.alloc());
        let mut paint_seq: u32 = 0;

        for pass_layer in 0u8..=1u8 {
            dfs_stack.clear();
            dfs_stack.try_push(0).map_err(|r| UiError::Alloc(r.why))?;
            while let Some(ni) = dfs_stack.pop() {
                let Some(node) = self.nodes.get(ni).copied() else { continue };
                if node.layer == pass_layer {
                    self.paint_order
                        .try_push(node.entity)
                        .map_err(|r| UiError::Alloc(r.why))?;
                    world.set_copy(
                        node.entity,
                        ComputedRect {
                            rect: node.rect,
                            content_rect: node.content_rect,
                            clip_id: node.clip_id,
                            child_clip_id: node.child_clip_id,
                            layer: node.layer,
                            paint_order: paint_seq,
                        },
                    )?;
                    paint_seq = paint_seq.saturating_add(1);
                }
                // Push children in reverse so lower `ChildOrder` paints first.
                let mut rev = node.child_count;
                while rev > 0 {
                    rev -= 1;
                    if let Some(&c_idx) = self.children_buf.get(node.first_child + rev) {
                        dfs_stack.try_push(c_idx).map_err(|r| UiError::Alloc(r.why))?;
                    }
                }
            }
        }

        Ok(u32::try_from(self.nodes.len()).unwrap_or(u32::MAX))
    }
}

/// Returns `true` when `style` establishes a layout containment boundary whose outer box cannot
/// be affected by descendant intrinsic sizes.
#[inline]
pub const fn is_layout_boundary(style: LayoutStyle) -> bool {
    (matches!(style.width, Sizing::Fixed(_)) && matches!(style.height, Sizing::Fixed(_)))
        || style.scrollable
        || style.clip
}

fn is_hidden<A: Allocator + Copy + 'static>(world: &World<A>, entity: Entity) -> bool {
    world.has_tag_typed::<Hidden>(entity).unwrap_or(false)
}

fn get_layout_style<A: Allocator + Copy + 'static>(world: &World<A>, entity: Entity) -> LayoutStyle {
    world
        .get_inherited::<LayoutStyle>(entity)
        .ok()
        .flatten()
        .copied()
        .unwrap_or_default()
}

fn measure_leaf_intrinsic<A: Allocator + Copy + 'static>(
    world: &World<A>,
    entity: Entity,
    draw: &DrawList<A>,
) -> Vec2 {
    if let Ok(Some(intr)) = world.get::<IntrinsicSize>(entity) {
        return intr.0;
    }
    let kind = world
        .get::<WidgetKind>(entity)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(WidgetKind::Container);
    let font_px = world
        .get_inherited::<PaintStyle>(entity)
        .ok()
        .flatten()
        .map_or(12.5, |p| p.text_size);
    let label_w = world
        .get::<LabelText<A>>(entity)
        .ok()
        .flatten()
        .map_or(0.0, |l| draw.measure_text(l.0.as_str(), font_px));
    let sub_w = world
        .get::<SubText<A>>(entity)
        .ok()
        .flatten()
        .map_or(0.0, |s| draw.measure_text(s.0.as_str(), font_px * 0.92));

    match kind {
        WidgetKind::Label => Vec2::new(label_w, font_px * 1.35),
        WidgetKind::Button | WidgetKind::DockTab => {
            Vec2::new(label_w + sub_w + 18.0, (font_px * 1.4).max(24.0))
        }
        WidgetKind::Badge => Vec2::new(label_w + 12.0, (font_px * 1.25).max(18.0)),
        WidgetKind::Toggle => Vec2::new(label_w + 44.0, 24.0),
        WidgetKind::Slider => Vec2::new((label_w + 120.0).max(160.0), 26.0),
        WidgetKind::TextInput => {
            let txt_w = world
                .get::<TextInputState<A>>(entity)
                .ok()
                .flatten()
                .map_or(80.0, |s| {
                    let display = if s.text.is_empty() { s.placeholder.as_str() } else { s.text.as_str() };
                    draw.measure_text(display, font_px).max(80.0)
                });
            Vec2::new(txt_w + 16.0, 26.0)
        }
        WidgetKind::TreeRow => Vec2::new(label_w + sub_w + 28.0, 22.0),
        WidgetKind::PropertyRow => Vec2::new(label_w + sub_w + 24.0, 24.0),
        WidgetKind::Tooltip => Vec2::new(label_w + 18.0, (font_px * 1.5).max(24.0)),
        _ => Vec2::ZERO,
    }
}

fn sort_order_pairs(items: &mut [(u32, Entity)]) {
    let mut i = 1usize;
    while i < items.len() {
        let mut j = i;
        while j > 0 {
            let (prev_ord, curr_ord) = match (items.get(j - 1), items.get(j)) {
                (Some(a), Some(b)) => (a.0, b.0),
                _ => break,
            };
            if prev_ord <= curr_ord {
                break;
            }
            items.swap(j - 1, j);
            j -= 1;
        }
        i += 1;
    }
}
