//! Pure sans-IO Input, Hit-Testing, Pointer Capture, Scroll, and Keyboard State Machine.
//!
//! Mutates real ECS interaction tags (`Hovered`, `Active`, `Focused`) directly on `World<A>`
//! and emits typed [`UiEvent`]s for application logic.

use core::alloc::Allocator;

use blaze_ecs::{Entity, World};
use blaze_own::Vec as OwnVec;

use crate::components::{
    ActionTag, Active, ComputedRect, Disabled, Focusable, Focused, Hovered, Interactive,
    ScrollOffset, SliderState, TextInputState, ToggleState, WidgetKind,
};
use crate::dock::{DOCK_HEADER_H, DockDrag, DockTab, DockWorkspace, SplitDir};
use crate::draw::DrawList;
use crate::error::UiError;
use crate::types::Rect;

/// Semantic cursor hint returned to the host shell (WebGPU canvas or native window).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorHint {
    #[default]
    Default,
    Pointer,
    Text,
    ColResize,
    RowResize,
    EwResize,
    Grabbing,
}

impl CursorHint {
    #[inline]
    pub const fn css_name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Pointer => "pointer",
            Self::Text => "text",
            Self::ColResize => "col-resize",
            Self::RowResize => "row-resize",
            Self::EwResize => "ew-resize",
            Self::Grabbing => "grabbing",
        }
    }
}

/// Platform-neutral keyboard key codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Escape,
    Enter,
    Tab,
    Backspace,
    Delete,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Space,
    Char(char),
}

/// Pure sans-IO input event fed from host (WebGPU browser events, winit, or tests).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    PointerMove { x: f32, y: f32 },
    PointerDown { x: f32, y: f32, button: u8, shift: bool },
    PointerUp { x: f32, y: f32, button: u8 },
    DoubleClick { x: f32, y: f32 },
    Wheel { x: f32, y: f32, dx: f32, dy: f32, ctrl_or_meta: bool },
    KeyDown { key: Key, shift: bool, ctrl_or_meta: bool },
    TextChar(char),
}

/// Application-level semantic UI events produced by the input system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UiEvent {
    Click { entity: Entity, action: u32 },
    ToggleChanged { entity: Entity, action: u32, checked: bool },
    SliderChanged { entity: Entity, action: u32, value: f32 },
    TextChanged { entity: Entity, action: u32 },
    TabSelected { leaf: Entity, tab: Entity, pane_id: u32 },
    DockChanged,
    CanvasClick { entity: Entity, local_x: f32, local_y: f32 },
    CanvasWheel { entity: Entity, local_x: f32, local_y: f32, dy: f32, ctrl_or_meta: bool },
}

/// Persistent pointer, capture, focus, dirty-entity hints, and event queue state.
pub struct InputState<A: Allocator + Copy> {
    pub pointer_x: f32,
    pub pointer_y: f32,
    pub hovered: Option<Entity>,
    pub active: Option<Entity>,
    pub focused: Option<Entity>,
    pub cursor: CursorHint,
    pub events: OwnVec<UiEvent, A>,
    /// Entities whose visual state (`Hovered`/`Active`/`Focused`/toggle/slider/text) changed.
    pub dirty_paint: OwnVec<Entity, A>,
    /// Set when dock chrome, splitter ratios, or scroll offsets require layout/full redraw.
    pub dirty_layout: bool,
    tab_drag_origin: Option<(Entity, Entity, f32, f32)>,
}

impl<A: Allocator + Copy + 'static> InputState<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            pointer_x: -1.0,
            pointer_y: -1.0,
            hovered: None,
            active: None,
            focused: None,
            cursor: CursorHint::Default,
            events: OwnVec::new_in(alloc),
            dirty_paint: OwnVec::new_in(alloc),
            dirty_layout: false,
            tab_drag_origin: None,
        }
    }

    fn mark_paint(&mut self, entity: Entity) {
        if !self.dirty_paint.iter().any(|&e| e == entity) {
            let _ = self.dirty_paint.try_push(entity);
        }
    }

    /// Process a single [`InputEvent`] against the current laid-out UI `world`, `paint_order`,
    /// GPU `draw` clip table, and `dock` workspace.
    pub fn handle_event(
        &mut self,
        world: &mut World<A>,
        paint_order: &[Entity],
        draw: &DrawList<A>,
        dock: &mut DockWorkspace<A>,
        ev: InputEvent,
    ) -> Result<(), UiError> {
        match ev {
            InputEvent::PointerMove { x, y } => {
                self.pointer_x = x;
                self.pointer_y = y;

                // 1. Active dock splitter drag
                if dock.dragging_splitter.is_some() {
                    if dock.drag_splitter(world, x, y)? {
                        self.dirty_layout = true;
                        self.events.try_push(UiEvent::DockChanged).map_err(|r| UiError::Alloc(r.why))?;
                    }
                    return Ok(());
                }

                // 2. Active or pending dock tab drag
                if let Some(mut drag) = dock.dragging_tab {
                    drag.pointer_x = x;
                    drag.pointer_y = y;
                    drag.target = dock.compute_drop_target(x, y);
                    dock.dragging_tab = Some(drag);
                    self.dirty_layout = true;
                    self.cursor = CursorHint::Grabbing;
                    return Ok(());
                }
                if let Some((tab, src_leaf, ox, oy)) = self.tab_drag_origin {
                    let dx = x - ox;
                    let dy = y - oy;
                    if dx * dx + dy * dy >= 36.0 {
                        let target = dock.compute_drop_target(x, y);
                        dock.dragging_tab = Some(DockDrag {
                            tab,
                            source_leaf: src_leaf,
                            pointer_x: x,
                            pointer_y: y,
                            target,
                        });
                        self.tab_drag_origin = None;
                        self.dirty_layout = true;
                        self.cursor = CursorHint::Grabbing;
                        return Ok(());
                    }
                }

                // 3. Active slider scrub capture
                if let Some(active_ent) = self.active {
                    if world.is_alive(active_ent)
                        && world.get::<WidgetKind>(active_ent)? == Some(&WidgetKind::Slider)
                    {
                        self.scrub_slider(world, active_ent, x)?;
                        self.cursor = CursorHint::EwResize;
                        return Ok(());
                    }
                }

                // 4. Dock splitter hover cursor
                if let Some(sp) = dock.hit_splitter(x, y) {
                    self.set_hovered(world, None)?;
                    self.dirty_layout = true;
                    self.cursor = match sp.dir {
                        SplitDir::Horizontal => CursorHint::ColResize,
                        SplitDir::Vertical => CursorHint::RowResize,
                    };
                    return Ok(());
                }

                // 5. Dock header tab hover
                if hit_dock_tab(world, dock, x, y)?.is_some() {
                    self.set_hovered(world, None)?;
                    self.dirty_layout = true;
                    self.cursor = CursorHint::Pointer;
                    return Ok(());
                }

                // 6. Reverse paint-order ECS hit test respecting GPU clip rects
                let hit = hit_test_world(world, paint_order, draw, x, y, true)?;
                self.set_hovered(world, hit)?;
                self.cursor = match hit {
                    Some(e) => cursor_for_entity(world, e)?,
                    None => CursorHint::Default,
                };
            }

            InputEvent::PointerDown { x, y, button, shift: _ } => {
                self.pointer_x = x;
                self.pointer_y = y;
                if button != 0 {
                    return Ok(());
                }

                // 1. Check dock splitters first
                if let Some(sp) = dock.hit_splitter(x, y) {
                    dock.dragging_splitter = Some(sp.split);
                    self.cursor = match sp.dir {
                        SplitDir::Horizontal => CursorHint::ColResize,
                        SplitDir::Vertical => CursorHint::RowResize,
                    };
                    return Ok(());
                }

                // 2. Check dock header tabs & expand button
                if let Some(hit_tab) = hit_dock_tab(world, dock, x, y)? {
                    self.dirty_layout = true;
                    match hit_tab {
                        DockHeaderHit::Tab { leaf, tab, idx, pane_id } => {
                            dock.select_tab(world, leaf, idx)?;
                            self.tab_drag_origin = Some((tab, leaf, x, y));
                            self.events
                                .try_push(UiEvent::TabSelected { leaf, tab, pane_id })
                                .map_err(|r| UiError::Alloc(r.why))?;
                            return Ok(());
                        }
                        DockHeaderHit::Maximize { tab } => {
                            dock.toggle_maximize(tab);
                            self.events.try_push(UiEvent::DockChanged).map_err(|r| UiError::Alloc(r.why))?;
                            return Ok(());
                        }
                    }
                }

                // 3. ECS widget hit test
                let hit = hit_test_world(world, paint_order, draw, x, y, true)?;
                self.set_hovered(world, hit)?;
                self.set_active(world, hit)?;

                if let Some(e) = hit {
                    if world.has_tag_typed::<Focusable>(e)? {
                        self.set_focused(world, Some(e))?;
                    } else {
                        self.set_focused(world, None)?;
                    }
                    let kind = world.get::<WidgetKind>(e)?.copied().unwrap_or(WidgetKind::Container);
                    if kind == WidgetKind::Slider {
                        self.scrub_slider(world, e, x)?;
                        self.cursor = CursorHint::EwResize;
                    } else if kind == WidgetKind::Canvas {
                        if let Some(cr) = world.get::<ComputedRect>(e)?.copied() {
                            self.events
                                .try_push(UiEvent::CanvasClick {
                                    entity: e,
                                    local_x: x - cr.content_rect.x,
                                    local_y: y - cr.content_rect.y,
                                })
                                .map_err(|r| UiError::Alloc(r.why))?;
                        }
                    }
                } else {
                    self.set_focused(world, None)?;
                }
            }

            InputEvent::PointerUp { x, y, button } => {
                self.pointer_x = x;
                self.pointer_y = y;
                if button != 0 {
                    return Ok(());
                }

                if dock.dragging_splitter.take().is_some() {
                    self.dirty_layout = true;
                    self.cursor = CursorHint::Default;
                    return Ok(());
                }

                self.tab_drag_origin = None;
                if let Some(drag) = dock.dragging_tab.take() {
                    self.dirty_layout = true;
                    if let Some((target_leaf, zone, _)) = drag.target {
                        if dock.drop_tab(world, drag.tab, drag.source_leaf, target_leaf, zone)? {
                            self.events.try_push(UiEvent::DockChanged).map_err(|r| UiError::Alloc(r.why))?;
                        }
                    }
                    self.cursor = CursorHint::Default;
                    return Ok(());
                }

                let prev_active = self.active;
                self.set_active(world, None)?;

                let hit = hit_test_world(world, paint_order, draw, x, y, true)?;
                if let (Some(a), Some(h)) = (prev_active, hit) {
                    if a == h && world.is_alive(a) && !world.has_tag_typed::<Disabled>(a)? {
                        self.activate_entity(world, a)?;
                    }
                }
                self.set_hovered(world, hit)?;
                self.cursor = match hit {
                    Some(e) => cursor_for_entity(world, e)?,
                    None => CursorHint::Default,
                };
            }

            InputEvent::DoubleClick { x, y } => {
                // Double-clicking a dock header toggles maximize on its active tab.
                for &leaf_r in dock.leaves.as_slice() {
                    if leaf_r.header.contains(x, y) {
                        if let Some(tab) = dock.active_tab_of(world, leaf_r.leaf)? {
                            dock.toggle_maximize(tab);
                            self.events.try_push(UiEvent::DockChanged).map_err(|r| UiError::Alloc(r.why))?;
                            return Ok(());
                        }
                    }
                }
            }

            InputEvent::Wheel { x, y, dx, dy, ctrl_or_meta } => {
                // First check if pointer is over a Canvas widget (for zoom/pan).
                let hit_canvas = hit_test_world(world, paint_order, draw, x, y, false)?;
                if let Some(e) = hit_canvas {
                    if world.get::<WidgetKind>(e)? == Some(&WidgetKind::Canvas) {
                        if let Some(cr) = world.get::<ComputedRect>(e)?.copied() {
                            self.events
                                .try_push(UiEvent::CanvasWheel {
                                    entity: e,
                                    local_x: x - cr.content_rect.x,
                                    local_y: y - cr.content_rect.y,
                                    dy,
                                    ctrl_or_meta,
                                })
                                .map_err(|r| UiError::Alloc(r.why))?;
                            return Ok(());
                        }
                    }
                }

                // Walk up from deepest hit entity to nearest scrollable container.
                let mut cur = hit_canvas;
                while let Some(ent) = cur {
                    if let Some(mut scroll) = world.get::<ScrollOffset>(ent)?.copied() {
                        if let Some(cr) = world.get::<ComputedRect>(ent)?.copied() {
                            let max_y = (scroll.content_h - cr.content_rect.h).max(0.0);
                            let max_x = (scroll.content_w - cr.content_rect.w).max(0.0);
                            let ny = (scroll.y + dy).clamp(0.0, max_y);
                            let nx = (scroll.x + dx).clamp(0.0, max_x);
                            if (ny - scroll.y).abs() > 0.01 || (nx - scroll.x).abs() > 0.01 {
                                scroll.y = ny;
                                scroll.x = nx;
                                world.set_copy(ent, scroll)?;
                                self.dirty_layout = true;
                            }
                            return Ok(());
                        }
                    }
                    cur = world.parent_of(ent)?;
                }
            }

            InputEvent::KeyDown { key, shift: _, ctrl_or_meta: _ } => {
                match key {
                    Key::Escape => {
                        if dock.maximized_tab.take().is_some() {
                            self.events.try_push(UiEvent::DockChanged).map_err(|r| UiError::Alloc(r.why))?;
                        }
                        self.set_focused(world, None)?;
                    }
                    Key::Enter | Key::Space => {
                        if let Some(f) = self.focused {
                            if world.is_alive(f) {
                                let kind = world.get::<WidgetKind>(f)?.copied().unwrap_or(WidgetKind::Container);
                                if kind == WidgetKind::TextInput && key == Key::Space {
                                    self.insert_text_char(world, f, ' ')?;
                                } else if kind != WidgetKind::TextInput {
                                    self.activate_entity(world, f)?;
                                }
                            }
                        }
                    }
                    Key::Backspace => {
                        if let Some(f) = self.focused {
                            if world.is_alive(f) && world.get::<WidgetKind>(f)? == Some(&WidgetKind::TextInput) {
                                let mut changed = false;
                                if let Some(st) = world.get_mut::<TextInputState<A>>(f)? {
                                    if !st.text.is_empty() {
                                        let _ = st.text.pop();
                                        st.cursor = st.text.len();
                                        changed = true;
                                    }
                                }
                                if changed {
                                    self.mark_paint(f);
                                    let action = world.get::<ActionTag>(f)?.map_or(0, |a| a.0);
                                    self.events
                                        .try_push(UiEvent::TextChanged { entity: f, action })
                                        .map_err(|r| UiError::Alloc(r.why))?;
                                }
                            }
                        }
                    }
                    Key::Char(ch) => {
                        if let Some(f) = self.focused {
                            if world.is_alive(f) && world.get::<WidgetKind>(f)? == Some(&WidgetKind::TextInput) {
                                self.insert_text_char(world, f, ch)?;
                            }
                        }
                    }
                    _ => {}
                }
            }

            InputEvent::TextChar(ch) => {
                if let Some(f) = self.focused {
                    if world.is_alive(f) && world.get::<WidgetKind>(f)? == Some(&WidgetKind::TextInput) {
                        self.insert_text_char(world, f, ch)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn insert_text_char(
        &mut self,
        world: &mut World<A>,
        entity: Entity,
        ch: char,
    ) -> Result<(), UiError> {
        if ch.is_control() {
            return Ok(());
        }
        if let Some(st) = world.get_mut::<TextInputState<A>>(entity)? {
            if st.text.len() < 128 {
                st.text.try_push(ch)?;
                st.cursor = st.text.len();
            }
        }
        self.mark_paint(entity);
        let action = world.get::<ActionTag>(entity)?.map_or(0, |a| a.0);
        self.events
            .try_push(UiEvent::TextChanged { entity, action })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    fn activate_entity(&mut self, world: &mut World<A>, entity: Entity) -> Result<(), UiError> {
        let action = world.get::<ActionTag>(entity)?.map_or(0, |a| a.0);
        let kind = world.get::<WidgetKind>(entity)?.copied().unwrap_or(WidgetKind::Container);
        if kind == WidgetKind::Toggle {
            let next = !world.get::<ToggleState>(entity)?.map_or(false, |t| t.checked);
            world.set_copy(entity, ToggleState { checked: next })?;
            self.mark_paint(entity);
            self.events
                .try_push(UiEvent::ToggleChanged { entity, action, checked: next })
                .map_err(|r| UiError::Alloc(r.why))?;
            return Ok(());
        }
        self.events
            .try_push(UiEvent::Click { entity, action })
            .map_err(|r| UiError::Alloc(r.why))?;
        Ok(())
    }

    fn scrub_slider(
        &mut self,
        world: &mut World<A>,
        entity: Entity,
        pointer_x: f32,
    ) -> Result<(), UiError> {
        let Some(cr) = world.get::<ComputedRect>(entity)?.copied() else {
            return Ok(());
        };
        let Some(mut slider) = world.get::<SliderState>(entity)?.copied() else {
            return Ok(());
        };
        // Track spans the right 55% of the slider widget (left side holds label, right holds track).
        let track_x0 = cr.rect.x + cr.rect.w * 0.42;
        let track_w = (cr.rect.w * 0.56 - 44.0).max(24.0);
        let t = ((pointer_x - track_x0) / track_w).clamp(0.0, 1.0);
        if slider.set_normalized(t) {
            world.set_copy(entity, slider)?;
            self.mark_paint(entity);
            let action = world.get::<ActionTag>(entity)?.map_or(0, |a| a.0);
            self.events
                .try_push(UiEvent::SliderChanged { entity, action, value: slider.value })
                .map_err(|r| UiError::Alloc(r.why))?;
        }
        Ok(())
    }

    fn set_hovered(&mut self, world: &mut World<A>, next: Option<Entity>) -> Result<(), UiError> {
        if self.hovered == next {
            return Ok(());
        }
        if let Some(prev) = self.hovered {
            if world.is_alive(prev) {
                let _ = world.remove_tag_typed::<Hovered>(prev)?;
                self.mark_paint(prev);
            }
        }
        if let Some(n) = next {
            if world.is_alive(n) {
                world.add_tag_typed::<Hovered>(n)?;
                self.mark_paint(n);
            }
        }
        self.hovered = next;
        Ok(())
    }

    fn set_active(&mut self, world: &mut World<A>, next: Option<Entity>) -> Result<(), UiError> {
        if self.active == next {
            return Ok(());
        }
        if let Some(prev) = self.active {
            if world.is_alive(prev) {
                let _ = world.remove_tag_typed::<Active>(prev)?;
                self.mark_paint(prev);
            }
        }
        if let Some(n) = next {
            if world.is_alive(n) {
                world.add_tag_typed::<Active>(n)?;
                self.mark_paint(n);
            }
        }
        self.active = next;
        Ok(())
    }

    fn set_focused(&mut self, world: &mut World<A>, next: Option<Entity>) -> Result<(), UiError> {
        if self.focused == next {
            return Ok(());
        }
        if let Some(prev) = self.focused {
            if world.is_alive(prev) {
                let _ = world.remove_tag_typed::<Focused>(prev)?;
                self.mark_paint(prev);
            }
        }
        if let Some(n) = next {
            if world.is_alive(n) {
                world.add_tag_typed::<Focused>(n)?;
                self.mark_paint(n);
            }
        }
        self.focused = next;
        Ok(())
    }
}

enum DockHeaderHit {
    Tab { leaf: Entity, tab: Entity, idx: usize, pane_id: u32 },
    Maximize { tab: Entity },
}

fn hit_dock_tab<A: Allocator + Copy + 'static>(
    world: &World<A>,
    dock: &DockWorkspace<A>,
    px: f32,
    py: f32,
) -> Result<Option<DockHeaderHit>, UiError> {
    for &leaf_r in dock.leaves.as_slice() {
        if !leaf_r.header.contains(px, py) {
            continue;
        }
        // Rightmost 24px of header is the maximize/restore button (`⤢`).
        let exp_rect = Rect::new(
            leaf_r.header.right() - 26.0,
            leaf_r.header.y + 3.0,
            22.0,
            (DOCK_HEADER_H - 6.0).max(12.0),
        );
        if exp_rect.contains(px, py) {
            if let Some(tab) = dock.active_tab_of(world, leaf_r.leaf)? {
                return Ok(Some(DockHeaderHit::Maximize { tab }));
            }
        }

        let tabs = dock.tabs_of(world, leaf_r.leaf)?;
        let mut tx = leaf_r.header.x + 6.0;
        for (idx, &tab) in tabs.as_slice().iter().enumerate() {
            let name = world.entity_name(tab).unwrap_or("Pane");
            let chars = u16::try_from(name.chars().count()).map_or(10.0, f32::from);
            let tw = (chars * 7.2 + 22.0).clamp(58.0, 160.0);
            let tab_rect = Rect::new(tx, leaf_r.header.y + 3.0, tw, (DOCK_HEADER_H - 6.0).max(12.0));
            if tab_rect.contains(px, py) {
                let pane_id = world.get::<DockTab>(tab)?.map_or(0, |t| t.pane_id);
                return Ok(Some(DockHeaderHit::Tab { leaf: leaf_r.leaf, tab, idx, pane_id }));
            }
            tx += tw + 4.0;
        }
    }
    Ok(None)
}

fn hit_test_world<A: Allocator + Copy + 'static>(
    world: &World<A>,
    paint_order: &[Entity],
    draw: &DrawList<A>,
    px: f32,
    py: f32,
    interactive_only: bool,
) -> Result<Option<Entity>, UiError> {
    let mut i = paint_order.len();
    while i > 0 {
        i -= 1;
        let Some(&e) = paint_order.get(i) else { continue };
        if !world.is_alive(e) || world.has_tag_typed::<Disabled>(e)? {
            continue;
        }
        if interactive_only && !world.has_tag_typed::<Interactive>(e)? {
            continue;
        }
        let Some(cr) = world.get::<ComputedRect>(e)?.copied() else {
            continue;
        };
        if !cr.rect.contains(px, py) {
            continue;
        }
        let clip = draw.clip_rect(cr.clip_id);
        if !clip.contains(px, py) {
            continue;
        }
        return Ok(Some(e));
    }
    Ok(None)
}

fn cursor_for_entity<A: Allocator + Copy + 'static>(
    world: &World<A>,
    entity: Entity,
) -> Result<CursorHint, UiError> {
    let kind = world.get::<WidgetKind>(entity)?.copied().unwrap_or(WidgetKind::Container);
    Ok(match kind {
        WidgetKind::TextInput => CursorHint::Text,
        WidgetKind::Slider => CursorHint::EwResize,
        WidgetKind::Button
        | WidgetKind::Toggle
        | WidgetKind::TreeRow
        | WidgetKind::DockTab => CursorHint::Pointer,
        _ => CursorHint::Default,
    })
}
