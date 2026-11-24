//! Pure `#![no_std]` Open Graph (`1200×630`) social-card generator driven 100% by `blaze_ecs::World`
//! + SwiftUI-style ECS view primitives (`blaze_ui::ViewScope`: `ZStack`, `VStack`, `HStack`,
//! `Spacer`, `Divider`, `RoundedRect`, `Circle`, `Text`, `ShapedText`) + `blaze_ui::LayoutEngine`
//! + our ACTUAL Rust GPU Slug + SDF fragment shader (`gpu::slug::slug_site_fs`).
//!
//! - Zero `&PageModel` access — ALL text, glyphs, roles, icons, and metadata are queried directly
//!   from `blaze_ecs::World` via `ecs_query!` and `world.singleton::<SiteMeta<A>>()`.
//! - Zero hardcoded pixel coordinates — layout is solved by `blaze_ui::LayoutEngine` over SwiftUI-
//!   style ECS entities.
//! - Zero headless browser, zero external image crates, zero `as` casts, zero stops, zero recursion!

use core::alloc::Allocator;

use blaze_ecs::{ChildOf, World, ecs_query};
use blaze_own::{String as OwnString, Vec as OwnVec};
use blaze_ui::slug::SlugFontTable;
use blaze_ui::slug_shader::{SiteInst, SiteUniform, SlugSiteVaryings, slug_site_fs};
use blaze_ui::{
    Align, ChildOrder, DrawList, Edges, Justify, LayoutEngine, Rect, Rgba, ShapedGlyph, Sizing,
    ViewScope, extract_slug_insts,
};
use gpu::prelude::{Flat, Position, Sampler, Texture2d, Vec4, vec2, vec4};

use crate::error::SiteError;
use crate::model::{
    EcsWords, SiteBacked, SiteCopyLabel, SiteFeatureItem, SiteFeatureList, SiteHeroTitle,
    SiteInstallCmd, SiteMeta, SiteWordmark, Word,
};
use crate::palette::{ImageName, RoleName, SiteTheme};

pub const OG_WIDTH: u32 = 1200;
pub const OG_HEIGHT: u32 = 630;

/// Telemetry returned by [`render_og_card_rgba`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OgStats {
    pub width: u32,
    pub height: u32,
    pub ecs_entities_queried: u32,
    pub view_entities_laid_out: u32,
    pub inst_count: u32,
    pub glyph_count: u32,
}

struct ShapedRun<A: Allocator + Copy> {
    glyphs: OwnVec<ShapedGlyph, A>,
    adv_em: f32,
}

impl<A: Allocator + Copy> ShapedRun<A> {
    fn empty_in(alloc: A) -> Self {
        Self { glyphs: OwnVec::new_in(alloc), adv_em: 0.0 }
    }

    fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }
}

/// Convert a slice of ECS-stored [`Word`]s into a [`ShapedRun`] ready for SwiftUI `ShapedText`.
fn pack_words_run<A: Allocator + Copy>(
    words: &[Word<A>],
    theme: SiteTheme,
    override_ink: Option<Rgba>,
    keep_role_colors: bool,
    alloc: A,
) -> Result<ShapedRun<A>, SiteError> {
    let mut glyphs: OwnVec<ShapedGlyph, A> = OwnVec::new_in(alloc);
    let mut x_em = 0.0f32;
    let mut last_drawn_end = 0.0f32;

    for (wi, w) in words.iter().enumerate() {
        if w.image.is_some() {
            continue;
        }
        let col = if keep_role_colors {
            theme.color(w.role.spec().ink)
        } else {
            override_ink.unwrap_or_else(|| theme.color(w.role.spec().ink))
        };
        for k in 0..w.glyphs.len() {
            let g = w.glyphs.get(k).copied().unwrap_or(-1);
            if g < 0 {
                continue;
            }
            let gid = u32::try_from(g).unwrap_or(0);
            let gx = x_em + w.xs.get(k).copied().unwrap_or(0.0);
            glyphs
                .try_push(ShapedGlyph { glyph_id: gid, x_em: gx, color: col })
                .map_err(|r| SiteError::Alloc(r.why))?;
        }
        x_em += w.adv;
        last_drawn_end = x_em;
        if wi + 1 < words.len() && w.space > 0.0 {
            x_em += w.space;
        }
    }
    Ok(ShapedRun { glyphs, adv_em: last_drawn_end })
}

/// Query ALL card content directly from `blaze_ecs::World` (`ecs_query!` + `singleton`), compose
/// the `1200×630` card using SwiftUI-style ECS primitives (`ViewScope`), lay it out with
/// `blaze_ui::LayoutEngine`, and extract Rust GPU Slug/SDF [`SiteInst`] quads.
pub fn build_og_scene<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    table: &SlugFontTable<A>,
    dark: bool,
    insts: &mut OwnVec<SiteInst, A>,
    alloc: A,
) -> Result<OgStats, SiteError> {
    insts.clear();
    let theme = if dark { SiteTheme::dark() } else { SiteTheme::light() };
    let mut queried: u32 = 0;

    // -------------------------------------------------------------------------
    // 1. Query ALL text, glyph runs, icons, and metadata strictly FROM the ECS!
    // -------------------------------------------------------------------------
    let mut wordmark_run = ShapedRun::empty_in(alloc);
    ecs_query!(world, &EcsWords<A>, tag(SiteWordmark) => |w| {
        queried = queried.saturating_add(1);
        if wordmark_run.is_empty() {
            if let Ok(run) = pack_words_run(w.0.as_slice(), theme, Some(theme.ink_1), false, alloc) {
                wordmark_run = run;
            }
        }
    })?;

    // Backed-by stream (e.g. `Backed by [yc.svg] Y Combinator`): split cleanly at `ImageName::Yc`!
    let mut backed_before = ShapedRun::empty_in(alloc);
    let mut backed_after = ShapedRun::empty_in(alloc);
    let mut has_yc_badge = false;
    let mut yc_letter = OwnString::new_in(alloc);
    ecs_query!(world, &EcsWords<A>, tag(SiteBacked) => |w| {
        queried = queried.saturating_add(1);
        let slice = w.0.as_slice();
        let yc_pos = slice.iter().position(|wd| wd.image == Some(ImageName::Yc));
        if let Some(pos) = yc_pos {
            has_yc_badge = true;
            if let Some(before) = slice.get(..pos) {
                if let Ok(run) = pack_words_run(before, theme, Some(theme.ink_3), false, alloc) {
                    backed_before = run;
                }
            }
            if let Some(after) = slice.get(pos + 1..) {
                // Extract the single-letter monogram from the ECS words (e.g. `"Y"` in `"Backed by Y Combinator"`).
                if let Some(mono) = after.iter().find(|wd| wd.text.as_str().chars().count() == 1) {
                    if let Some(ch) = mono.text.as_str().chars().next() {
                        let _ = yc_letter.try_push(ch);
                    }
                } else if let Some(first_after) = after.first() {
                    if let Some(ch) = first_after.text.as_str().chars().next() {
                        let _ = yc_letter.try_push(ch);
                    }
                }
                if let Ok(run) = pack_words_run(after, theme, Some(theme.ink_3), false, alloc) {
                    backed_after = run;
                }
            }
        } else if let Ok(run) = pack_words_run(slice, theme, Some(theme.ink_3), false, alloc) {
            backed_before = run;
        }
    })?;

    // Domain string from Flecs-style ECS singleton `SiteMeta<A>`
    let domain_str = match world.singleton::<SiteMeta<A>>()? {
        Some(meta) => {
            queried = queried.saturating_add(1);
            OwnString::try_from_str_in(meta.domain.as_str(), alloc)?
        }
        None => OwnString::new_in(alloc),
    };

    // Hero headline from `SiteHeroTitle` (automatically stripping trailing superscript footnote mark)
    let mut hero_run = ShapedRun::empty_in(alloc);
    ecs_query!(world, &EcsWords<A>, tag(SiteHeroTitle) => |w| {
        queried = queried.saturating_add(1);
        if hero_run.is_empty() {
            let slice = w.0.as_slice();
            let mut end = slice.len();
            if slice.last().is_some_and(|wd| wd.role == RoleName::TitleMark || wd.raise > 0.0) {
                end = end.saturating_sub(1);
            }
            if let Some(trimmed) = slice.get(..end) {
                if let Ok(run) = pack_words_run(trimmed, theme, Some(theme.ink_1), false, alloc) {
                    hero_run = run;
                }
            }
        }
    })?;

    // Install command from `SiteInstallCmd` (preserving role syntax colors!)
    let mut cmd_run = ShapedRun::empty_in(alloc);
    ecs_query!(world, &EcsWords<A>, tag(SiteInstallCmd) => |w| {
        queried = queried.saturating_add(1);
        if cmd_run.is_empty() {
            if let Ok(run) = pack_words_run(w.0.as_slice(), theme, None, true, alloc) {
                cmd_run = run;
            }
        }
    })?;

    // Copy button label from `SiteCopyLabel`
    let mut copy_run = ShapedRun::empty_in(alloc);
    ecs_query!(world, &EcsWords<A>, tag(SiteCopyLabel) => |w| {
        queried = queried.saturating_add(1);
        if copy_run.is_empty() {
            if let Ok(run) = pack_words_run(w.0.as_slice(), theme, Some(theme.ink_2), false, alloc) {
                copy_run = run;
            }
        }
    })?;

    // Top 3 feature headlines queried via `(ChildOf, list_ent)` + `SiteFeatureItem` ordered by `ChildOrder`!
    let mut bullet_runs: OwnVec<(u32, ShapedRun<A>), A> = OwnVec::new_in(alloc);
    let list_ent = ecs_query!(world, &ChildOrder, tag(SiteFeatureList))?.first_entity()?;
    if let Some(list_e) = list_ent {
        queried = queried.saturating_add(1);
        ecs_query!(world, &EcsWords<A>, &ChildOrder, tag(SiteFeatureItem), (ChildOf, list_e) => |w, ord| {
            queried = queried.saturating_add(1);
            if let Ok(run) = pack_words_run(w.0.as_slice(), theme, Some(theme.ink_2), false, alloc) {
                let _ = bullet_runs.try_push((ord.0, run));
            }
        })?;
        // Insertion sort by `ChildOrder` (stable, deterministic, zero stops)
        let mut i = 1usize;
        while i < bullet_runs.len() {
            let mut j = i;
            while j > 0 {
                let swap = match (bullet_runs.get(j - 1), bullet_runs.get(j)) {
                    (Some(a), Some(b)) => a.0 > b.0,
                    _ => false,
                };
                if !swap {
                    break;
                }
                bullet_runs.as_mut_slice().swap(j - 1, j);
                j -= 1;
            }
            i += 1;
        }
    }

    // -------------------------------------------------------------------------
    // 2. Compose the 1200×630 card with SwiftUI-style ECS primitives!
    // -------------------------------------------------------------------------
    let mut view_world = World::new_in(alloc)?;
    let card_inner_w = 1200.0 - 2.0 * 28.0 - 2.0 * 56.0; // 1032px
    let title_size = if hero_run.adv_em > 0.01 {
        (card_inner_w / hero_run.adv_em).min(64.0)
    } else {
        62.0
    };
    let cmd_avail_w = card_inner_w - 172.0; // leave room for padding + Copy pill
    let cmd_size = if cmd_run.adv_em > 0.01 {
        (cmd_avail_w / cmd_run.adv_em).min(25.0)
    } else {
        25.0
    };
    let yc_orange = Rgba::new(251.0 / 255.0, 101.0 / 255.0, 30.0 / 255.0, 1.0);

    let mut scope = ViewScope::root(&mut view_world, alloc);
    let card_root = scope
        .z_stack(|outer| {
            // Main obsidian card container (`VStack`)
            outer
                .v_stack(0.0, |col| {
                    // Header row (`HStack`): wordmark | [Y] Backed by Y Combinator  <Spacer>  domain
                    col.h_stack(16.0, |hdr| {
                        if !wordmark_run.is_empty() {
                            hdr.shaped_text(wordmark_run.glyphs, 30.0, wordmark_run.adv_em)?;
                        }
                        if !backed_before.is_empty() || has_yc_badge || !backed_after.is_empty() {
                            hdr.divider(1.5, 22.0, theme.line_2)?;
                            hdr.h_stack(9.0, |bk| {
                                if !backed_before.is_empty() {
                                    bk.shaped_text(backed_before.glyphs, 20.0, backed_before.adv_em)?;
                                }
                                if has_yc_badge {
                                    bk.z_stack(|badge| {
                                        if !yc_letter.is_empty() {
                                            badge.text(
                                                yc_letter.as_str(),
                                                15.5,
                                                0,
                                                600,
                                                16.0,
                                                Rgba::WHITE,
                                                table,
                                            )?;
                                        }
                                        Ok(())
                                    })?
                                    .frame(23.0, 23.0)?
                                    .alignment(Align::Center)?
                                    .justify(Justify::Center)?
                                    .background(yc_orange)?
                                    .corner_radius(5.0)?;
                                }
                                if !backed_after.is_empty() {
                                    bk.shaped_text(backed_after.glyphs, 20.0, backed_after.adv_em)?;
                                }
                                Ok(())
                            })?;
                        }
                        hdr.spacer()?;
                        if !domain_str.is_empty() {
                            hdr.text(
                                domain_str.as_str(),
                                20.0,
                                1,
                                400,
                                0.0,
                                theme.ink_3,
                                table,
                            )?;
                        }
                        Ok(())
                    })?
                    .fill_width()?;

                    // Generous vertical rhythm before hero headline
                    col.spacer()?;

                    // Hero headline (`ShapedText` queried straight from ECS `SiteHeroTitle`)
                    if !hero_run.is_empty() {
                        col.shaped_text(hero_run.glyphs, title_size, hero_run.adv_em)?;
                    }

                    // Fixed gap before install command box
                    col.rounded_rect(0.0)?.frame(1.0, 28.0)?;

                    // Install command box (`HStack` with rounded plate + stroke + `Spacer` + Copy pill)
                    col.h_stack(16.0, |cmd_box| {
                        if !cmd_run.is_empty() {
                            cmd_box.shaped_text(cmd_run.glyphs, cmd_size, cmd_run.adv_em)?;
                        }
                        cmd_box.spacer()?;
                        if !copy_run.is_empty() {
                            cmd_box
                                .z_stack(|pill| {
                                    pill.shaped_text(copy_run.glyphs, 18.5, copy_run.adv_em)?;
                                    Ok(())
                                })?
                                .padding_xy(22.0, 10.0)?
                                .alignment(Align::Center)?
                                .justify(Justify::Center)?
                                .background(theme.s_tint)?
                                .corner_radius(11.0)?;
                        }
                        Ok(())
                    })?
                    .fill_width()?
                    .height(Sizing::Fixed(82.0))?
                    .padding_edges(Edges::new(0.0, 16.0, 0.0, 28.0))?
                    .background(theme.bg)?
                    .border(theme.line_2, 1.5)?
                    .corner_radius(16.0)?;

                    col.spacer()?;

                    // Top 3 ECS feature items (`VStack` of `HStack { Circle, ShapedText }`)
                    col.v_stack(16.0, |list_v| {
                        let take_n = bullet_runs.len().min(3);
                        bullet_runs.truncate(take_n);
                        bullet_runs.as_mut_slice().reverse();
                        let mut idx = 0usize;
                        while let Some((_, run)) = bullet_runs.pop() {
                            let dot_col = if idx == 0 { theme.fire } else { theme.ink_4 };
                            list_v.h_stack(16.0, |row| {
                                row.circle(8.0, dot_col)?;
                                row.shaped_text(run.glyphs, 25.0, run.adv_em)?;
                                Ok(())
                            })?;
                            idx += 1;
                        }
                        Ok(())
                    })?
                    .fill_width()?;

                    Ok(())
                })?
                .padding_edges(Edges::new(48.0, 56.0, 52.0, 56.0))?
                .fill()?
                .background(theme.bg_2.with_alpha(0.55))?
                .border(theme.line, 1.5)?
                .corner_radius(22.0)?;

            // Warm `--fire` ember accent line sitting flush on top-left of card border
            outer
                .rounded_rect(1.5)?
                .frame(96.0, 3.0)?
                .background(theme.fire)?
                .offset(84.0, 27.5)?;

            Ok(())
        })?
        .padding(28.0)?
        .fill()?
        .id();

    // -------------------------------------------------------------------------
    // 3. Run `blaze_ui::LayoutEngine` on our SwiftUI ECS view tree & extract Slug quads!
    // -------------------------------------------------------------------------
    let mut layout = LayoutEngine::new_in(alloc);
    let mut draw = DrawList::new_in(alloc);
    let view_nodes = layout.compute(
        &mut view_world,
        card_root,
        Rect::new(0.0, 0.0, f32::from(u16::try_from(OG_WIDTH).unwrap_or(1200)), f32::from(u16::try_from(OG_HEIGHT).unwrap_or(630))),
        &mut draw,
    )?;
    let glyph_count = extract_slug_insts(&view_world, layout.paint_order.as_slice(), table, insts)?;

    Ok(OgStats {
        width: OG_WIDTH,
        height: OG_HEIGHT,
        ecs_entities_queried: queried,
        view_entities_laid_out: view_nodes,
        inst_count: u32::try_from(insts.len()).unwrap_or(0),
        glyph_count,
    })
}

/// Render the `1200×630` Open Graph card into an `RGBA8` pixel buffer by executing our **actual
/// Rust GPU fragment shader** (`gpu::slug::slug_site_fs`) directly on the CPU!
pub fn render_og_card_rgba<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    table: &SlugFontTable<A>,
    dark: bool,
    rgba_out: &mut OwnVec<u8, A>,
    alloc: A,
) -> Result<OgStats, SiteError> {
    let mut insts: OwnVec<SiteInst, A> = OwnVec::new_in(alloc);
    let stats = build_og_scene(world, table, dark, &mut insts, alloc)?;

    let w = usize::try_from(OG_WIDTH).unwrap_or(1200);
    let h = usize::try_from(OG_HEIGHT).unwrap_or(630);
    let total_bytes = w.saturating_mul(h).saturating_mul(4);
    rgba_out.clear();
    rgba_out.try_reserve(total_bytes)?;

    let theme = if dark { SiteTheme::dark() } else { SiteTheme::light() };
    let bg_r = float_to_u8(theme.bg.0[0]);
    let bg_g = float_to_u8(theme.bg.0[1]);
    let bg_b = float_to_u8(theme.bg.0[2]);
    for _ in 0..w.saturating_mul(h) {
        rgba_out.try_push(bg_r).map_err(|r| SiteError::Alloc(r.why))?;
        rgba_out.try_push(bg_g).map_err(|r| SiteError::Alloc(r.why))?;
        rgba_out.try_push(bg_b).map_err(|r| SiteError::Alloc(r.why))?;
        rgba_out.try_push(255).map_err(|r| SiteError::Alloc(r.why))?;
    }

    let mut curves_v4: OwnVec<Vec4, A> = OwnVec::try_with_capacity_in(table.curves.len(), alloc)?;
    for c in table.curves.as_slice() {
        curves_v4.try_push(vec4(c[0], c[1], c[2], c[3])).map_err(|r| SiteError::Alloc(r.why))?;
    }
    let tex_px = [Vec4::splat(1.0)];
    let tex = Texture2d::from_rgba32f(1, 1, &tex_px);
    let smp = Sampler::LINEAR_CLAMP;

    let uniform = SiteUniform {
        view_origin: vec4(
            f32::from(u16::try_from(OG_WIDTH).unwrap_or(1200)),
            f32::from(u16::try_from(OG_HEIGHT).unwrap_or(630)),
            0.0,
            0.0,
        ),
        pulse: vec4(0.0, 0.0, 1.0, 0.0),
        pulse_ink: vec4(
            theme.ink_1.0[0],
            theme.ink_1.0[1],
            theme.ink_1.0[2],
            if dark { 1.0 } else { 0.0 },
        ),
        slug: vec4(0.008, 0.88, 1.12, 0.22),
    };

    let fb = rgba_out.as_mut_slice();
    let bands = table.bands.as_slice();
    let curves = curves_v4.as_slice();

    for inst in insts.as_slice() {
        let rx = inst.rect.x;
        let ry = inst.rect.y;
        let rw = inst.rect.z;
        let rh = inst.rect.w;
        if rw <= 0.0 || rh <= 0.0 {
            continue;
        }
        let x0 = floor_clamp_usize(rx, w);
        let y0 = floor_clamp_usize(ry, h);
        let x1 = ceil_clamp_usize(rx + rw, w);
        let y1 = ceil_clamp_usize(ry + rh, h);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }

        let half_e = vec2(rw * 0.5, rh * 0.5);
        let ppem = rw / inst.a.z.abs().max(0.000001);
        let inv_w = 1.0 / rw;
        let inv_h = 1.0 / rh;

        for py in y0..y1 {
            let fy = usize_to_f32(py) + 0.5;
            let ty = (fy - ry) * inv_h;
            let uv_y = inst.a.y + ty * inst.a.w;
            let loc_y = (ty - 0.5) * 2.0 * half_e.y;
            let row_off = py.saturating_mul(w);
            for px in x0..x1 {
                let fx = usize_to_f32(px) + 0.5;
                let tx = (fx - rx) * inv_w;
                let varyings = SlugSiteVaryings {
                    pos: Position(vec4(fx, fy, 0.0, 1.0)),
                    uv: vec2(inst.a.x + tx * inst.a.z, uv_y),
                    local: vec2((tx - 0.5) * 2.0 * half_e.x, loc_y),
                    color: Flat(inst.color),
                    half_ext: Flat(half_e),
                    radii: Flat(inst.a),
                    fx: Flat(inst.fx),
                    band_tf: Flat(inst.band_tf),
                    gdata: Flat(inst.gdata),
                    ppem: Flat(ppem),
                };
                let src = slug_site_fs(varyings, &uniform, curves, bands, &smp, &tex);
                if src.w <= 0.001 {
                    continue;
                }
                let off = row_off.saturating_add(px).saturating_mul(4);
                let inv_a = (1.0 - src.w).clamp(0.0, 1.0);
                if let (Some(&dr), Some(&dg), Some(&db)) =
                    (fb.get(off), fb.get(off + 1), fb.get(off + 2))
                {
                    let nr = (src.x + f32::from(dr) / 255.0 * inv_a).clamp(0.0, 1.0);
                    let ng = (src.y + f32::from(dg) / 255.0 * inv_a).clamp(0.0, 1.0);
                    let nb = (src.z + f32::from(db) / 255.0 * inv_a).clamp(0.0, 1.0);
                    if let Some(s) = fb.get_mut(off) {
                        *s = float_to_u8(nr);
                    }
                    if let Some(s) = fb.get_mut(off + 1) {
                        *s = float_to_u8(ng);
                    }
                    if let Some(s) = fb.get_mut(off + 2) {
                        *s = float_to_u8(nb);
                    }
                }
            }
        }
    }

    Ok(stats)
}

#[inline]
fn float_to_u8(v: f32) -> u8 {
    let scaled = gpu::math::round(v.clamp(0.0, 1.0) * 255.0);
    let mut r: u8 = 0;
    let mut bit: u8 = 128;
    while bit > 0 {
        let cand = r | bit;
        if f32::from(cand) <= scaled {
            r = cand;
        }
        bit >>= 1;
    }
    r
}

fn floor_clamp_usize(v: f32, max: usize) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let f = gpu::math::floor(v);
    f32_to_usize_clamped(f, max)
}

fn ceil_clamp_usize(v: f32, max: usize) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    let f = gpu::math::ceil(v);
    f32_to_usize_clamped(f, max)
}

fn f32_to_usize_clamped(v: f32, max: usize) -> usize {
    let mut r: u16 = 0;
    let mut bit: u16 = 1 << 14;
    while bit > 0 {
        let cand = r | bit;
        if f32::from(cand) <= v {
            r = cand;
        }
        bit >>= 1;
    }
    usize::from(r).min(max)
}

fn usize_to_f32(v: usize) -> f32 {
    let u = u32::try_from(v).unwrap_or(u32::MAX);
    let hi = u16::try_from(u >> 16).unwrap_or(0);
    let lo = u16::try_from(u & 0xffff).unwrap_or(0);
    f32::from(hi) * 65536.0 + f32::from(lo)
}
