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
use crate::types::{Rect, Rgba, Vec2};

// =============================================================================
// Typed ECS Relations
// =============================================================================

/// Links a dock tab / pane entity to its owning `DockLeaf` container (Flecs 4.2 `DontFragment`).
pub struct DockedIn;
impl Relation for DockedIn {
    const EXCLUSIVE: bool = true;
    const DONT_FRAGMENT: bool = true;
}

/// Links a floating tooltip overlay entity to the widget entity it describes (`DontFragment`).
pub struct TooltipFor;
impl Relation for TooltipFor {
    const EXCLUSIVE: bool = true;
    const DONT_FRAGMENT: bool = true;
}

/// Links an inspector entity to the entity it currently inspects (`DontFragment` — zero table move!).
pub struct Inspects;
impl Relation for Inspects {
    const EXCLUSIVE: bool = true;
    const DONT_FRAGMENT: bool = true;
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

/// Retained per-entity GPU instance buffer range enabling $O(\Delta)$ in-place WebGPU patching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuSlot {
    pub epoch: u64,
    pub layer: u8,
    pub rect_start: u32,
    pub rect_len: u32,
    pub rect_cap: u32,
    pub glyph_start: u32,
    pub glyph_len: u32,
    pub glyph_cap: u32,
}

/// Per-frame incremental execution telemetry proving true $O(\Delta)$ scaling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub full_layout: bool,
    pub full_extract: bool,
    pub layout_nodes_visited: u32,
    pub paint_nodes_visited: u32,
    pub rect_patches: u32,
    pub glyph_patches: u32,
    pub gpu_bytes_patched: u32,
    pub reactive_evals: u32,
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

/// Explicit intrinsic `(width, height)` override measured from Slug / HarfBuzz outlines so
/// `LayoutEngine` sizes SwiftUI-style `Text` / `ShapedText` ECS leaves with sub-pixel accuracy.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IntrinsicSize(pub Vec2);

/// Typography parameters for a SwiftUI-style `Text` ECS entity rendered with `den` Slug outlines.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub family: u32,
    pub weight: u32,
    pub opsz: f32,
    pub color: Rgba,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self { size: 16.0, family: 0, weight: 400, opsz: 14.0, color: Rgba::WHITE }
    }
}

/// One pre-shaped HarfBuzz glyph inside a [`ShapedText`] ECS component.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    pub glyph_id: u32,
    pub x_em: f32,
    pub color: Rgba,
}

/// Pre-shaped vector text run stored directly as an ECS component (measured by `LayoutEngine` and
/// rendered by the Rust GPU Slug fragment shader).
pub struct ShapedText<A: Allocator + Copy> {
    pub glyphs: OwnVec<ShapedGlyph, A>,
    pub size: f32,
    pub adv_em: f32,
}

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
    let _ = world.register_copy::<GpuSlot>()?;
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

    // Flecs 4.2 `DontFragment`: transient UI state tags NEVER trigger archetype table migrations!
    let _ = world.mark_tag_dont_fragment::<Hovered>()?;
    let _ = world.mark_tag_dont_fragment::<Active>()?;
    let _ = world.mark_tag_dont_fragment::<Focused>()?;
    let _ = world.mark_tag_dont_fragment::<Selected>()?;
    let _ = world.mark_tag_dont_fragment::<Disabled>()?;
    let _ = world.mark_tag_dont_fragment::<Hidden>()?;
    let _ = world.tag_entity::<Interactive>()?;
    let _ = world.tag_entity::<Focusable>()?;
    Ok(())
}
