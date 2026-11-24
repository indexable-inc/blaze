//! Ergonomic high-level [`UiContext`] owning the relational ECS [`World`], glitch-free push-pull
//! [`ReactiveGraph`], `IsA` [`StylePrefabs`], incremental [`LayoutEngine`], [`DockWorkspace`],
//! [`InputState`], and retained [`DrawList`] with $O(\Delta)$ GPU slab patching.

use core::alloc::Allocator;

use blaze_ecs::{ChildOf, Entity, IsA, Memo, ReactiveGraph, ReadCx, Signal, World};
use blaze_own::{Box as OwnBox, String as OwnString, Vec as OwnVec};

use crate::components::{
    ActionTag, BadgeTone, CanvasBatch, ChildOrder, ComputedRect, Focusable, FrameStats,
    Interactive, LabelText, ScrollOffset, SliderState, SubText, TextInputState, ToggleState,
    TreeRowState, WidgetKind, register_ui_schema,
};
use crate::dock::{DockTab, DockWorkspace};
use crate::draw::DrawList;
use crate::error::UiError;
use crate::extract::{extract_dock_chrome, extract_entities, patch_entity_gpu};
use crate::input::{CursorHint, InputEvent, InputState, UiEvent};
use crate::layout::{LayoutEngine, is_layout_boundary};
use crate::style::{Align, Direction, LayoutStyle, PaintStyle, Sizing, StylePrefabs};
use crate::types::{Edges, Rect, Theme, ThemeKind};

/// Kind of run-once reactive hole bound to an ECS widget entity.
enum ReactiveBinding<A: Allocator + Copy + 'static> {
    Label {
        entity: Entity,
        memo: Memo<OwnString<A>>,
        last_epoch: u64,
    },
    SubText {
        entity: Entity,
        memo: Memo<OwnString<A>>,
        last_epoch: u64,
    },
    Toggle {
        entity: Entity,
        memo: Memo<bool>,
        last_epoch: u64,
    },
    BadgeTone {
        entity: Entity,
        memo: Memo<BadgeTone>,
        last_epoch: u64,
    },
}

/// Complete `#![no_std]` incremental relational ECS UI context.
pub struct UiContext<A: Allocator + Copy + 'static> {
    alloc: A,
    pub world: World<A>,
    /// Glitch-free push-pull reactive signal/memo graph (Solid / Svelte 5 Runes runtime).
    pub reactive: ReactiveGraph<A>,
    pub theme: Theme,
    pub prefabs: StylePrefabs,
    pub root: Entity,
    pub dock: DockWorkspace<A>,
    pub layout: LayoutEngine<A>,
    pub input: InputState<A>,
    pub draw: DrawList<A>,
    scratch_draw: OwnBox<DrawList<A>, A>,
    bindings: OwnVec<ReactiveBinding<A>, A>,
    dirty_layout_roots: OwnVec<Entity, A>,
    dirty_paint: OwnVec<Entity, A>,
    full_layout_dirty: bool,
    full_extract_dirty: bool,
    extract_epoch: u64,
    last_vp: (f32, f32, f32),
    last_dock_bounds: Option<Rect>,
    /// Exact incremental execution stats for the most recently rendered frame.
    pub last_stats: FrameStats,
    next_order: u32,
}

impl<A: Allocator + Copy + 'static> UiContext<A> {
    /// Construct a fresh relational ECS UI context backed by explicit allocator `alloc`.
    pub fn new_in(alloc: A) -> Result<Self, UiError> {
        let mut world = World::new_in(alloc)?;
        register_ui_schema(&mut world)?;
        let theme = Theme::obsidian();
        let prefabs = StylePrefabs::spawn_in(&mut world, &theme)?;

        let root = world.spawn_named("UiRoot")?;
        world.set_copy(root, WidgetKind::Container)?;
        world.set_copy(root, LayoutStyle::column().fill().gap(0.0))?;
        world.relate::<IsA>(root, prefabs.root)?;

        let scratch_draw = OwnBox::try_new_in(DrawList::new_in(alloc), alloc)
            .map_err(|r| UiError::Alloc(r.why))?;

        Ok(Self {
            alloc,
            world,
            reactive: ReactiveGraph::new_in(alloc),
            theme,
            prefabs,
            root,
            dock: DockWorkspace::new_in(alloc),
            layout: LayoutEngine::new_in(alloc),
            input: InputState::new_in(alloc),
            draw: DrawList::new_in(alloc),
            scratch_draw,
            bindings: OwnVec::new_in(alloc),
            dirty_layout_roots: OwnVec::new_in(alloc),
            dirty_paint: OwnVec::new_in(alloc),
            full_layout_dirty: true,
            full_extract_dirty: true,
            extract_epoch: 0,
            last_vp: (0.0, 0.0, 0.0),
            last_dock_bounds: None,
            last_stats: FrameStats::default(),
            next_order: 1,
        })
    }

    #[inline]
    pub fn alloc(&self) -> A {
        self.alloc
    }

    /// Force a full layout & extract on the next frame (e.g. structural tree change).
    #[inline]
    pub fn mark_full_dirty(&mut self) {
        self.full_layout_dirty = true;
        self.full_extract_dirty = true;
    }

    /// Mark a single entity's visuals dirty (`0` layout nodes will run — patches GPU slab in-place!).
    pub fn mark_paint_dirty(&mut self, entity: Entity) -> Result<(), UiError> {
        if !self.dirty_paint.iter().any(|&e| e == entity) {
            self.dirty_paint.try_push(entity).map_err(|r| UiError::Alloc(r.why))?;
        }
        Ok(())
    }

    /// Mark `entity`'s layout dirty, bubbling up `ChildOf` ONLY to the nearest layout containment
    /// boundary (`Sizing::Fixed`, `clip`, `scrollable`, or pane root)!
    pub fn mark_layout_dirty(&mut self, entity: Entity) -> Result<(), UiError> {
        let mut cur = entity;
        let mut steps = 0usize;
        while steps < 64 {
            steps += 1;
            let style = self
                .world
                .get_inherited::<LayoutStyle>(cur)?
                .copied()
                .unwrap_or_default();
            if cur == self.root || is_layout_boundary(style) {
                break;
            }
            match self.world.parent_of(cur)? {
                Some(parent) => cur = parent,
                None => break,
            }
        }
        if cur == self.root {
            self.full_layout_dirty = true;
            self.full_extract_dirty = true;
        } else if !self.dirty_layout_roots.iter().any(|&e| e == cur) {
            self.dirty_layout_roots.try_push(cur).map_err(|r| UiError::Alloc(r.why))?;
        }
        Ok(())
    }

    /// Switch the active [`ThemeKind`] and cascade new [`PaintStyle`]s to every widget via `IsA`.
    pub fn set_theme(&mut self, kind: ThemeKind) -> Result<(), UiError> {
        if self.theme.kind == kind {
            return Ok(());
        }
        self.theme = Theme::from_kind(kind);
        self.prefabs.apply_theme(&mut self.world, &self.theme)?;
        self.full_extract_dirty = true;
        Ok(())
    }

    /// Apply a custom modified [`Theme`] and cascade via `IsA`.
    pub fn apply_custom_theme(&mut self, theme: Theme) -> Result<(), UiError> {
        if self.theme == theme {
            return Ok(());
        }
        self.theme = theme;
        self.prefabs.apply_theme(&mut self.world, &self.theme)?;
        self.full_extract_dirty = true;
        Ok(())
    }

    /// Equality-guarded label update: returns `Ok(false)` in `< 1 µs` with **0 dirty work** if
    /// `text` matches the current `LabelText`; otherwise marks ONLY `entity` `DirtyPaint` when
    /// layout-contained (or nearest boundary `DirtyLayout` if `Auto`-sized).
    pub fn set_label_text(&mut self, entity: Entity, text: &str) -> Result<bool, UiError> {
        if !self.world.is_alive(entity) {
            return Ok(false);
        }
        if let Some(existing) = self.world.get::<LabelText<A>>(entity)? {
            if existing.0.as_str() == text {
                return Ok(false);
            }
        }
        if let Some(existing) = self.world.get_mut::<LabelText<A>>(entity)? {
            existing.0.clear();
            existing.0.try_push_str(text)?;
        } else {
            let mut s = OwnString::new_in(self.alloc);
            s.try_push_str(text)?;
            self.world.set(entity, LabelText(s))?;
        }
        let ls = self
            .world
            .get_inherited::<LayoutStyle>(entity)?
            .copied()
            .unwrap_or_default();
        if matches!(ls.width, Sizing::Auto) {
            self.mark_layout_dirty(entity)?;
        } else {
            self.mark_paint_dirty(entity)?;
        }
        Ok(true)
    }

    /// Equality-guarded secondary/value text update (always layout-contained → `DirtyPaint` only!).
    pub fn set_sub_text(&mut self, entity: Entity, text: &str) -> Result<bool, UiError> {
        if !self.world.is_alive(entity) {
            return Ok(false);
        }
        if let Some(existing) = self.world.get::<SubText<A>>(entity)? {
            if existing.0.as_str() == text {
                return Ok(false);
            }
        }
        if let Some(existing) = self.world.get_mut::<SubText<A>>(entity)? {
            existing.0.clear();
            existing.0.try_push_str(text)?;
        } else {
            let mut s = OwnString::new_in(self.alloc);
            s.try_push_str(text)?;
            self.world.set(entity, SubText(s))?;
        }
        self.mark_paint_dirty(entity)?;
        Ok(true)
    }

    /// Equality-guarded `PaintStyle` override on `entity` → `DirtyPaint` only (`0` layout nodes!).
    pub fn set_paint_style(&mut self, entity: Entity, style: PaintStyle) -> Result<bool, UiError> {
        if !self.world.set_copy_if_changed(entity, style)? {
            return Ok(false);
        }
        self.mark_paint_dirty(entity)?;
        Ok(true)
    }

    /// Equality-guarded `IsA` style prefab update: uses $O(1)$ `world.isa_dependents_slice(prefab)`
    /// to dirty ONLY the exact widgets inheriting from `prefab` (`0` layout nodes unless `text_size` changed!).
    pub fn set_prefab_style(&mut self, prefab: Entity, style: PaintStyle) -> Result<bool, UiError> {
        let prev_text_size = self.world.get::<PaintStyle>(prefab)?.map(|p| p.text_size);
        if !self.world.set_copy_if_changed(prefab, style)? {
            return Ok(false);
        }
        let text_resized = prev_text_size.is_some_and(|s| (s - style.text_size).abs() > 0.01);
        let mut deps = OwnVec::new_in(self.alloc);
        for &dep in self.world.isa_dependents_slice(prefab) {
            deps.try_push(dep).map_err(|r| UiError::Alloc(r.why))?;
        }
        for &dep in deps.as_slice() {
            if text_resized {
                self.mark_layout_dirty(dep)?;
            } else {
                self.mark_paint_dirty(dep)?;
            }
        }
        Ok(true)
    }

    // =========================================================================
    // Run-Once Reactive Holes (Solid / Svelte 5 Runes Style over `ReactiveGraph`)
    // =========================================================================

    /// Create a fine-grained reactive [`Signal<T>`] owned by this UI context.
    #[inline]
    pub fn signal<T: 'static>(&mut self, initial: T) -> Result<Signal<T>, UiError> {
        Ok(self.reactive.signal(initial)?)
    }

    /// Equality-guarded write to a reactive [`Signal<T>`].
    #[inline]
    pub fn set_signal<T: PartialEq + 'static>(
        &mut self,
        sig: Signal<T>,
        value: T,
    ) -> Result<bool, UiError> {
        Ok(self.reactive.set(sig, value)?)
    }

    /// Bind a run-once reactive label hole to `entity`: `compute` runs ONLY when a tracked
    /// signal/memo it reads changes, and writes via equality-guarded [`Self::set_label_text`].
    pub fn bind_label<F>(&mut self, entity: Entity, mut compute: F) -> Result<(), UiError>
    where
        F: FnMut(&mut ReadCx<'_, A>, &mut OwnString<A>) + 'static,
    {
        let alloc = self.alloc;
        let memo = self.reactive.memo(move |cx| {
            let mut s = OwnString::new_in(alloc);
            compute(cx, &mut s);
            s
        })?;
        if let Some(val) = self.reactive.get_memo(memo)? {
            let mut copy = OwnString::new_in(alloc);
            copy.try_push_str(val.as_str())?;
            let _ = self.set_label_text(entity, copy.as_str())?;
        }
        let ep = self.reactive.epoch();
        self.bindings
            .try_push(ReactiveBinding::Label { entity, memo, last_epoch: ep })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    /// Bind a run-once reactive `SubText` hole to `entity`.
    pub fn bind_sub_text<F>(&mut self, entity: Entity, mut compute: F) -> Result<(), UiError>
    where
        F: FnMut(&mut ReadCx<'_, A>, &mut OwnString<A>) + 'static,
    {
        let alloc = self.alloc;
        let memo = self.reactive.memo(move |cx| {
            let mut s = OwnString::new_in(alloc);
            compute(cx, &mut s);
            s
        })?;
        if let Some(val) = self.reactive.get_memo(memo)? {
            let mut copy = OwnString::new_in(alloc);
            copy.try_push_str(val.as_str())?;
            let _ = self.set_sub_text(entity, copy.as_str())?;
        }
        let ep = self.reactive.epoch();
        self.bindings
            .try_push(ReactiveBinding::SubText { entity, memo, last_epoch: ep })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    /// Bind a run-once reactive [`ToggleState`] hole to `entity`.
    pub fn bind_toggle<F>(&mut self, entity: Entity, compute: F) -> Result<(), UiError>
    where
        F: FnMut(&mut ReadCx<'_, A>) -> bool + 'static,
    {
        let memo = self.reactive.memo(compute)?;
        if let Some(&checked) = self.reactive.get_memo(memo)? {
            if self.world.set_copy_if_changed(entity, ToggleState { checked })? {
                self.mark_paint_dirty(entity)?;
            }
        }
        let ep = self.reactive.epoch();
        self.bindings
            .try_push(ReactiveBinding::Toggle { entity, memo, last_epoch: ep })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    /// Bind a run-once reactive [`BadgeTone`] hole to `entity`.
    pub fn bind_badge_tone<F>(&mut self, entity: Entity, compute: F) -> Result<(), UiError>
    where
        F: FnMut(&mut ReadCx<'_, A>) -> BadgeTone + 'static,
    {
        let memo = self.reactive.memo(compute)?;
        if let Some(&tone) = self.reactive.get_memo(memo)? {
            if self.world.set_copy_if_changed(entity, tone)? {
                self.mark_paint_dirty(entity)?;
            }
        }
        let ep = self.reactive.epoch();
        self.bindings
            .try_push(ReactiveBinding::BadgeTone { entity, memo, last_epoch: ep })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    /// Flush all dirty reactive holes into ECS components, returning how many memo evaluations ran.
    pub fn flush_reactive(&mut self) -> Result<u32, UiError> {
        let before = self.reactive.memo_evals;
        let _ = self.reactive.flush()?;
        let cur_epoch = self.reactive.epoch();
        let alloc = self.alloc;
        let mut scratch = OwnString::new_in(alloc);

        let count = self.bindings.len();
        for i in 0..count {
            enum PendingUpdate {
                None,
                Label(Entity),
                SubText(Entity),
                Toggle(Entity, bool),
                Badge(Entity, BadgeTone),
            }
            let upd = match self.bindings.get_mut(i) {
                Some(ReactiveBinding::Label { entity, memo, last_epoch }) => {
                    if *last_epoch == cur_epoch {
                        PendingUpdate::None
                    } else {
                        let e = *entity;
                        let m = *memo;
                        *last_epoch = cur_epoch;
                        if let Some(s) = self.reactive.get_memo(m)? {
                            scratch.clear();
                            scratch.try_push_str(s.as_str())?;
                            PendingUpdate::Label(e)
                        } else {
                            PendingUpdate::None
                        }
                    }
                }
                Some(ReactiveBinding::SubText { entity, memo, last_epoch }) => {
                    if *last_epoch == cur_epoch {
                        PendingUpdate::None
                    } else {
                        let e = *entity;
                        let m = *memo;
                        *last_epoch = cur_epoch;
                        if let Some(s) = self.reactive.get_memo(m)? {
                            scratch.clear();
                            scratch.try_push_str(s.as_str())?;
                            PendingUpdate::SubText(e)
                        } else {
                            PendingUpdate::None
                        }
                    }
                }
                Some(ReactiveBinding::Toggle { entity, memo, last_epoch }) => {
                    if *last_epoch == cur_epoch {
                        PendingUpdate::None
                    } else {
                        let e = *entity;
                        let m = *memo;
                        *last_epoch = cur_epoch;
                        if let Some(&b) = self.reactive.get_memo(m)? {
                            PendingUpdate::Toggle(e, b)
                        } else {
                            PendingUpdate::None
                        }
                    }
                }
                Some(ReactiveBinding::BadgeTone { entity, memo, last_epoch }) => {
                    if *last_epoch == cur_epoch {
                        PendingUpdate::None
                    } else {
                        let e = *entity;
                        let m = *memo;
                        *last_epoch = cur_epoch;
                        if let Some(&t) = self.reactive.get_memo(m)? {
                            PendingUpdate::Badge(e, t)
                        } else {
                            PendingUpdate::None
                        }
                    }
                }
                None => PendingUpdate::None,
            };

            match upd {
                PendingUpdate::None => {}
                PendingUpdate::Label(e) => {
                    let _ = self.set_label_text(e, scratch.as_str())?;
                }
                PendingUpdate::SubText(e) => {
                    let _ = self.set_sub_text(e, scratch.as_str())?;
                }
                PendingUpdate::Toggle(e, checked) => {
                    if self.world.set_copy_if_changed(e, ToggleState { checked })? {
                        self.mark_paint_dirty(e)?;
                    }
                }
                PendingUpdate::Badge(e, tone) => {
                    if self.world.set_copy_if_changed(e, tone)? {
                        self.mark_paint_dirty(e)?;
                    }
                }
            }
        }
        Ok(u32::try_from(self.reactive.memo_evals.saturating_sub(before)).unwrap_or(u32::MAX))
    }

    // =========================================================================
    // Widget Spawners
    // =========================================================================

    #[inline]
    fn next_child_order(&mut self) -> ChildOrder {
        let ord = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        ChildOrder(ord)
    }

    /// Remove all `ChildOf` children of `parent` (cascading despawn cleanly).
    pub fn clear_children(&mut self, parent: Entity) -> Result<(), UiError> {
        let children = self.world.children_of(parent)?;
        if !children.is_empty() {
            for &c in children.as_slice() {
                if self.world.is_alive(c) {
                    self.world.despawn(c)?;
                }
            }
            self.mark_full_dirty();
        }
        Ok(())
    }

    /// Spawn a generic container attached under `parent` via `(ChildOf, parent)`.
    pub fn spawn_container(
        &mut self,
        parent: Entity,
        name: &str,
        kind: WidgetKind,
        layout: LayoutStyle,
        style_prefab: Option<Entity>,
    ) -> Result<Entity, UiError> {
        let ord = self.next_child_order();
        let e = if self.world.entity_by_name(name).is_none() {
            self.world.spawn_named(name)?
        } else {
            let mut uniq = OwnString::new_in(self.alloc);
            let _ = core::fmt::write(&mut uniq, format_args!("{name}#{}", ord.0));
            self.world.spawn_named(uniq.as_str())?
        };
        self.world.set_copy(e, kind)?;
        self.world.set_copy(e, layout)?;
        self.world.set_copy(e, ord)?;
        if layout.scrollable {
            self.world.set_copy(e, ScrollOffset::default())?;
        }
        self.world.relate::<ChildOf>(e, parent)?;
        if let Some(prefab) = style_prefab {
            self.world.relate::<IsA>(e, prefab)?;
        }
        self.mark_full_dirty();
        Ok(e)
    }

    /// Spawn a floating root-free pane container meant to live inside a [`DockWorkspace`] tab.
    pub fn spawn_pane_root(&mut self, name: &str, scrollable: bool) -> Result<Entity, UiError> {
        let e = self.world.spawn_named(name)?;
        self.world.set_copy(e, WidgetKind::Panel)?;
        let mut ls = LayoutStyle::column()
            .fill()
            .pad(Edges::all(8.0))
            .gap(6.0)
            .clip(true);
        if scrollable {
            ls = ls.scrollable(true);
            self.world.set_copy(e, ScrollOffset::default())?;
        }
        self.world.set_copy(e, ls)?;
        self.mark_full_dirty();
        Ok(e)
    }

    /// Spawn a styled card container inheriting `Style::Card` via `IsA`.
    pub fn spawn_card(
        &mut self,
        parent: Entity,
        name: &str,
        layout: LayoutStyle,
    ) -> Result<Entity, UiError> {
        self.spawn_container(parent, name, WidgetKind::Card, layout, Some(self.prefabs.card))
    }

    /// Spawn a toolbar container inheriting `Style::Toolbar` via `IsA`.
    pub fn spawn_toolbar(&mut self, parent: Entity, name: &str, height: f32) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row()
            .width(Sizing::FILL)
            .height(Sizing::Fixed(height))
            .pad(Edges::xy(10.0, 4.0))
            .gap(6.0)
            .align(Align::Center);
        self.spawn_container(parent, name, WidgetKind::Toolbar, ls, Some(self.prefabs.toolbar))
    }

    /// Spawn a text label widget.
    pub fn spawn_label(
        &mut self,
        parent: Entity,
        name: &str,
        text: &str,
        muted: bool,
    ) -> Result<Entity, UiError> {
        let e = self.spawn_container(parent, name, WidgetKind::Label, LayoutStyle::row(), None)?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(text)?;
        self.world.set(e, LabelText(s))?;
        if muted {
            let mut ps = PaintStyle::transparent(&self.theme);
            ps.text = self.theme.ink3;
            ps.text_size = self.theme.font_sm;
            self.world.set_copy(e, ps)?;
        }
        Ok(e)
    }

    /// Spawn a layout-contained fixed-width label (edits NEVER trigger layout!).
    pub fn spawn_fixed_label(
        &mut self,
        parent: Entity,
        name: &str,
        text: &str,
        width: f32,
        height: f32,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().width(Sizing::Fixed(width)).height(Sizing::Fixed(height));
        let e = self.spawn_container(parent, name, WidgetKind::Label, ls, None)?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(text)?;
        self.world.set(e, LabelText(s))?;
        Ok(e)
    }

    /// Spawn a semantic status badge pill.
    pub fn spawn_badge(
        &mut self,
        parent: Entity,
        name: &str,
        text: &str,
        tone: BadgeTone,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().height(Sizing::Fixed(20.0));
        let e = self.spawn_container(parent, name, WidgetKind::Badge, ls, Some(self.prefabs.button))?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(text)?;
        self.world.set(e, LabelText(s))?;
        self.world.set_copy(e, tone)?;
        Ok(e)
    }

    /// Spawn an interactive button inheriting `Style::Button` (or `Style::ButtonPrimary`) via `IsA`.
    pub fn spawn_button(
        &mut self,
        parent: Entity,
        name: &str,
        label: &str,
        action: u32,
        primary: bool,
    ) -> Result<Entity, UiError> {
        let prefab = if primary { self.prefabs.button_primary } else { self.prefabs.button };
        let ls = LayoutStyle::row().height(Sizing::Fixed(24.0));
        let e = self.spawn_container(parent, name, WidgetKind::Button, ls, Some(prefab))?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(label)?;
        self.world.set(e, LabelText(s))?;
        self.world.set_copy(e, ActionTag(action))?;
        self.world.add_tag_typed::<Interactive>(e)?;
        self.world.add_tag_typed::<Focusable>(e)?;
        Ok(e)
    }

    /// Spawn an interactive boolean toggle switch.
    pub fn spawn_toggle(
        &mut self,
        parent: Entity,
        name: &str,
        label: &str,
        checked: bool,
        action: u32,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(26.0));
        let e = self.spawn_container(parent, name, WidgetKind::Toggle, ls, Some(self.prefabs.row))?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(label)?;
        self.world.set(e, LabelText(s))?;
        self.world.set_copy(e, ToggleState { checked })?;
        self.world.set_copy(e, ActionTag(action))?;
        self.world.add_tag_typed::<Interactive>(e)?;
        self.world.add_tag_typed::<Focusable>(e)?;
        Ok(e)
    }

    /// Spawn an interactive drag-to-scrub numeric slider.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_slider(
        &mut self,
        parent: Entity,
        name: &str,
        label: &str,
        state: SliderState,
        action: u32,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(26.0));
        let e = self.spawn_container(parent, name, WidgetKind::Slider, ls, Some(self.prefabs.row))?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(label)?;
        self.world.set(e, LabelText(s))?;
        self.world.set_copy(e, state)?;
        self.world.set_copy(e, ActionTag(action))?;
        self.world.add_tag_typed::<Interactive>(e)?;
        self.world.add_tag_typed::<Focusable>(e)?;
        Ok(e)
    }

    /// Spawn an editable single-line text input inheriting `Style::Input` via `IsA`.
    pub fn spawn_text_input(
        &mut self,
        parent: Entity,
        name: &str,
        initial: &str,
        placeholder: &str,
        action: u32,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(26.0));
        let e = self.spawn_container(parent, name, WidgetKind::TextInput, ls, Some(self.prefabs.input))?;
        let mut text = OwnString::new_in(self.alloc);
        text.try_push_str(initial)?;
        let mut ph = OwnString::new_in(self.alloc);
        ph.try_push_str(placeholder)?;
        let cursor = text.len();
        self.world.set(e, TextInputState { text, placeholder: ph, cursor })?;
        self.world.set_copy(e, ActionTag(action))?;
        self.world.add_tag_typed::<Interactive>(e)?;
        self.world.add_tag_typed::<Focusable>(e)?;
        Ok(e)
    }

    /// Spawn a selectable tree-view row inheriting `Style::Row` via `IsA`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_tree_row(
        &mut self,
        parent: Entity,
        name: &str,
        label: &str,
        sub: &str,
        row_state: TreeRowState,
        action: u32,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(22.0));
        let e = self.spawn_container(parent, name, WidgetKind::TreeRow, ls, Some(self.prefabs.row))?;
        let mut s = OwnString::new_in(self.alloc);
        s.try_push_str(label)?;
        self.world.set(e, LabelText(s))?;
        if !sub.is_empty() {
            let mut sub_s = OwnString::new_in(self.alloc);
            sub_s.try_push_str(sub)?;
            self.world.set(e, SubText(sub_s))?;
        }
        self.world.set_copy(e, row_state)?;
        self.world.set_copy(e, ActionTag(action))?;
        self.world.add_tag_typed::<Interactive>(e)?;
        Ok(e)
    }

    /// Spawn a key-value inspector property row.
    pub fn spawn_property_row(
        &mut self,
        parent: Entity,
        name: &str,
        key: &str,
        val: &str,
    ) -> Result<Entity, UiError> {
        let ls = LayoutStyle::row().width(Sizing::FILL).height(Sizing::Fixed(22.0));
        let e = self.spawn_container(parent, name, WidgetKind::PropertyRow, ls, Some(self.prefabs.row))?;
        let mut k = OwnString::new_in(self.alloc);
        k.try_push_str(key)?;
        let mut v = OwnString::new_in(self.alloc);
        v.try_push_str(val)?;
        self.world.set(e, LabelText(k))?;
        self.world.set(e, SubText(v))?;
        Ok(e)
    }

    /// Spawn a hybrid dense GPU [`CanvasBatch`] entity (flamegraphs, timelines, force-graphs).
    pub fn spawn_canvas(&mut self, parent: Entity, name: &str, action: u32) -> Result<Entity, UiError> {
        let ls = LayoutStyle::column().fill().clip(true);
        let e = self.spawn_container(parent, name, WidgetKind::Canvas, ls, Some(self.prefabs.panel))?;
        self.world.set(e, CanvasBatch::new_in(self.alloc))?;
        self.world.set_copy(e, ActionTag(action))?;
        self.world.add_tag_typed::<Interactive>(e)?;
        Ok(e)
    }

    /// Spawn a horizontal flex spacer (`dir = Row`, `width = Fill`) to push subsequent items right.
    pub fn spawn_spacer(&mut self, parent: Entity) -> Result<Entity, UiError> {
        let ls = LayoutStyle {
            dir: Direction::Row,
            width: Sizing::FILL,
            height: Sizing::Fixed(1.0),
            ..LayoutStyle::default()
        };
        self.spawn_container(parent, "Spacer", WidgetKind::Container, ls, None)
    }

    /// Feed a sans-IO [`InputEvent`] into the UI state machine.
    pub fn handle_input(&mut self, ev: InputEvent) -> Result<(), UiError> {
        self.input.handle_event(
            &mut self.world,
            self.layout.paint_order.as_slice(),
            &self.draw,
            &mut self.dock,
            ev,
        )?;
        let mut idx = 0usize;
        while idx < self.input.dirty_paint.len() {
            if let Some(&e) = self.input.dirty_paint.get(idx) {
                if !self.dirty_paint.as_slice().contains(&e) {
                    self.dirty_paint.try_push(e).map_err(|r| UiError::Alloc(r.why))?;
                }
            }
            idx += 1;
        }
        self.input.dirty_paint.clear();
        if self.input.dirty_layout {
            self.input.dirty_layout = false;
            self.mark_full_dirty();
        }
        Ok(())
    }

    /// Drain all semantic [`UiEvent`]s produced since the last call.
    pub fn drain_events(&mut self, out: &mut OwnVec<UiEvent, A>) -> Result<(), UiError> {
        out.clear();
        for &ev in self.input.events.as_slice() {
            out.try_push(ev).map_err(|r| UiError::Alloc(r.why))?;
        }
        self.input.events.clear();
        Ok(())
    }

    #[inline]
    pub fn cursor(&self) -> CursorHint {
        self.input.cursor
    }

    /// Run one incremental frame:
    /// 1. Flush dirty reactive holes (`Signal`/`Memo` -> ECS components).
    /// 2. If nothing changed (`!full_layout_dirty && dirty_layout_roots.is_empty() && dirty_paint.is_empty()`),
    ///    return immediately in **0 µs** with `0` layout nodes, `0` paint nodes, and `0` GPU bytes!
    /// 3. If only `dirty_paint` (or localized `dirty_layout_roots`), re-layout ONLY dirty boundaries
    ///    and patch ONLY dirty entities' retained [`GpuSlot`]s in place!
    /// 4. Fall back seamlessly to full layout + retained extract on resize, dock change, or slab overflow.
    pub fn render_frame(
        &mut self,
        width: f32,
        height: f32,
        dpr: f32,
        time: f32,
        dock_bounds: Option<Rect>,
    ) -> Result<(), UiError> {
        let reactive_evals = self.flush_reactive()?;

        // Check if a focused TextInput needs blinking caret paint update.
        if let Some(foc) = self.input.focused {
            if self.world.is_alive(foc)
                && self.world.get::<WidgetKind>(foc)? == Some(&WidgetKind::TextInput)
            {
                self.mark_paint_dirty(foc)?;
            }
        }

        let vp_key = (width.max(1.0), height.max(1.0), dpr.max(1.0));
        if vp_key != self.last_vp || dock_bounds != self.last_dock_bounds {
            self.last_vp = vp_key;
            self.last_dock_bounds = dock_bounds;
            self.full_layout_dirty = true;
            self.full_extract_dirty = true;
        }

        // Fast path 0: 0 µs idle frame (nothing dirty at all!).
        if !self.full_layout_dirty
            && !self.full_extract_dirty
            && self.dirty_layout_roots.is_empty()
            && self.dirty_paint.is_empty()
        {
            self.draw.begin_incremental_frame(vp_key.0, vp_key.1, vp_key.2, time);
            self.last_stats = FrameStats {
                full_layout: false,
                full_extract: false,
                layout_nodes_visited: 0,
                paint_nodes_visited: 0,
                rect_patches: 0,
                glyph_patches: 0,
                gpu_bytes_patched: 0,
                reactive_evals,
            };
            return Ok(());
        }

        // Fast path 1: Localized layout boundaries + in-place GPU slab patching!
        if !self.full_layout_dirty && !self.full_extract_dirty {
            self.draw.begin_incremental_frame(vp_key.0, vp_key.1, vp_key.2, time);
            let mut layout_visited: u32 = 0;

            if !self.dirty_layout_roots.is_empty() {
                let mut saved_paint = OwnVec::new_in(self.alloc);
                core::mem::swap(&mut saved_paint, &mut self.layout.paint_order);
                let mut roots = OwnVec::new_in(self.alloc);
                core::mem::swap(&mut roots, &mut self.dirty_layout_roots);

                for &boundary in roots.as_slice() {
                    if !self.world.is_alive(boundary) {
                        continue;
                    }
                    let Some(cr) = self.world.get::<ComputedRect>(boundary)?.copied() else {
                        self.full_layout_dirty = true;
                        self.full_extract_dirty = true;
                        break;
                    };
                    let n = self.layout.compute_with_clip(
                        &mut self.world,
                        boundary,
                        cr.rect,
                        cr.clip_id,
                        &mut self.draw,
                    )?;
                    layout_visited = layout_visited.saturating_add(n);
                    for &e in self.layout.paint_order.as_slice() {
                        if !self.dirty_paint.iter().any(|&d| d == e) {
                            self.dirty_paint.try_push(e).map_err(|r| UiError::Alloc(r.why))?;
                        }
                    }
                }
                self.layout.paint_order = saved_paint;
            }

            if !self.full_layout_dirty && !self.full_extract_dirty {
                let mut paint_visited: u32 = 0;
                let mut overflowed = false;
                let mut pending = OwnVec::new_in(self.alloc);
                core::mem::swap(&mut pending, &mut self.dirty_paint);

                for &ent in pending.as_slice() {
                    let ok = patch_entity_gpu(
                        &mut self.world,
                        ent,
                        &self.theme,
                        time,
                        self.extract_epoch,
                        &mut self.scratch_draw,
                        &mut self.draw,
                    )?;
                    if !ok {
                        overflowed = true;
                        break;
                    }
                    paint_visited = paint_visited.saturating_add(1);
                }

                if !overflowed {
                    let r_p = u32::try_from(
                        self.draw.rect_patches.len().saturating_add(self.draw.ov_rect_patches.len()),
                    )
                    .unwrap_or(u32::MAX);
                    let g_p = u32::try_from(
                        self.draw.glyph_patches.len().saturating_add(self.draw.ov_glyph_patches.len()),
                    )
                    .unwrap_or(u32::MAX);
                    self.last_stats = FrameStats {
                        full_layout: false,
                        full_extract: false,
                        layout_nodes_visited: layout_visited,
                        paint_nodes_visited: paint_visited,
                        rect_patches: r_p,
                        glyph_patches: g_p,
                        gpu_bytes_patched: self.draw.patched_bytes(),
                        reactive_evals,
                    };
                    return Ok(());
                }
                self.full_extract_dirty = true;
            }
        }

        // Full layout + retained full extract pass.
        self.extract_epoch = self.extract_epoch.saturating_add(1);
        let ep = self.extract_epoch;
        let vp = Rect::new(0.0, 0.0, vp_key.0, vp_key.1);
        self.draw.begin_frame(vp.w, vp.h, vp_key.2, time)?;
        self.dirty_layout_roots.clear();
        self.dirty_paint.clear();

        let mut total_layout: u32 = 0;
        let mut total_paint: u32 = 0;

        total_layout = total_layout.saturating_add(self.layout.compute(
            &mut self.world,
            self.root,
            vp,
            &mut self.draw,
        )?);
        total_paint = total_paint.saturating_add(extract_entities(
            &mut self.world,
            self.layout.paint_order.as_slice(),
            &self.theme,
            time,
            ep,
            &mut self.draw,
        )?);

        if let Some(d_bounds) = dock_bounds {
            self.dock.compute_layout(&self.world, d_bounds)?;
            extract_dock_chrome(
                &self.world,
                &self.dock,
                &self.theme,
                self.input.pointer_x,
                self.input.pointer_y,
                &mut self.draw,
            )?;

            let mut combined_paint = OwnVec::new_in(self.alloc);
            for &e in self.layout.paint_order.as_slice() {
                combined_paint.try_push(e).map_err(|r| UiError::Alloc(r.why))?;
            }

            let leaf_count = self.dock.leaves.len();
            for li in 0..leaf_count {
                let Some(leaf_r) = self.dock.leaves.get(li).copied() else { continue };
                let Some(active_tab) = self.dock.active_tab_of(&self.world, leaf_r.leaf)? else {
                    continue;
                };
                let Some(tab_comp) = self.world.get::<DockTab>(active_tab)?.copied() else {
                    continue;
                };
                if !self.world.is_alive(tab_comp.content) {
                    continue;
                }
                total_layout = total_layout.saturating_add(self.layout.compute(
                    &mut self.world,
                    tab_comp.content,
                    leaf_r.body,
                    &mut self.draw,
                )?);
                total_paint = total_paint.saturating_add(extract_entities(
                    &mut self.world,
                    self.layout.paint_order.as_slice(),
                    &self.theme,
                    time,
                    ep,
                    &mut self.draw,
                )?);
                for &e in self.layout.paint_order.as_slice() {
                    combined_paint.try_push(e).map_err(|r| UiError::Alloc(r.why))?;
                }
            }
            self.layout.paint_order = combined_paint;
        }

        self.full_layout_dirty = false;
        self.full_extract_dirty = false;
        let full_bytes = u32::try_from(
            (self.draw.rects.len() + self.draw.overlay_rects.len()).saturating_mul(80)
                + (self.draw.glyphs.len() + self.draw.overlay_glyphs.len()).saturating_mul(64),
        )
        .unwrap_or(u32::MAX);
        self.last_stats = FrameStats {
            full_layout: true,
            full_extract: true,
            layout_nodes_visited: total_layout,
            paint_nodes_visited: total_paint,
            rect_patches: 0,
            glyph_patches: 0,
            gpu_bytes_patched: full_bytes,
            reactive_evals,
        };
        Ok(())
    }
}
