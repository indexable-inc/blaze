//! Core relational ECS components, tags, and relations for `blaze-ui`.
//!
//! Every widget, dock split, pane, tab, toolbar button, slider, tree row, property row, and
//! dense GPU canvas is a real `blaze_ecs::Entity` connected with Flecs-style relations:
//! - `ChildOf`: exclusive cascade-delete UI hierarchy
//! - `IsA`: live theme / style prefab inheritance
//! - [`DockedIn`]: associates a pane/tab with its dock leaf container
//! - [`TooltipFor`]: associates a floating tooltip overlay with its target widget
//! - [`Inspects`]: links an inspector pane to the entity it inspects (including UI self-inspection)

use core::alloc::Allocator;

use blaze_ecs::{Entity, Relation, World};
use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::error::UiError;
use crate::shaders::{GlyphInst, RectInst};
use crate::style::{LayoutStyle, PaintStyle};
use crate::types::Rect;

// =============================================================================
// Typed ECS Relations
// =============================================================================

/// Links a dock tab / pane entity to its owning `DockLeaf` container.
pub struct DockedIn;
impl Relation for DockedIn {
    const EXCLUSIVE: bool = true;
}

/// Links a floating tooltip overlay entity to the widget entity it describes.
pub struct TooltipFor;
impl Relation for TooltipFor {
    const EXCLUSIVE: bool = true;
}

/// Links an inspector entity to the entity it currently inspects (supports UI self-inspection!).
pub struct Inspects;
impl Relation for Inspects {
    const EXCLUSIVE: bool = true;
}

// =============================================================================
// Interaction & Visibility ECS Tags
// =============================================================================

/// Pointer currently hovers this entity.
pub struct Hovered;
/// Pointer is pressed/captured on this entity.
pub struct Active;
/// Keyboard focus currently resides on this entity.
pub struct Focused;
/// Entity is selected (active tab, tree row, segmented option, inspected node).
pub struct Selected;
/// Entity is disabled (rendered muted, ignores input).
pub struct Disabled;
/// Entity and its `ChildOf` subtree are hidden from layout, hit-testing, and rendering.
pub struct Hidden;
/// Entity participates in pointer hit-testing.
pub struct Interactive;
/// Entity can receive keyboard focus via `Tab` navigation or click.
pub struct Focusable;

// =============================================================================
// Core UI Components
// =============================================================================

/// Explicit deterministic sibling order within a `ChildOf` parent.
///
/// Because adding/removing tags like `Hovered` or `Selected` moves an entity between ECS
/// archetype tables, `ChildOrder` guarantees stable, flicker-free layout and draw order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChildOrder(pub u32);

/// Semantic widget role used by the layout, input, and GPU extraction systems.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WidgetKind {
    #[default]
    Container,
    Panel,
    Card,
    Toolbar,
    Button,
    Toggle,
    Badge,
    Label,
    Slider,
    TextInput,
    TreeRow,
    PropertyRow,
    Canvas,
    Tooltip,
    DockSplit,
    DockLeaf,
    DockTab,
}

impl WidgetKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Container => "Container",
            Self::Panel => "Panel",
            Self::Card => "Card",
            Self::Toolbar => "Toolbar",
            Self::Button => "Button",
            Self::Toggle => "Toggle",
            Self::Badge => "Badge",
            Self::Label => "Label",
            Self::Slider => "Slider",
            Self::TextInput => "TextInput",
            Self::TreeRow => "TreeRow",
            Self::PropertyRow => "PropertyRow",
            Self::Canvas => "Canvas",
            Self::Tooltip => "Tooltip",
            Self::DockSplit => "DockSplit",
            Self::DockLeaf => "DockLeaf",
            Self::DockTab => "DockTab",
        }
    }
}

/// Output written by the two-pass layout engine for every visible UI entity.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ComputedRect {
    /// Outer border-box in logical CSS pixels.
    pub rect: Rect,
    /// Inner content-box after padding.
    pub content_rect: Rect,
    /// Active GPU clip-table index (`clips[clip_id]`) constraining this entity.
    pub clip_id: u32,
    /// GPU clip-table index created by this entity for its descendants (equals `clip_id` if unclipped).
    pub child_clip_id: u32,
    /// Render layer (`0` = base UI, `1` = floating overlay / tooltip / drag preview).
    pub layer: u8,
    /// Monotonic paint sequence written during layout for reverse hit-testing.
    pub paint_order: u32,
}

/// Scroll offset and measured content extent for scrollable containers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollOffset {
    pub x: f32,
    pub y: f32,
    pub content_w: f32,
    pub content_h: f32,
}

/// Stable application-level semantic command / action identifier on interactive widgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ActionTag(pub u32);

/// Semantic tone for badges, status dots, and accents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeTone {
    #[default]
    Neutral,
    Fire,
    Ok,
    Warn,
    Danger,
    Blue,
    Violet,
}

/// Primary owned UTF-8 label string component.
pub struct LabelText<A: Allocator + Copy>(pub OwnString<A>);

/// Secondary owned UTF-8 value / metadata / shortcut text component.
pub struct SubText<A: Allocator + Copy>(pub OwnString<A>);

/// Boolean toggle / checkbox state component.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToggleState {
    pub checked: bool,
}

/// Continuous / stepped numeric drag-slider state component.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderState {
    pub value: f32,
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub decimals: u8,
}

impl SliderState {
    pub const fn new(value: f32, min: f32, max: f32, step: f32, decimals: u8) -> Self {
        Self { value, min, max, step, decimals }
    }

    #[inline]
    pub fn normalized(self) -> f32 {
        let span = self.max - self.min;
        if span <= 1e-6 {
            0.0
        } else {
            ((self.value - self.min) / span).clamp(0.0, 1.0)
        }
    }

    #[inline]
    pub fn set_normalized(&mut self, t: f32) -> bool {
        let clamped = t.clamp(0.0, 1.0);
        let mut v = self.min + (self.max - self.min) * clamped;
        if self.step > 0.0 {
            let steps = gpu::math::round((v - self.min) / self.step);
            v = (self.min + steps * self.step).clamp(self.min, self.max);
        }
        let changed = (v - self.value).abs() > 1e-5;
        self.value = v;
        changed
    }
}

/// Single-line editable text input component.
pub struct TextInputState<A: Allocator + Copy> {
    pub text: OwnString<A>,
    pub placeholder: OwnString<A>,
    pub cursor: usize,
}

/// Hierarchical tree-view row metadata.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeRowState {
    pub depth: u16,
    pub has_children: bool,
    pub expanded: bool,
    /// Optional domain or UI entity represented by this tree row.
    pub target: Option<Entity>,
}

/// Hybrid dense GPU primitive batch attached to a `WidgetKind::Canvas` entity.
///
/// Enables ultra-dense plots (100k-span flamegraphs, timelines, relational force-graphs,
/// sparklines) to stream directly into the same WebGPU SDF/glyph draw calls with the owning
/// entity's rounded GPU clip rect automatically applied!
pub struct CanvasBatch<A: Allocator + Copy> {
    /// Local-space (relative to entity's `content_rect` top-left) or screen-space SDF primitives.
    pub rects: OwnVec<RectInst, A>,
    /// Local-space or screen-space glyphs.
    pub glyphs: OwnVec<GlyphInst, A>,
    /// When `true`, coordinates in `rects` / `glyphs` are translated by `content_rect.xy`.
    pub local_space: bool,
}

impl<A: Allocator + Copy> CanvasBatch<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            rects: OwnVec::new_in(alloc),
            glyphs: OwnVec::new_in(alloc),
            local_space: true,
        }
    }

    pub fn clear(&mut self) {
        self.rects.clear();
        self.glyphs.clear();
    }
}

/// Register all canonical `blaze-ui` components, relations, and tags in `world`.
pub fn register_ui_schema<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
) -> Result<(), UiError> {
    let _ = world.register_copy::<LayoutStyle>()?;
    let _ = world.register_copy::<PaintStyle>()?;
    let _ = world.register_copy::<ChildOrder>()?;
    let _ = world.register_copy::<WidgetKind>()?;
    let _ = world.register_copy::<ComputedRect>()?;
    let _ = world.register_copy::<ScrollOffset>()?;
    let _ = world.register_copy::<ActionTag>()?;
    let _ = world.register_copy::<BadgeTone>()?;
    let _ = world.register_copy::<ToggleState>()?;
    let _ = world.register_copy::<SliderState>()?;
    let _ = world.register_copy::<TreeRowState>()?;

    let _ = world.register::<LabelText<A>>()?;
    let _ = world.register::<SubText<A>>()?;
    let _ = world.register::<TextInputState<A>>()?;
    let _ = world.register::<CanvasBatch<A>>()?;

    let _ = world.relation_entity::<DockedIn>()?;
    let _ = world.relation_entity::<TooltipFor>()?;
    let _ = world.relation_entity::<Inspects>()?;

    let _ = world.tag_entity::<Hovered>()?;
    let _ = world.tag_entity::<Active>()?;
    let _ = world.tag_entity::<Focused>()?;
    let _ = world.tag_entity::<Selected>()?;
    let _ = world.tag_entity::<Disabled>()?;
    let _ = world.tag_entity::<Hidden>()?;
    let _ = world.tag_entity::<Interactive>()?;
    let _ = world.tag_entity::<Focusable>()?;
    Ok(())
}
