//! GPU-independent minimal layout engine following the `ix.dev/blaze` aesthetic:
//! warm charcoal surfaces (`#101011`), monochrome-first typography (`#ebeae6`..`#57575a`),
//! `--fire` (`#e0461a`) as the one brand accent, crisp GPU SDF icons next to every semantic
//! entity, custom typed dataframe column renderers (file/folder icons, proportional size bars,
//! human relative times, status dots), and an inline `$ ...` recessed composer.

use blaze_own::Alloc;
use blaze_sh::lex::{HighlightSpan, highlight};
use blaze_sh::{
    Bytes, ColDisplay, DateTime, PathKind, Record, ShellError, Table, Value, format_relative_time,
    line_col,
};
use blaze_vim::{CursorShape, Mode as VimMode};
use blaze_vt::color::Colors;
use blaze_vt::page::Wide;
use blaze_vt::style::{Color as VtColor, Rgb as VtRgb, Underline as VtUnderline};
use blaze_vt::terminal::{Mode as VtMode, Terminal};

use crate::blocks::{Block, BlockBody, BlockStore, TABLE_FOLD_ROWS};
use crate::editor::{CompletionKind, Editor};
use crate::icons::{Icon, draw_icon, icon_for_record_key};
use crate::prompt::PromptInfo;
use crate::svg_icons::icon_name_for_file;
use crate::theme;
use crate::view::{
    ActiveDrag, CellMetrics, CursorTrail, DragPayload, HitAction, HitRect, Scene, TextSelection,
    compute_text_selection, format_duration_ms, format_value_cell_at, is_expandable_cell,
    truncate_chars, wrap_lines,
};

pub const TOP_BAR_H: f32 = 34.0;
const TRAFFIC_LIGHTS_PAD: f32 = 80.0;
const OUTER_PAD_X: f32 = 16.0;
const BLOCK_PAD_Y: f32 = 6.0;
const TABLE_ROW_PAD_Y: f32 = 2.5;
const MAX_POPUP_ROWS: usize = 8;
const SIZE_BAR_W: f32 = 42.0;

/// All inputs required to lay out one frame of `blaze-shell` (deterministic: `now_ns` injected).
pub struct LayoutInput<'a> {
    pub store: &'a BlockStore,
    pub editor: &'a Editor,
    pub prompt: &'a PromptInfo,
    pub viewport_w: f32,
    pub viewport_h: f32,
    pub metrics: CellMetrics,
    pub scroll_y: f32,
    pub cursor_visible: bool,
    /// Current UTC timestamp in nanoseconds for human relative time formatting (`3m ago`).
    pub now_ns: i64,
    /// Current UI zoom percentage (100 = default `13pt`).
    pub zoom_pct: u16,
    /// True when the top bar sits inline inside macOS's `FullSizeContentView` titlebar.
    pub inline_titlebar: bool,
    /// In-flight drag-and-drop payload and pointer position, if any.
    pub drag: Option<&'a ActiveDrag>,
    /// Active Ghostty-style eased cursor sweep trail (`EaseOutCubic`, `0.2s`), if animating.
    pub cursor_trail: Option<CursorTrail>,
    /// Active mouse drag-selection over rendered output/terminal text (screen coordinates).
    pub text_selection: Option<TextSelection>,
}

/// Lay out the full `ix.dev/blaze` + Starship inline-stream shell interface.
///
/// The live prompt flows directly underneath the last command output (like a native
/// terminal / TUI), with zero separate docked bottom pane.
pub fn build_scene(input: &LayoutInput<'_>) -> Scene {
    let mut scene = Scene::new();
    let vw = input.viewport_w.max(320.0);
    let vh = input.viewport_h.max(240.0);
    let m = input.metrics;

    // 1. Full-height scrollable stream below the unified inline titlebar (`clip_id = 1`).
    let feed_top = TOP_BAR_H;
    let feed_bottom = vh.max(feed_top + 1.0);
    let feed_clip = scene.push_clip(0.0, feed_top, vw, feed_bottom, 0.0);
    scene.scroll_viewport_height = (feed_bottom - feed_top).max(1.0);

    // 2. Open borderless command & output stream.
    let col_x0 = OUTER_PAD_X;
    let col_x1 = (vw - OUTER_PAD_X).max(col_x0 + 200.0);
    let blocks = input.store.active_blocks();

    let mut content_y = 8.0f32;
    for (idx, blk) in blocks.iter().enumerate() {
        if idx > 0 {
            let sep_y = feed_top + content_y - input.scroll_y;
            scene.line(col_x0, sep_y, col_x1, sep_y, 1.0, theme::LINE_FAINT, feed_clip);
            content_y += BLOCK_PAD_Y;
        }
        // Always keep the context capsule on every command block (never vanishes after running).
        let draw_y = feed_top + content_y - input.scroll_y;
        let h = layout_block(
            &mut scene,
            blk,
            true,
            col_x0,
            draw_y,
            col_x1,
            m,
            feed_clip,
            input.cursor_visible,
            input.now_ns,
        );
        content_y += h + BLOCK_PAD_Y;
    }

    // 3. Live inline Starship prompt directly underneath the last command output!
    if !blocks.is_empty() {
        let sep_y = feed_top + content_y - input.scroll_y;
        scene.line(col_x0, sep_y, col_x1, sep_y, 1.0, theme::LINE_FAINT, feed_clip);
        content_y += BLOCK_PAD_Y;
    }
    let prompt_y0 = feed_top + content_y - input.scroll_y;
    scene.input_top_y = prompt_y0;
    let prompt_h = layout_inline_prompt(
        &mut scene,
        input.editor,
        input.prompt,
        col_x0,
        prompt_y0,
        col_x1,
        m,
        feed_clip,
        input.cursor_visible,
        input.drag,
        input.cursor_trail,
    );
    let prompt_y1 = prompt_y0 + prompt_h;
    content_y += prompt_h + 14.0;
    scene.total_content_height = content_y;

    // 3b. Translucent selection highlight over any drag-selected scrollback/output text.
    if let Some(sel) = input.text_selection {
        let (sel_rects, _) = compute_text_selection(&scene.text_runs, m, sel, feed_top);
        for [rx0, ry0, rx1, ry1] in sel_rects {
            scene.over_round_rect(rx0, ry0, rx1, ry1, 2.0, [0.24, 0.50, 0.90, 0.34], feed_clip);
        }
    }

    // 4. Unified inline macOS titlebar (branch tabs, cwd/git pill, zoom, icon fork button).
    layout_top_bar(
        &mut scene,
        input.store,
        input.prompt,
        vw,
        m,
        input.zoom_pct,
        input.inline_titlebar,
    );

    // 5. Floating overlays: Atuin-class Ctrl+R history palette, Tab completion popup, drag chip.
    if let Some(rs) = &input.editor.reverse_search {
        layout_reverse_search_overlay(&mut scene, rs, vw, vh, m, input.now_ns);
    } else if !input.editor.completions.is_empty() {
        layout_completion_popup(&mut scene, input.editor, col_x0, prompt_y0, prompt_y1, vh, col_x1, m);
    }
    if let Some(drag) = input.drag {
        layout_drag_overlay(&mut scene, drag, m);
    }

    scene
}

fn layout_top_bar(
    scene: &mut Scene,
    store: &BlockStore,
    prompt: &PromptInfo,
    vw: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
) {
    scene.round_rect(0.0, 0.0, vw, TOP_BAR_H, 0.0, theme::BG_TOPBAR, 0.0, [0.0; 4], 0);
    scene.line(0.0, TOP_BAR_H - 0.5, vw, TOP_BAR_H - 0.5, 1.0, theme::LINE, 0);

    let mid_y = (TOP_BAR_H - m.ch) * 0.5;
    let icon_s = 11.5;
    let icon_y = (TOP_BAR_H - icon_s) * 0.5;
    let mut x = if inline_titlebar { TRAFFIC_LIGHTS_PAD } else { OUTER_PAD_X };

    // Right-aligned inline controls inside the titlebar: icon-only fork button, zoom badge, cwd/git.
    let mut rx = vw - 14.0;
    let btn_y0 = 6.0;
    let btn_y1 = TOP_BAR_H - 6.0;
    let fork_w = 26.0;
    let fork_x0 = (rx - fork_w).max(x + 20.0);
    scene.round_rect(fork_x0, btn_y0, rx, btn_y1, 6.0, theme::BG_CHIP, 1.0, theme::LINE, 0);
    draw_icon(scene, Icon::Branch, fork_x0 + (fork_w - icon_s) * 0.5, icon_y, icon_s, theme::INK_2, 0);
    scene.push_hit(HitRect::new(fork_x0, btn_y0, rx, btn_y1), HitAction::ForkActiveBranch);
    rx = fork_x0 - 10.0;

    // Quiet clickable zoom pill when zoomed in/out (`Cmd+`/`Cmd-`; click or `Cmd+0` to reset).
    if zoom_pct != 100 {
        let z_lbl = format!("{zoom_pct}%");
        let z_w = z_lbl.len() as f32 * m.cw + 14.0;
        let zx0 = (rx - z_w).max(x + 20.0);
        if zx0 > x + 60.0 {
            scene.round_rect(zx0, btn_y0, rx, btn_y1, 6.0, theme::BG_CHIP, 1.0, theme::LINE, 0);
            scene.text(zx0 + 7.0, mid_y, z_lbl, theme::INK_3, false, false, 0);
            scene.push_hit(HitRect::new(zx0, btn_y0, rx, btn_y1), HitAction::ResetZoom);
            rx = zx0 - 10.0;
        }
    }

    // Inline repo/directory + branch + uncommitted file icons tucked cleanly on the right of the titlebar.
    let dir_txt = truncate_chars(&prompt.dir_short, 24);
    let branch_part = prompt.git_branch.as_deref().unwrap_or("");
    let top_icons: &[(&'static str, u32)] = prompt
        .git_status
        .as_ref()
        .map(|s| s.changed_icons.as_slice())
        .unwrap_or(&[]);
    let top_dirty_total: u32 = prompt
        .git_status
        .as_ref()
        .map(|s| s.conflicted + s.staged + s.modified + s.deleted + s.untracked)
        .unwrap_or(0);
    let dir_icon_s = 13.0;
    let mini_icon_s = 11.5;
    let shown_top_icons = top_icons.len().min(3);
    let dirty_count_str = if top_dirty_total > 0 {
        format!("{top_dirty_total}")
    } else {
        String::new()
    };
    let mut info_w = dir_icon_s + 6.0 + dir_txt.chars().count() as f32 * m.cw;
    if !branch_part.is_empty() {
        info_w += 14.0 + icon_s + 5.0 + branch_part.chars().count() as f32 * m.cw;
        if shown_top_icons > 0 {
            info_w += 8.0 + shown_top_icons as f32 * (mini_icon_s + 2.5);
            if !dirty_count_str.is_empty() {
                info_w += 2.0 + dirty_count_str.len() as f32 * m.cw;
            }
        }
    }
    let info_x0 = rx - info_w - 8.0;
    let tabs_limit = if info_x0 > x + 220.0 {
        let mut ix = info_x0;
        scene.svg_icon(ix, (TOP_BAR_H - dir_icon_s) * 0.5, dir_icon_s, "folder_macos", 0);
        ix += dir_icon_s + 6.0;
        scene.text(ix, mid_y, &dir_txt, theme::INK_2, false, false, 0);
        ix += dir_txt.chars().count() as f32 * m.cw;
        if !branch_part.is_empty() {
            ix += 14.0;
            draw_icon(scene, Icon::Branch, ix, icon_y, icon_s, theme::INK_3, 0);
            ix += icon_s + 5.0;
            scene.text(ix, mid_y, branch_part, theme::INK_2, false, false, 0);
            ix += branch_part.chars().count() as f32 * m.cw;
            if shown_top_icons > 0 {
                ix += 8.0;
                let miy = (TOP_BAR_H - mini_icon_s) * 0.5;
                for &(ic, _) in top_icons.iter().take(shown_top_icons) {
                    scene.svg_icon(ix, miy, mini_icon_s, ic, 0);
                    ix += mini_icon_s + 2.5;
                }
                if !dirty_count_str.is_empty() {
                    scene.text(ix + 2.0, mid_y, &dirty_count_str, theme::INK_3, false, false, 0);
                }
            }
        }
        info_x0 - 18.0
    } else {
        rx - 12.0
    };

    // Clean minimal underline tabs with branch icons; active tab carries the one `--fire` bar.
    let active_id = store.active_branch_id();
    for branch in store.branches() {
        let is_active = branch.id == active_id;
        let name = branch.name.as_str();
        let count_str = format!(" {}", branch.blocks.len());
        let name_w = name.chars().count() as f32 * m.cw;
        let count_w = count_str.chars().count() as f32 * m.cw;
        let tab_w = icon_s + 6.0 + name_w + count_w;
        if x + tab_w > tabs_limit {
            break;
        }
        let icon_col = if is_active { theme::FIRE } else { theme::INK_4 };
        let name_col = if is_active { theme::INK_1 } else { theme::INK_3 };
        let count_col = if is_active { theme::INK_3 } else { theme::INK_4 };
        draw_icon(scene, Icon::Branch, x, icon_y, icon_s, icon_col, 0);
        scene.text(x + icon_s + 6.0, mid_y, name, name_col, is_active, false, 0);
        scene.text(x + icon_s + 6.0 + name_w, mid_y, count_str, count_col, false, false, 0);
        if is_active {
            scene.over_round_rect(x, TOP_BAR_H - 1.8, x + tab_w, TOP_BAR_H, 0.9, theme::FIRE, 0);
        }
        scene.push_hit(
            HitRect::new(x - 4.0, 0.0, x + tab_w + 4.0, TOP_BAR_H),
            HitAction::SwitchBranch(branch.id),
        );
        x += tab_w + 20.0;
    }
}

/// Draw a bespoke, monochrome-first native GPU prompt capsule with 1px hairline dividers,
/// Retina IntelliJ file icons for uncommitted files, SDF stash/divergence marks, and project
/// toolchain icons — replacing primitive ANSI rainbow Starship text.
#[allow(clippy::too_many_arguments)]
fn layout_starship_line(
    scene: &mut Scene,
    prompt: &PromptInfo,
    x0: f32,
    y: f32,
    max_w: f32,
    m: CellMetrics,
    clip_id: u32,
    is_live: bool,
) -> f32 {
    let cap_h = m.ch + 4.0;
    let ty = y + 2.0;
    let pad_x = 8.0;
    let sep_pad = 8.0;
    let folder_s = 12.5;
    let branch_s = 11.0;
    let file_s = 12.0;
    let tool_s = 11.5;

    let (cap_bg, cap_bd, sep_col, dir_col, branch_col, meta_col, icon_alpha) = if is_live {
        (
            theme::rgb(0x16, 0x16, 0x18),
            theme::rgb(0x28, 0x28, 0x2c),
            theme::rgb(0x26, 0x26, 0x2a),
            theme::INK_1,
            theme::INK_2,
            theme::INK_3,
            1.0f32,
        )
    } else {
        (
            theme::rgb(0x13, 0x13, 0x15),
            theme::rgb(0x1e, 0x1e, 0x21),
            theme::rgb(0x1f, 0x1f, 0x22),
            theme::INK_2,
            theme::INK_3,
            theme::INK_4,
            0.82f32,
        )
    };

    // Reserve outer capsule rect slot so we can patch exact width after laying out segments.
    let cap_idx = scene.rects.len();
    scene.round_rect(x0, y, x0 + 60.0, y + cap_h, 6.0, cap_bg, 1.0, cap_bd, clip_id);

    let mut cx = x0 + pad_x;
    let draw_divider = |scene: &mut Scene, x: f32| {
        scene.line(x, y + 4.0, x, y + cap_h - 4.0, 1.0, sep_col, clip_id);
    };

    // Segment 1: Location (macOS folder SVG + quiet parent path + crisp leaf dir).
    scene.svg_icon_alpha(cx, ty + (m.ch - folder_s) * 0.5, folder_s, "folder_macos", icon_alpha, clip_id);
    cx += folder_s + 6.0;

    let max_dir = ((max_w * 0.42) / m.cw).floor().max(12.0) as usize;
    let dir_lbl = truncate_chars(
        if prompt.dir_short.is_empty() { &prompt.cwd_display } else { &prompt.dir_short },
        max_dir,
    );
    if let Some(slash) = dir_lbl.rfind('/')
        && slash + 1 < dir_lbl.len()
    {
        let parent = &dir_lbl[..=slash];
        let leaf = &dir_lbl[slash + 1..];
        scene.text(cx, ty, parent, meta_col, false, false, clip_id);
        cx += parent.chars().count() as f32 * m.cw;
        scene.text(cx, ty, leaf, dir_col, is_live, false, clip_id);
        cx += leaf.chars().count() as f32 * m.cw;
    } else {
        scene.text(cx, ty, &dir_lbl, dir_col, is_live, false, clip_id);
        cx += dir_lbl.chars().count() as f32 * m.cw;
    }

    // Segment 2: Git branch + in-progress state (separated by crisp 1px vertical hairline).
    if let Some(branch) = &prompt.git_branch {
        cx += sep_pad;
        draw_divider(scene, cx);
        cx += sep_pad;

        let branch_icon_col = if prompt.git_detached {
            theme::FIRE
        } else if is_live {
            theme::INK_3
        } else {
            theme::INK_4
        };
        draw_icon(scene, Icon::Branch, cx, ty + (m.ch - branch_s) * 0.5, branch_s, branch_icon_col, clip_id);
        cx += branch_s + 5.0;

        let branch_lbl = if prompt.git_detached {
            format!("@{branch}")
        } else {
            branch.clone()
        };
        scene.text(cx, ty, &branch_lbl, branch_col, false, false, clip_id);
        cx += branch_lbl.chars().count() as f32 * m.cw;

        if let Some(st) = &prompt.git_status {
            if let Some(state_lbl) = &st.state_label {
                cx += 6.0;
                scene.text(cx, ty, state_lbl, theme::FIRE, true, false, clip_id);
                cx += state_lbl.chars().count() as f32 * m.cw;
            }

            // Segment 3: Uncommitted files (IntelliJ SVG file icons + quiet ember/green count).
            let total_dirty = st.conflicted + st.staged + st.modified + st.deleted + st.untracked;
            if !st.changed_icons.is_empty() {
                cx += sep_pad;
                draw_divider(scene, cx);
                cx += sep_pad;

                let shown_icons = st.changed_icons.len().min(4);
                let f_iy = ty + (m.ch - file_s) * 0.5;
                for &(icon_name, _) in st.changed_icons.iter().take(shown_icons) {
                    scene.svg_icon_alpha(cx, f_iy, file_s, icon_name, icon_alpha, clip_id);
                    cx += file_s + 3.0;
                }
                if total_dirty > 0 {
                    let cnt = format!("{total_dirty}");
                    let cnt_col = if st.conflicted > 0 {
                        theme::DANGER
                    } else if st.staged > 0 && st.modified == 0 && st.untracked == 0 && st.deleted == 0 {
                        theme::OK
                    } else if is_live {
                        theme::FIRE
                    } else {
                        meta_col
                    };
                    cx += 2.0;
                    scene.text(cx, ty, &cnt, cnt_col, is_live, false, clip_id);
                    cx += cnt.len() as f32 * m.cw;
                }
            } else if st.staged > 0 || st.conflicted > 0 {
                // Staged-only or conflict-only state (clean subtle dot + count, never ANSI brackets).
                cx += sep_pad;
                draw_divider(scene, cx);
                cx += sep_pad;
                let dot_col = if st.conflicted > 0 { theme::DANGER } else { theme::OK };
                scene.over_circle(cx + 3.0, ty + m.ch * 0.5, 2.6, dot_col, clip_id);
                cx += 9.0;
                let n = if st.conflicted > 0 { st.conflicted } else { st.staged };
                let cnt = format!("{n}");
                scene.text(cx, ty, &cnt, meta_col, false, false, clip_id);
                cx += cnt.len() as f32 * m.cw;
            }

            // Segment 4: Ahead/behind & stash count (crisp SDF layers icon, never pink `[$3]`).
            if st.ahead > 0 || st.behind > 0 || st.stashed > 0 {
                cx += sep_pad;
                draw_divider(scene, cx);
                cx += sep_pad;

                let mut need_gap = false;
                if st.ahead > 0 {
                    let s = format!("↑{}", st.ahead);
                    scene.text(cx, ty, &s, meta_col, false, false, clip_id);
                    cx += s.chars().count() as f32 * m.cw;
                    need_gap = true;
                }
                if st.behind > 0 {
                    if need_gap {
                        cx += 5.0;
                    }
                    let s = format!("↓{}", st.behind);
                    scene.text(cx, ty, &s, meta_col, false, false, clip_id);
                    cx += s.chars().count() as f32 * m.cw;
                    need_gap = true;
                }
                if st.stashed > 0 {
                    if need_gap {
                        cx += 6.0;
                    }
                    let st_s = 11.0;
                    draw_icon(scene, Icon::Stash, cx, ty + (m.ch - st_s) * 0.5, st_s, meta_col, clip_id);
                    cx += st_s + 3.5;
                    let s = format!("{}", st.stashed);
                    scene.text(cx, ty, &s, meta_col, false, false, clip_id);
                    cx += s.len() as f32 * m.cw;
                }
            }
        }
    }

    // Segment 5: Detected project toolchain SVG icons (quietly separated inside the capsule).
    if !prompt.project_icons.is_empty() {
        cx += sep_pad;
        draw_divider(scene, cx);
        cx += sep_pad;
        let t_iy = ty + (m.ch - tool_s) * 0.5;
        for (i, &icon_name) in prompt.project_icons.iter().take(3).enumerate() {
            if i > 0 {
                cx += 4.0;
            }
            scene.svg_icon_alpha(cx, t_iy, tool_s, icon_name, icon_alpha * 0.9, clip_id);
            cx += tool_s;
        }
    }

    let cap_x1 = (cx + pad_x).min(x0 + max_w);
    scene.rects[cap_idx].rect[2] = cap_x1;

    // Quiet elapsed time outside the capsule when the previous command took >= 1s.
    if prompt.last_duration_ms >= 1000 && cap_x1 + 70.0 < x0 + max_w {
        let took = format!("took {}", format_duration_ms(prompt.last_duration_ms));
        scene.text(cap_x1 + 10.0, ty, &took, meta_col, false, false, clip_id);
    }

    cap_h
}

#[allow(clippy::too_many_arguments)]
fn layout_block(
    scene: &mut Scene,
    block: &Block,
    show_context: bool,
    x0: f32,
    y0: f32,
    x1: f32,
    m: CellMetrics,
    clip_id: u32,
    cursor_visible: bool,
    now_ns: i64,
) -> f32 {
    let mut cur_y = y0;

    // Native segmented prompt capsule (always kept on every command block, calmer than live).
    if show_context {
        let cap_h = layout_starship_line(scene, &block.prompt_snapshot, x0, cur_y, x1 - x0, m, clip_id, false);
        cur_y += cap_h + 4.0;
    }

    // Right-aligned quiet icon buttons on the command row: fork icon, optional grid/table icon, duration, exit.
    let mut rx = x1;
    let act_s = 12.0;
    let act_iy = cur_y + (m.ch - act_s) * 0.5;
    let fx0 = rx - act_s;
    draw_icon(scene, Icon::Branch, fx0, act_iy, act_s, theme::INK_4, clip_id);
    scene.push_hit_clipped(
        HitRect::new(fx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0),
        clip_id,
        HitAction::ForkBlock(block.id),
    );
    rx = fx0 - 12.0;

    // Quiet icon-only table/grid view toggle on file-entry tables (`ls`, `ls -l`, `ls | sort-by ...`).
    if let BlockBody::Value(Value::Table(t)) = &block.body
        && is_file_table(t)
    {
        let in_grid = block.is_grid_view(t.default_grid);
        let toggle_icon = if in_grid { Icon::Table } else { Icon::Grid };
        let mx0 = rx - act_s;
        draw_icon(scene, toggle_icon, mx0, act_iy, act_s, theme::INK_4, clip_id);
        scene.push_hit_clipped(
            HitRect::new(mx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0),
            clip_id,
            HitAction::ToggleTableView(block.id),
        );
        rx = mx0 - 12.0;
    }

    if matches!(&block.body, BlockBody::Running { .. } | BlockBody::Terminal { running: true, .. }) {
        let elapsed_ms = (now_ns.max(0) as u64 / 1_000_000).saturating_sub(block.started_ms);
        let lbl = format_duration_ms(elapsed_ms);
        let w = lbl.len() as f32 * m.cw;
        scene.text(rx - w, cur_y, lbl, theme::FIRE, false, false, clip_id);
        scene.over_circle(rx - w - 9.0, cur_y + m.ch * 0.5, 3.0, theme::FIRE, clip_id);
        rx -= w + 20.0;
    } else {
        let dur_lbl = format_duration_ms(block.duration_ms);
        let dur_w = dur_lbl.len() as f32 * m.cw;
        scene.text(rx - dur_w, cur_y, dur_lbl, theme::INK_4, false, false, clip_id);
        rx -= dur_w + 12.0;

        if block.exit_code != 0 {
            let lbl = format!("exit {}", block.exit_code);
            let w = lbl.len() as f32 * m.cw;
            scene.text(rx - w, cur_y, lbl, theme::DANGER, false, false, clip_id);
            rx -= w + 12.0;
        }
    }

    // Command row: clean monochrome `❯` prompt (with `--fire` for Rust cells, red for errors).
    let prompt_col = match &block.body {
        BlockBody::Error { .. } | BlockBody::Value(Value::Error(_)) => theme::DANGER,
        BlockBody::RustCell { .. } => theme::FIRE,
        _ if block.exit_code != 0 => theme::DANGER,
        _ => theme::INK_3,
    };
    scene.text(x0, cur_y, "❯", prompt_col, true, false, clip_id);

    let cmd_x = x0 + 1.8 * m.cw;
    let cmd_max_x = (rx - 8.0).max(cmd_x + 100.0);
    let spans: Vec<HighlightSpan> = highlight(&block.source, Alloc::global())
        .iter()
        .cloned()
        .collect();
    let cmd_h = layout_highlighted_source(
        scene,
        &block.source,
        &spans,
        cmd_x,
        cur_y,
        cmd_max_x,
        m,
        clip_id,
    );
    cur_y += cmd_h;

    // Body flows flush left at `x0` (just like a native terminal — zero extra left indent!).
    let has_body = !matches!(&block.body, BlockBody::Value(Value::Nothing));
    if has_body {
        cur_y += 3.0;
        let body_w = (x1 - x0).max(120.0);
        let body_h = match &block.body {
            BlockBody::Value(val) => {
                layout_value(scene, block, val, x0, cur_y, body_w, m, clip_id, 0, now_ns)
            }
            BlockBody::StreamAndValue { lines, value } => {
                let mut sy = cur_y;
                let max_cols = (body_w / m.cw).floor().max(20.0) as usize;
                for line in lines {
                    for wrapped in wrap_lines(line, max_cols) {
                        scene.text(x0, sy, wrapped, theme::INK_2, false, false, clip_id);
                        sy += m.ch + 1.0;
                    }
                }
                if !matches!(value, Value::Nothing) {
                    sy += 3.0;
                    sy += layout_value(scene, block, value, x0, sy, body_w, m, clip_id, 0, now_ns);
                }
                sy - cur_y
            }
            BlockBody::Error { err } => {
                layout_error_panel(scene, &block.source, err, x0, cur_y, body_w, m, clip_id)
            }
            BlockBody::RustCell { compile_ms, run_ms, stdout, value, .. } => {
                layout_rust_cell_body(
                    scene,
                    block,
                    *compile_ms,
                    *run_ms,
                    stdout,
                    value,
                    x0,
                    cur_y,
                    body_w,
                    m,
                    clip_id,
                    now_ns,
                )
            }
            BlockBody::Terminal { term, running, .. } => {
                layout_inline_terminal(
                    scene,
                    term,
                    *running && cursor_visible,
                    x0,
                    cur_y,
                    body_w,
                    m,
                    clip_id,
                )
            }
            BlockBody::Running { label } => {
                scene.over_circle(x0 + 5.0, cur_y + m.ch * 0.5, 3.0, theme::FIRE, clip_id);
                let txt = if label.is_empty() { "running…" } else { label.as_str() };
                scene.text(x0 + 15.0, cur_y, txt, theme::INK_3, false, false, clip_id);
                m.ch + 2.0
            }
        };
        cur_y += body_h;
    }

    (cur_y - y0).max(m.ch)
}

#[allow(clippy::too_many_arguments)]
fn layout_highlighted_source(
    scene: &mut Scene,
    source: &str,
    spans: &[HighlightSpan],
    x0: f32,
    y0: f32,
    x1: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    if source.is_empty() {
        return m.ch;
    }
    let max_cols = ((x1 - x0) / m.cw).floor().max(10.0) as usize;
    let line_step = m.ch + 4.0;

    let mut byte_colors: Vec<[f32; 4]> = vec![theme::INK_1; source.len()];
    for sp in spans {
        let s = (sp.span.start as usize).min(source.len());
        let e = (sp.span.end as usize).min(source.len());
        let col = theme::highlight_color(sp.kind);
        for c in &mut byte_colors[s..e] {
            *c = col;
        }
    }

    let mut cur_y = y0;
    let mut byte_pos = 0usize;
    for raw_line in source.split('\n') {
        let line_len = raw_line.len();
        let line_start = byte_pos;
        byte_pos = (byte_pos + line_len + 1).min(source.len());

        let mut col = 0usize;
        let mut run_start_col = 0usize;
        let mut run_color = theme::INK_1;
        let mut run_buf = String::new();

        for (rel_idx, ch) in raw_line.char_indices() {
            if col >= max_cols {
                if !run_buf.is_empty() {
                    scene.text(
                        x0 + run_start_col as f32 * m.cw,
                        cur_y,
                        core::mem::take(&mut run_buf),
                        run_color,
                        false,
                        false,
                        clip_id,
                    );
                }
                cur_y += line_step;
                col = 0;
                run_start_col = 0;
            }
            let c = byte_colors.get(line_start + rel_idx).copied().unwrap_or(theme::INK_1);
            if !run_buf.is_empty() && c != run_color {
                scene.text(
                    x0 + run_start_col as f32 * m.cw,
                    cur_y,
                    core::mem::take(&mut run_buf),
                    run_color,
                    false,
                    false,
                    clip_id,
                );
                run_start_col = col;
                run_color = c;
            } else if run_buf.is_empty() {
                run_start_col = col;
                run_color = c;
            }
            run_buf.push(ch);
            col += 1;
        }
        if !run_buf.is_empty() {
            scene.text(
                x0 + run_start_col as f32 * m.cw,
                cur_y,
                run_buf,
                run_color,
                false,
                false,
                clip_id,
            );
        }
        cur_y += line_step;
    }

    (cur_y - y0 - 4.0).max(m.ch)
}

#[allow(clippy::too_many_arguments)]
fn layout_value(
    scene: &mut Scene,
    block: &Block,
    val: &Value<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
    depth: usize,
    now_ns: i64,
) -> f32 {
    match val {
        Value::Nothing => 0.0,
        Value::Table(t) => {
            if depth == 0 && is_file_table(t) && block.is_grid_view(t.default_grid) {
                layout_file_grid(scene, block, t, x0, y0, width, m, clip_id)
            } else {
                layout_table(scene, block, t, x0, y0, width, m, clip_id, depth, now_ns)
            }
        }
        Value::Record(r) => layout_record(scene, block, r, x0, y0, width, m, clip_id, depth, now_ns),
        Value::List(items) => layout_list(scene, block, items, x0, y0, width, m, clip_id, depth, now_ns),
        Value::Error(err) => {
            layout_error_panel(scene, &block.source, err, x0, y0, width, m, clip_id)
        }
        scalar => layout_scalar(scene, scalar, x0, y0, width, m, clip_id, now_ns),
    }
}

/// True when `table` represents filesystem entries (has a `FileEntry` `name` column).
fn is_file_table(table: &Table<'_>) -> bool {
    if let Some(ni) = table.col_index("name") {
        return table.col_display(ni) == ColDisplay::FileEntry || table.col_index("type").is_some();
    }
    false
}

/// If a block's command starts with `ls <dir>` (ignoring flags), return `<dir>` so dragging
/// a file out of that listing inserts `<dir>/<name>` cleanly.
fn resolve_block_dir_prefix(source: &str) -> Option<&str> {
    let stage = source.split(['|', ';', '\n']).next()?.trim();
    let mut parts = stage.split_whitespace();
    if parts.next()? != "ls" {
        return None;
    }
    for tok in parts {
        if tok.starts_with('-') {
            continue;
        }
        let cleaned = tok.trim_matches(|c| c == '"' || c == '\'').trim_end_matches('/');
        if !cleaned.is_empty() && cleaned != "." {
            return Some(cleaned);
        }
    }
    None
}

fn drag_path_for_entry(block_source: &str, entry_name: &str) -> String {
    if entry_name.contains('/') || entry_name.starts_with('~') {
        return entry_name.to_owned();
    }
    if let Some(dir) = resolve_block_dir_prefix(block_source) {
        format!("{dir}/{entry_name}")
    } else {
        entry_name.to_owned()
    }
}

/// Compact `eza --grid`-style multi-column file & directory view using authentic macOS blue
/// folders and IntelliJ New UI dark SVG icons — dense, instant scan, zero wasted height.
#[allow(clippy::too_many_arguments)]
fn layout_file_grid(
    scene: &mut Scene,
    block: &Block,
    table: &Table<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let total = table.nrows();
    if total == 0 {
        scene.text(x0, y0, "empty directory", theme::INK_4, false, true, clip_id);
        return m.ch;
    }
    let Some(name_col) = table.col_index("name") else {
        return m.ch;
    };
    let type_col = table.col_index("type");

    // In compact grid mode we can comfortably display up to 60 items before folding.
    const GRID_FOLD_ITEMS: usize = 60;
    let visible = if block.folded && total > GRID_FOLD_ITEMS {
        GRID_FOLD_ITEMS
    } else {
        total
    };

    let icon_s = 13.5;
    let icon_gap = 5.0;
    let col_gap = 14.0;

    // Pre-collect display labels + SVG icon names.
    struct GridItem {
        raw: String,
        label: String,
        icon: &'static str,
        is_dir: bool,
        w_px: f32,
    }
    let mut items: Vec<GridItem> = Vec::with_capacity(visible);
    for r in 0..visible {
        let raw = table
            .cell(r, name_col)
            .and_then(Value::as_str)
            .unwrap_or("");
        let kind = type_col.and_then(|ti| table.cell(r, ti)).and_then(Value::as_str);
        let is_dir = matches!(kind, Some("dir")) || raw.ends_with('/');
        let label = if is_dir && !raw.ends_with('/') {
            format!("{raw}/")
        } else {
            raw.to_owned()
        };
        let icon = icon_name_for_file(raw, kind);
        let label_chars = label.chars().count().min(32);
        let w_px = icon_s + icon_gap + label_chars as f32 * m.cw;
        items.push(GridItem { raw: raw.to_owned(), label, icon, is_dir, w_px });
    }

    // Pick the largest column count `ncols` (1..=6) whose column-major widths fit in `width`
    // (exact `eza` optimal grid packing!).
    let max_cols = visible.min(6);
    let mut best_cols = 1usize;
    let mut best_col_widths = vec![width];
    for nc in (1..=max_cols).rev() {
        let nrows = visible.div_ceil(nc);
        let mut widths = vec![0.0f32; nc];
        for (i, item) in items.iter().enumerate() {
            let c = i / nrows;
            if c < nc {
                widths[c] = widths[c].max(item.w_px);
            }
        }
        let sum: f32 = widths.iter().sum::<f32>() + (nc.saturating_sub(1) as f32) * col_gap;
        if sum <= width || nc == 1 {
            best_cols = nc;
            best_col_widths = widths;
            break;
        }
    }

    let nrows = visible.div_ceil(best_cols);
    let row_h = m.ch + 2.0;
    let mut cur_y = y0;

    for r in 0..nrows {
        let ty = cur_y + 1.0;
        let iy = ty + (m.ch - icon_s) * 0.5;
        let mut cx = x0;
        for c in 0..best_cols {
            let idx = c * nrows + r;
            let cw = best_col_widths[c];
            if let Some(item) = items.get(idx) {
                let max_ch = ((cw - icon_s - icon_gap).max(m.cw * 3.0) / m.cw).floor() as usize;
                let txt = truncate_chars(&item.label, max_ch.max(3));
                let item_w = icon_s + icon_gap + txt.chars().count() as f32 * m.cw;
                scene.svg_icon(cx, iy, icon_s, item.icon, clip_id);
                let col = if item.is_dir { theme::INK_1 } else { theme::INK_2 };
                scene.text(cx + icon_s + icon_gap, ty, txt, col, false, false, clip_id);
                scene.push_drag_source_clipped(
                    HitRect::new(cx - 3.0, cur_y, cx + item_w + 4.0, cur_y + row_h),
                    clip_id,
                    DragPayload {
                        label: item.label.clone(),
                        insert_text: drag_path_for_entry(&block.source, &item.raw),
                        icon: Some(item.icon),
                    },
                );
            }
            cx += cw + col_gap;
        }
        cur_y += row_h;
    }

    if total > GRID_FOLD_ITEMS {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!("+ {} more entries", total - GRID_FOLD_ITEMS)
        } else {
            format!("− collapse to {GRID_FOLD_ITEMS} entries")
        };
        let pw = lbl.chars().count() as f32 * m.cw + 8.0;
        scene.text(x0, cur_y, lbl, theme::INK_3, false, false, clip_id);
        scene.push_hit_clipped(
            HitRect::new(x0 - 4.0, cur_y - 2.0, x0 + pw, cur_y + m.ch + 2.0),
            clip_id,
            HitAction::ToggleFold(block.id),
        );
        cur_y += m.ch + 4.0;
    }

    cur_y - y0
}

/// Resolve the effective semantic display type of column `c` in `table` (using explicit
/// `ColDisplay` from builtins like `ls`/`ps`, or inferring cleanly for user tables).
fn effective_col_display(table: &Table<'_>, c: usize, visible_rows: usize) -> ColDisplay {
    let explicit = table.col_display(c);
    if explicit != ColDisplay::Auto {
        return explicit;
    }
    let name = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    if lower == "crate" || lower == "package" {
        return ColDisplay::Crate;
    }
    if lower == "file" || lower == "path" || (lower == "name" && table.col_index("type").is_some()) {
        return ColDisplay::FileEntry;
    }
    if lower == "status" || lower == "ok" || lower == "ready" {
        return ColDisplay::Status;
    }
    if lower == "modified" || lower == "updated" || lower == "created" {
        return ColDisplay::HumanTime;
    }
    // Inspect first non-nothing cell value for automatic typing.
    for r in 0..visible_rows {
        if let Some(cell) = table.cell(r, c) {
            match cell {
                Value::Nothing => continue,
                Value::Size(_) => return ColDisplay::FileSize,
                Value::Date(_) => return ColDisplay::HumanTime,
                Value::Duration(_) => return ColDisplay::Duration,
                Value::Bool(_) => return ColDisplay::Status,
                _ => break,
            }
        }
    }
    ColDisplay::Auto
}

/// Typed editorial dataframe renderer: custom icons per column type, inline folder/rust chips,
/// proportional micro-bars for byte sizes, human relative time for timestamps, and status dots.
#[allow(clippy::too_many_arguments)]
fn layout_table(
    scene: &mut Scene,
    block: &Block,
    table: &Table<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
    depth: usize,
    now_ns: i64,
) -> f32 {
    let ncols = table.ncols();
    if ncols == 0 {
        scene.text(x0, y0, "empty table", theme::INK_4, false, true, clip_id);
        return m.ch;
    }

    let visible_rows = if depth == 0 && block.folded && table.nrows() > TABLE_FOLD_ROWS {
        TABLE_FOLD_ROWS
    } else {
        table.nrows()
    };

    let type_col_idx = table.col_index("type");
    let icon_s = 14.0;
    let icon_pad = icon_s + 7.0;

    let idx_chars = table.nrows().max(1).to_string().len().max(2);
    let idx_w = idx_chars as f32 * m.cw + 16.0;
    let col_gap = 22.0;
    let avail_w = (width - idx_w).max(80.0);

    let mut col_displays: Vec<ColDisplay> = Vec::with_capacity(ncols);
    let mut col_chars: Vec<usize> = Vec::with_capacity(ncols);
    let mut col_right_align: Vec<bool> = Vec::with_capacity(ncols);
    let mut col_extra_px: Vec<f32> = Vec::with_capacity(ncols);
    let mut col_max_num: Vec<f64> = Vec::with_capacity(ncols);

    for c in 0..ncols {
        let disp = effective_col_display(table, c, visible_rows);
        col_displays.push(disp);

        let hdr = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
        let mut max_c = hdr.chars().count().max(3);
        let mut numeric_votes = 0usize;
        let mut non_empty = 0usize;
        let mut max_num = 0.0f64;

        for r in 0..visible_rows {
            if let Some(cell) = table.cell(r, c) {
                let mut txt = format_value_cell_at(cell, Some(now_ns));
                if disp == ColDisplay::FileEntry
                    && type_col_idx.and_then(|ti| table.cell(r, ti)).and_then(Value::as_str) == Some("dir")
                    && !txt.ends_with('/')
                {
                    txt.push('/');
                }
                max_c = max_c.max(txt.chars().count());
                if !matches!(cell, Value::Nothing) {
                    non_empty += 1;
                    if theme::is_right_aligned(cell) {
                        numeric_votes += 1;
                    }
                    match cell {
                        Value::Size(Bytes(b)) => max_num = max_num.max((*b).max(0) as f64),
                        Value::Float(f) if disp == ColDisplay::Percent => max_num = max_num.max(f.max(0.0)),
                        _ => {}
                    }
                }
            }
        }
        col_chars.push(max_c.min(40));
        col_right_align.push(
            matches!(disp, ColDisplay::FileSize | ColDisplay::Percent | ColDisplay::HumanTime)
                || (non_empty > 0 && numeric_votes * 2 >= non_empty),
        );
        let extra = match disp {
            ColDisplay::FileEntry | ColDisplay::Crate | ColDisplay::Process => icon_pad + 10.0,
            ColDisplay::Status => 14.0,
            ColDisplay::FileSize | ColDisplay::Percent if max_num > 0.0 => SIZE_BAR_W + 10.0,
            _ => 0.0,
        };
        col_extra_px.push(extra);
        col_max_num.push(max_num);
    }

    let mut col_widths: Vec<f32> = (0..ncols)
        .map(|c| col_chars[c] as f32 * m.cw + col_extra_px[c] + col_gap)
        .collect();
    let total_w: f32 = col_widths.iter().sum();
    if total_w > avail_w && total_w > 0.0 {
        let scale = avail_w / total_w;
        for w in &mut col_widths {
            *w = (*w * scale).max(52.0);
        }
    }
    let table_w = (idx_w + col_widths.iter().sum::<f32>() - col_gap * 0.4).min(width);
    let x1 = x0 + table_w;

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;

    // Minimal column headers + one crisp hairline underneath.
    scene.text(x0, y0, "#", theme::INK_4, false, false, clip_id);
    let mut cx = x0 + idx_w;
    for c in 0..ncols {
        let cw = col_widths[c];
        let content_w = (cw - col_gap).max(m.cw * 2.0);
        let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
        let hdr = truncate_chars(table.columns.get(c).map(|s| s.as_ref()).unwrap_or(""), max_ch);
        let tx = if col_right_align[c] {
            cx + content_w - hdr.chars().count() as f32 * m.cw
        } else {
            cx
        };
        scene.text(tx, y0, hdr, theme::INK_3, false, false, clip_id);
        cx += cw;
    }

    let hdr_line_y = y0 + m.ch + 6.0;
    scene.line(x0, hdr_line_y, x1, hdr_line_y, 1.0, theme::LINE, clip_id);

    let mut cur_y = hdr_line_y + 5.0;

    if visible_rows == 0 {
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, "0 rows", theme::INK_4, false, true, clip_id);
        cur_y += row_h;
    } else {
        for r in 0..visible_rows {
            let ty = cur_y + TABLE_ROW_PAD_Y;
            let iy = ty + (m.ch - icon_s) * 0.5;
            scene.text(x0, ty, r.to_string(), theme::INK_4, false, false, clip_id);

            let row_file_kind = type_col_idx
                .and_then(|ti| table.cell(r, ti))
                .and_then(Value::as_str);

            let mut expanded_for_row: Vec<(usize, &Value<'_>)> = Vec::new();
            let mut rx = x0 + idx_w;
            for c in 0..ncols {
                let cw = col_widths[c];
                let content_w = (cw - col_gap).max(m.cw * 2.0);
                let disp = col_displays[c];

                if let Some(cell) = table.cell(r, c) {
                    if depth == 0 && is_expandable_cell(cell) {
                        let raw = format_value_cell_at(cell, Some(now_ns));
                        let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
                        let txt = truncate_chars(&raw, max_ch);
                        let expanded = block.is_cell_expanded(r, c);
                        let badge_w = (txt.chars().count() as f32 * m.cw + 12.0).min(content_w + 4.0);
                        let bx0 = rx - 4.0;
                        let by0 = cur_y + 2.0;
                        let by1 = cur_y + row_h - 2.0;
                        let (bg, bd) = if expanded {
                            (theme::BG_TRACK, theme::LINE_STRONG)
                        } else {
                            (theme::BG_CHIP, theme::LINE)
                        };
                        scene.round_rect(bx0, by0, bx0 + badge_w, by1, 4.0, bg, 1.0, bd, clip_id);
                        scene.text(bx0 + 6.0, ty, txt, theme::INK_1, false, false, clip_id);
                        scene.push_hit_clipped(
                            HitRect::new(bx0, by0, bx0 + badge_w, by1),
                            clip_id,
                            HitAction::ToggleExpandCell { block: block.id, row: r, col: c },
                        );
                        if expanded {
                            expanded_for_row.push((c, cell));
                        }
                    } else {
                        match disp {
                            ColDisplay::FileEntry => {
                                let raw_name = format_value_cell_at(cell, Some(now_ns));
                                let icon_name = icon_name_for_file(&raw_name, row_file_kind);
                                let is_dir = matches!(row_file_kind, Some("dir")) || raw_name.ends_with('/');
                                let display_name = if is_dir && !raw_name.ends_with('/') {
                                    format!("{raw_name}/")
                                } else {
                                    raw_name.clone()
                                };
                                let max_ch = ((content_w - icon_pad - 8.0) / m.cw).floor().max(3.0) as usize;
                                let txt = truncate_chars(&display_name, max_ch);
                                let chip_w = (icon_pad + txt.chars().count() as f32 * m.cw + 10.0)
                                    .min(content_w + 2.0);
                                let bx0 = rx - 5.0;
                                let by0 = cur_y + 2.0;
                                let by1 = cur_y + row_h - 2.0;

                                // Sleek `ix.dev/blaze` inline chip for folders and `.rs` files.
                                if is_dir || icon_name == "rust_dark" {
                                    scene.round_rect(bx0, by0, bx0 + chip_w, by1, 4.0, theme::BG_CHIP, 0.0, [0.0; 4], clip_id);
                                }
                                scene.svg_icon(rx, iy, icon_s, icon_name, clip_id);
                                scene.text(rx + icon_pad, ty, txt, theme::INK_1, false, false, clip_id);
                                scene.push_drag_source_clipped(
                                    HitRect::new(bx0, cur_y, bx0 + chip_w, cur_y + row_h),
                                    clip_id,
                                    DragPayload {
                                        label: display_name,
                                        insert_text: drag_path_for_entry(&block.source, &raw_name),
                                        icon: Some(icon_name),
                                    },
                                );
                            }
                            ColDisplay::Crate => {
                                let raw = format_value_cell_at(cell, Some(now_ns));
                                let max_ch = ((content_w - icon_pad) / m.cw).floor().max(3.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                let item_w = icon_pad + txt.chars().count() as f32 * m.cw + 6.0;
                                scene.svg_icon(rx, iy, icon_s, "rust_dark", clip_id);
                                scene.text(rx + icon_pad, ty, txt, theme::INK_1, false, false, clip_id);
                                scene.push_drag_source_clipped(
                                    HitRect::new(rx - 4.0, cur_y, rx + item_w, cur_y + row_h),
                                    clip_id,
                                    DragPayload {
                                        label: raw.clone(),
                                        insert_text: raw,
                                        icon: Some("rust_dark"),
                                    },
                                );
                            }
                            ColDisplay::Process => {
                                let raw = format_value_cell_at(cell, Some(now_ns));
                                let max_ch = ((content_w - icon_pad) / m.cw).floor().max(3.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                draw_icon(scene, Icon::Terminal, rx, iy, icon_s, theme::INK_3, clip_id);
                                scene.text(rx + icon_pad, ty, txt, theme::INK_1, false, false, clip_id);
                            }
                            ColDisplay::Status => {
                                let raw = format_value_cell_at(cell, Some(now_ns));
                                let is_ok = matches!(cell, Value::Bool(true))
                                    || raw.eq_ignore_ascii_case("running")
                                    || raw.eq_ignore_ascii_case("ok")
                                    || raw == "R"
                                    || raw == "S";
                                let dot_y = ty + m.ch * 0.5;
                                if is_ok {
                                    scene.over_circle(rx + 4.0, dot_y, 3.0, theme::OK, clip_id);
                                } else {
                                    scene.over_ring(rx + 4.0, dot_y, 3.2, 1.1, theme::INK_4, clip_id);
                                }
                                let max_ch = ((content_w - 14.0) / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                let col = if is_ok { theme::INK_2 } else { theme::INK_3 };
                                scene.text(rx + 13.0, ty, txt, col, false, false, clip_id);
                            }
                            ColDisplay::FileSize => {
                                let raw = format_value_cell_at(cell, Some(now_ns));
                                let bar_on = col_max_num[c] > 0.0;
                                let text_area_w = if bar_on {
                                    (content_w - SIZE_BAR_W - 10.0).max(m.cw * 3.0)
                                } else {
                                    content_w
                                };
                                let max_ch = (text_area_w / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                let tx = rx + text_area_w - txt.chars().count() as f32 * m.cw;
                                scene.text(tx, ty, txt, theme::INK_2, false, false, clip_id);

                                if bar_on && let Value::Size(Bytes(b)) = cell {
                                    let frac = ((*b).max(0) as f64 / col_max_num[c]).clamp(0.0, 1.0) as f32;
                                    let bx0 = rx + text_area_w + 10.0;
                                    let by0 = ty + m.ch * 0.5 - 2.0;
                                    let by1 = by0 + 4.0;
                                    scene.round_rect(bx0, by0, bx0 + SIZE_BAR_W, by1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
                                    if frac > 0.01 {
                                        let fill_w = (SIZE_BAR_W * frac).max(3.0);
                                        let fill_col = if frac >= 0.99 { theme::FIRE } else { theme::INK_4 };
                                        scene.round_rect(bx0, by0, bx0 + fill_w, by1, 2.0, fill_col, 0.0, [0.0; 4], clip_id);
                                    }
                                }
                            }
                            ColDisplay::HumanTime => {
                                let raw = format_value_cell_at(cell, Some(now_ns));
                                let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                let tx = rx + content_w - txt.chars().count() as f32 * m.cw;
                                scene.text(tx, ty, txt, theme::INK_3, false, false, clip_id);
                            }
                            _ => {
                                let raw = format_value_cell_at(cell, Some(now_ns));
                                let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                let color = if c == 0 && matches!(cell, Value::String(_)) {
                                    theme::INK_1
                                } else if matches!(cell, Value::String(_)) {
                                    theme::INK_3
                                } else {
                                    theme::value_color(cell)
                                };
                                let tx = if col_right_align[c] {
                                    rx + content_w - txt.chars().count() as f32 * m.cw
                                } else {
                                    rx
                                };
                                scene.text(tx, ty, txt, color, false, false, clip_id);
                            }
                        }
                    }
                }
                rx += cw;
            }
            cur_y += row_h;

            for (col_idx, nested) in expanded_for_row {
                let col_name = table.columns.get(col_idx).map(|s| s.as_ref()).unwrap_or("cell");
                let drawer_x0 = x0 + idx_w;
                let drawer_w = (width - idx_w).max(120.0);
                let rail_top = cur_y + 2.0;
                scene.text(drawer_x0 + 12.0, cur_y + 2.0, col_name, theme::INK_3, false, false, clip_id);
                cur_y += m.ch + 6.0;
                let sub_h = layout_value(
                    scene,
                    block,
                    nested,
                    drawer_x0 + 12.0,
                    cur_y,
                    (drawer_w - 12.0).max(80.0),
                    m,
                    clip_id,
                    depth + 1,
                    now_ns,
                );
                cur_y += sub_h + 6.0;
                scene.over_round_rect(
                    drawer_x0,
                    rail_top,
                    drawer_x0 + 1.5,
                    cur_y - 4.0,
                    0.75,
                    theme::LINE_STRONG,
                    clip_id,
                );
            }
        }
    }

    if depth == 0 && table.nrows() > TABLE_FOLD_ROWS {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!("+ {} more rows", table.nrows() - TABLE_FOLD_ROWS)
        } else {
            format!("− collapse to {TABLE_FOLD_ROWS} rows")
        };
        let pw = lbl.chars().count() as f32 * m.cw + 8.0;
        scene.text(x0 + idx_w, cur_y, lbl, theme::INK_3, false, false, clip_id);
        scene.push_hit_clipped(
            HitRect::new(x0 + idx_w - 4.0, cur_y - 2.0, x0 + idx_w + pw, cur_y + m.ch + 2.0),
            clip_id,
            HitAction::ToggleFold(block.id),
        );
        cur_y += m.ch + 4.0;
    }

    cur_y - y0
}

/// Editorial borderless record with semantic SDF icons per known key + live memory bar.
#[allow(clippy::too_many_arguments)]
fn layout_record(
    scene: &mut Scene,
    block: &Block,
    rec: &Record<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
    depth: usize,
    now_ns: i64,
) -> f32 {
    if rec.is_empty() {
        scene.text(x0, y0, "{}", theme::INK_4, false, false, clip_id);
        return m.ch;
    }
    let has_any_icon = rec.iter().any(|(k, _)| icon_for_record_key(k).is_some());
    let icon_s = 12.0;
    let icon_gutter = if has_any_icon { icon_s + 8.0 } else { 0.0 };

    let max_key_chars = rec
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 28);
    let key_col_w = (icon_gutter + max_key_chars as f32 * m.cw + 22.0).min(width * 0.38);
    let val_x = x0 + key_col_w;
    let val_w = (width - key_col_w).max(80.0);
    let max_val_cols = (val_w / m.cw).floor().max(12.0) as usize;

    let mem_total_bytes = match rec.get("mem_total") {
        Some(Value::Size(Bytes(b))) if *b > 0 => Some(*b as f64),
        _ => None,
    };

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;
    let mut cur_y = y0;
    for (k, v) in rec.iter() {
        let start_y = cur_y;
        let ty = cur_y + TABLE_ROW_PAD_Y;
        let is_nested = depth < 2 && matches!(v, Value::Table(_) | Value::Record(_) | Value::List(_));

        if let Some((icon, icol)) = icon_for_record_key(k) {
            draw_icon(scene, icon, x0, ty + (m.ch - icon_s) * 0.5, icon_s, icol, clip_id);
        }
        let k_lbl = truncate_chars(k, max_key_chars);
        scene.text(x0 + icon_gutter, ty, k_lbl, theme::INK_3, false, false, clip_id);

        if is_nested {
            let sub_h = layout_value(scene, block, v, val_x, cur_y + 2.0, val_w, m, clip_id, depth + 1, now_ns);
            cur_y += (sub_h + 8.0).max(row_h);
        } else {
            let formatted = format_value_cell_at(v, Some(now_ns));
            let lines = wrap_lines(&formatted, max_val_cols);
            let color = theme::value_color(v);
            for (li, line) in lines.iter().enumerate() {
                scene.text(
                    val_x,
                    start_y + TABLE_ROW_PAD_Y + li as f32 * (m.ch + 2.0),
                    line,
                    color,
                    false,
                    false,
                    clip_id,
                );
            }
            // Live proportional usage bar right beside `mem_used` when `mem_total` is present.
            if k == "mem_used"
                && let (Some(total), Value::Size(Bytes(used))) = (mem_total_bytes, v)
                && *used > 0
            {
                let frac = (*used as f64 / total).clamp(0.0, 1.0) as f32;
                let bar_x0 = val_x + (formatted.chars().count() as f32 * m.cw) + 14.0;
                let bar_w = 88.0f32.min((val_w - (bar_x0 - val_x) - 42.0).max(0.0));
                if bar_w >= 24.0 {
                    let by0 = ty + m.ch * 0.5 - 2.5;
                    scene.round_rect(bar_x0, by0, bar_x0 + bar_w, by0 + 5.0, 2.5, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
                    let fill_col = if frac > 0.85 { theme::FIRE } else { theme::INK_3 };
                    scene.round_rect(
                        bar_x0,
                        by0,
                        bar_x0 + (bar_w * frac).max(3.0),
                        by0 + 5.0,
                        2.5,
                        fill_col,
                        0.0,
                        [0.0; 4],
                        clip_id,
                    );
                    let pct = format!("{:.0}%", frac * 100.0);
                    scene.text(bar_x0 + bar_w + 8.0, ty, pct, theme::INK_4, false, false, clip_id);
                }
            }
            let text_h = lines.len() as f32 * (m.ch + 2.0) + TABLE_ROW_PAD_Y * 2.0 - 2.0;
            cur_y += text_h.max(row_h);
        }
    }
    cur_y - y0
}

#[allow(clippy::too_many_arguments)]
fn layout_list(
    scene: &mut Scene,
    block: &Block,
    items: &blaze_sh::PVec<'_, Value<'_>>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
    depth: usize,
    now_ns: i64,
) -> f32 {
    if items.is_empty() {
        scene.text(x0, y0, "[]", theme::INK_4, false, false, clip_id);
        return m.ch;
    }
    let show = if depth == 0 && block.folded && items.len() > TABLE_FOLD_ROWS {
        TABLE_FOLD_ROWS
    } else {
        items.len()
    };
    let idx_w = (items.len().to_string().len().max(2) as f32 * m.cw + 16.0).max(32.0);
    let val_x = x0 + idx_w;
    let max_val_cols = ((width - idx_w) / m.cw).floor().max(12.0) as usize;

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;
    let mut cur_y = y0;
    for i in 0..show {
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, i.to_string(), theme::INK_4, false, false, clip_id);
        if let Some(val) = items.get(i) {
            let txt = truncate_chars(&format_value_cell_at(val, Some(now_ns)), max_val_cols);
            scene.text(val_x, cur_y + TABLE_ROW_PAD_Y, txt, theme::value_color(val), false, false, clip_id);
        }
        cur_y += row_h;
    }
    if depth == 0 && items.len() > TABLE_FOLD_ROWS {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!("+ {} more items", items.len() - TABLE_FOLD_ROWS)
        } else {
            format!("− collapse to {TABLE_FOLD_ROWS} items")
        };
        let pw = lbl.chars().count() as f32 * m.cw + 8.0;
        scene.text(val_x, cur_y, lbl, theme::INK_3, false, false, clip_id);
        scene.push_hit_clipped(
            HitRect::new(val_x - 4.0, cur_y - 2.0, val_x + pw, cur_y + m.ch + 2.0),
            clip_id,
            HitAction::ToggleFold(block.id),
        );
        cur_y += m.ch + 4.0;
    }
    cur_y - y0
}

#[allow(clippy::too_many_arguments)]
fn layout_scalar(
    scene: &mut Scene,
    val: &Value<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
    now_ns: i64,
) -> f32 {
    // Rich typed `Value::Path` (returned by `pwd`, etc.): macOS folder / IntelliJ file icon,
    // dimmed parent path + bright leaf segment, subtle `path` pill, and draggable into the prompt!
    if let Value::Path { path, kind } = val {
        let raw = path.as_ref();
        let kind_hint = match kind {
            PathKind::Dir => Some("dir"),
            PathKind::File => Some("file"),
            PathKind::Symlink => Some("symlink"),
            PathKind::Unknown => None,
        };
        let icon_name = if *kind == PathKind::Dir {
            "folder_macos"
        } else {
            icon_name_for_file(raw, kind_hint)
        };
        let icon_s = 14.0;
        let icon_pad = icon_s + 6.0;
        let type_lbl = "path";
        let pill_w = type_lbl.len() as f32 * m.cw + 14.0;
        let bx0 = x0 + width - pill_w;
        scene.round_rect(
            bx0,
            y0 - 1.0,
            x0 + width,
            y0 + m.ch + 1.0,
            (m.ch + 2.0) * 0.5,
            [0.0; 4],
            1.0,
            theme::LINE,
            clip_id,
        );
        scene.text(bx0 + 7.0, y0, type_lbl, theme::INK_3, false, false, clip_id);

        let avail_w = (bx0 - 14.0 - x0 - icon_pad).max(m.cw * 8.0);
        let max_ch = (avail_w / m.cw).floor().max(8.0) as usize;
        let shown = truncate_chars(raw, max_ch);
        let chip_w = (icon_pad + shown.chars().count() as f32 * m.cw + 12.0).min(bx0 - 10.0 - x0);
        scene.round_rect(
            x0 - 4.0,
            y0 - 2.0,
            x0 - 4.0 + chip_w,
            y0 + m.ch + 2.0,
            5.0,
            theme::BG_CHIP,
            0.0,
            [0.0; 4],
            clip_id,
        );
        scene.svg_icon(x0 + 2.0, y0 + (m.ch - icon_s) * 0.5, icon_s, icon_name, clip_id);
        let tx = x0 + 2.0 + icon_pad;
        if let Some(slash) = shown.rfind('/')
            && slash + 1 < shown.len()
        {
            let parent = &shown[..=slash];
            let leaf = &shown[slash + 1..];
            scene.text(tx, y0, parent, theme::INK_3, false, false, clip_id);
            let px = tx + parent.chars().count() as f32 * m.cw;
            let leaf_col = if *kind == PathKind::Dir { theme::PROMPT_DIR } else { theme::INK_1 };
            scene.text(px, y0, leaf, leaf_col, true, false, clip_id);
        } else {
            let leaf_col = if *kind == PathKind::Dir { theme::PROMPT_DIR } else { theme::INK_1 };
            scene.text(tx, y0, &shown, leaf_col, true, false, clip_id);
        }
        scene.push_drag_source_clipped(
            HitRect::new(x0 - 4.0, y0 - 2.0, x0 - 4.0 + chip_w, y0 + m.ch + 2.0),
            clip_id,
            DragPayload {
                label: shown,
                insert_text: raw.to_owned(),
                icon: Some(icon_name),
            },
        );
        return m.ch + 4.0;
    }

    let type_lbl = val.type_name();
    let type_w = type_lbl.len() as f32 * m.cw;
    let text_w = (width - type_w - 20.0).max(80.0);
    let max_cols = (text_w / m.cw).floor().max(10.0) as usize;
    let formatted = format_value_cell_at(val, Some(now_ns));
    let lines = wrap_lines(&formatted, max_cols);
    let color = theme::value_color(val);

    // Subtle pill-outlined type tag matching `Opt-in` in `ix.dev/blaze`!
    let pill_w = type_w + 14.0;
    let bx0 = x0 + width - pill_w;
    scene.round_rect(bx0, y0 - 1.0, x0 + width, y0 + m.ch + 1.0, (m.ch + 2.0) * 0.5, [0.0; 4], 1.0, theme::LINE, clip_id);
    scene.text(bx0 + 7.0, y0, type_lbl, theme::INK_3, false, false, clip_id);

    let mut cur_y = y0;
    for line in &lines {
        scene.text(x0, cur_y, line, color, false, false, clip_id);
        cur_y += m.ch + 2.0;
    }
    (cur_y - y0).max(m.ch + 2.0)
}

#[allow(clippy::too_many_arguments)]
fn layout_error_panel(
    scene: &mut Scene,
    source: &str,
    err: &ShellError<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let x1 = x0 + width;
    let bg_idx = scene.rects.len();
    scene.round_rect(x0, y0, x1, y0 + 24.0, 6.0, theme::DANGER_FAINT, 0.0, [0.0; 4], clip_id);
    let rail_idx = scene.rects.len();
    scene.round_rect(x0, y0, x0 + 2.0, y0 + 24.0, 1.0, theme::DANGER, 0.0, [0.0; 4], clip_id);

    let px = x0 + 14.0;
    let mut cur_y = y0 + 8.0;
    let icon_s = 12.0;

    draw_icon(scene, Icon::Error, px, cur_y + (m.ch - icon_s) * 0.5, icon_s, theme::DANGER, clip_id);
    let kind_x = px + icon_s + 7.0;
    let kind_lbl = err.kind.as_str();
    scene.text(kind_x, cur_y, kind_lbl, theme::DANGER, true, false, clip_id);

    let msg_x = kind_x + (kind_lbl.len() as f32 + 2.0) * m.cw;
    let msg_cols = ((x1 - 12.0 - msg_x) / m.cw).floor().max(14.0) as usize;
    let msg_lines = wrap_lines(err.message.as_str(), msg_cols);
    for (i, line) in msg_lines.iter().enumerate() {
        let tx = if i == 0 { msg_x } else { px };
        scene.text(tx, cur_y, line, theme::INK_1, false, false, clip_id);
        cur_y += m.ch + 3.0;
    }

    if !source.is_empty() {
        cur_y += 3.0;
        let lc = line_col(source, err.span.start);
        let line_text = &source[lc.line_start..lc.line_end];
        let gutter = format!("{} │ ", lc.line);
        let gutter_w = gutter.chars().count() as f32 * m.cw;

        scene.text(px, cur_y, gutter, theme::INK_4, false, false, clip_id);
        scene.text(px + gutter_w, cur_y, line_text, theme::INK_2, false, false, clip_id);

        let start_b = (err.span.start as usize).clamp(lc.line_start, lc.line_end);
        let end_b = (err.span.end as usize).clamp(start_b, lc.line_end);
        let col0 = source[lc.line_start..start_b].chars().count();
        let span_chars = source[start_b..end_b].chars().count().max(1);
        let ux0 = px + gutter_w + col0 as f32 * m.cw;
        let ux1 = (ux0 + span_chars as f32 * m.cw).min(x1 - 12.0);
        let uy = cur_y + m.ch + 1.0;
        scene.over_round_rect(ux0, uy, ux1.max(ux0 + m.cw), uy + 2.0, 1.0, theme::DANGER, clip_id);

        cur_y += m.ch + 8.0;
    }

    if let Some(help) = &err.help {
        let help_lbl = "help  ";
        scene.text(px, cur_y, help_lbl, theme::INK_3, false, false, clip_id);
        let hx = px + help_lbl.len() as f32 * m.cw;
        let h_cols = ((x1 - 12.0 - hx) / m.cw).floor().max(16.0) as usize;
        for (i, line) in wrap_lines(help.as_str(), h_cols).iter().enumerate() {
            scene.text(if i == 0 { hx } else { px }, cur_y, line, theme::INK_2, false, false, clip_id);
            cur_y += m.ch + 2.0;
        }
    }

    cur_y += 6.0;
    scene.rects[bg_idx].rect[3] = cur_y;
    scene.rects[rail_idx].rect[3] = cur_y;
    cur_y - y0
}

#[allow(clippy::too_many_arguments)]
fn layout_rust_cell_body(
    scene: &mut Scene,
    block: &Block,
    compile_ms: u64,
    run_ms: u64,
    stdout: &str,
    value: &Value<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
    now_ns: i64,
) -> f32 {
    let rail_idx = scene.rects.len();
    scene.round_rect(x0, y0, x0 + 2.0, y0 + 20.0, 1.0, theme::FIRE, 0.0, [0.0; 4], clip_id);

    let px = x0 + 14.0;
    let inner_w = (width - 14.0).max(80.0);
    let mut cur_y = y0;
    let icon_s = 14.0;

    scene.svg_icon(px, cur_y + (m.ch - icon_s) * 0.5, icon_s, "rust_dark", clip_id);
    let timing = if run_ms > 0 {
        format!(
            "blaze run · {} / {}",
            format_duration_ms(compile_ms),
            format_duration_ms(run_ms)
        )
    } else {
        format!("blaze run · {}", format_duration_ms(compile_ms))
    };
    scene.text(px + icon_s + 7.0, cur_y, timing, theme::INK_3, false, false, clip_id);
    cur_y += m.ch + 7.0;

    if !stdout.is_empty() {
        let max_cols = (inner_w / m.cw).floor().max(16.0) as usize;
        for line in wrap_lines(stdout, max_cols) {
            scene.text(px, cur_y, line, theme::INK_2, false, false, clip_id);
            cur_y += m.ch + 2.0;
        }
        cur_y += 4.0;
    }

    if !matches!(value, Value::Nothing) {
        cur_y += layout_value(scene, block, value, px, cur_y, inner_w, m, clip_id, 0, now_ns);
    }

    let end_y = cur_y.max(y0 + m.ch);
    scene.rects[rail_idx].rect[3] = end_y;
    end_y - y0
}

#[allow(clippy::too_many_arguments)]
fn layout_inline_terminal(
    scene: &mut Scene,
    term: &Terminal<'_>,
    cursor_on: bool,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let screen = term.screen();
    let top = screen.pages.active_top();
    let rows = screen.rows();

    let mut last_non_empty = if cursor_on { screen.cursor.y } else { 0 };
    for y in 0..rows {
        let cells = screen.pages.cells(top + y);
        if cells.iter().any(|c| c.codepoint() > 0x20) {
            last_non_empty = last_non_empty.max(y);
        }
    }
    let show_rows = (last_non_empty + 1).min(rows).max(1);
    let pad = 10.0;
    let total_h = show_rows as f32 * m.ch + pad * 2.0;
    let x1 = x0 + width;

    scene.round_rect(x0, y0, x1, y0 + total_h, 8.0, theme::BG_SURFACE, 1.0, theme::LINE, clip_id);
    let inner_clip = scene.push_clip(x0 + 1.0, y0 + 1.0, x1 - 1.0, y0 + total_h - 1.0, 7.0);
    draw_terminal_rows(
        scene,
        term,
        x0 + pad,
        y0 + pad,
        show_rows,
        m,
        cursor_on,
        inner_clip,
    );
    total_h
}

/// Lay out a full-screen `blaze-vt` terminal when an interactive program enters alternate screen.
pub fn layout_fullscreen_terminal(
    term: &Terminal<'_>,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    cursor_on: bool,
) -> Scene {
    let mut scene = Scene::new();
    let colors = term.colors();
    let bg0 = colors
        .background
        .get()
        .unwrap_or(VtRgb { r: 0x10, g: 0x10, b: 0x11 });
    scene.clear_color = [
        f32::from(bg0.r) / 255.0,
        f32::from(bg0.g) / 255.0,
        f32::from(bg0.b) / 255.0,
    ];
    scene.scroll_viewport_height = viewport_h;
    scene.input_top_y = viewport_h;
    let _ = viewport_w;
    let pad = 8.0;
    let rows = term.screen().rows();
    draw_terminal_rows(&mut scene, term, pad, pad, rows, m, cursor_on, 0);
    scene
}

#[allow(clippy::too_many_arguments)]
fn draw_terminal_rows(
    scene: &mut Scene,
    term: &Terminal<'_>,
    origin_x: f32,
    origin_y: f32,
    row_count: usize,
    m: CellMetrics,
    cursor_on: bool,
    clip_id: u32,
) {
    let colors = term.colors();
    let fg0 = colors
        .foreground
        .get()
        .unwrap_or(VtRgb { r: 0xeb, g: 0xea, b: 0xe6 });
    let bg0 = colors
        .background
        .get()
        .unwrap_or(VtRgb { r: 0x14, g: 0x14, b: 0x15 });
    let cursor_c = colors.cursor.get().unwrap_or(VtRgb { r: 0xeb, g: 0xea, b: 0xe6 });
    let screen = term.screen();
    let top = screen.pages.active_top();
    let (cx, cy) = (screen.cursor.x, screen.cursor.y);
    let cursor_visible = cursor_on && term.mode(VtMode::CursorVisible);

    for y in 0..row_count.min(screen.rows()) {
        let cells = screen.pages.cells(top + y);
        let y0 = origin_y + y as f32 * m.ch;
        for (x, cell) in cells.iter().enumerate() {
            let wide = cell.wide();
            if wide == Wide::SpacerTail || wide == Wide::SpacerHead {
                continue;
            }
            let st = screen.style(cell.style_id());
            let f = st.flags;
            let mut fg = resolve_vt_color(st.fg, colors, fg0, f.bold);
            let mut bg = resolve_vt_color(st.bg, colors, bg0, false);
            let has_bg = st.bg != VtColor::None;
            if f.inverse {
                core::mem::swap(&mut fg, &mut bg);
            }
            let span = if wide == Wide::Wide { 2.0 } else { 1.0 };
            let x0 = origin_x + x as f32 * m.cw;
            let x1 = x0 + m.cw * span;
            let at_cursor = cursor_visible && x == cx && y == cy;
            if at_cursor {
                scene.round_rect(x0, y0, x1, y0 + m.ch, 1.0, vt_rgbf(cursor_c), 0.0, [0.0; 4], clip_id);
                fg = bg0;
            } else if has_bg || f.inverse {
                scene.round_rect(
                    x0 - 0.5,
                    y0 - 0.5,
                    x1 + 0.5,
                    y0 + m.ch + 0.5,
                    0.0,
                    vt_rgbf(bg),
                    0.0,
                    [0.0; 4],
                    clip_id,
                );
            }
            let mut color = vt_rgbf(fg);
            if f.faint {
                color[3] = 0.55;
            }
            if f.underline != VtUnderline::None {
                let uy = y0 + m.ch - 2.0;
                scene.over_round_rect(x0, uy, x1, uy + 1.0, 0.0, color, clip_id);
            }
            if f.strikethrough {
                let sy = y0 + m.ch * 0.55;
                scene.over_round_rect(x0, sy, x1, sy + 1.0, 0.0, color, clip_id);
            }
            let cp = cell.codepoint();
            if cp <= 0x20 || f.invisible {
                continue;
            }
            if let Some(ch) = char::from_u32(cp) {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                scene.text(x0, y0, s, color, f.bold, f.italic, clip_id);
            }
        }
    }
}

fn vt_rgbf(c: VtRgb) -> [f32; 4] {
    [f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0, 1.0]
}

fn resolve_vt_color(c: VtColor, colors: &Colors, fallback: VtRgb, bold_bright: bool) -> VtRgb {
    match c {
        VtColor::None => fallback,
        VtColor::Palette(i) => {
            let idx = if bold_bright && i < 8 { i + 8 } else { i };
            colors.palette.current[usize::from(idx)]
        }
        VtColor::Rgb(rgb) => rgb,
    }
}

/// Live inline Starship prompt flowing directly underneath the last command output:
/// `folder dir on  branch [?] ✦` followed directly by `❯ <editable input + Fish ghost>`.
/// Zero separate bottom box; lights up softly as a drop target only when dragging an item.
#[allow(clippy::too_many_arguments)]
fn layout_inline_prompt(
    scene: &mut Scene,
    editor: &Editor,
    prompt: &PromptInfo,
    x0: f32,
    y0: f32,
    x1: f32,
    m: CellMetrics,
    clip_id: u32,
    cursor_visible: bool,
    drag: Option<&ActiveDrag>,
    cursor_trail: Option<CursorTrail>,
) -> f32 {
    let trimmed = editor.text.trim_start();
    let is_rust_mode = trimmed.starts_with("rust") || trimmed.starts_with(":rs");
    let drop_hover = drag.is_some_and(|d| d.over_composer);
    let vim_mode = editor.vim_mode();
    let vim_pending = editor.vim_pending();

    // Line 1: Bespoke segmented native prompt capsule (elevated live styling) + quiet right metadata.
    let cap_h = layout_starship_line(scene, prompt, x0, y0, (x1 - x0) * 0.78, m, clip_id, true);
    let meta_y = y0 + 2.0;

    let mut rx = x1;
    // Sleek Vim mode indicator when in Normal / Visual / VisualLine (plus pending keys like `ci`, `2d`).
    if vim_mode != VimMode::Insert || !vim_pending.is_empty() {
        let mode_lbl = if vim_pending.is_empty() {
            vim_mode.short_label().to_owned()
        } else {
            format!("{} {vim_pending}", vim_mode.short_label())
        };
        let mw = mode_lbl.chars().count() as f32 * m.cw + 12.0;
        let mx0 = rx - mw;
        let (pill_bg, pill_fg) = match vim_mode {
            VimMode::Normal => (theme::BG_CHIP, theme::INK_2),
            VimMode::Visual | VimMode::VisualLine => (theme::rgba(0xe0, 0x46, 0x1a, 0.14), theme::FIRE),
            VimMode::Insert => (theme::FIRE_FAINT, theme::FIRE),
        };
        scene.round_rect(mx0, y0, rx, y0 + cap_h, 5.0, pill_bg, 1.0, theme::LINE, clip_id);
        scene.text(mx0 + 6.0, meta_y, mode_lbl, pill_fg, true, false, clip_id);
        rx = mx0 - 10.0;
    }

    let right_txt = if drop_hover {
        format!("drop `{}` to insert", drag.unwrap().payload.label)
    } else if is_rust_mode {
        "rust · blaze run".to_owned()
    } else if prompt.last_exit != 0 {
        format!("exit {}  {}", prompt.last_exit, format_duration_ms(prompt.last_duration_ms))
    } else if prompt.last_duration_ms > 0 {
        format_duration_ms(prompt.last_duration_ms)
    } else {
        String::new()
    };
    if !right_txt.is_empty() {
        let right_col = if drop_hover || is_rust_mode {
            theme::FIRE
        } else if prompt.last_exit != 0 {
            theme::DANGER
        } else {
            theme::INK_4
        };
        let rw = right_txt.chars().count() as f32 * m.cw;
        scene.text(rx - rw, meta_y, right_txt, right_col, false, false, clip_id);
    }

    // Line 2+: `❯` prompt + editable syntax-highlighted text + Fish ghost text directly inline.
    let input_y = y0 + cap_h + 4.0;
    let line_step = m.ch + 2.0;
    let num_lines = editor.text.split('\n').count().max(1);
    let text_h = (num_lines as f32 * line_step - 2.0).max(m.ch);

    // Subtle inline drop halo ONLY when dragging an item (`Cargo.lock`, etc.) into the prompt.
    if drag.is_some() {
        let halo_border = if drop_hover { theme::FIRE } else { theme::LINE };
        let halo_fill = if drop_hover { theme::BG_SURFACE } else { [0.0; 4] };
        let bw = if drop_hover { 1.4 } else { 1.0 };
        scene.round_rect(
            x0 - 8.0,
            input_y - 4.0,
            x1 + 8.0,
            input_y + text_h + 4.0,
            6.0,
            halo_fill,
            bw,
            halo_border,
            clip_id,
        );
    }

    let prompt_col = if is_rust_mode || matches!(vim_mode, VimMode::Visual | VimMode::VisualLine) {
        theme::FIRE
    } else if vim_mode == VimMode::Normal {
        theme::INK_3
    } else if prompt.last_exit != 0 {
        theme::DANGER
    } else {
        theme::INK_1
    };
    scene.text(x0, input_y, "❯", prompt_col, true, false, clip_id);

    let text_x = x0 + 1.8 * m.cw;
    scene.composer_rect = HitRect::new(
        (x0 - 8.0).max(0.0),
        input_y - 4.0,
        x1 + 8.0,
        input_y + text_h + 6.0,
    );
    scene.composer_text_origin = [text_x, input_y];

    if let Some((sel_s, sel_e)) = editor.selection_range() {
        let mut line_start_b = 0usize;
        for (li, line) in editor.text.split('\n').enumerate() {
            let line_end_b = line_start_b + line.len();
            if sel_e > line_start_b && sel_s < line_end_b {
                let s_in = sel_s.saturating_sub(line_start_b).min(line.len());
                let e_in = sel_e.saturating_sub(line_start_b).min(line.len());
                let col_s = line[..s_in].chars().count();
                let col_e = line[..e_in].chars().count();
                let sx0 = text_x + col_s as f32 * m.cw;
                let sx1 = text_x + col_e as f32 * m.cw;
                let sy0 = input_y + li as f32 * line_step;
                scene.round_rect(sx0, sy0, sx1, sy0 + m.ch, 2.0, theme::BG_SELECTION, 0.0, [0.0; 4], clip_id);
            }
            line_start_b = line_end_b + 1;
        }
    }

    let rendered_h = if editor.text.is_empty() {
        m.ch
    } else {
        let spans = editor.highlight_spans();
        let h = layout_highlighted_source(scene, &editor.text, &spans, text_x, input_y, x1, m, clip_id);

        // Fish/Nushell inline ghost autosuggestion directly after the cursor on single-line input.
        if vim_mode == VimMode::Insert
            && let Some(ghost) = &editor.ghost
            && !editor.text.contains('\n')
            && editor.cursor == editor.text.len()
        {
            let gx = text_x + editor.text.chars().count() as f32 * m.cw;
            let avail_ch = ((x1 - gx) / m.cw).floor().max(0.0) as usize;
            if avail_ch > 0 {
                let gtxt = truncate_chars(ghost, avail_ch);
                scene.text(gx, input_y, gtxt, theme::GHOST_INK, false, false, clip_id);
            }
        }
        h
    };

    // Vim cursor shape (Block / Beam / Underline) + Ghostty-style EaseOutCubic sweep trail!
    let (cline, ccol) = editor.cursor_line_col();
    let cur_x = text_x + ccol as f32 * m.cw;
    let cur_y = input_y + cline as f32 * line_step;
    let shape = editor.vim_cursor_shape();
    let cur_rect = match shape {
        CursorShape::Beam => [cur_x, cur_y + 1.0, cur_x + 2.0, cur_y + m.ch - 1.0],
        CursorShape::Block => [cur_x, cur_y, cur_x + m.cw, cur_y + m.ch],
        CursorShape::Underline => [cur_x, cur_y + m.ch - 2.5, cur_x + m.cw, cur_y + m.ch],
    };
    scene.cursor_rect = Some(cur_rect);

    if cursor_visible {
        let cur_col = if is_rust_mode || drop_hover {
            theme::FIRE
        } else if shape == CursorShape::Underline {
            theme::FIRE
        } else if matches!(vim_mode, VimMode::Visual | VimMode::VisualLine) {
            theme::PROMPT_DIR
        } else {
            theme::INK_1
        };

        // Ghostty `crt.glsl` eased sweep trail (`DURATION = 0.2s`, `TRAIL_LENGTH = 0.5`, `EaseOutCubic`).
        if let Some(trail) = cursor_trail
            && trail.progress < 1.0
        {
            layout_ghostty_cursor_trail(scene, trail.prev_rect, cur_rect, trail.progress, cur_col, clip_id);
        }

        match shape {
            CursorShape::Beam => {
                scene.over_round_rect(
                    cur_rect[0],
                    cur_rect[1],
                    cur_rect[2],
                    cur_rect[3],
                    1.0,
                    cur_col,
                    clip_id,
                );
            }
            CursorShape::Underline => {
                scene.over_round_rect(
                    cur_rect[0],
                    cur_rect[1],
                    cur_rect[2],
                    cur_rect[3],
                    1.0,
                    cur_col,
                    clip_id,
                );
            }
            CursorShape::Block => {
                // Solid Vim block behind text + re-render character under block in inverted dark ink.
                scene.round_rect(
                    cur_rect[0],
                    cur_rect[1],
                    cur_rect[2],
                    cur_rect[3],
                    1.5,
                    cur_col,
                    0.0,
                    [0.0; 4],
                    clip_id,
                );
                if let Some(ch) = editor.text.get(editor.cursor..).and_then(|s| s.chars().next())
                    && ch != '\n'
                {
                    let mut tmp = [0u8; 4];
                    let s = ch.encode_utf8(&mut tmp);
                    scene.text(cur_x, cur_y, s, theme::BG_CANVAS, true, false, clip_id);
                }
            }
        }
    }

    (input_y + rendered_h.max(text_h) - y0) + 2.0
}

/// Ghostty `crt.glsl`-faithful EaseOutCubic cursor sweep trail (`TRAIL_LENGTH = 0.5`, `BLUR = 2.0`).
fn layout_ghostty_cursor_trail(
    scene: &mut Scene,
    prev: [f32; 4],
    cur: [f32; 4],
    progress: f32,
    color: [f32; 4],
    clip_id: u32,
) {
    const TRAIL_LENGTH: f32 = 0.5;
    let p = progress.clamp(0.0, 1.0);
    // EaseOutCubic: 1 - (1 - p)^3
    let shrink = 1.0 - (1.0 - p).powi(3);

    let prev_c = [(prev[0] + prev[2]) * 0.5, (prev[1] + prev[3]) * 0.5];
    let cur_c = [(cur[0] + cur[2]) * 0.5, (cur[1] + cur[3]) * 0.5];
    let cur_w = (cur[2] - cur[0]).max(2.0);
    let cur_h = (cur[3] - cur[1]).max(2.0);

    let dx = cur_c[0] - prev_c[0];
    let dy = cur_c[1] - prev_c[1];
    let dist = dx.hypot(dy);
    // Skip tiny 1-char typing jitters; fire cleanly on Vim motions / jumps / completions.
    if dist <= cur_h * 0.85 {
        return;
    }

    let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let alpha = (1.0 - shrink) * 0.52;
    if alpha <= 0.01 {
        return;
    }

    let is_straight = dx.abs() < 1.0 || dy.abs() < 1.0;
    if is_straight {
        // Exact `crt.glsl` straight-move shrunk bounding-rect sweep + 2px soft glow.
        let min_cx = prev_c[0].min(cur_c[0]);
        let max_cx = prev_c[0].max(cur_c[0]);
        let min_cy = prev_c[1].min(cur_c[1]);
        let max_cy = prev_c[1].max(cur_c[1]);

        let full_w = (max_cx - min_cx) + cur_w;
        let full_h = (max_cy - min_cy) + cur_h;
        let full_cx = (min_cx + max_cx) * 0.5;
        let full_cy = (min_cy + max_cy) * 0.5;

        let start_w = mix(cur_w, full_w, TRAIL_LENGTH);
        let start_h = mix(cur_h, full_h, TRAIL_LENGTH);
        let start_cx = mix(cur_c[0], full_cx, TRAIL_LENGTH);
        let start_cy = mix(cur_c[1], full_cy, TRAIL_LENGTH);

        let anim_w = mix(start_w, cur_w, shrink);
        let anim_h = mix(start_h, cur_h, shrink);
        let anim_cx = mix(start_cx, cur_c[0], shrink);
        let anim_cy = mix(start_cy, cur_c[1], shrink);

        let rx0 = anim_cx - anim_w * 0.5;
        let ry0 = anim_cy - anim_h * 0.5;
        let rx1 = anim_cx + anim_w * 0.5;
        let ry1 = anim_cy + anim_h * 0.5;
        let glow_col = [color[0], color[1], color[2], alpha * 0.65];
        scene.shadow(rx0, ry0, rx1, ry1, 1.5, 2.5, glow_col, clip_id);
        scene.round_rect(
            rx0,
            ry0,
            rx1,
            ry1,
            1.5,
            [color[0], color[1], color[2], alpha],
            0.0,
            [0.0; 4],
            clip_id,
        );
    } else {
        // Smooth diagonal sweep tail (e.g. `gg`/`G`/multi-line jumps).
        let tail_start_x = mix(cur_c[0], prev_c[0], TRAIL_LENGTH);
        let tail_start_y = mix(cur_c[1], prev_c[1], TRAIL_LENGTH);
        let tail_x = mix(tail_start_x, cur_c[0], shrink);
        let tail_y = mix(tail_start_y, cur_c[1], shrink);
        const STEPS: usize = 6;
        for i in 1..=STEPS {
            let frac = i as f32 / STEPS as f32;
            let cx = mix(tail_x, cur_c[0], frac);
            let cy = mix(tail_y, cur_c[1], frac);
            let step_a = alpha * (0.25 + 0.75 * frac) * 0.65;
            scene.round_rect(
                cx - cur_w * 0.5,
                cy - cur_h * 0.5,
                cx + cur_w * 0.5,
                cy + cur_h * 0.5,
                1.5,
                [color[0], color[1], color[2], step_a],
                0.0,
                [0.0; 4],
                clip_id,
            );
        }
    }
}

/// IntelliJ-style live autocomplete popup anchored directly below the inline prompt
/// (or flipped above it automatically when the prompt is near the bottom of the viewport).
#[allow(clippy::too_many_arguments)]
fn layout_completion_popup(
    scene: &mut Scene,
    editor: &Editor,
    x0: f32,
    prompt_y0: f32,
    prompt_y1: f32,
    vh: f32,
    x1: f32,
    m: CellMetrics,
) {
    let total = editor.completions.len();
    if total == 0 {
        return;
    }
    let show = total.min(MAX_POPUP_ROWS);
    let win_start = if editor.completion_selected >= show {
        editor.completion_selected + 1 - show
    } else {
        0
    };
    let row_h = m.ch + 8.0;
    let doc_h = m.ch + 12.0;
    let popup_h = show as f32 * row_h + doc_h + 10.0;
    let popup_w = (x1 - x0).min(560.0);

    // Anchor horizontally near the active cursor token (clamped inside `[x0, x1 - popup_w]`).
    let (_, ccol) = editor.cursor_line_col();
    let desired_x = scene.composer_text_origin[0] + (ccol as f32 * m.cw) - 36.0;
    let px0 = desired_x.clamp(x0, (x1 - popup_w).max(x0));
    let px1 = px0 + popup_w;

    // Prefer opening directly BELOW the inline prompt line; flip ABOVE if near window bottom.
    let below_y0 = prompt_y1 + 6.0;
    let py0 = if below_y0 + popup_h <= vh - 12.0 {
        below_y0.max(TOP_BAR_H + 8.0)
    } else {
        (prompt_y0 - 8.0 - popup_h).max(TOP_BAR_H + 8.0)
    };
    let py1 = py0 + popup_h;

    scene.overlay_shadow(px0, py0 + 4.0, px1, py1 + 4.0, 8.0, 18.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(
        px0,
        py0,
        px1,
        py1,
        8.0,
        theme::BG_POPUP,
        1.0,
        theme::LINE_STRONG,
        0,
    );

    let icon_s = 14.0;
    let mut ry = py0 + 5.0;
    for offset in 0..show {
        let idx = win_start + offset;
        let item = &editor.completions[idx];
        let is_sel = idx == editor.completion_selected;
        if is_sel {
            scene.overlay_round_rect(
                px0 + 5.0,
                ry,
                px1 - 5.0,
                ry + row_h,
                5.0,
                theme::BG_SELECTED,
                0.0,
                [0.0; 4],
                0,
            );
            // Crisp left selection rail inside the selected row (IntelliJ style).
            scene.overlay_round_rect(
                px0 + 5.0,
                ry + 4.0,
                px0 + 7.0,
                ry + row_h - 4.0,
                1.0,
                theme::FIRE,
                0.0,
                [0.0; 4],
                0,
            );
        }

        let ty = ry + (row_h - m.ch) * 0.5;
        let ix = px0 + 13.0;
        let iy = ry + (row_h - icon_s) * 0.5;

        if let Some(svg_name) = item.icon {
            scene.overlay_svg_icon(ix, iy, icon_s, svg_name, 0);
        } else {
            let (glyph, bg_col, fg_col) = match item.kind {
                CompletionKind::Abbrev => ("→", theme::FIRE_FAINT, theme::FIRE),
                CompletionKind::Builtin => ("f", theme::rgba(0x39, 0x87, 0xe5, 0.16), theme::SERIES_BLUE),
                CompletionKind::Def => ("λ", theme::FIRE_FAINT, theme::FIRE),
                CompletionKind::Keyword => ("k", theme::rgba(0xc7, 0x9b, 0xf2, 0.16), theme::PROMPT_BRANCH),
                CompletionKind::Flag => ("-", theme::BG_TRACK, theme::INK_3),
                CompletionKind::Variable => ("$", theme::rgba(0x6e, 0xd8, 0xce, 0.16), theme::PROMPT_DIR),
                CompletionKind::Column => ("▤", theme::rgba(0x7a, 0xd8, 0x96, 0.16), theme::PROMPT_OK),
                CompletionKind::Dir => ("d", theme::rgba(0x39, 0x87, 0xe5, 0.16), theme::SERIES_BLUE),
                CompletionKind::File => ("·", theme::BG_TRACK, theme::INK_3),
                CompletionKind::External => ("›", theme::BG_TRACK, theme::INK_2),
            };
            scene.overlay_round_rect(ix, iy, ix + icon_s, iy + icon_s, 3.5, bg_col, 0.0, [0.0; 4], 0);
            let gx = ix + (icon_s - m.cw) * 0.5;
            scene.overlay_text_run(gx, ty, glyph, fg_col, true, false, 0);
        }

        // Right-aligned type / file-size detail.
        let detail_txt = truncate_chars(&item.detail, 18);
        let detail_w = detail_txt.chars().count() as f32 * m.cw;
        let detail_x = px1 - 14.0 - detail_w;
        scene.overlay_text_run(
            detail_x,
            ty,
            &detail_txt,
            if is_sel { theme::INK_3 } else { theme::INK_4 },
            false,
            false,
            0,
        );

        // Label with IntelliJ-style matched-character highlighting.
        let label_x = ix + icon_s + 9.0;
        let max_label_ch = ((detail_x - 14.0 - label_x) / m.cw).floor().max(6.0) as usize;
        let label_txt = truncate_chars(&item.label, max_label_ch);
        let base_col = if is_sel { theme::INK_1 } else { theme::INK_2 };

        if item.match_indices.is_empty() {
            scene.overlay_text_run(label_x, ty, &label_txt, base_col, is_sel, false, 0);
        } else {
            let mut run = String::new();
            let mut run_start_col = 0usize;
            let mut run_matched = false;
            for (ci, ch) in label_txt.chars().enumerate() {
                let matched = item.match_indices.contains(&ci);
                if !run.is_empty() && matched != run_matched {
                    let col = if run_matched { theme::MATCH_HIGHLIGHT } else { base_col };
                    scene.overlay_text_run(
                        label_x + run_start_col as f32 * m.cw,
                        ty,
                        core::mem::take(&mut run),
                        col,
                        run_matched || is_sel,
                        false,
                        0,
                    );
                    run_start_col = ci;
                    run_matched = matched;
                } else if run.is_empty() {
                    run_start_col = ci;
                    run_matched = matched;
                }
                run.push(ch);
            }
            if !run.is_empty() {
                let col = if run_matched { theme::MATCH_HIGHLIGHT } else { base_col };
                scene.overlay_text_run(
                    label_x + run_start_col as f32 * m.cw,
                    ty,
                    run,
                    col,
                    run_matched || is_sel,
                    false,
                    0,
                );
            }
        }

        scene.push_hit(
            HitRect::new(px0 + 5.0, ry, px1 - 5.0, ry + row_h),
            HitAction::SelectCompletion(idx),
        );
        ry += row_h;
    }

    // Bottom IntelliJ Quick-Doc strip for the currently selected item.
    let div_y = py1 - doc_h;
    scene.overlay_line(px0 + 1.0, div_y, px1 - 1.0, div_y, 1.0, theme::LINE, 0);
    let doc_ty = div_y + (doc_h - m.ch) * 0.5;
    let hint = "↑↓  tab  esc";
    let hint_w = hint.chars().count() as f32 * m.cw;
    scene.overlay_text_run(px1 - 12.0 - hint_w, doc_ty, hint, theme::INK_4, false, false, 0);

    if let Some(sel) = editor.completions.get(editor.completion_selected) {
        let max_doc = ((px1 - 24.0 - hint_w - (px0 + 13.0)) / m.cw).floor().max(8.0) as usize;
        let doc_txt = truncate_chars(&sel.doc, max_doc);
        scene.overlay_text_run(px0 + 13.0, doc_ty, doc_txt, theme::INK_3, false, false, 0);
    }
}

/// Floating pill rendered under the cursor while dragging a file/folder/crate into the hotbar.
fn layout_drag_overlay(scene: &mut Scene, drag: &ActiveDrag, m: CellMetrics) {
    let icon_s = 14.5;
    let label = truncate_chars(&drag.payload.label, 34);
    let text_w = label.chars().count() as f32 * m.cw;
    let has_icon = drag.payload.icon.is_some();
    let pad_x = 10.0;
    let pill_w = pad_x * 2.0 + if has_icon { icon_s + 7.0 } else { 0.0 } + text_w;
    let pill_h = m.ch + 10.0;
    let x0 = drag.x + 12.0;
    let y0 = (drag.y - pill_h * 0.5).max(TOP_BAR_H + 4.0);
    let x1 = x0 + pill_w;
    let y1 = y0 + pill_h;
    let border = if drag.over_composer { theme::FIRE } else { theme::LINE_STRONG };

    scene.overlay_shadow(x0, y0 + 3.0, x1, y1 + 3.0, 7.0, 14.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(x0, y0, x1, y1, 7.0, theme::BG_POPUP, 1.2, border, 0);

    let mut cx = x0 + pad_x;
    if let Some(svg_name) = drag.payload.icon {
        scene.overlay_svg_icon(cx, y0 + (pill_h - icon_s) * 0.5, icon_s, svg_name, 0);
        cx += icon_s + 7.0;
    }
    scene.overlay_text_run(cx, y0 + (pill_h - m.ch) * 0.5, label, theme::INK_1, false, false, 0);
}

/// Native Atuin-class `Ctrl+R` command history palette: scored fuzzy search, color-coded
/// exit/duration column, relative timestamps, directory tags, clickable scope pill (`GLOBAL` /
/// `DIR` / `SESSION`), fuzzy match highlights, and live command preview.
fn layout_reverse_search_overlay(
    scene: &mut Scene,
    rs: &crate::editor::ReverseSearch,
    vw: f32,
    vh: f32,
    m: CellMetrics,
    now_ns: i64,
) {
    let max_rows = 12usize;
    let total_matches = rs.results.len();
    let visible = total_matches.min(max_rows).max(1);
    let win_start = if rs.selected >= max_rows {
        rs.selected + 1 - max_rows
    } else {
        0
    };

    let pad_x = 14.0;
    let header_h = m.ch + 16.0;
    let row_h = m.ch + 7.0;
    let input_h = m.ch + 16.0;
    let preview_h = m.ch + 14.0;
    let list_h = visible as f32 * row_h + 8.0;
    let modal_h = (header_h + list_h + input_h + preview_h).min(vh - TOP_BAR_H - 28.0);
    let modal_w = (vw - 48.0).clamp(440.0, 860.0);
    let x0 = ((vw - modal_w) * 0.5).max(12.0);
    let x1 = x0 + modal_w;
    let y0 = (TOP_BAR_H + ((vh - TOP_BAR_H - modal_h) * 0.42)).max(TOP_BAR_H + 12.0);
    let y1 = y0 + modal_h;

    // Subtle full-window backdrop dim + elevated crisp modal card.
    scene.overlay_round_rect(0.0, TOP_BAR_H, vw, vh, 0.0, theme::rgba(0x00, 0x00, 0x00, 0.36), 0.0, [0.0; 4], 0);
    scene.overlay_shadow(x0, y0 + 6.0, x1, y1 + 6.0, 10.0, 24.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(x0, y0, x1, y1, 10.0, theme::BG_POPUP, 1.0, theme::LINE_STRONG, 0);

    // 1. Header bar: title, match count, quiet shortcuts.
    let h_ty = y0 + (header_h - m.ch) * 0.5;
    scene.overlay_text_run(x0 + pad_x, h_ty, "history", theme::INK_1, true, false, 0);
    let count_lbl = format!("  {total_matches} / {}", rs.total_entries);
    scene.overlay_text_run(x0 + pad_x + 7.0 * m.cw, h_ty, &count_lbl, theme::INK_4, false, false, 0);
    let hints = "esc exit  ·  tab edit  ·  enter run  ·  ctrl+r scope";
    let hints_w = hints.chars().count() as f32 * m.cw;
    if x1 - pad_x - hints_w > x0 + pad_x + 20.0 * m.cw {
        scene.overlay_text_run(x1 - pad_x - hints_w, h_ty, hints, theme::INK_4, false, false, 0);
    }
    scene.overlay_line(x0 + 1.0, y0 + header_h, x1 - 1.0, y0 + header_h, 1.0, theme::LINE, 0);

    // 2. Results list (Atuin bottom-up: best/newest match sits directly above the search input!).
    let list_top = y0 + header_h + 4.0;
    let list_bottom = y1 - input_h - preview_h - 4.0;
    if rs.results.is_empty() {
        let mid_y = (list_top + list_bottom - m.ch) * 0.5;
        scene.overlay_text_run(x0 + pad_x, mid_y, "no matching commands", theme::INK_4, false, true, 0);
    } else {
        let dur_col_w = 7.5 * m.cw;
        let ago_col_w = 10.0 * m.cw;
        let idx_col_w = 2.4 * m.cw;
        let dir_col_w = if rs.scope == crate::editor::HistoryScope::Global && modal_w > 640.0 {
            16.0 * m.cw
        } else {
            0.0
        };

        let show_count = total_matches.saturating_sub(win_start).min(max_rows);
        for slot in 0..show_count {
            // Render index `win_start + slot` from bottom upward (`slot == 0` is bottom-most!).
            let idx = win_start + slot;
            let row_from_top = show_count - 1 - slot;
            let ry = list_top + row_from_top as f32 * row_h;
            if ry + row_h > list_bottom + 2.0 {
                continue;
            }
            let item = &rs.results[idx];
            let is_sel = idx == rs.selected;
            let ty = ry + (row_h - m.ch) * 0.5;

            if is_sel {
                scene.overlay_round_rect(
                    x0 + 6.0,
                    ry,
                    x1 - 6.0,
                    ry + row_h,
                    5.0,
                    theme::BG_SELECTED,
                    0.0,
                    [0.0; 4],
                    0,
                );
                scene.overlay_round_rect(
                    x0 + 6.0,
                    ry + 4.0,
                    x0 + 8.0,
                    ry + row_h - 4.0,
                    1.0,
                    theme::FIRE,
                    0.0,
                    [0.0; 4],
                    0,
                );
            }

            let mut cx = x0 + pad_x;
            // Selection caret or compact relative shortcut number (`1..9` above selected).
            let idx_lbl = if is_sel {
                "❯".to_owned()
            } else if idx > rs.selected && idx - rs.selected <= 9 {
                format!("{}", idx - rs.selected)
            } else {
                String::new()
            };
            if !idx_lbl.is_empty() {
                let col = if is_sel { theme::FIRE } else { theme::INK_4 };
                scene.overlay_text_run(cx, ty, idx_lbl, col, is_sel, false, 0);
            }
            cx += idx_col_w;

            // Color-coded duration (`OK` green when exit == 0, `DANGER` rose when exit != 0).
            let dur_lbl = format_duration_ms(item.entry.duration_ms);
            let dur_col = if item.entry.exit_code == 0 {
                theme::OK
            } else {
                theme::DANGER
            };
            let dur_x = cx + (dur_col_w - dur_lbl.len() as f32 * m.cw - m.cw).max(0.0);
            scene.overlay_text_run(dur_x, ty, dur_lbl, dur_col, false, false, 0);
            cx += dur_col_w;

            // Relative timestamp (`5m ago`, `2h ago`, `26d ago`).
            let ago_lbl = if item.entry.timestamp_ns > 0 {
                let mut s = String::new();
                let _ = format_relative_time(
                    DateTime { utc_ns: item.entry.timestamp_ns, offset_secs: 0 },
                    now_ns,
                    &mut s,
                );
                s
            } else {
                "session".to_owned()
            };
            let ago_x = cx + (ago_col_w - ago_lbl.len() as f32 * m.cw - 1.2 * m.cw).max(0.0);
            scene.overlay_text_run(ago_x, ty, ago_lbl, theme::PROMPT_SPARK, false, false, 0);
            cx += ago_col_w;

            // Command text with fuzzy matched-character highlighting!
            let cmd_right = x1 - pad_x - dir_col_w;
            let max_cmd_ch = ((cmd_right - cx) / m.cw).floor().max(10.0) as usize;
            let cmd_one_line = item.entry.command.replace('\n', " ↵ ");
            let shown_cmd = truncate_chars(&cmd_one_line, max_cmd_ch);
            let base_col = if is_sel { theme::INK_1 } else { theme::INK_2 };

            if item.match_indices.is_empty() {
                scene.overlay_text_run(cx, ty, &shown_cmd, base_col, is_sel, false, 0);
            } else {
                let mut run = String::new();
                let mut run_start = 0usize;
                let mut run_m = false;
                for (ci, ch) in shown_cmd.chars().enumerate() {
                    let matched = item.match_indices.contains(&ci);
                    if !run.is_empty() && matched != run_m {
                        let col = if run_m { theme::MATCH_HIGHLIGHT } else { base_col };
                        scene.overlay_text_run(
                            cx + run_start as f32 * m.cw,
                            ty,
                            core::mem::take(&mut run),
                            col,
                            run_m || is_sel,
                            false,
                            0,
                        );
                        run_start = ci;
                        run_m = matched;
                    } else if run.is_empty() {
                        run_start = ci;
                        run_m = matched;
                    }
                    run.push(ch);
                }
                if !run.is_empty() {
                    let col = if run_m { theme::MATCH_HIGHLIGHT } else { base_col };
                    scene.overlay_text_run(
                        cx + run_start as f32 * m.cw,
                        ty,
                        run,
                        col,
                        run_m || is_sel,
                        false,
                        0,
                    );
                }
            }

            // Optional right-aligned leaf dir badge in GLOBAL mode.
            if dir_col_w > 0.0 && !item.entry.cwd.is_empty() {
                let leaf = item.entry.cwd.rsplit('/').next().unwrap_or(&item.entry.cwd);
                let leaf_txt = truncate_chars(leaf, 14);
                let lw = leaf_txt.chars().count() as f32 * m.cw;
                scene.overlay_text_run(x1 - pad_x - lw, ty, leaf_txt, theme::INK_4, false, false, 0);
            }

            scene.push_hit(
                HitRect::new(x0 + 6.0, ry, x1 - 6.0, ry + row_h),
                HitAction::SelectHistory(idx),
            );
        }
    }

    // 3. Search input bar with clickable `[ GLOBAL | DIR | SESSION ]` scope pill.
    let in_y0 = y1 - input_h - preview_h;
    scene.overlay_line(x0 + 1.0, in_y0, x1 - 1.0, in_y0, 1.0, theme::LINE, 0);
    let in_ty = in_y0 + (input_h - m.ch) * 0.5;
    let scope_lbl = rs.scope.label();
    let scope_w = scope_lbl.len() as f32 * m.cw + 16.0;
    let sb_x0 = x0 + pad_x;
    let sb_y0 = in_y0 + 5.0;
    let sb_y1 = in_y0 + input_h - 5.0;
    scene.overlay_round_rect(sb_x0, sb_y0, sb_x0 + scope_w, sb_y1, 5.0, theme::BG_CHIP, 1.0, theme::LINE_STRONG, 0);
    scene.overlay_text_run(sb_x0 + 8.0, in_ty, scope_lbl, theme::PROMPT_DIR, true, false, 0);
    scene.push_hit(HitRect::new(sb_x0, sb_y0, sb_x0 + scope_w, sb_y1), HitAction::CycleHistoryScope);

    let qx = sb_x0 + scope_w + 12.0;
    scene.overlay_text_run(qx, in_ty, "❯", theme::FIRE, true, false, 0);
    let q_txt_x = qx + 1.8 * m.cw;
    if rs.query.is_empty() {
        scene.overlay_text_run(q_txt_x, in_ty, "fuzzy search command history…", theme::INK_4, false, false, 0);
    } else {
        scene.overlay_text_run(q_txt_x, in_ty, &rs.query, theme::INK_1, false, false, 0);
    }
    let cur_x = q_txt_x + rs.query.chars().count() as f32 * m.cw;
    scene.overlay_round_rect(cur_x, in_ty + 1.0, cur_x + 1.8, in_ty + m.ch - 1.0, 0.9, theme::FIRE, 0.0, [0.0; 4], 0);

    // 4. Bottom full-command preview & metadata strip.
    let pv_y0 = y1 - preview_h;
    scene.overlay_line(x0 + 1.0, pv_y0, x1 - 1.0, pv_y0, 1.0, theme::LINE_FAINT, 0);
    let pv_ty = pv_y0 + (preview_h - m.ch) * 0.5;
    if let Some(sel) = rs.results.get(rs.selected) {
        let mut meta = String::new();
        if !sel.entry.cwd.is_empty() {
            meta.push_str(&sel.entry.cwd);
        }
        if let Some(br) = &sel.entry.git_branch {
            if !meta.is_empty() {
                meta.push_str("  ");
            }
            meta.push_str(br);
        }
        if sel.entry.exit_code != 0 {
            if !meta.is_empty() {
                meta.push_str("  ");
            }
            meta.push_str(&format!("exit {}", sel.entry.exit_code));
        }
        let meta_w = (meta.chars().count().min(36) as f32) * m.cw;
        let meta_shown = truncate_chars(&meta, 36);
        if !meta_shown.is_empty() {
            scene.overlay_text_run(x1 - pad_x - meta_w, pv_ty, meta_shown, theme::INK_4, false, false, 0);
        }
        let max_pv = ((x1 - pad_x * 2.0 - meta_w - 14.0) / m.cw).floor().max(12.0) as usize;
        let pv_cmd = truncate_chars(&sel.entry.command.replace('\n', " ↵ "), max_pv);
        scene.overlay_text_run(x0 + pad_x, pv_ty, pv_cmd, theme::INK_3, false, false, 0);
    }
}
