//! Constant-memory [`VirtualList`] for 100,000+ rows with fixed or variable row heights.
//!
//! - Uses a Fenwick (Binary Indexed) tree over row heights so both `offset_for_row(i)` and
//!   `row_at_offset(scroll_y)` run in $O(\log N)$ (and $O(1)$ when `fixed_row_height` is used).
//! - Maintains a fixed pool of `visible_rows + overscan` ECS row entities (e.g. 24–40 entities
//!   total even for 100,000 items), recycling them via equality-guarded updates with zero entity
//!   spawn/despawn churn while scrolling!

use core::alloc::Allocator;

use blaze_ecs::Entity;
use blaze_own::Vec as OwnVec;

use crate::components::{ActionTag, Hidden, TreeRowState};
use crate::error::UiError;
use crate::ui::UiContext;

/// Visible slice computed by [`VirtualList::visible_range`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VisibleRange {
    pub first_row: usize,
    pub count: usize,
    pub first_row_y: f32,
    pub total_height: f32,
}

/// High-scale virtualized list controller backed by a fixed pool of reusable ECS row entities.
pub struct VirtualList<A: Allocator + Copy + 'static> {
    /// Total logical item count (e.g. 100,000).
    total_items: usize,
    /// Uniform row height when variable heights are not enabled.
    default_row_h: f32,
    /// Optional per-row heights + 1-indexed Fenwick prefix-sum tree for variable heights.
    heights: OwnVec<f32, A>,
    fenwick: OwnVec<f32, A>,
    /// Current vertical scroll offset in CSS pixels.
    pub scroll_y: f32,
    /// Fixed pool of reusable ECS row entities (`pool.len() ≈ viewport_rows + 4`).
    pub pool: OwnVec<Entity, A>,
    /// Last bound logical item index per pool slot (skips re-binding unchanged rows!).
    bound_item: OwnVec<Option<usize>, A>,
}

impl<A: Allocator + Copy + 'static> VirtualList<A> {
    /// Create a fixed-height virtual list over `total_items` items (zero per-item allocation!).
    pub fn new_fixed_in(total_items: usize, row_height: f32, alloc: A) -> Self {
        Self {
            total_items,
            default_row_h: row_height.max(8.0),
            heights: OwnVec::new_in(alloc),
            fenwick: OwnVec::new_in(alloc),
            scroll_y: 0.0,
            pool: OwnVec::new_in(alloc),
            bound_item: OwnVec::new_in(alloc),
        }
    }

    /// Create a variable-height virtual list backed by an $O(\log N)$ Fenwick prefix-sum tree.
    pub fn new_variable_in(row_heights: &[f32], alloc: A) -> Result<Self, UiError> {
        let n = row_heights.len();
        let mut heights = OwnVec::try_with_capacity_in(n, alloc)?;
        let mut fenwick = OwnVec::try_with_capacity_in(n.saturating_add(1), alloc)?;
        fenwick.try_push(0.0).map_err(|r| UiError::Alloc(r.why))?;
        for &h in row_heights {
            let clamped = h.max(8.0);
            heights.try_push(clamped).map_err(|r| UiError::Alloc(r.why))?;
            fenwick.try_push(0.0).map_err(|r| UiError::Alloc(r.why))?;
        }
        let mut list = Self {
            total_items: n,
            default_row_h: 22.0,
            heights,
            fenwick,
            scroll_y: 0.0,
            pool: OwnVec::new_in(alloc),
            bound_item: OwnVec::new_in(alloc),
        };
        for i in 0..n {
            let h = list.heights.get(i).copied().unwrap_or(22.0);
            list.fenwick_add(i + 1, h);
        }
        Ok(list)
    }

    #[inline]
    pub const fn total_items(&self) -> usize {
        self.total_items
    }

    #[inline]
    pub fn is_variable(&self) -> bool {
        !self.heights.is_empty()
    }

    /// Total scrollable content height in CSS pixels.
    pub fn total_height(&self) -> f32 {
        if self.is_variable() {
            self.fenwick_sum(self.heights.len())
        } else {
            usize_to_f32(self.total_items) * self.default_row_h
        }
    }

    /// Update a single variable-height item in $O(\log N)$.
    pub fn set_row_height(&mut self, index: usize, new_height: f32) {
        let clamped = new_height.max(8.0);
        if let Some(slot) = self.heights.get_mut(index) {
            let delta = clamped - *slot;
            if delta.abs() > 1e-4 {
                *slot = clamped;
                self.fenwick_add(index + 1, delta);
            }
        }
    }

    /// Y offset of logical item `index` (`O(1)` fixed, `O(log N)` variable).
    pub fn offset_of(&self, index: usize) -> f32 {
        let clamped = index.min(self.total_items);
        if self.is_variable() {
            self.fenwick_sum(clamped)
        } else {
            usize_to_f32(clamped) * self.default_row_h
        }
    }

    /// Find the first visible row at vertical offset `scroll_y` (`O(1)` fixed, `O(log N)` variable).
    pub fn row_at_offset(&self, scroll_y: f32) -> usize {
        if self.total_items == 0 || scroll_y <= 0.0 {
            return 0;
        }
        if !self.is_variable() {
            let idx = f32_floor_usize(scroll_y / self.default_row_h);
            return idx.min(self.total_items.saturating_sub(1));
        }
        // Fenwick binary lifting in O(log N)!
        let n = self.heights.len();
        let mut idx: usize = 0;
        let mut acc: f32 = 0.0;
        let mut bit: usize = 1;
        while (bit << 1) <= n {
            bit <<= 1;
        }
        while bit > 0 {
            let next = idx + bit;
            if next <= n {
                let v = self.fenwick.get(next).copied().unwrap_or(0.0);
                if acc + v <= scroll_y {
                    acc += v;
                    idx = next;
                }
            }
            bit >>= 1;
        }
        idx.min(self.total_items.saturating_sub(1))
    }

    /// Compute the exact visible window for `viewport_h` with `overscan` extra rows.
    pub fn visible_range(&self, viewport_h: f32, overscan: usize) -> VisibleRange {
        let total_h = self.total_height();
        if self.total_items == 0 || viewport_h <= 0.0 {
            return VisibleRange { first_row: 0, count: 0, first_row_y: 0.0, total_height: total_h };
        }
        let max_scroll = (total_h - viewport_h).max(0.0);
        let sy = self.scroll_y.clamp(0.0, max_scroll);
        let first_row = self.row_at_offset(sy);
        let first_y = self.offset_of(first_row);
        let mut y = first_y;
        let end_y = sy + viewport_h;
        let mut count = 0usize;
        while first_row + count < self.total_items && y < end_y {
            let rh = if self.is_variable() {
                self.heights.get(first_row + count).copied().unwrap_or(self.default_row_h)
            } else {
                self.default_row_h
            };
            y += rh;
            count += 1;
        }
        let with_over = (count + overscan).min(self.total_items.saturating_sub(first_row));
        VisibleRange { first_row, count: with_over, first_row_y: first_y, total_height: total_h }
    }

    /// Scroll by `delta_y` clamped cleanly to `[0, total_height - viewport_h]`.
    pub fn scroll_by(&mut self, delta_y: f32, viewport_h: f32) -> bool {
        let max_scroll = (self.total_height() - viewport_h).max(0.0);
        let next = (self.scroll_y + delta_y).clamp(0.0, max_scroll);
        if (next - self.scroll_y).abs() > 0.01 {
            self.scroll_y = next;
            true
        } else {
            false
        }
    }

    /// Mount the fixed pool of `pool_size` reusable ECS row entities under `parent` ONCE.
    pub fn mount_pool(
        &mut self,
        ui: &mut UiContext<A>,
        parent: Entity,
        prefix: &str,
        pool_size: usize,
        action_base: u32,
    ) -> Result<(), UiError> {
        self.pool.clear();
        self.bound_item.clear();
        let alloc = ui.alloc();
        let mut name_buf = blaze_own::String::new_in(alloc);
        for i in 0..pool_size {
            name_buf.clear();
            let _ = core::fmt::write(&mut name_buf, format_args!("{prefix}_{i}"));
            let act = action_base.saturating_add(u32::try_from(i).unwrap_or(0));
            let row = ui.spawn_tree_row(
                parent,
                name_buf.as_str(),
                "",
                "",
                TreeRowState { depth: 0, has_children: false, expanded: false, target: None },
                act,
            )?;
            self.pool.try_push(row).map_err(|r| UiError::Alloc(r.why))?;
            self.bound_item.try_push(None).map_err(|r| UiError::Alloc(r.why))?;
        }
        Ok(())
    }

    /// Synchronize the fixed ECS row pool to the currently visible slice using `bind_row(row_idx,
    /// label_out, sub_out)`. Returns how many pooled row entities actually changed text!
    pub fn sync_visible<F>(
        &mut self,
        ui: &mut UiContext<A>,
        viewport_h: f32,
        mut bind_row: F,
    ) -> Result<usize, UiError>
    where
        F: FnMut(usize, &mut blaze_own::String<A>, &mut blaze_own::String<A>),
    {
        let range = self.visible_range(viewport_h, 0);
        let pool_n = self.pool.len();
        let visible_n = range.count.min(pool_n);
        let alloc = ui.alloc();
        let mut lbl_buf = blaze_own::String::new_in(alloc);
        let mut sub_buf = blaze_own::String::new_in(alloc);
        let mut updated: usize = 0;

        for slot_i in 0..pool_n {
            let Some(&row_ent) = self.pool.get(slot_i) else { continue };
            if slot_i < visible_n {
                let item_idx = range.first_row + slot_i;
                if ui.world.has_tag_typed::<Hidden>(row_ent)? {
                    let _ = ui.world.remove_tag_typed::<Hidden>(row_ent)?;
                    ui.mark_layout_dirty(row_ent)?;
                }
                let prev = self.bound_item.get(slot_i).copied().flatten();
                if prev != Some(item_idx) {
                    lbl_buf.clear();
                    sub_buf.clear();
                    bind_row(item_idx, &mut lbl_buf, &mut sub_buf);
                    let c1 = ui.set_label_text(row_ent, lbl_buf.as_str())?;
                    let c2 = ui.set_sub_text(row_ent, sub_buf.as_str())?;
                    let _ = ui
                        .world
                        .set_copy_if_changed(row_ent, ActionTag(u32::try_from(item_idx).unwrap_or(0)))?;
                    if let Some(b) = self.bound_item.get_mut(slot_i) {
                        *b = Some(item_idx);
                    }
                    if c1 || c2 {
                        updated += 1;
                    }
                }
            } else if !ui.world.has_tag_typed::<Hidden>(row_ent)? {
                ui.world.add_tag_typed::<Hidden>(row_ent)?;
                if let Some(b) = self.bound_item.get_mut(slot_i) {
                    *b = None;
                }
                ui.mark_layout_dirty(row_ent)?;
            }
        }
        Ok(updated)
    }

    /// Force re-binding a single logical item `item_idx` if it is currently visible in the pool.
    pub fn invalidate_item(&mut self, item_idx: usize) {
        for slot in self.bound_item.as_mut_slice() {
            if *slot == Some(item_idx) {
                *slot = None;
            }
        }
    }

    fn fenwick_add(&mut self, mut i: usize, delta: f32) {
        let n = self.fenwick.len();
        while i < n {
            if let Some(slot) = self.fenwick.get_mut(i) {
                *slot += delta;
            }
            i += i & i.wrapping_neg();
        }
    }

    fn fenwick_sum(&self, mut i: usize) -> f32 {
        let mut s = 0.0f32;
        let max_i = self.fenwick.len().saturating_sub(1);
        i = i.min(max_i);
        while i > 0 {
            s += self.fenwick.get(i).copied().unwrap_or(0.0);
            i -= i & i.wrapping_neg();
        }
        s
    }
}

#[inline]
fn usize_to_f32(v: usize) -> f32 {
    let clamped = u32::try_from(v).unwrap_or(u32::MAX);
    let hi = u16::try_from(clamped >> 16).unwrap_or(0);
    let lo = u16::try_from(clamped & 0xFFFF).unwrap_or(0);
    f32::from(hi) * 65536.0 + f32::from(lo)
}

fn f32_floor_usize(v: f32) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let fv = gpu::math::floor(v);
    if fv >= 1_000_000_000.0 {
        return 1_000_000_000;
    }
    let mut lo: u32 = 0;
    let mut hi: u32 = 1_000_000_000;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        let mf = f32::from(u16::try_from(mid >> 16).unwrap_or(0)) * 65536.0
            + f32::from(u16::try_from(mid & 0xFFFF).unwrap_or(0));
        if mf <= fv {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    usize::try_from(lo).unwrap_or(0)
}
