//! SwiftUI-style declarative ECS view primitives (`VStack`, `HStack`, `ZStack`, `Text`,
//! `ShapedText`, `Spacer`, `Divider`, `RoundedRect`, `Circle`) built directly on
//! `blaze_ecs::World` + `blaze_ui::LayoutEngine` + `gpu::slug` (`SiteInst`).
//!
//! Every primitive spawns a first-class [`blaze_ecs::Entity`] linked via `ChildOf` + [`ChildOrder`]
//! with [`LayoutStyle`], [`PaintStyle`], and text/vector data stored directly as ECS components.
//! Zero external state, zero hardcoded pixel positions, zero `unsafe`, zero recursion.

use core::alloc::Allocator;

use blaze_ecs::{ChildOf, Entity, World};
use blaze_own::{String as OwnString, Vec as OwnVec};
use gpu::prelude::{UVec4, Vec4, uvec4, vec4};

use crate::components::{
    ChildOrder, ComputedRect, IntrinsicSize, LabelText, ShapedGlyph, ShapedText, TextStyle,
    WidgetKind,
};
use crate::error::UiError;
use crate::slug::SlugFontTable;
use crate::slug_shader::{SiteInst, site_kind};
use crate::style::{Align, Direction, Justify, LayoutStyle, PaintStyle, Sizing};
use crate::types::{Edges, Rgba, Theme, Vec2};

const ZERO_V4: Vec4 = Vec4::splat(0.0);
const ZERO_U4: UVec4 = UVec4::splat(0);

/// Scoped SwiftUI-style ECS builder bound to an optional `ChildOf` parent entity.
pub struct ViewScope<'w, A: Allocator + Copy + 'static> {
    world: &'w mut World<A>,
    parent: Option<Entity>,
    next_order: u32,
    alloc: A,
}

impl<'w, A: Allocator + Copy + 'static> ViewScope<'w, A> {
    /// Start a root-level SwiftUI ECS scope (entities spawned here have no parent).
    #[inline]
    pub fn root(world: &'w mut World<A>, alloc: A) -> Self {
        Self { world, parent: None, next_order: 0, alloc }
    }

    /// Start a SwiftUI ECS scope attaching all spawned children to `parent`.
    #[inline]
    pub fn under(world: &'w mut World<A>, parent: Entity, alloc: A) -> Self {
        Self { world, parent: Some(parent), next_order: 0, alloc }
    }

    /// Allocator used by this builder.
    #[inline]
    pub fn alloc(&self) -> A {
        self.alloc
    }

    fn spawn_entity(&mut self, layout: LayoutStyle, kind: WidgetKind) -> Result<Entity, UiError> {
        let ent = self.world.spawn()?;
        let ord = self.next_order;
        self.next_order = self.next_order.saturating_add(1);
        self.world.set_copy(ent, layout)?;
        self.world.set_copy(ent, PaintStyle::transparent(&Theme::obsidian()))?;
        self.world.set_copy(ent, kind)?;
        self.world.set_copy(ent, ChildOrder(ord))?;
        if let Some(p) = self.parent {
            self.world.relate::<ChildOf>(ent, p)?;
        }
        Ok(ent)
    }

    /// Vertical stack (`SwiftUI.VStack`): lays out `ChildOf` children top-to-bottom with `gap`.
    pub fn v_stack<F>(&mut self, gap: f32, build: F) -> Result<ViewNode<'_, A>, UiError>
    where
        F: FnOnce(&mut ViewScope<'_, A>) -> Result<(), UiError>,
    {
        let layout = LayoutStyle::column().gap(gap).align(Align::Start);
        let ent = self.spawn_entity(layout, WidgetKind::Container)?;
        let mut child_scope = ViewScope::under(self.world, ent, self.alloc);
        build(&mut child_scope)?;
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// Horizontal stack (`SwiftUI.HStack`): lays out `ChildOf` children left-to-right with `gap`.
    pub fn h_stack<F>(&mut self, gap: f32, build: F) -> Result<ViewNode<'_, A>, UiError>
    where
        F: FnOnce(&mut ViewScope<'_, A>) -> Result<(), UiError>,
    {
        let layout = LayoutStyle::row().gap(gap).align(Align::Center);
        let ent = self.spawn_entity(layout, WidgetKind::Container)?;
        let mut child_scope = ViewScope::under(self.world, ent, self.alloc);
        build(&mut child_scope)?;
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// Overlay stack (`SwiftUI.ZStack`): overlays `ChildOf` children inside the same content box.
    pub fn z_stack<F>(&mut self, build: F) -> Result<ViewNode<'_, A>, UiError>
    where
        F: FnOnce(&mut ViewScope<'_, A>) -> Result<(), UiError>,
    {
        let mut layout = LayoutStyle::default();
        layout.dir = Direction::Stack;
        layout.align = Align::Start;
        layout.justify = Justify::Start;
        let ent = self.spawn_entity(layout, WidgetKind::Container)?;
        let mut child_scope = ViewScope::under(self.world, ent, self.alloc);
        build(&mut child_scope)?;
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// Flexible spacer (`SwiftUI.Spacer`): expands to absorb remaining main-axis space.
    pub fn spacer(&mut self) -> Result<ViewNode<'_, A>, UiError> {
        let layout = LayoutStyle::default().width(Sizing::FILL).height(Sizing::FILL);
        let ent = self.spawn_entity(layout, WidgetKind::Container)?;
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// Thin visual divider (`SwiftUI.Divider`) painted as a crisp SDF rect.
    pub fn divider(&mut self, width: f32, height: f32, color: Rgba) -> Result<ViewNode<'_, A>, UiError> {
        let layout = LayoutStyle::default().width(Sizing::Fixed(width)).height(Sizing::Fixed(height));
        let ent = self.spawn_entity(layout, WidgetKind::Container)?;
        if let Some(p) = self.world.get_mut::<PaintStyle>(ent)? {
            p.bg = color;
        }
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// Pure SDF rounded rectangle (`SwiftUI.RoundedRectangle`).
    pub fn rounded_rect(&mut self, radius: f32) -> Result<ViewNode<'_, A>, UiError> {
        let ent = self.spawn_entity(LayoutStyle::default(), WidgetKind::Container)?;
        if let Some(p) = self.world.get_mut::<PaintStyle>(ent)? {
            p.radius = radius;
        }
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// Pure SDF circle (`SwiftUI.Circle`) of given diameter and fill color.
    pub fn circle(&mut self, diameter: f32, color: Rgba) -> Result<ViewNode<'_, A>, UiError> {
        let layout = LayoutStyle::default()
            .width(Sizing::Fixed(diameter))
            .height(Sizing::Fixed(diameter));
        let ent = self.spawn_entity(layout, WidgetKind::Container)?;
        if let Some(p) = self.world.get_mut::<PaintStyle>(ent)? {
            p.bg = color;
            p.radius = diameter * 0.5;
        }
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// SwiftUI-style `Text` entity storing `LabelText<A>` + `TextStyle` + Slug-measured `IntrinsicSize`.
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &mut self,
        content: &str,
        size: f32,
        family: u32,
        weight: u32,
        opsz: f32,
        color: Rgba,
        table: &SlugFontTable<A>,
    ) -> Result<ViewNode<'_, A>, UiError> {
        let ent = self.spawn_entity(LayoutStyle::default(), WidgetKind::Label)?;
        let owned = OwnString::try_from_str_in(content, self.alloc)?;
        let measured_w = measure_ui_slug_text(table, content, size, family, weight, opsz);
        self.world.set(ent, LabelText(owned))?;
        self.world.set_copy(ent, TextStyle { size, family, weight, opsz, color })?;
        self.world.set_copy(ent, IntrinsicSize(Vec2::new(measured_w, size * 1.24)))?;
        Ok(ViewNode { world: self.world, entity: ent })
    }

    /// HarfBuzz-shaped vector text entity (`ShapedText<A>`) measured accurately in em units.
    pub fn shaped_text(
        &mut self,
        glyphs: OwnVec<ShapedGlyph, A>,
        size: f32,
        adv_em: f32,
    ) -> Result<ViewNode<'_, A>, UiError> {
        let ent = self.spawn_entity(LayoutStyle::default(), WidgetKind::Label)?;
        let w = (adv_em * size).max(0.0);
        let h = size * 1.24;
        self.world.set(ent, ShapedText { glyphs, size, adv_em })?;
        self.world.set_copy(ent, IntrinsicSize(Vec2::new(w, h)))?;
        Ok(ViewNode { world: self.world, entity: ent })
    }
}

/// Fluent SwiftUI-style modifier handle for an ECS view entity.
pub struct ViewNode<'w, A: Allocator + Copy + 'static> {
    world: &'w mut World<A>,
    entity: Entity,
}

impl<'w, A: Allocator + Copy + 'static> ViewNode<'w, A> {
    /// Underlying ECS [`Entity`] handle.
    #[inline]
    pub fn id(&self) -> Entity {
        self.entity
    }

    /// Uniform padding around children (`SwiftUI .padding(v)`).
    pub fn padding(self, all: f32) -> Result<Self, UiError> {
        self.padding_edges(Edges::all(all))
    }

    /// Horizontal & vertical symmetric padding (`SwiftUI .padding(.horizontal/.vertical)`).
    pub fn padding_xy(self, horizontal: f32, vertical: f32) -> Result<Self, UiError> {
        self.padding_edges(Edges::xy(horizontal, vertical))
    }

    /// Explicit per-edge padding (`top`, `right`, `bottom`, `left`).
    pub fn padding_edges(self, edges: Edges) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.padding = edges;
        }
        Ok(self)
    }

    /// Fixed logical width & height (`SwiftUI .frame(width:height:)`).
    pub fn frame(self, width: f32, height: f32) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.width = Sizing::Fixed(width);
            l.height = Sizing::Fixed(height);
        }
        Ok(self)
    }

    /// Explicit width sizing rule.
    pub fn width(self, width: Sizing) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.width = width;
        }
        Ok(self)
    }

    /// Explicit height sizing rule.
    pub fn height(self, height: Sizing) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.height = height;
        }
        Ok(self)
    }

    /// Expand to fill parent width & height.
    pub fn fill(self) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.width = Sizing::FILL;
            l.height = Sizing::FILL;
        }
        Ok(self)
    }

    /// Expand to fill parent width while keeping intrinsic height.
    pub fn fill_width(self) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.width = Sizing::FILL;
        }
        Ok(self)
    }

    /// Cross-axis alignment (`SwiftUI alignment:`).
    pub fn alignment(self, align: Align) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.align = align;
        }
        Ok(self)
    }

    /// Main-axis justification (and vertical alignment inside `ZStack`).
    pub fn justify(self, justify: Justify) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.justify = justify;
        }
        Ok(self)
    }

    /// Position absolutely relative to parent's top-left corner (`SwiftUI .offset(x:y:)`).
    pub fn offset(self, x: f32, y: f32) -> Result<Self, UiError> {
        if let Some(l) = self.world.get_mut::<LayoutStyle>(self.entity)? {
            l.abs_pos = Some(Vec2::new(x, y));
        }
        Ok(self)
    }

    /// Solid SDF fill background (`SwiftUI .background(color)`).
    pub fn background(self, color: Rgba) -> Result<Self, UiError> {
        if let Some(p) = self.world.get_mut::<PaintStyle>(self.entity)? {
            p.bg = color;
        }
        Ok(self)
    }

    /// Rounded-rect corner radius (`SwiftUI .cornerRadius(r)`).
    pub fn corner_radius(self, radius: f32) -> Result<Self, UiError> {
        if let Some(p) = self.world.get_mut::<PaintStyle>(self.entity)? {
            p.radius = radius;
        }
        Ok(self)
    }

    /// Crisp SDF stroke border (`SwiftUI .overlay(RoundedRectangle().stroke(...))`).
    pub fn border(self, color: Rgba, width: f32) -> Result<Self, UiError> {
        if let Some(p) = self.world.get_mut::<PaintStyle>(self.entity)? {
            p.border = color;
            p.border_width = width;
        }
        Ok(self)
    }

    /// Attach an ECS marker tag `T` to this view entity.
    pub fn tag<T: 'static>(self) -> Result<Self, UiError> {
        self.world.add_tag_typed::<T>(self.entity)?;
        Ok(self)
    }
}

/// Measure a plain UTF-8 string using `SlugFontTable` outline bounds (matches `extract_slug_insts`).
pub fn measure_ui_slug_text<A: Allocator + Copy>(
    table: &SlugFontTable<A>,
    text: &str,
    size: f32,
    family: u32,
    wght: u32,
    opsz: f32,
) -> f32 {
    let Some(inst) = table.find_instance(family, wght, opsz).copied() else {
        let len = u16::try_from(text.chars().count()).map_or(0.0, f32::from);
        return len * size * 0.52;
    };
    let mut cx = 0.0f32;
    for ch in text.chars() {
        if ch == ' ' {
            cx += if family == 1 { 0.60 * size } else { 0.28 * size };
            continue;
        }
        if let Some(gid) = table.lookup_glyph(&inst, ch) {
            if let Some(meta) = table.glyphs.get(usize::try_from(gid).unwrap_or(usize::MAX)).copied() {
                let adv = if family == 1 {
                    0.56
                } else {
                    (meta.bbox[2].max(0.30) + 0.065).clamp(0.24, 0.92)
                };
                cx += adv * size;
                continue;
            }
        }
        cx += 0.50 * size;
    }
    cx
}

/// Extract Rust GPU Slug + SDF [`SiteInst`] quads directly from a laid-out SwiftUI ECS tree
/// (`ComputedRect` + `PaintStyle` + `ShapedText<A>` / `LabelText<A>` + `TextStyle`).
///
/// Returns the total number of Slug vector glyphs emitted.
pub fn extract_slug_insts<A: Allocator + Copy + 'static>(
    world: &World<A>,
    paint_order: &[Entity],
    table: &SlugFontTable<A>,
    insts: &mut OwnVec<SiteInst, A>,
) -> Result<u32, UiError> {
    let mut glyph_count: u32 = 0;

    for &ent in paint_order {
        let Some(cr) = world.get::<ComputedRect>(ent)?.copied() else {
            continue;
        };
        let r = cr.rect;
        if r.is_empty() {
            continue;
        }

        // 1. SDF background fill (`PLATE`) & stroke border (`RING`)
        if let Some(paint) = world.get::<PaintStyle>(ent)?.copied() {
            let rad = paint.radius.min(r.w * 0.5).min(r.h * 0.5);
            if !paint.bg.is_transparent() {
                insts
                    .try_push(SiteInst {
                        rect: vec4(r.x, r.y, r.w, r.h),
                        a: vec4(rad, rad, rad, rad),
                        color: vec4(paint.bg.0[0], paint.bg.0[1], paint.bg.0[2], paint.bg.0[3]),
                        fx: vec4(site_kind::PLATE, 0.0, 0.0, 0.0),
                        band_tf: ZERO_V4,
                        gdata: ZERO_U4,
                    })
                    .map_err(|e| UiError::Alloc(e.why))?;
            }
            if paint.border_width > 0.0 && !paint.border.is_transparent() {
                insts
                    .try_push(SiteInst {
                        rect: vec4(r.x, r.y, r.w, r.h),
                        a: vec4(rad, rad, rad, rad),
                        color: vec4(
                            paint.border.0[0],
                            paint.border.0[1],
                            paint.border.0[2],
                            paint.border.0[3],
                        ),
                        fx: vec4(site_kind::RING, 0.0, paint.border_width, 0.0),
                        band_tf: ZERO_V4,
                        gdata: ZERO_U4,
                    })
                    .map_err(|e| UiError::Alloc(e.why))?;
            }
        }

        // 2. HarfBuzz-shaped Slug vector text (`ShapedText<A>`)
        if let Some(shaped) = world.get::<ShapedText<A>>(ent)? {
            let size = shaped.size;
            let pad_em = 1.0 / size.max(1.0);
            let baseline_y = cr.content_rect.y + size * 0.90;
            let start_x = cr.content_rect.x;
            for g in shaped.glyphs.as_slice() {
                let gi = usize::try_from(g.glyph_id).unwrap_or(usize::MAX);
                let Some(meta) = table.glyphs.get(gi).copied() else { continue };
                if !meta.drawable {
                    continue;
                }
                let gx = start_x + g.x_em * size;
                let x0 = meta.bbox[0] - pad_em;
                let y0 = meta.bbox[1] - pad_em;
                let x1 = meta.bbox[2] + pad_em;
                let y1 = meta.bbox[3] + pad_em;
                glyph_count = glyph_count.saturating_add(1);
                insts
                    .try_push(SiteInst {
                        rect: vec4(
                            gx + x0 * size,
                            baseline_y - y1 * size,
                            (x1 - x0) * size,
                            (y1 - y0) * size,
                        ),
                        a: vec4(x0, y1, x1 - x0, y0 - y1),
                        color: vec4(g.color.0[0], g.color.0[1], g.color.0[2], g.color.0[3]),
                        fx: vec4(site_kind::SLUG_GLYPH, 0.0, 0.0, 0.0),
                        band_tf: vec4(
                            meta.band_tf[0],
                            meta.band_tf[1],
                            meta.band_tf[2],
                            meta.band_tf[3],
                        ),
                        gdata: uvec4(meta.gdata[0], meta.gdata[1], meta.gdata[2], meta.gdata[3]),
                    })
                    .map_err(|e| UiError::Alloc(e.why))?;
            }
            continue;
        }

        // 3. Plain UTF-8 `Text` entity (`LabelText<A>` + `TextStyle`)
        if let (Some(label), Some(ts)) = (world.get::<LabelText<A>>(ent)?, world.get::<TextStyle>(ent)?.copied()) {
            let Some(inst) = table.find_instance(ts.family, ts.weight, ts.opsz).copied() else {
                continue;
            };
            let size = ts.size;
            let pad_em = 1.0 / size.max(1.0);
            let baseline_y = cr.content_rect.y + size * 0.90;
            let color_v = vec4(ts.color.0[0], ts.color.0[1], ts.color.0[2], ts.color.0[3]);
            let mut cx = cr.content_rect.x;
            for ch in label.0.as_str().chars() {
                if ch == ' ' {
                    cx += if ts.family == 1 { 0.56 * size } else { 0.28 * size };
                    continue;
                }
                if let Some(gid) = table.lookup_glyph(&inst, ch) {
                    if let Some(meta) = table.glyphs.get(usize::try_from(gid).unwrap_or(usize::MAX)).copied() {
                        if meta.drawable {
                            let x0 = meta.bbox[0] - pad_em;
                            let y0 = meta.bbox[1] - pad_em;
                            let x1 = meta.bbox[2] + pad_em;
                            let y1 = meta.bbox[3] + pad_em;
                            glyph_count = glyph_count.saturating_add(1);
                            insts
                                .try_push(SiteInst {
                                    rect: vec4(
                                        cx + x0 * size,
                                        baseline_y - y1 * size,
                                        (x1 - x0) * size,
                                        (y1 - y0) * size,
                                    ),
                                    a: vec4(x0, y1, x1 - x0, y0 - y1),
                                    color: color_v,
                                    fx: vec4(site_kind::SLUG_GLYPH, 0.0, 0.0, 0.0),
                                    band_tf: vec4(
                                        meta.band_tf[0],
                                        meta.band_tf[1],
                                        meta.band_tf[2],
                                        meta.band_tf[3],
                                    ),
                                    gdata: uvec4(
                                        meta.gdata[0],
                                        meta.gdata[1],
                                        meta.gdata[2],
                                        meta.gdata[3],
                                    ),
                                })
                                .map_err(|e| UiError::Alloc(e.why))?;
                        }
                        let adv = if ts.family == 1 {
                            0.56
                        } else {
                            (meta.bbox[2].max(0.30) + 0.065).clamp(0.24, 0.92)
                        };
                        cx += adv * size;
                        continue;
                    }
                }
                cx += 0.50 * size;
            }
        }
    }

    Ok(glyph_count)
}
