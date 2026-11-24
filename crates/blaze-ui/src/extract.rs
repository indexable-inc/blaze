//! GPU Extraction System: maps the laid-out relational ECS UI world + docked workspace into
//! flat `RectInst` / `GlyphInst` / `ClipRectGpu` buffers for WebGPU & Metal.
//!
//! Resolves `PaintStyle` via `world.get_inherited::<PaintStyle>(entity)` so changes to `IsA`
//! Style Prefab entities cascade instantaneously to every widget in the scene.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_ecs::{Entity, World};
use blaze_own::String as OwnString;

use crate::components::{
    Active, BadgeTone, CanvasBatch, ComputedRect, Disabled, Focused, GpuSlot, Hovered, LabelText,
    ScrollOffset, Selected, SliderState, SubText, TextInputState, ToggleState, TreeRowState,
    WidgetKind,
};
use crate::dock::{DOCK_HEADER_H, DockWorkspace, SplitDir};
use crate::draw::DrawList;
use crate::error::UiError;
use crate::style::PaintStyle;
use crate::types::{Rect, Rgba, Theme};

/// Render dock leaf frames, headers, tabs, splitters, and active drag-to-dock overlays.
pub fn extract_dock_chrome<A: Allocator + Copy + 'static>(
    world: &World<A>,
    dock: &DockWorkspace<A>,
    theme: &Theme,
    pointer_x: f32,
    pointer_y: f32,
    draw: &mut DrawList<A>,
) -> Result<(), UiError> {
    draw.set_overlay(false);

    // 1. Draw each dock leaf chrome (background, crisp 1px border, header bar, tabs, expand icon).
    for &leaf_r in dock.leaves.as_slice() {
        draw.rounded_rect_bordered(leaf_r.rect, theme.radius_md, theme.bg2, theme.line, 1.0)?;

        let clip_id = draw.push_clip(leaf_r.rect, theme.radius_md)?;
        let _ = clip_id;

        // Header bar background + bottom hairline
        draw.fill_rect(leaf_r.header, theme.bg3)?;
        draw.line(
            leaf_r.header.x,
            leaf_r.header.bottom(),
            leaf_r.header.right(),
            leaf_r.header.bottom(),
            1.0,
            theme.line,
        )?;

        // Tabs
        let tabs = dock.tabs_of(world, leaf_r.leaf)?;
        let active_idx = if tabs.is_empty() { 0 } else { leaf_r.active_tab.min(tabs.len() - 1) };
        let mut tx = leaf_r.header.x + 6.0;
        let tab_h = (DOCK_HEADER_H - 6.0).max(14.0);

        for (idx, &tab) in tabs.as_slice().iter().enumerate() {
            let name = world.entity_name(tab).unwrap_or("Pane");
            let chars = u16::try_from(name.chars().count()).map_or(10.0, f32::from);
            let tw = (chars * 7.2 + 22.0).clamp(58.0, 160.0);
            let tr = Rect::new(tx, leaf_r.header.y + 3.0, tw, tab_h);
            let is_active = idx == active_idx;
            let is_hov = tr.contains(pointer_x, pointer_y);

            let (bg, border, ink) = if is_active {
                (theme.bg2, theme.line2, theme.ink1)
            } else if is_hov {
                (theme.hover, theme.line, theme.ink2)
            } else {
                (Rgba::TRANSPARENT, Rgba::TRANSPARENT, theme.ink3)
            };

            draw.rounded_rect_bordered(tr, theme.radius_sm, bg, border, if is_active || is_hov { 1.0 } else { 0.0 })?;
            if is_active {
                // Signature fire top accent bar on active tab
                draw.rounded_rect(
                    Rect::new(tr.x + 6.0, tr.y, (tr.w - 12.0).max(4.0), 2.0),
                    1.0,
                    theme.fire,
                )?;
            }
            let text_y = tr.y + (tr.h - theme.font_sm) * 0.5;
            let _ = draw.text_max_width(tr.x + 10.0, text_y, name, theme.font_sm, ink, tr.w - 16.0)?;
            tx += tw + 4.0;
        }

        // Right-side maximize/restore button (`⤢`)
        let exp_r = Rect::new(leaf_r.header.right() - 26.0, leaf_r.header.y + 3.0, 22.0, tab_h);
        let exp_hov = exp_r.contains(pointer_x, pointer_y);
        let is_max = dock.maximized_tab.is_some();
        if exp_hov || is_max {
            draw.rounded_rect_bordered(
                exp_r,
                theme.radius_sm,
                if is_max { theme.fire_soft } else { theme.hover },
                if is_max { theme.fire_dim } else { theme.line2 },
                1.0,
            )?;
        }
        let exp_col = if is_max { theme.fire } else if exp_hov { theme.ink1 } else { theme.ink3 };
        let _ = draw.text(exp_r.x + 6.0, exp_r.y + (exp_r.h - theme.font_sm) * 0.5, "⤢", theme.font_sm, exp_col)?;

        draw.pop_clip();
    }

    // 2. Draw interactive splitters
    for &sp in dock.splitters.as_slice() {
        let is_drag = dock.dragging_splitter == Some(sp.split);
        let expanded = Rect::new(sp.rect.x - 3.0, sp.rect.y - 3.0, sp.rect.w + 6.0, sp.rect.h + 6.0);
        let is_hov = expanded.contains(pointer_x, pointer_y);
        if is_drag || is_hov {
            let bar = match sp.dir {
                SplitDir::Horizontal => Rect::new(sp.rect.x + 1.0, sp.rect.y + 4.0, 3.0, (sp.rect.h - 8.0).max(4.0)),
                SplitDir::Vertical => Rect::new(sp.rect.x + 4.0, sp.rect.y + 1.0, (sp.rect.w - 8.0).max(4.0), 3.0),
            };
            draw.rounded_rect(bar, 1.5, if is_drag { theme.fire } else { theme.fire_dim })?;
        }
    }

    // 3. Overlay pass: live drag-to-dock drop zone preview & floating tab ghost
    if let Some(drag) = dock.dragging_tab {
        draw.set_overlay(true);
        if let Some((_, _, preview)) = drag.target {
            let inset = Rect::new(preview.x + 3.0, preview.y + 3.0, (preview.w - 6.0).max(4.0), (preview.h - 6.0).max(4.0));
            draw.rounded_rect_bordered(inset, theme.radius_md, theme.fire_soft, theme.fire, 2.0)?;
        }
        let label = world.entity_name(drag.tab).unwrap_or("Pane");
        let gw = (draw.measure_text(label, theme.font_sm) + 24.0).max(76.0);
        let ghost = Rect::new(drag.pointer_x - gw * 0.5, drag.pointer_y - 13.0, gw, 26.0);
        draw.shadow(ghost, theme.radius_sm, 12.0, 4.0, theme.shadow)?;
        draw.rounded_rect_bordered(ghost, theme.radius_sm, theme.bg3, theme.fire, 1.5)?;
        let _ = draw.text(ghost.x + 12.0, ghost.y + 7.0, label, theme.font_sm, theme.ink1)?;
        draw.set_overlay(false);
    }

    Ok(())
}

/// Extract every entity in `paint_order` into `draw` AND record retained [`GpuSlot`] ranges on
/// each entity so subsequent sparse updates can patch GPU buffers in-place in $O(\Delta)$.
pub fn extract_entities<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    paint_order: &[Entity],
    theme: &Theme,
    time: f32,
    extract_epoch: u64,
    draw: &mut DrawList<A>,
) -> Result<u32, UiError> {
    let mut num_buf = OwnString::new_in(world.alloc());
    let mut visited: u32 = 0;

    for &entity in paint_order {
        if !world.is_alive(entity) {
            continue;
        }
        let Some(cr) = world.get::<ComputedRect>(entity)?.copied() else {
            continue;
        };
        if cr.rect.is_empty() {
            world.set_copy(entity, GpuSlot { epoch: extract_epoch, ..GpuSlot::default() })?;
            continue;
        }

        let (r0, g0) = draw.layer_counts(cr.layer);
        extract_single_entity(world, entity, cr, theme, time, &mut num_buf, draw)?;
        let (r1, g1) = draw.layer_counts(cr.layer);
        let r_len = r1.saturating_sub(r0);
        let g_len = g1.saturating_sub(g0);

        let kind = world.get::<WidgetKind>(entity)?.copied().unwrap_or(WidgetKind::Container);
        let (r_slack, g_slack) = entity_slack(kind);
        if r_slack > 0 || g_slack > 0 {
            draw.reserve_slack(cr.layer, r_slack, g_slack)?;
        }

        world.set_copy(
            entity,
            GpuSlot {
                epoch: extract_epoch,
                layer: cr.layer,
                rect_start: r0,
                rect_len: r_len,
                rect_cap: r_len.saturating_add(r_slack),
                glyph_start: g0,
                glyph_len: g_len,
                glyph_cap: g_len.saturating_add(g_slack),
            },
        )?;
        visited = visited.saturating_add(1);
    }

    draw.set_overlay(false);
    Ok(visited)
}

/// Re-extract a SINGLE dirty entity into `scratch` and patch its retained [`GpuSlot`] in-place
/// inside `draw`. Returns `Ok(true)` if patched in-place ($O(1)$) or currently unmounted/hidden,
/// or `Ok(false)` if the entity overflowed its reserved slack (triggering a full extract fallback).
pub fn patch_entity_gpu<A: Allocator + Copy + 'static>(
    world: &mut World<A>,
    entity: Entity,
    theme: &Theme,
    time: f32,
    extract_epoch: u64,
    scratch: &mut DrawList<A>,
    draw: &mut DrawList<A>,
) -> Result<bool, UiError> {
    if !world.is_alive(entity) {
        return Ok(true);
    }
    let Some(mut slot) = world.get::<GpuSlot>(entity)?.copied() else {
        // Entity is currently in an inactive dock tab or hidden subtree: nothing on GPU to patch.
        return Ok(true);
    };
    if slot.epoch != extract_epoch {
        return Ok(true);
    }
    let Some(cr) = world.get::<ComputedRect>(entity)?.copied() else {
        return Ok(true);
    };
    if slot.layer != cr.layer {
        return Ok(false);
    }

    scratch.advance_ratio = draw.advance_ratio;
    scratch.begin_frame(draw.viewport.size[0], draw.viewport.size[1], draw.viewport.dpr, time)?;
    let mut num_buf = OwnString::new_in(world.alloc());
    if !cr.rect.is_empty() {
        extract_single_entity(world, entity, cr, theme, time, &mut num_buf, scratch)?;
    }

    let (new_rects, new_glyphs) = if cr.layer > 0 {
        (scratch.overlay_rects.as_slice(), scratch.overlay_glyphs.as_slice())
    } else {
        (scratch.rects.as_slice(), scratch.glyphs.as_slice())
    };

    let patched = draw.patch_slot(&mut slot, new_rects, new_glyphs)?;
    if patched {
        world.set_copy(entity, slot)?;
    }
    Ok(patched)
}

#[inline]
const fn entity_slack(kind: WidgetKind) -> (u32, u32) {
    match kind {
        WidgetKind::Button
        | WidgetKind::Badge
        | WidgetKind::Label
        | WidgetKind::Toggle
        | WidgetKind::Slider
        | WidgetKind::TextInput
        | WidgetKind::TreeRow
        | WidgetKind::PropertyRow
        | WidgetKind::Tooltip => (2, 32),
        WidgetKind::Panel | WidgetKind::Card | WidgetKind::Toolbar => (1, 0),
        _ => (0, 0),
    }
}

fn extract_single_entity<A: Allocator + Copy + 'static>(
    world: &World<A>,
    entity: Entity,
    cr: ComputedRect,
    theme: &Theme,
    time: f32,
    num_buf: &mut OwnString<A>,
    draw: &mut DrawList<A>,
) -> Result<(), UiError> {
    draw.set_overlay(cr.layer > 0);
    draw.push_clip_id(cr.clip_id)?;

    let style = world
        .get_inherited::<PaintStyle>(entity)?
        .copied()
        .unwrap_or_else(|| PaintStyle::transparent(theme));
    let kind = world.get::<WidgetKind>(entity)?.copied().unwrap_or(WidgetKind::Container);
    let is_hov = world.has_tag_typed::<Hovered>(entity)?;
    let is_act = world.has_tag_typed::<Active>(entity)?;
    let is_foc = world.has_tag_typed::<Focused>(entity)?;
    let is_sel = world.has_tag_typed::<Selected>(entity)?;
    let is_dis = world.has_tag_typed::<Disabled>(entity)?;

    // Resolve interactive state colors from inherited `PaintStyle` prefab.
    let mut bg = if is_act {
        style.bg_active
    } else if is_sel {
        style.bg_selected
    } else if is_hov {
        style.bg_hover
    } else {
        style.bg
    };
    let mut border = if is_foc {
        style.border_focus
    } else if is_hov {
        style.border_hover
    } else {
        style.border
    };
    let mut ink = style.text;

    if is_dis {
        bg = bg.with_alpha(bg.0[3] * 0.5);
        ink = theme.ink4;
    }

    // Badge overrides bg/border/ink cleanly by semantic tone.
    if kind == WidgetKind::Badge {
        let tone = world.get::<BadgeTone>(entity)?.copied().unwrap_or(BadgeTone::Neutral);
        let (b_bg, b_bd, b_ink) = match tone {
            BadgeTone::Neutral => (theme.bg4, theme.line2, theme.ink2),
            BadgeTone::Fire => (theme.fire_soft, theme.fire_dim, theme.fire),
            BadgeTone::Ok => (theme.ok_soft, theme.ok.with_alpha(0.45), theme.ok),
            BadgeTone::Warn => (theme.warn_soft, theme.warn.with_alpha(0.45), theme.warn),
            BadgeTone::Danger => (theme.danger_soft, theme.danger.with_alpha(0.45), theme.danger),
            BadgeTone::Blue => (theme.blue_soft, theme.blue.with_alpha(0.45), theme.blue),
            BadgeTone::Violet => (theme.violet.with_alpha(0.16), theme.violet.with_alpha(0.45), theme.violet),
        };
        bg = b_bg;
        border = b_bd;
        ink = b_ink;
    }

    // Soft SDF drop shadow (if configured on style prefab or entity).
    if style.shadow_blur > 0.1 && !style.shadow_color.is_transparent() {
        draw.shadow(cr.rect, style.radius, style.shadow_blur, style.shadow_offset_y, style.shadow_color)?;
    }

    // Primary background + integrated AA border (single SDF instance!).
    let bw = if is_foc { style.border_width.max(1.5) } else { style.border_width };
    draw.rounded_rect_bordered(cr.rect, style.radius, bg, border, bw)?;

    // Selected left accent bar for tree rows.
    if kind == WidgetKind::TreeRow && is_sel {
        draw.rounded_rect(
            Rect::new(cr.rect.x + 1.0, cr.rect.y + 3.0, 2.5, (cr.rect.h - 6.0).max(4.0)),
            1.0,
            theme.fire,
        )?;
    }

    // Widget-specific visuals & typography.
    match kind {
        WidgetKind::Label | WidgetKind::Tooltip => {
            if let Some(lbl) = world.get::<LabelText<A>>(entity)? {
                let ty = cr.content_rect.y + (cr.content_rect.h - style.text_size) * 0.5;
                let _ = draw.text_max_width(
                    cr.content_rect.x,
                    ty,
                    lbl.0.as_str(),
                    style.text_size,
                    ink,
                    cr.content_rect.w,
                )?;
            }
        }
        WidgetKind::Button | WidgetKind::Badge => {
            if let Some(lbl) = world.get::<LabelText<A>>(entity)? {
                let tw = draw.measure_text(lbl.0.as_str(), style.text_size);
                let tx = cr.rect.x + ((cr.rect.w - tw) * 0.5).max(6.0);
                let ty = cr.rect.y + (cr.rect.h - style.text_size) * 0.5;
                let _ = draw.text_max_width(
                    tx,
                    ty,
                    lbl.0.as_str(),
                    style.text_size,
                    ink,
                    (cr.rect.w - 12.0).max(4.0),
                )?;
            }
        }
        WidgetKind::Toggle => {
            let checked = world.get::<ToggleState>(entity)?.map_or(false, |t| t.checked);
            let ty = cr.rect.y + (cr.rect.h - style.text_size) * 0.5;
            if let Some(lbl) = world.get::<LabelText<A>>(entity)? {
                let _ = draw.text_max_width(
                    cr.rect.x + 6.0,
                    ty,
                    lbl.0.as_str(),
                    style.text_size,
                    ink,
                    (cr.rect.w - 48.0).max(16.0),
                )?;
            }
            let pill_w = 32.0;
            let pill_h = 16.0;
            let px = cr.rect.right() - pill_w - 6.0;
            let py = cr.rect.y + (cr.rect.h - pill_h) * 0.5;
            let track_bg = if checked { theme.fire } else { theme.bg4 };
            let track_bd = if checked { theme.fire } else { theme.line2 };
            draw.rounded_rect_bordered(Rect::new(px, py, pill_w, pill_h), 8.0, track_bg, track_bd, 1.0)?;
            let knob_cx = if checked { px + pill_w - 8.0 } else { px + 8.0 };
            draw.circle(knob_cx, py + pill_h * 0.5, 5.5, 0.0, Rgba::WHITE)?;
        }
        WidgetKind::Slider => {
            let st = world
                .get::<SliderState>(entity)?
                .copied()
                .unwrap_or_else(|| SliderState::new(0.0, 0.0, 1.0, 0.0, 2));
            let ty = cr.rect.y + (cr.rect.h - style.text_size) * 0.5;
            if let Some(lbl) = world.get::<LabelText<A>>(entity)? {
                let _ = draw.text_max_width(
                    cr.rect.x + 6.0,
                    ty,
                    lbl.0.as_str(),
                    style.text_size,
                    theme.ink2,
                    (cr.rect.w * 0.40 - 8.0).max(16.0),
                )?;
            }
            let track_x0 = cr.rect.x + cr.rect.w * 0.42;
            let track_w = (cr.rect.w * 0.56 - 44.0).max(24.0);
            let track_y = cr.rect.y + cr.rect.h * 0.5 - 2.5;
            let norm = st.normalized();
            draw.rounded_rect_bordered(
                Rect::new(track_x0, track_y, track_w, 5.0),
                2.5,
                theme.bg,
                theme.line,
                1.0,
            )?;
            let fill_w = (track_w * norm).max(3.0);
            draw.rounded_rect(Rect::new(track_x0, track_y, fill_w, 5.0), 2.5, theme.fire)?;
            let thumb_cx = track_x0 + track_w * norm;
            let thumb_r = if is_act || is_hov { 6.5 } else { 5.2 };
            draw.circle(thumb_cx, track_y + 2.5, thumb_r, 0.0, theme.ink1)?;
            draw.circle(thumb_cx, track_y + 2.5, thumb_r, 1.4, theme.fire)?;

            // Formatted numeric readout on right
            num_buf.clear();
            format_fixed(num_buf, st.value, st.decimals);
            let _ = draw.text_max_width(
                track_x0 + track_w + 8.0,
                ty,
                num_buf.as_str(),
                style.text_size,
                theme.ink1,
                38.0,
            )?;
        }
        WidgetKind::TextInput => {
            if let Some(st) = world.get::<TextInputState<A>>(entity)? {
                let ty = cr.rect.y + (cr.rect.h - style.text_size) * 0.5;
                let tx = cr.rect.x + 8.0;
                let avail = (cr.rect.w - 16.0).max(8.0);
                if st.text.is_empty() {
                    let _ = draw.text_max_width(tx, ty, st.placeholder.as_str(), style.text_size, theme.ink3, avail)?;
                } else {
                    let drawn_w = draw.text_max_width(tx, ty, st.text.as_str(), style.text_size, ink, avail)?;
                    if is_foc {
                        // Crisp blinking caret (~1.6 Hz)
                        let phase = time - gpu::math::floor(time * 1.6) / 1.6;
                        if phase < 0.38 {
                            draw.fill_rect(
                                Rect::new(tx + drawn_w + 1.0, cr.rect.y + 5.0, 1.5, (cr.rect.h - 10.0).max(6.0)),
                                theme.fire,
                            )?;
                        }
                    }
                }
                if is_foc && st.text.is_empty() {
                    draw.fill_rect(
                        Rect::new(tx + 1.0, cr.rect.y + 5.0, 1.5, (cr.rect.h - 10.0).max(6.0)),
                        theme.fire,
                    )?;
                }
            }
        }
        WidgetKind::TreeRow => {
            let row_st = world.get::<TreeRowState>(entity)?.copied().unwrap_or_default();
            let indent = f32::from(row_st.depth.min(16)) * 12.0;
            let x0 = cr.rect.x + 8.0 + indent;
            let ty = cr.rect.y + (cr.rect.h - style.text_size) * 0.5;
            let glyph = if row_st.has_children {
                if row_st.expanded { "▾" } else { "▸" }
            } else {
                "·"
            };
            let glyph_col = if is_sel { theme.fire } else { theme.ink3 };
            let _ = draw.text(x0, ty, glyph, style.text_size, glyph_col)?;

            let sub_w = world
                .get::<SubText<A>>(entity)?
                .map_or(0.0, |s| draw.measure_text(s.0.as_str(), theme.font_sm) + 12.0);
            if let Some(lbl) = world.get::<LabelText<A>>(entity)? {
                let max_lbl = (cr.rect.right() - (x0 + 14.0) - sub_w - 6.0).max(16.0);
                let lbl_col = if is_sel { theme.ink1 } else { ink };
                let _ = draw.text_max_width(x0 + 14.0, ty, lbl.0.as_str(), style.text_size, lbl_col, max_lbl)?;
            }
            if let Some(sub) = world.get::<SubText<A>>(entity)? {
                let sw = draw.measure_text(sub.0.as_str(), theme.font_sm);
                let sx = (cr.rect.right() - sw - 8.0).max(x0 + 24.0);
                let _ = draw.text_max_width(sx, ty, sub.0.as_str(), theme.font_sm, theme.ink3, sw + 4.0)?;
            }
        }
        WidgetKind::PropertyRow => {
            let ty = cr.rect.y + (cr.rect.h - style.text_size) * 0.5;
            let split_x = cr.rect.x + cr.rect.w * 0.40;
            if let Some(lbl) = world.get::<LabelText<A>>(entity)? {
                let _ = draw.text_max_width(
                    cr.rect.x + 8.0,
                    ty,
                    lbl.0.as_str(),
                    style.text_size,
                    theme.ink3,
                    (split_x - cr.rect.x - 12.0).max(16.0),
                )?;
            }
            if let Some(sub) = world.get::<SubText<A>>(entity)? {
                let _ = draw.text_max_width(
                    split_x,
                    ty,
                    sub.0.as_str(),
                    style.text_size,
                    ink,
                    (cr.rect.right() - split_x - 8.0).max(16.0),
                )?;
            }
        }
        WidgetKind::Canvas => {
            if let Some(batch) = world.get::<CanvasBatch<A>>(entity)? {
                let inner_clip = cr.child_clip_id;
                let ox = if batch.local_space { cr.content_rect.x } else { 0.0 };
                let oy = if batch.local_space { cr.content_rect.y } else { 0.0 };
                for &r in batch.rects.as_slice() {
                    let mut inst = r;
                    if batch.local_space {
                        inst.rect[0] += ox;
                        inst.rect[1] += oy;
                        inst.rect[2] += ox;
                        inst.rect[3] += oy;
                    }
                    if inst.clip_info[0] == 0 {
                        inst.clip_info[0] = inner_clip;
                    }
                    draw.push_rect_inst(inst)?;
                }
                for &g in batch.glyphs.as_slice() {
                    let mut inst = g;
                    if batch.local_space {
                        inst.rect[0] += ox;
                        inst.rect[1] += oy;
                        inst.rect[2] += ox;
                        inst.rect[3] += oy;
                    }
                    if inst.clip_info[0] == 0 {
                        inst.clip_info[0] = inner_clip;
                    }
                    draw.push_glyph_inst(inst)?;
                }
            }
        }
        _ => {}
    }

    // Subtle scrollbar thumb for scrollable containers whose content overflows vertically.
    if let Some(scroll) = world.get::<ScrollOffset>(entity)?.copied() {
        if scroll.content_h > cr.content_rect.h + 2.0 && cr.content_rect.h > 20.0 {
            let frac = (cr.content_rect.h / scroll.content_h).clamp(0.08, 1.0);
            let thumb_h = (cr.content_rect.h * frac).max(18.0);
            let max_scroll = (scroll.content_h - cr.content_rect.h).max(1.0);
            let thumb_y = cr.content_rect.y + (scroll.y / max_scroll) * (cr.content_rect.h - thumb_h);
            draw.rounded_rect(
                Rect::new(cr.rect.right() - 5.0, thumb_y, 3.0, thumb_h),
                1.5,
                theme.line2,
            )?;
        }
    }

    draw.pop_clip();
    draw.set_overlay(false);
    Ok(())
}

fn format_fixed<A: Allocator + Copy>(out: &mut OwnString<A>, val: f32, decimals: u8) {
    let neg = val < 0.0;
    let abs_v = if neg { -val } else { val };
    if neg {
        let _ = out.try_push('-');
    }
    let scale: f32 = match decimals {
        0 => 1.0,
        1 => 10.0,
        2 => 100.0,
        _ => 1000.0,
    };
    let scaled_f = gpu::math::round(abs_v * scale);
    let scaled = f32_to_u32_sat(scaled_f);
    let div: u32 = match decimals {
        0 => 1,
        1 => 10,
        2 => 100,
        _ => 1000,
    };
    let whole = scaled / div;
    let frac = scaled % div;
    let _ = write!(out, "{whole}");
    if decimals == 1 {
        let _ = write!(out, ".{frac:01}");
    } else if decimals == 2 {
        let _ = write!(out, ".{frac:02}");
    } else if decimals >= 3 {
        let _ = write!(out, ".{frac:03}");
    }
}

fn f32_to_u32_sat(v: f32) -> u32 {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    if v >= 1_000_000_000.0 {
        return 1_000_000_000;
    }
    // Zero `as` cast conversion using exact binary search over 30 bits.
    let mut lo: u32 = 0;
    let mut hi: u32 = 1_000_000_000;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if u32_to_f32_exact(mid) <= v {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

fn u32_to_f32_exact(v: u32) -> f32 {
    let hi = u16::try_from(v >> 16).unwrap_or(0);
    let lo = u16::try_from(v & 0xFFFF).unwrap_or(0);
    f32::from(hi) * 65536.0 + f32::from(lo)
}
