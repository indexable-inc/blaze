//! Ergonomic high-level [`UiContext`] owning the relational ECS [`World`], `IsA` [`StylePrefabs`],
//! [`LayoutEngine`], [`DockWorkspace`], [`InputState`], and [`DrawList`].

use core::alloc::Allocator;

use blaze_ecs::{ChildOf, Entity, IsA, World};
use blaze_own::String as OwnString;

use crate::components::{
    ActionTag, BadgeTone, CanvasBatch, ChildOrder, Focusable, Interactive, LabelText, ScrollOffset,
    SliderState, SubText, TextInputState, ToggleState, TreeRowState, WidgetKind,
    register_ui_schema,
};
use crate::dock::{DockTab, DockWorkspace};
use crate::draw::DrawList;
use crate::error::UiError;
use crate::extract::{extract_dock_chrome, extract_entities};
use crate::input::{CursorHint, InputEvent, InputState, UiEvent};
use crate::layout::LayoutEngine;
use crate::style::{Align, Direction, LayoutStyle, PaintStyle, Sizing, StylePrefabs};
use crate::types::{Edges, Rect, Theme, ThemeKind};

/// Complete `#![no_std]` relational ECS UI context.
pub struct UiContext<A: Allocator + Copy + 'static> {
    alloc: A,
    pub world: World<A>,
    pub theme: Theme,
    pub prefabs: StylePrefabs,
    pub root: Entity,
    pub dock: DockWorkspace<A>,
    pub layout: LayoutEngine<A>,
    pub input: InputState<A>,
    pub draw: DrawList<A>,
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

        Ok(Self {
            alloc,
            world,
            theme,
            prefabs,
            root,
            dock: DockWorkspace::new_in(alloc),
            layout: LayoutEngine::new_in(alloc),
            input: InputState::new_in(alloc),
            draw: DrawList::new_in(alloc),
            next_order: 1,
        })
    }

    #[inline]
    pub fn alloc(&self) -> A {
        self.alloc
    }

    /// Switch the active [`ThemeKind`] and cascade new [`PaintStyle`]s to every widget via `IsA`.
    pub fn set_theme(&mut self, kind: ThemeKind) -> Result<(), UiError> {
        self.theme = Theme::from_kind(kind);
        self.prefabs.apply_theme(&mut self.world, &self.theme)?;
        Ok(())
    }

    /// Apply a custom modified [`Theme`] and cascade via `IsA`.
    pub fn apply_custom_theme(&mut self, theme: Theme) -> Result<(), UiError> {
        self.theme = theme;
        self.prefabs.apply_theme(&mut self.world, &self.theme)?;
        Ok(())
    }

    #[inline]
    fn next_child_order(&mut self) -> ChildOrder {
        let ord = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        ChildOrder(ord)
    }

    /// Remove all `ChildOf` children of `parent` (cascading despawn cleanly).
    pub fn clear_children(&mut self, parent: Entity) -> Result<(), UiError> {
        let children = self.world.children_of(parent)?;
        for &c in children.as_slice() {
            if self.world.is_alive(c) {
                self.world.despawn(c)?;
            }
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
        self.input
            .handle_event(&mut self.world, self.layout.paint_order.as_slice(), &self.draw, &mut self.dock, ev)
    }

    /// Drain all semantic [`UiEvent`]s produced since the last call.
    pub fn drain_events(&mut self, out: &mut blaze_own::Vec<UiEvent, A>) -> Result<(), UiError> {
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

    /// Run full frame: layout root + docked panes, extract dock chrome + UI entities + hybrid
    /// GPU `CanvasBatch` streams into `self.draw`.
    pub fn render_frame(
        &mut self,
        width: f32,
        height: f32,
        dpr: f32,
        time: f32,
        dock_bounds: Option<Rect>,
    ) -> Result<(), UiError> {
        let vp = Rect::new(0.0, 0.0, width.max(1.0), height.max(1.0));
        self.draw.begin_frame(vp.w, vp.h, dpr, time)?;

        // 1. Layout & extract root chrome (e.g. top toolbar + status bar).
        self.layout.compute(&mut self.world, self.root, vp, &mut self.draw)?;
        extract_entities(&self.world, self.layout.paint_order.as_slice(), &self.theme, time, &mut self.draw)?;

        // 2. If a dock workspace is active, lay out its binary tree, draw chrome, and lay out &
        //    extract every active tab's content subtree inside its leaf body rect.
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

            // Accumulate all paint-ordered entities (root + active pane subtrees) so hit-testing
            // covers both chrome AND every docked pane seamlessly!
            let mut combined_paint = blaze_own::Vec::new_in(self.alloc);
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
                self.layout.compute(&mut self.world, tab_comp.content, leaf_r.body, &mut self.draw)?;
                extract_entities(
                    &self.world,
                    self.layout.paint_order.as_slice(),
                    &self.theme,
                    time,
                    &mut self.draw,
                )?;
                for &e in self.layout.paint_order.as_slice() {
                    combined_paint.try_push(e).map_err(|r| UiError::Alloc(r.why))?;
                }
            }
            self.layout.paint_order = combined_paint;
        }

        Ok(())
    }
}
