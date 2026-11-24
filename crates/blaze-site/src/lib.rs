//! Pure-Blaze front page remake (`#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]`):
//! - Relational ECS (`blaze-ecs` `ChildOf` + Flecs 4.2 `DontFragment` hover/focus/press tags)
//! - Fine-grained glitch-free signals (`blaze_ecs::ReactiveGraph`)
//! - `den` Slug GPU quadratic Bézier outline font rendering via dogfooded Rust GPU (`gpu::slug`)
//! - Smooth cursor-anchored trackpad pinch-to-zoom (`25%`–`1600%`) with razor-sharp typography
//! - Liquid selection shapes, copy pulse, hover cards, scrambled footnotes, and native WebGPU Cmd+F!

#![no_std]
#![forbid(unsafe_code)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]

pub mod error;
pub mod html;
pub mod layout;
pub mod model;
pub mod og;
pub mod palette;
pub mod selshape;
pub mod site;

#[cfg(test)]
mod tests;

pub use blaze_ui::slug_shader::{SiteInst, SiteUniform, emit_slug_site_msl, emit_slug_site_wgsl};
pub use error::SiteError;
pub use html::project_ecs_html;
pub use layout::{CharBox, CharRange, PageLayout, Pic, PlacedGlyph, Shape, ShapeKind, TipLayout, Viewport};
pub use model::{Block, ChipKind, ItemNode, Link, PageModel, SiteEcsHandles, SiteFocused, SiteHovered, SiteNodeKind, SitePressed};
pub use og::{OG_HEIGHT, OG_WIDTH, OgStats, build_og_scene, render_og_card_rgba};
pub use palette::{ImageName, RoleName, SiteTheme, Token};
pub use selshape::{Fillet, PulseState, Row, join_rows, outline_rows, pulse_scale, shape_of};
pub use site::{BlazeSite, CursorKind, MAX_ZOOM, MIN_ZOOM, key};
