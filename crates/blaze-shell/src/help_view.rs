//! Thin GPU renderer for structured CLI Help & Man-Page cards backed by the pure `#![no_std]`
//! sans-IO parser in [`blaze_cli_spec::help_doc`].

pub use blaze_cli_spec::help_doc::{
    HelpDoc, HelpExample, HelpItem, HelpSection, is_help_invocation, parse_help_doc_in,
    primary_flag_token, promote_single_quoted_commands_in,
};
use blaze_own::Alloc;
use blaze_sh::lex::highlight;

use crate::diag_view::layout_inline_markdown;
use crate::icons::{Icon, draw_icon};
use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, HoverKind, Scene, TextFace, truncate_chars};

/// Render a syntax-highlighted CLI usage line with clickable flags (`--foo`, `-C`), amber
/// placeholders (`<path>`), and muted brackets (`[`, `]`, `|`). Returns vertical height consumed.
#[allow(clippy::too_many_arguments)]
fn layout_usage_line(
    scene: &mut Scene,
    line: &str,
    root_cmd: &str,
    x0: f32,
    y0: f32,
    max_w: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let line_h = m.ch + 3.0;
    let mut cx = x0;
    let mut cy = y0;
    let x1 = x0 + max_w;
    let flag_col = theme::rgb(0x74, 0xb9, 0xff);
    let arg_col = theme::rgb(0xe8, 0xa8, 0x48);

    for (ti, tok) in line.split_whitespace().enumerate() {
        let tok_w = tok.chars().count() as f32 * m.cw;
        if ti > 0 {
            if cx + m.cw + tok_w > x1 && cx > x0 {
                cy += line_h;
                cx = x0 + 4.0 * m.cw;
            } else {
                cx += m.cw;
            }
        }

        let mut rest = tok;
        while !rest.is_empty() {
            if let Some(ch) = rest.chars().next()
                && matches!(ch, '[' | ']' | '|' | '(' | ')')
            {
                let s = &rest[..ch.len_utf8()];
                scene.text(cx, cy, s, theme::INK_4, false, false, clip_id);
                cx += m.cw;
                rest = &rest[ch.len_utf8()..];
                continue;
            }
            if rest.starts_with('<')
                && let Some(end) = rest.find('>')
            {
                let seg = &rest[..=end];
                scene.text(cx, cy, seg, arg_col, false, false, clip_id);
                cx += seg.chars().count() as f32 * m.cw;
                rest = &rest[end + 1..];
                continue;
            }
            let seg_end = rest
                .find(|c: char| matches!(c, '[' | ']' | '|' | '(' | ')' | '<'))
                .unwrap_or(rest.len());
            let seg = &rest[..seg_end];
            let seg_w = seg.chars().count() as f32 * m.cw;
            if ti == 0 && seg == root_cmd.split_whitespace().next().unwrap_or("") {
                scene.text(cx, cy, seg, theme::INK_1, true, false, clip_id);
            } else if seg.starts_with('-') {
                scene.text(cx, cy, seg, flag_col, false, false, clip_id);
                let flag_tok = seg.trim_end_matches('=').to_owned();
                if flag_tok.len() >= 2 {
                    let hit = HitRect::new(cx, cy, cx + seg_w, cy + m.ch);
                    scene.push_hit_clipped(hit, clip_id, HitAction::InsertToken(flag_tok.clone()));
                    scene.push_hover_clipped(
                        hit,
                        clip_id,
                        HoverKind::Tooltip {
                            title: flag_tok,
                            subtitle: Some("click to insert flag".to_owned()),
                        },
                    );
                }
            } else {
                scene.text(cx, cy, seg, theme::INK_2, false, false, clip_id);
            }
            cx += seg_w;
            rest = &rest[seg_end..];
        }
    }
    (cy - y0) + m.ch
}

/// Render a structured [`HelpDoc`] as a native GPU manual card with Usage, runnable Examples,
/// grouped subcommands/flags aligned on a single vertical spine, inline Markdown chips, and
/// clickable footer links.
#[allow(clippy::too_many_arguments)]
pub fn layout_help_doc(
    scene: &mut Scene,
    doc: &HelpDoc<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    top_right_reserve: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let x1 = x0 + width;
    let bg_idx = scene.rects.len();
    scene.round_rect(
        x0,
        y0,
        x1,
        y0 + 40.0,
        8.0,
        theme::rgb(0x13, 0x13, 0x17),
        1.0,
        theme::LINE,
        clip_id,
    );

    let pad_x = 14.0;
    let inner_x0 = x0 + pad_x;
    let inner_x1 = x1 - pad_x;
    let inner_w = (inner_x1 - inner_x0).max(160.0);
    let mut cur_y = y0;

    // 1. Header bar.
    let hdr_h = m.ch + 14.0;
    scene.round_rect(
        x0 + 1.0,
        y0 + 1.0,
        x1 - 1.0,
        y0 + hdr_h,
        7.0,
        theme::rgb(0x18, 0x18, 0x1e),
        0.0,
        [0.0; 4],
        clip_id,
    );
    scene.line(x0 + 1.0, y0 + hdr_h, x1 - 1.0, y0 + hdr_h, 1.0, theme::LINE_FAINT, clip_id);
    let hdr_ty = y0 + (hdr_h - m.ch) * 0.5;
    let ic_s = m.icon_sm();
    draw_icon(scene, Icon::Doc, inner_x0, y0 + (hdr_h - ic_s) * 0.5, ic_s, theme::SERIES_BLUE, clip_id);
    let cmd_x = inner_x0 + ic_s + 8.0;
    let cmd_str = doc.command.as_str();
    scene.text(cmd_x, hdr_ty, cmd_str, theme::INK_1, true, false, clip_id);
    let cmd_w = cmd_str.chars().count() as f32 * m.cw;
    let sub_x = cmd_x + cmd_w + 10.0;
    let sub_avail = (inner_x1 - top_right_reserve - sub_x - 8.0).max(0.0);
    if sub_avail >= 60.0 {
        let sub_txt = doc
            .summary
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("manual & quick reference");
        let max_c = (sub_avail / m.measure("n", TextFace::UiSmall).max(5.5))
            .floor()
            .max(8.0) as usize;
        scene.ui_small(
            sub_x,
            hdr_ty + 1.0,
            truncate_chars(sub_txt, max_c),
            theme::INK_3,
            false,
            false,
            clip_id,
        );
    }
    cur_y += hdr_h + 10.0;

    let small_ch = m.line_h(TextFace::UiSmall);
    let flag_col = theme::rgb(0x74, 0xb9, 0xff);
    let subcmd_col = theme::rgb(0x7c, 0xd4, 0xa8);
    let warn_col = theme::rgb(0xe8, 0xa8, 0x48);

    // 2. USAGE box.
    if !doc.usage.is_empty() {
        scene.ui_small(inner_x0, cur_y, "USAGE", theme::INK_4, true, false, clip_id);
        cur_y += small_ch + 4.0;

        let u_box_idx = scene.rects.len();
        let u_y0 = cur_y;
        scene.round_rect(
            inner_x0,
            u_y0,
            inner_x1,
            u_y0 + 24.0,
            6.0,
            theme::rgb(0x10, 0x10, 0x13),
            1.0,
            theme::LINE_FAINT,
            clip_id,
        );
        cur_y += 7.0;
        let u_inner_x = inner_x0 + 10.0;
        let u_inner_w = (inner_w - 20.0).max(120.0);
        for uline in doc.usage.iter() {
            let h = layout_usage_line(scene, uline.as_str(), cmd_str, u_inner_x, cur_y, u_inner_w, m, clip_id);
            cur_y += h + 3.0;
        }
        cur_y += 4.0;
        scene.rects[u_box_idx].rect[3] = cur_y;
        cur_y += 12.0;
    }

    // 3. EXAMPLES card.
    if !doc.examples.is_empty() {
        scene.ui_small(inner_x0, cur_y, "EXAMPLES", theme::OK, true, false, clip_id);
        let hint = "click to insert";
        let hw = m.measure(hint, TextFace::UiSmall);
        scene.ui_small(inner_x1 - hw, cur_y, hint, theme::INK_4, false, false, clip_id);
        cur_y += small_ch + 4.0;

        let ex_bg_idx = scene.rects.len();
        let ex_y0 = cur_y;
        scene.round_rect(
            inner_x0,
            ex_y0,
            inner_x1,
            ex_y0 + 24.0,
            6.0,
            theme::rgba(0x4c, 0xc2, 0x7a, 0.045),
            1.0,
            theme::rgba(0x4c, 0xc2, 0x7a, 0.20),
            clip_id,
        );
        let ex_rail_idx = scene.rects.len();
        scene.round_rect(
            inner_x0,
            ex_y0,
            inner_x0 + 2.5,
            ex_y0 + 24.0,
            1.25,
            theme::OK,
            0.0,
            [0.0; 4],
            clip_id,
        );
        cur_y += 6.0;

        let ex_in_x = inner_x0 + 11.0;
        let ex_in_w = (inner_x1 - 10.0 - ex_in_x).max(120.0);
        let max_cmd_c = doc
            .examples
            .iter()
            .map(|e| e.cmd.as_str().chars().count())
            .max()
            .unwrap_or(16)
            .clamp(16, 36);
        let cmd_col_w = ((max_cmd_c + 3) as f32 * m.cw).min(ex_in_w * 0.56);
        let row_h = m.ch + 5.0;

        for ex in doc.examples.iter().take(6) {
            let ex_cmd = ex.cmd.as_str();
            let ex_desc = ex.desc.as_str();
            let row_rect = HitRect::new(inner_x0 + 4.0, cur_y, inner_x1 - 4.0, cur_y + row_h);
            let ty = cur_y + (row_h - m.ch) * 0.5;
            scene.text(ex_in_x, ty, "$ ", theme::INK_4, false, false, clip_id);
            let mut sx = ex_in_x + 2.0 * m.cw;
            let cmd_shown = truncate_chars(
                ex_cmd,
                ((cmd_col_w - 2.0 * m.cw) / m.cw).floor().max(8.0) as usize,
            );
            let spans = highlight(&cmd_shown, Alloc::global());
            let mut prev_b = 0usize;
            for sp in spans.iter() {
                let s = (sp.span.start as usize).min(cmd_shown.len());
                let e = (sp.span.end as usize).min(cmd_shown.len());
                if s > prev_b {
                    let gap = &cmd_shown[prev_b..s];
                    scene.text(sx, ty, gap, theme::INK_2, false, false, clip_id);
                    sx += gap.chars().count() as f32 * m.cw;
                }
                if e > s {
                    let s_txt = &cmd_shown[s..e];
                    scene.text(sx, ty, s_txt, theme::highlight_color(sp.kind), false, false, clip_id);
                    sx += s_txt.chars().count() as f32 * m.cw;
                }
                prev_b = e;
            }
            if prev_b < cmd_shown.len() {
                scene.text(sx, ty, &cmd_shown[prev_b..], theme::INK_2, false, false, clip_id);
            }
            if !ex_desc.is_empty() {
                let desc_x = ex_in_x + cmd_col_w + 10.0;
                let rem_w = (inner_x1 - 10.0 - desc_x).max(40.0);
                let max_dc = (rem_w / m.cw).floor().max(8.0) as usize;
                scene.text(
                    desc_x,
                    ty,
                    truncate_chars(ex_desc, max_dc),
                    theme::INK_3,
                    false,
                    false,
                    clip_id,
                );
            }
            scene.push_hit_clipped(row_rect, clip_id, HitAction::InsertToken(ex_cmd.to_owned()));
            scene.push_hover_clipped(
                row_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: ex_cmd.to_owned(),
                    subtitle: Some("click to insert example into prompt".to_owned()),
                },
            );
            cur_y += row_h;
        }
        cur_y += 5.0;
        scene.rects[ex_bg_idx].rect[3] = cur_y;
        scene.rects[ex_rail_idx].rect[3] = cur_y;
        cur_y += 12.0;
    }

    // 4. Categorized Sections aligned on one vertical spine.
    let root_cmd = cmd_str.split_whitespace().next().unwrap_or(cmd_str);
    let global_max_name_c = doc
        .sections
        .iter()
        .flat_map(|s| s.items.iter())
        .map(|it| it.name.as_str().chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 26);

    for (si, sec) in doc.sections.iter().enumerate() {
        if si > 0 {
            cur_y += 10.0;
        }
        let heading = sec.title.as_str().to_ascii_uppercase();
        scene.ui_small(inner_x0, cur_y, &heading, theme::INK_3, true, false, clip_id);
        let head_w = m.measure_styled(&heading, TextFace::UiSmall, true);
        let mut line_x1 = inner_x1;
        if let Some(see) = &sec.see_also {
            let see_s = see.as_str();
            let lbl = format!("{see_s} ↗");
            let lw = m.measure(&lbl, TextFace::UiSmall) + 12.0;
            let sx0 = (inner_x1 - lw).max(inner_x0 + head_w + 14.0);
            let s_rect = HitRect::new(sx0, cur_y - 1.5, inner_x1, cur_y + small_ch + 1.5);
            scene.round_rect(
                s_rect.x0,
                s_rect.y0,
                s_rect.x1,
                s_rect.y1,
                4.0,
                theme::rgba(0x58, 0x9d, 0xf6, 0.10),
                1.0,
                theme::rgba(0x58, 0x9d, 0xf6, 0.28),
                clip_id,
            );
            scene.ui_small(sx0 + 6.0, cur_y, &lbl, theme::SERIES_BLUE, false, false, clip_id);
            scene.push_hit_clipped(s_rect, clip_id, HitAction::RunCommand(see_s.to_owned()));
            scene.push_hover_clipped(
                s_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: see_s.to_owned(),
                    subtitle: Some("click to run".to_owned()),
                },
            );
            line_x1 = sx0 - 10.0;
        }
        if inner_x0 + head_w + 12.0 < line_x1 {
            let ly = cur_y + small_ch * 0.5;
            scene.line(inner_x0 + head_w + 10.0, ly, line_x1, ly, 1.0, theme::LINE_FAINT, clip_id);
        }
        cur_y += small_ch + 6.0;

        let has_subcmd_drill = sec.items.iter().any(|it| !it.is_flag);
        let drill_extra = if has_subcmd_drill { 20.0 } else { 0.0 };
        let left_col_w = ((global_max_name_c as f32 * m.cw) + 18.0 + drill_extra).min(inner_w * 0.42);
        let desc_x = inner_x0 + left_col_w + 10.0;
        let desc_w = (inner_x1 - desc_x).max(100.0);

        for item in sec.items.iter() {
            let item_name = item.name.as_str();
            let name_max_c = ((left_col_w - 14.0 - drill_extra) / m.cw).floor().max(6.0) as usize;
            let shown_name = truncate_chars(item_name, name_max_c);
            let nw = shown_name.chars().count() as f32 * m.cw;

            let chip_x0 = inner_x0;
            let chip_w = nw + 10.0;
            let chip_rect = HitRect::new(chip_x0, cur_y - 1.0, chip_x0 + chip_w, cur_y + m.ch + 1.0);
            let (chip_bg, chip_border, chip_fg) = if item.is_flag {
                (
                    theme::rgba(0x74, 0xb9, 0xff, 0.09),
                    theme::rgba(0x74, 0xb9, 0xff, 0.24),
                    flag_col,
                )
            } else {
                (
                    theme::rgba(0x7c, 0xd4, 0xa8, 0.08),
                    theme::rgba(0x7c, 0xd4, 0xa8, 0.24),
                    subcmd_col,
                )
            };
            scene.round_rect(
                chip_rect.x0,
                chip_rect.y0,
                chip_rect.x1,
                chip_rect.y1,
                4.0,
                chip_bg,
                1.0,
                chip_border,
                clip_id,
            );
            scene.text(chip_x0 + 5.0, cur_y, &shown_name, chip_fg, !item.is_flag, false, clip_id);

            let insert_str = if item.is_flag {
                primary_flag_token(item_name).to_owned()
            } else {
                format!("{root_cmd} {}", item_name.split_whitespace().next().unwrap_or(item_name))
            };
            scene.push_hit_clipped(chip_rect, clip_id, HitAction::InsertToken(insert_str.clone()));
            scene.push_hover_clipped(
                chip_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: insert_str,
                    subtitle: Some(if item.is_flag {
                        "click to insert flag".to_owned()
                    } else {
                        "click to insert command".to_owned()
                    }),
                },
            );

            if !item.is_flag {
                let sub = item_name.split_whitespace().next().unwrap_or(item_name);
                let qx0 = chip_rect.x1 + 4.0;
                let qw = m.cw + 6.0;
                if qx0 + qw < desc_x - 4.0 {
                    let q_rect = HitRect::new(qx0, cur_y - 0.5, qx0 + qw, cur_y + m.ch + 0.5);
                    scene.round_rect(
                        q_rect.x0,
                        q_rect.y0,
                        q_rect.x1,
                        q_rect.y1,
                        4.0,
                        theme::BG_CHIP,
                        1.0,
                        theme::LINE_FAINT,
                        clip_id,
                    );
                    scene.text(qx0 + 3.0, cur_y, "?", theme::INK_4, false, false, clip_id);
                    let help_cmd = format!("{root_cmd} {sub} --help");
                    scene.push_hit_clipped(q_rect, clip_id, HitAction::RunCommand(help_cmd.clone()));
                    scene.push_hover_clipped(
                        q_rect,
                        clip_id,
                        HoverKind::Tooltip {
                            title: help_cmd,
                            subtitle: Some("click to inspect subcommand manual".to_owned()),
                        },
                    );
                }
            }

            let mut desc_indent = 0.0f32;
            if let Some(badge) = &item.badge {
                let b_str = badge.as_str();
                let bw = m.measure(b_str, TextFace::UiSmall) + 10.0;
                let is_exp = b_str.eq_ignore_ascii_case("EXPERIMENTAL")
                    || b_str.eq_ignore_ascii_case("DEPRECATED");
                let (b_bg, b_bd, b_fg) = if is_exp {
                    (
                        theme::rgba(0xe8, 0xa8, 0x48, 0.14),
                        theme::rgba(0xe8, 0xa8, 0x48, 0.36),
                        warn_col,
                    )
                } else {
                    (theme::BG_CHIP, theme::LINE, theme::INK_3)
                };
                scene.round_rect(
                    desc_x,
                    cur_y,
                    desc_x + bw,
                    cur_y + m.ch - 1.0,
                    4.0,
                    b_bg,
                    1.0,
                    b_bd,
                    clip_id,
                );
                scene.ui_small(desc_x + 5.0, cur_y + 0.5, b_str, b_fg, is_exp, false, clip_id);
                desc_indent = bw + 7.0;
            }
            let dh = layout_inline_markdown(
                scene,
                item.desc.as_str(),
                desc_x,
                cur_y,
                desc_w,
                desc_indent,
                0.0,
                theme::INK_2,
                m,
                clip_id,
            );
            cur_y += dh.max(m.ch) + 6.0;
        }
    }

    // 5. Footer Notes.
    if !doc.notes.is_empty() {
        cur_y += 4.0;
        scene.line(inner_x0, cur_y, inner_x1, cur_y, 1.0, theme::LINE_FAINT, clip_id);
        cur_y += 8.0;
        for note in doc.notes.iter() {
            let promoted = promote_single_quoted_commands_in(note.as_str(), cmd_str, Alloc::global());
            let nh = layout_inline_markdown(
                scene,
                promoted.as_str(),
                inner_x0,
                cur_y,
                inner_w,
                0.0,
                0.0,
                theme::INK_3,
                m,
                clip_id,
            );
            cur_y += nh + 4.0;
        }
    }

    cur_y += 8.0;
    scene.rects[bg_idx].rect[3] = cur_y;
    cur_y - y0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_git_help_doc_with_interactive_run_insert_and_copy_hits() {
        let sample = [
            "usage: git [-v | --version] [-h | --help] [-C <path>]",
            "           <command> [<args>]",
            "",
            "start a working area (see also: git help tutorial)",
            "   clone      Clone a repository into a new directory",
            "",
            "grow, mark and tweak your common history",
            "   reset      Set `HEAD` or the index to a known state",
        ];
        let doc = parse_help_doc_in("git --help", &sample, Alloc::global()).expect("git --help");
        let mut scene = Scene::new();
        let h = layout_help_doc(&mut scene, &doc, 16.0, 20.0, 820.0, 100.0, CellMetrics::default(), 0);
        assert!(h > 180.0);
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::RunCommand("git help tutorial".into())));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::RunCommand("git clone --help".into())));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::CopyText("HEAD".into())));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::InsertToken("git status".into())));
    }
}
