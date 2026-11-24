//! `blaze-ui`: zero-dependency, `#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]`
//! relational ECS UI library & Rust-authored WebGPU/Metal SDF renderer.
//!
//! Architecture:
//! - **Relational ECS Scene Graph** ([`components`], [`style`]): UI widgets, dock trees, and
//!   themes are first-class entities in `blaze_ecs::World` connected by `ChildOf` (ordered via
//!   [`ChildOrder`]), `IsA` (cascading [`StylePrefabs`]), [`DockedIn`], [`TooltipFor`], and
//!   [`Inspects`] relations, plus live ECS interaction tags (`Hovered`, `Active`, `Focused`,
//!   `Selected`).
//! - **Deterministic Zero-Recursion Two-Pass Layout** ([`layout`]): Bottom-up intrinsic sizing +
//!   top-down flex/row/column/stack/scroll distribution + rounded-rect GPU clip table (`clips[]`).
//! - **ECS Binary-Tree Docking** ([`dock`]): Nested splits, live splitters, drag-to-dock
//!   (`Left`/`Right`/`Top`/`Bottom`/`Center` drop zones), maximize/restore, and auto-pruning.
//! - **Rust-Authored GPU Shaders** ([`shaders`]): `#[gpu::vertex]` / `#[gpu::fragment]` /
//!   `#[derive(gpu::Varyings)]` emitted via `gpu::emit` to both **WGSL** (WebGPU) and **MSL 2.4**
//!   (Metal), with fragment-shader rounded-rect clipping (`clips[clip_id]`) for zero scissor breaks.
//! - **Hybrid Dense Plotting** ([`CanvasBatch`]): Ultra-dense GPU flamegraphs, timelines, and
//!   force-directed graphs stream directly into the same instanced SDF & glyph draw calls.
//! - **Self-Hosting Workbench** ([`workbench::WorkbenchApp`]): Live interactive ECS UI workbench
//!   that inspects and edits its own ECS world at 120 FPS.

#![no_std]
#![forbid(unsafe_code)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]

pub mod components;
pub mod dock;
pub mod draw;
pub mod error;
pub mod extract;
pub mod input;
pub mod layout;
pub mod shaders;
pub mod slug;
pub mod slug_shader;
pub mod style;
pub mod types;
pub mod ui;
pub mod view;
pub mod virtual_list;
pub mod workbench;

#[cfg(test)]
mod tests;

pub use components::{
    ActionTag, Active, BadgeTone, CanvasBatch, ChildOrder, ComputedRect, Disabled, DockedIn,
    Focusable, Focused, FrameStats, GpuSlot, Hidden, Hovered, Inspects, Interactive, IntrinsicSize,
    LabelText, ScrollOffset, Selected, ShapedGlyph, ShapedText, SliderState, SubText,
    TextInputState, TextStyle, ToggleState, TooltipFor, TreeRowState, WidgetKind,
    register_ui_schema,
};
pub use view::{ViewNode, ViewScope, extract_slug_insts, measure_ui_slug_text};
pub use dock::{
    DOCK_HEADER_H, DOCK_SPLIT_GAP, DockDrag, DockLeaf, DockLeafRect, DockSplit, DockSplitterRect,
    DockTab, DockWorkspace, DropZone, SplitDir,
};
pub use draw::{
    ATLAS_COLS, ATLAS_FIRST_CHAR, ATLAS_LAST_CHAR, ATLAS_ROWS, DrawList, GpuPatch,
    culled_glyph_inst, culled_rect_inst,
};
pub use error::UiError;
pub use input::{CursorHint, InputEvent, InputState, Key, UiEvent};
pub use layout::{LayoutEngine, LayoutNode, is_layout_boundary};
pub use shaders::{
    ClipRectGpu, GlyphInst, GlyphVaryings, RectInst, SdfVaryings, ViewportUniform,
    emit_ui_glyph_msl, emit_ui_glyph_wgsl, emit_ui_sdf_msl, emit_ui_sdf_wgsl, prim_kind,
};
pub use style::{Align, Direction, Justify, LayoutStyle, PaintStyle, Sizing, StylePrefabs};
pub use types::{Edges, Rect, Rgba, Theme, ThemeKind, Vec2};
pub use ui::UiContext;
pub use virtual_list::{VirtualList, VisibleRange};
pub use workbench::WorkbenchApp;
