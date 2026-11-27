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

use crate::blocks::{Block, BlockBody, BlockId, BlockStore, BranchId, TABLE_FOLD_ROWS};
use crate::editor::{
    COMPLETION_VISIBLE_ROWS, CommandPalette, CompletionKind, Editor, HISTORY_VISIBLE_ROWS,
    LiveHint, PALETTE_VISIBLE_ROWS,
};
use crate::icons::{Icon, draw_icon, icon_for_record_key};
use crate::prompt::{GitFileStatus, PromptInfo};
use crate::svg_icons::icon_name_for_file;
use crate::theme;
use crate::tiling::{PaneId, SplitAxis, SplitHandle, SplitId, TabId};
use crate::view::{
    ActiveDrag, ActivePopover, CellMetrics, CursorTrail, DragPayload, HitAction, HitRect,
    HoverKind, OutputNav, OutputVisualMode, Scene, TextFace, TextSelection, compute_text_selection,
    extract_visual_lines, format_duration_ms, format_duration_ns, format_value_cell_at,
    is_expandable_cell,
    output_nav_geometry, truncate_chars, truncate_middle, wrap_lines,
};

pub const TOP_BAR_H: f32 = 34.0;
pub const SIDEBAR_W: f32 = 240.0;
pub const TILE_GAP: f32 = 2.0;
const TRAFFIC_LIGHTS_PAD: f32 = 80.0;
const OUTER_PAD_X: f32 = 16.0;
const BLOCK_PAD_Y: f32 = 6.0;
const TABLE_ROW_PAD_Y: f32 = 2.5;
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
    /// True when an inline running PTY block in this pane currently holds keyboard focus.
    pub pty_focused: bool,
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
    /// Currently open hover/click popover anchored to a prompt segment or icon button.
    pub active_popover: Option<&'a ActivePopover>,
    /// Current logical pointer coordinates (for crisp row hover highlights inside popovers).
    pub mouse_pos: Option<[f32; 2]>,
    /// Active Vim Normal/Visual scrollback output cursor (`h`/`j`/`k`/`l`, `v`/`V`, `y`).
    pub output_nav: Option<OutputNav>,
}

/// Convert `[r, g, b, a]` bytes from `TermServer` entity metadata into normalized RGBA floats.
#[inline]
fn rgba_u8_to_f32(c: [u8; 4]) -> [f32; 4] {
    [
        f32::from(c[0]) / 255.0,
        f32::from(c[1]) / 255.0,
        f32::from(c[2]) / 255.0,
        f32::from(c[3]) / 255.0,
    ]
}

/// Logical display width of a symbol spec: checks the live `TermServer` custom symbol registry
/// first (stripping optional `"sym:"` prefix), otherwise falls back to emoji cluster width.
fn symbol_display_width(sym: &str, h: f32) -> f32 {
    let key = sym.strip_prefix("sym:").unwrap_or(sym);
    if let Some(reg) = crate::term_server::lookup_global_symbol(key) {
        return (reg.width_cells.max(1) as f32 * h).max(h);
    }
    if sym.chars().count() == 1
        && let Some(ch) = sym.chars().next()
        && let Some(reg) = crate::term_server::lookup_global_codepoint(ch as u32)
    {
        return (reg.width_cells.max(1) as f32 * h).max(h);
    }
    crate::view::symbol_badge_width(sym, h)
}

/// Draw either a registered Blaze Terminal API custom symbol (SVG, in-memory font glyph, PNG, or
/// composite text) OR a Unicode/emoji badge, returning its logical horizontal width.
#[allow(clippy::too_many_arguments)]
fn draw_symbol_or_emoji(
    scene: &mut Scene,
    x: f32,
    y: f32,
    h: f32,
    sym: &str,
    tint: Option<[f32; 4]>,
    alpha: f32,
    clip_id: u32,
) -> f32 {
    let key = sym.strip_prefix("sym:").unwrap_or(sym);
    if let Some(reg) = crate::term_server::lookup_global_symbol(key) {
        let w = (reg.width_cells.max(1) as f32 * h).max(h);
        let eff_tint = if reg.monochrome { tint } else { None };
        scene.custom_symbol(x, y, w, h, reg.name, eff_tint, alpha, clip_id);
        return w;
    }
    if sym.chars().count() == 1
        && let Some(ch) = sym.chars().next()
        && let Some(reg) = crate::term_server::lookup_global_codepoint(ch as u32)
    {
        let w = (reg.width_cells.max(1) as f32 * h).max(h);
        let eff_tint = if reg.monochrome { tint } else { None };
        scene.custom_symbol(x, y, w, h, reg.name, eff_tint, alpha, clip_id);
        return w;
    }
    scene.emoji_symbol(x, y, h, sym, alpha, clip_id)
}

/// Lay out the full `ix.dev/blaze` + Starship inline-stream shell interface.
///
/// The live prompt flows directly underneath the last command output (like a native
/// terminal / TUI), with zero separate docked bottom pane.
pub fn build_scene(input: &LayoutInput<'_>) -> Scene {
    build_scene_inner(input, true)
}

/// Lay out the shell stream inside a tiled pane (omits the global window titlebar).
pub fn build_pane_scene(input: &LayoutInput<'_>) -> Scene {
    build_scene_inner(input, false)
}

fn build_scene_inner(input: &LayoutInput<'_>, draw_top_bar: bool) -> Scene {
    let mut scene = Scene::new();
    let vw = input.viewport_w.max(220.0);
    let vh = input.viewport_h.max(160.0);
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
    let mut prev_cwd: Option<&str> = None;
    let mut prev_branch: Option<Option<&str>> = None;
    for (idx, blk) in blocks.iter().enumerate() {
        if idx > 0 {
            let sep_y = feed_top + content_y - input.scroll_y;
            scene.line(col_x0, sep_y, col_x1, sep_y, 1.0, theme::LINE_FAINT, feed_clip);
            content_y += BLOCK_PAD_Y;
        }
        // Delta prompt capsules: show full context capsule on block 0 or whenever cwd/git branch
        // changed; consecutive commands in the same context stay calm & scannable!
        let cur_cwd = blk.prompt_snapshot.cwd.as_str();
        let cur_branch = blk.prompt_snapshot.git_branch.as_deref();
        let show_capsule = prev_cwd != Some(cur_cwd) || prev_branch != Some(cur_branch);
        prev_cwd = Some(cur_cwd);
        prev_branch = Some(cur_branch);
        let draw_y = feed_top + content_y - input.scroll_y;
        scene.block_tops.push((blk.id, content_y));
        let h = layout_block(
            &mut scene,
            blk,
            show_capsule,
            col_x0,
            draw_y,
            col_x1,
            m,
            feed_clip,
            input.cursor_visible,
            input.pty_focused,
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
        input.cursor_visible && !input.pty_focused,
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

    // 3c. Vim scrollback/output browsing (`h`/`j`/`k`/`l`, `v`/`V`, `y` over previous outputs).
    // Even in Normal mode, prefer a clean item/word selection highlight over a harsh inverted
    // single-character block cursor so structured outputs (`ls`, tables) stay crisp & readable!
    if let Some(nav) = input.output_nav {
        let vlines = extract_visual_lines(&scene.text_runs, m, feed_top, feed_bottom);
        let (cur_rect, vis_rects, _) = output_nav_geometry(&vlines, m, nav);
        for [rx0, ry0, rx1, ry1] in vis_rects {
            scene.over_round_rect(rx0, ry0, rx1, ry1, 2.5, [0.35, 0.56, 0.88, 0.30], feed_clip);
        }
        if let Some([cx0, cy0, cx1, cy1]) = cur_rect {
            if nav.visual.is_none() {
                let mid_x = (cx0 + cx1) * 0.5;
                let mid_y = (cy0 + cy1) * 0.5;
                // Prefer highlighting the entire structured item (e.g. `blaze.lock` in `ls` grid
                // or a table cell) as a sleek selection pill rather than a 1-char block cursor!
                let structured_rect = scene
                    .hover_regions
                    .iter()
                    .rev()
                    .find(|reg| {
                        reg.rect.contains(mid_x, mid_y)
                            && matches!(
                                reg.kind,
                                HoverKind::FileEntry { .. }
                                    | HoverKind::FilePreview { .. }
                                    | HoverKind::DirPreview { .. }
                                    | HoverKind::Tooltip { .. }
                            )
                    })
                    .map(|reg| reg.rect);

                if let Some(sr) = structured_rect {
                    scene.overlay_round_rect(
                        sr.x0 - 2.0,
                        sr.y0 - 1.0,
                        sr.x1 + 2.0,
                        sr.y1 + 1.0,
                        5.0,
                        [0.34, 0.56, 0.90, 0.20],
                        1.0,
                        [0.48, 0.72, 1.0, 0.62],
                        feed_clip,
                    );
                } else {
                    // On plain text lines, highlight the active word/token as a calm selection
                    // with a subtle bottom accent under the exact character position.
                    let li = nav.line.min(vlines.len().saturating_sub(1));
                    let mut sel_x0 = cx0;
                    let mut sel_x1 = cx1;
                    if let Some(row) = vlines.get(li)
                        && !row.chars.is_empty()
                    {
                        let ci = nav.col.min(row.chars.len() - 1);
                        let is_word = |ch: char| !ch.is_whitespace();
                        if is_word(row.chars[ci].1) {
                            let mut s = ci;
                            while s > 0
                                && is_word(row.chars[s - 1].1)
                                && (row.chars[s].0 - row.chars[s - 1].0) <= m.cw * 1.6
                            {
                                s -= 1;
                            }
                            let mut e = ci;
                            while e + 1 < row.chars.len()
                                && is_word(row.chars[e + 1].1)
                                && (row.chars[e + 1].0 - row.chars[e].0) <= m.cw * 1.6
                            {
                                e += 1;
                            }
                            sel_x0 = row.chars[s].0;
                            sel_x1 = row.chars[e].0 + m.cw;
                        }
                    }
                    scene.overlay_round_rect(
                        sel_x0 - 2.0,
                        cy0 - 1.0,
                        sel_x1 + 2.0,
                        cy1 + 1.0,
                        3.5,
                        [0.34, 0.56, 0.90, 0.22],
                        1.0,
                        [0.48, 0.72, 1.0, 0.45],
                        feed_clip,
                    );
                    scene.over_round_rect(
                        cx0,
                        cy1 - 1.0,
                        cx1,
                        cy1 + 0.75,
                        0.75,
                        [0.64, 0.82, 1.0, 0.85],
                        feed_clip,
                    );
                }
            } else {
                // Visual selection active: subtle outline on the head character without stark inversion.
                scene.over_round_rect(
                    cx0,
                    cy1 - 1.0,
                    cx1,
                    cy1 + 0.75,
                    0.75,
                    [0.72, 0.86, 1.0, 0.90],
                    feed_clip,
                );
            }
        }
        let mode_lbl = match nav.visual {
            Some((_, _, OutputVisualMode::Line)) => "OUTPUT V-LINE  ·  hjkl move  y yank  esc exit",
            Some((_, _, OutputVisualMode::Char)) => "OUTPUT VISUAL  ·  hjkl move  y yank  esc exit",
            None => "OUTPUT NORMAL  ·  hjkl move  v/V select  y yank  i/esc prompt",
        };
        let pill_w = mode_lbl.chars().count() as f32 * m.cw + 20.0;
        let pill_h = m.ch + 8.0;
        let px1 = (vw - 14.0).max(pill_w + 14.0);
        let px0 = px1 - pill_w;
        let py1 = vh - 12.0;
        let py0 = py1 - pill_h;
        scene.overlay_round_rect(
            px0,
            py0,
            px1,
            py1,
            6.0,
            theme::rgb(0x16, 0x16, 0x19),
            1.0,
            theme::rgb(0x2c, 0x2c, 0x32),
            0,
        );
        scene.overlay_text_run(px0 + 10.0, py0 + 4.0, mode_lbl, theme::INK_2, false, false, 0);
    }

    // 4. Unified inline macOS titlebar (branch tabs, cwd/git pill, zoom, icon fork button).
    if draw_top_bar {
        layout_top_bar(
            &mut scene,
            input.store,
            input.prompt,
            vw,
            m,
            input.zoom_pct,
            input.inline_titlebar,
        );
    }

    // 5. Floating overlays: Atuin-class Ctrl+R history palette, Tab completion popup, hover popover, drag chip.
    if let Some(rs) = &input.editor.reverse_search {
        layout_reverse_search_overlay(&mut scene, rs, vw, vh, m, input.now_ns);
    } else if !input.editor.completions.is_empty() {
        layout_completion_popup(
            &mut scene,
            input.editor,
            col_x0,
            prompt_y0,
            prompt_y1,
            vh,
            col_x1,
            m,
            input.now_ns,
        );
    } else if let Some(pop) = input.active_popover {
        layout_active_popover(&mut scene, pop, vw, vh, m, input.mouse_pos, input.now_ns);
    } else if !input.pty_focused
        && let Some(hint) = input.editor.live_inline_hint()
    {
        layout_live_hint_strip(
            &mut scene,
            &hint,
            input.editor,
            col_x0,
            prompt_y0,
            prompt_y1,
            vh,
            col_x1,
            m,
            input.mouse_pos,
        );
    }
    if let Some(drag) = input.drag {
        layout_drag_overlay(&mut scene, drag, m);
    }

    scene
}

pub(crate) fn layout_top_bar(
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
    let fork_rect = HitRect::new(fork_x0, btn_y0, rx, btn_y1);
    scene.round_rect(fork_x0, btn_y0, rx, btn_y1, 6.0, theme::BG_CHIP, 1.0, theme::LINE, 0);
    draw_icon(scene, Icon::Branch, fork_x0 + (fork_w - icon_s) * 0.5, icon_y, icon_s, theme::INK_2, 0);
    scene.push_hit(fork_rect, HitAction::ForkActiveBranch);
    scene.push_hover_clipped(
        fork_rect,
        0,
        HoverKind::Tooltip {
            title: "Fork session branch".to_owned(),
            subtitle: Some("Cmd+B · O(1) CoW snapshot".to_owned()),
        },
    );
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

    // Sleek monochrome-first session tabs matching the prompt capsule (zero arbitrary orange).
    let active_id = store.active_branch_id();
    for branch in store.branches() {
        let is_active = branch.id == active_id;
        let name = branch.name.as_str();
        let count_str = format!(" {}", branch.blocks.len());
        let name_w = name.chars().count() as f32 * m.cw;
        let count_w = count_str.chars().count() as f32 * m.cw;
        let pad_x = 9.0;
        let tab_w = pad_x * 2.0 + icon_s + 6.0 + name_w + count_w;
        if x + tab_w > tabs_limit {
            break;
        }
        let icon_col = if is_active { theme::INK_2 } else { theme::INK_4 };
        let name_col = if is_active { theme::INK_1 } else { theme::INK_3 };
        let count_col = if is_active { theme::INK_3 } else { theme::INK_4 };
        if is_active {
            scene.round_rect(
                x,
                btn_y0,
                x + tab_w,
                btn_y1,
                6.0,
                theme::rgb(0x1a, 0x1a, 0x1d),
                1.0,
                theme::rgb(0x2a, 0x2a, 0x2e),
                0,
            );
        }
        draw_icon(scene, Icon::Branch, x + pad_x, icon_y, icon_s, icon_col, 0);
        scene.text(x + pad_x + icon_s + 6.0, mid_y, name, name_col, is_active, false, 0);
        scene.text(x + pad_x + icon_s + 6.0 + name_w, mid_y, count_str, count_col, false, false, 0);
        scene.push_hit(
            HitRect::new(x, btn_y0, x + tab_w, btn_y1),
            HitAction::SwitchBranch(branch.id),
        );
        x += tab_w + 8.0;
    }
}

/// Summary descriptor for one workspace tab pill in the unified window titlebar.
/// One node in a workspace's CoW session fork tree (`BlockStore::branches()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarBranchNode {
    pub id: BranchId,
    pub name: String,
    /// Tree nesting depth (`0` = root `main`, `1` = forked from `main`, `2` = forked from fork, ...).
    pub depth: usize,
    /// True when this node is the last child of its parent branch.
    pub is_last: bool,
    /// Bit `d` is set when the ancestor at depth `d` has further siblings below this node.
    pub ancestor_cont_mask: u32,
    pub active: bool,
    pub block_count: usize,
    /// Short preview of the command at `parent_block` where this fork originated.
    pub fork_cmd: Option<String>,
}

/// Build an ordered DFS hierarchy tree of a pane's CoW session branches (`parent_branch` links).
pub fn collect_branch_tree(store: &BlockStore) -> Vec<SidebarBranchNode> {
    let branches = store.branches();
    if branches.is_empty() {
        return Vec::new();
    }
    let active_id = store.active_branch_id();
    let mut visited = vec![false; branches.len()];
    let mut out = Vec::with_capacity(branches.len());

    #[allow(clippy::too_many_arguments)]
    fn dfs(
        store: &BlockStore,
        branches: &[crate::blocks::Branch],
        idx: usize,
        depth: usize,
        is_last: bool,
        ancestor_cont_mask: u32,
        active_id: BranchId,
        visited: &mut [bool],
        out: &mut Vec<SidebarBranchNode>,
    ) {
        if visited[idx] || depth > 16 {
            return;
        }
        visited[idx] = true;
        let br = &branches[idx];
        let fork_cmd = br
            .parent_block
            .and_then(|bid| store.block(bid))
            .map(|blk| blk.source.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        out.push(SidebarBranchNode {
            id: br.id,
            name: br.name.clone(),
            depth,
            is_last,
            ancestor_cont_mask,
            active: br.id == active_id,
            block_count: br.blocks.len(),
            fork_cmd,
        });
        let child_indices: Vec<usize> = branches
            .iter()
            .enumerate()
            .filter(|&(ci, cand)| !visited[ci] && cand.parent_branch == Some(br.id))
            .map(|(ci, _)| ci)
            .collect();
        let next_mask = if depth > 0 && !is_last && depth < 31 {
            ancestor_cont_mask | (1u32 << (depth - 1))
        } else {
            ancestor_cont_mask
        };
        let n_children = child_indices.len();
        for (pos, ci) in child_indices.into_iter().enumerate() {
            dfs(
                store,
                branches,
                ci,
                depth + 1,
                pos + 1 == n_children,
                next_mask,
                active_id,
                visited,
                out,
            );
        }
    }

    // Visit root branches first (`parent_branch` absent or pointing to a missing branch).
    let roots: Vec<usize> = branches
        .iter()
        .enumerate()
        .filter(|(_, br)| {
            br.parent_branch
                .is_none_or(|pid| !branches.iter().any(|b| b.id == pid))
        })
        .map(|(i, _)| i)
        .collect();
    let n_roots = roots.len();
    for (pos, ri) in roots.into_iter().enumerate() {
        dfs(
            store,
            branches,
            ri,
            0,
            pos + 1 == n_roots,
            0,
            active_id,
            &mut visited,
            &mut out,
        );
    }
    // Safety net: append any remaining unvisited cycle nodes at root depth.
    for i in 0..branches.len() {
        if !visited[i] {
            dfs(store, branches, i, 0, true, 0, active_id, &mut visited, &mut out);
        }
    }
    out
}

pub struct TabBarItem<'a> {
    pub id: TabId,
    /// 1-based workspace number (`Cmd+1..9`).
    pub slot: usize,
    pub symbol: Option<&'a str>,
    pub title: &'a str,
    pub active: bool,
    pub pane_count: usize,
    pub zoomed: bool,
    pub process_tag: Option<&'a str>,
    /// CoW session fork hierarchy for this workspace's focused pane.
    pub branches: Vec<SidebarBranchNode>,
}

/// Render the streamlined window-level titlebar: sidebar toggle (`Cmd+S`), compact active
/// workspace indicator (tabs live in the collapsible sidebar), `+` (`Cmd+T`), zoomed badge,
/// and split/window/fork controls (`Cmd+D`, `Cmd+Shift+D`, `Cmd+Shift+N`, `Cmd+B`).
#[allow(clippy::too_many_arguments)]
pub fn layout_window_top_bar(
    scene: &mut Scene,
    tabs: &[TabBarItem<'_>],
    focused_pane: PaneId,
    is_zoomed: bool,
    pane_count_in_tab: usize,
    store: &BlockStore,
    prompt: &PromptInfo,
    vw: f32,
    m: CellMetrics,
    zoom_pct: u16,
    inline_titlebar: bool,
    sidebar_open: bool,
) {
    scene.round_rect(0.0, 0.0, vw, TOP_BAR_H, 0.0, theme::BG_TOPBAR, 0.0, [0.0; 4], 0);
    scene.line(0.0, TOP_BAR_H - 0.5, vw, TOP_BAR_H - 0.5, 1.0, theme::LINE, 0);

    // Fixed native macOS 12pt SF Pro chrome metrics (immune to `Cmd+`/`Cmd-` zoom so titlebar never clips!).
    let chrome_ch = m.line_h(TextFace::Chrome);
    let mid_y = (TOP_BAR_H - chrome_ch) * 0.5;
    let icon_s = 11.5;
    let icon_y = (TOP_BAR_H - icon_s) * 0.5;
    let btn_y0 = 6.0;
    let btn_y1 = TOP_BAR_H - 6.0;
    let mut x = if inline_titlebar { TRAFFIC_LIGHTS_PAD } else { OUTER_PAD_X };

    // Right-aligned tiling & window controls: Fork (Cmd+B), New Window (Cmd+Shift+N), Split H, Split V.
    let mut rx = vw - 12.0;
    let btn_w = 24.0;
    let btn_gap = 5.0;
    let push_icon_btn = |scene: &mut Scene,
                             rx: &mut f32,
                             icon: Icon,
                             action: HitAction,
                             title: &str,
                             subtitle: &str| {
        let bx0 = (*rx - btn_w).max(x + 20.0);
        if bx0 <= x + 40.0 {
            return;
        }
        let rect = HitRect::new(bx0, btn_y0, *rx, btn_y1);
        scene.round_rect(bx0, btn_y0, *rx, btn_y1, 5.5, theme::BG_CHIP, 1.0, theme::LINE, 0);
        draw_icon(scene, icon, bx0 + (btn_w - icon_s) * 0.5, icon_y, icon_s, theme::INK_2, 0);
        scene.push_hit(rect, action);
        scene.push_hover_clipped(
            rect,
            0,
            HoverKind::Tooltip {
                title: title.to_owned(),
                subtitle: Some(subtitle.to_owned()),
            },
        );
        *rx = bx0 - btn_gap;
    };

    push_icon_btn(
        scene,
        &mut rx,
        Icon::Branch,
        HitAction::ForkActiveBranch,
        "Fork session branch",
        "Cmd+B · O(1) CoW snapshot",
    );
    push_icon_btn(
        scene,
        &mut rx,
        Icon::NewWindow,
        HitAction::NewWindow,
        "New OS window",
        "Cmd+Shift+N · separate window",
    );
    push_icon_btn(
        scene,
        &mut rx,
        Icon::SplitH,
        HitAction::SplitPane(SplitAxis::Horizontal),
        "Split horizontally",
        "Cmd+Shift+D · top / bottom",
    );
    push_icon_btn(
        scene,
        &mut rx,
        Icon::SplitV,
        HitAction::SplitPane(SplitAxis::Vertical),
        "Split vertically",
        "Cmd+D · left | right",
    );
    rx -= 5.0;

    // Zoomed-pane badge (`Cmd+Shift+Enter` to unzoom back to full tiled layout).
    if is_zoomed && pane_count_in_tab > 1 {
        let z_lbl = format!("zoom 1/{pane_count_in_tab}");
        let z_w = m.measure(&z_lbl, TextFace::Chrome) + 14.0;
        let zx0 = (rx - z_w).max(x + 20.0);
        if zx0 > x + 60.0 {
            let rect = HitRect::new(zx0, btn_y0, rx, btn_y1);
            scene.round_rect(
                zx0,
                btn_y0,
                rx,
                btn_y1,
                5.5,
                theme::rgb(0x23, 0x23, 0x28),
                1.0,
                theme::LINE_STRONG,
                0,
            );
            scene.chrome_text(zx0 + 7.0, mid_y, z_lbl, theme::INK_1, true, 0);
            scene.push_hit(rect, HitAction::ToggleZoomPane(focused_pane));
            scene.push_hover_clipped(
                rect,
                0,
                HoverKind::Tooltip {
                    title: "Unzoom pane".to_owned(),
                    subtitle: Some("Cmd+Shift+Enter".to_owned()),
                },
            );
            rx = zx0 - 8.0;
        }
    }

    // Quiet clickable UI zoom % pill when zoomed in/out (`Cmd+`/`Cmd-`).
    if zoom_pct != 100 {
        let z_lbl = format!("{zoom_pct}%");
        let z_w = m.measure(&z_lbl, TextFace::Chrome) + 14.0;
        let zx0 = (rx - z_w).max(x + 20.0);
        if zx0 > x + 60.0 {
            scene.round_rect(zx0, btn_y0, rx, btn_y1, 5.5, theme::BG_CHIP, 1.0, theme::LINE, 0);
            scene.chrome_text(zx0 + 7.0, mid_y, z_lbl, theme::INK_3, false, 0);
            scene.push_hit(HitRect::new(zx0, btn_y0, rx, btn_y1), HitAction::ResetZoom);
            rx = zx0 - 8.0;
        }
    }

    let left_limit = rx - 10.0;
    let _ = prompt;

    // 1. Sidebar toggle button (`Cmd+S`): opens/hides the workspace & fork hierarchy tree.
    let sb_w = 26.0;
    if x + sb_w <= left_limit {
        let rect = HitRect::new(x, btn_y0, x + sb_w, btn_y1);
        let (bg, border, ic_col) = if sidebar_open {
            (theme::rgb(0x21, 0x21, 0x27), theme::LINE_STRONG, theme::INK_1)
        } else {
            (theme::BG_CHIP, theme::LINE, theme::INK_3)
        };
        scene.round_rect(x, btn_y0, x + sb_w, btn_y1, 5.5, bg, 1.0, border, 0);
        draw_icon(scene, Icon::Sidebar, x + (sb_w - 13.0) * 0.5, (TOP_BAR_H - 13.0) * 0.5, 13.0, ic_col, 0);
        scene.push_hit(rect, HitAction::ToggleSidebar);
        scene.push_hover_clipped(
            rect,
            0,
            HoverKind::Tooltip {
                title: if sidebar_open {
                    "Hide workspace sidebar".to_owned()
                } else {
                    "Show workspace & fork sidebar".to_owned()
                },
                subtitle: Some(format!(
                    "Cmd+S · {} workspace{}",
                    tabs.len(),
                    if tabs.len() == 1 { "" } else { "s" }
                )),
            },
        );
        x += sb_w + 6.0;
    }

    // 2. Compact active workspace summary pill (clicking toggles the sidebar so tabs stay clean!).
    if let Some(active_tab) = tabs.iter().find(|t| t.active).or_else(|| tabs.first()) {
        let title_str = truncate_chars(active_tab.title, 26);
        let fork_count = store.branches().len();
        let active_branch_name = store
            .branches()
            .iter()
            .find(|b| b.id == store.active_branch_id())
            .map(|b| b.name.as_str());
        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(bn) = active_branch_name
            && fork_count > 1
        {
            meta_parts.push(bn.to_owned());
        }
        if tabs.len() > 1 {
            meta_parts.push(format!("{}/{}", active_tab.slot, tabs.len()));
        } else if active_tab.pane_count > 1 {
            meta_parts.push(format!("{} panes", active_tab.pane_count));
        }
        let meta_str = if meta_parts.is_empty() {
            String::new()
        } else {
            format!("  ·  {}", meta_parts.join(" · "))
        };
        let pad_x = 9.0;
        let sym_h = 13.0f32;
        let sym_w = active_tab
            .symbol
            .map(|s| symbol_display_width(s, sym_h) + if title_str.is_empty() { 0.0 } else { 5.0 })
            .unwrap_or(0.0);
        let title_w = if title_str.is_empty() {
            0.0
        } else {
            m.measure(&title_str, TextFace::Chrome) + 2.0
        };
        let meta_w = if meta_str.is_empty() {
            0.0
        } else {
            m.measure(&meta_str, TextFace::Chrome) + 2.0
        };
        let pill_w = (pad_x * 2.0 + sym_w + title_w + meta_w).min((left_limit - x - 32.0).max(0.0));
        if pill_w >= 44.0 {
            let rect = HitRect::new(x, btn_y0, x + pill_w, btn_y1);
            scene.round_rect(
                x,
                btn_y0,
                x + pill_w,
                btn_y1,
                6.0,
                theme::rgba(0x18, 0x18, 0x1c, 0.85),
                1.0,
                theme::rgba(0x28, 0x28, 0x2e, 0.85),
                0,
            );
            let mut tx = x + pad_x;
            if let Some(sym) = active_tab.symbol {
                let sw = draw_symbol_or_emoji(
                    scene,
                    tx,
                    btn_y0 + ((btn_y1 - btn_y0) - sym_h) * 0.5,
                    sym_h,
                    sym,
                    Some(theme::INK_1),
                    1.0,
                    0,
                );
                tx += sw + if title_str.is_empty() { 0.0 } else { 5.0 };
            }
            if !title_str.is_empty() {
                scene.chrome_text(tx, mid_y, &title_str, theme::INK_1, true, 0);
                tx += title_w;
            }
            if !meta_str.is_empty() && tx + meta_w <= x + pill_w - 4.0 {
                scene.chrome_text(tx, mid_y, &meta_str, theme::INK_3, false, 0);
            }
            scene.push_hit(rect, HitAction::ToggleSidebar);
            scene.push_hover_clipped(
                rect,
                0,
                HoverKind::Tooltip {
                    title: format!("Workspace {} · {}", active_tab.slot, active_tab.title),
                    subtitle: Some("Click or Cmd+S to view tabs & fork hierarchy".to_owned()),
                },
            );
            x += pill_w + 6.0;
        }
    }

    // 3. Crisp `+` new workspace tab button (`Cmd+T`).
    let plus_w = 24.0;
    if x + plus_w <= left_limit {
        let rect = HitRect::new(x, btn_y0, x + plus_w, btn_y1);
        scene.round_rect(x, btn_y0, x + plus_w, btn_y1, 5.5, theme::BG_CHIP, 1.0, theme::LINE, 0);
        draw_icon(scene, Icon::Plus, x + (plus_w - 10.5) * 0.5, (TOP_BAR_H - 10.5) * 0.5, 10.5, theme::INK_3, 0);
        scene.push_hit(rect, HitAction::NewTab);
        scene.push_hover_clipped(
            rect,
            0,
            HoverKind::Tooltip {
                title: "New workspace tab".to_owned(),
                subtitle: Some("Cmd+T".to_owned()),
            },
        );
    }
}

/// Render the collapsible left Workspace & Fork Hierarchy sidebar (`Cmd+S`), showing every
/// workspace tab and its nested CoW session fork tree (`main` → `fork-1` → `fork-2`) with
/// GPU vector tree rails, fork-origin commands, live process badges, and one-click switching.
#[allow(clippy::too_many_arguments)]
pub fn layout_workspace_sidebar(
    scene: &mut Scene,
    tabs: &[TabBarItem<'_>],
    sidebar_w: f32,
    vh: f32,
    m: CellMetrics,
    mouse_pos: Option<[f32; 2]>,
) {
    let y0 = TOP_BAR_H;
    let y1 = vh.max(y0 + 80.0);
    let clip_id = scene.push_clip(0.0, y0, sidebar_w, y1, 0.0);

    // Subtle recessed sidebar surface with crisp 1px right divider.
    scene.round_rect(0.0, y0, sidebar_w, y1, 0.0, theme::rgb(0x12, 0x12, 0x15), 0.0, [0.0; 4], clip_id);
    scene.line(sidebar_w - 0.5, y0, sidebar_w - 0.5, y1, 1.0, theme::LINE, 0);

    let is_hovered = |r: HitRect| -> bool {
        mouse_pos.is_some_and(|[mx, my]| r.contains(mx, my))
    };

    let pad_x = 10.0;
    let chrome_ch = m.line_h(TextFace::Chrome);
    let small_ch = m.line_h(TextFace::UiSmall);

    // Header row: `WORKSPACES` + `+` button (`Cmd+T`).
    let hdr_h = 30.0;
    let hdr_ty = y0 + (hdr_h - small_ch) * 0.5;
    scene.ui_small(pad_x, hdr_ty, "WORKSPACES", theme::INK_4, true, false, clip_id);

    let plus_s = 20.0;
    let plus_x0 = sidebar_w - pad_x - plus_s;
    let plus_y0 = y0 + (hdr_h - plus_s) * 0.5;
    let plus_rect = HitRect::new(plus_x0, plus_y0, plus_x0 + plus_s, plus_y0 + plus_s);
    scene.round_rect(
        plus_rect.x0,
        plus_rect.y0,
        plus_rect.x1,
        plus_rect.y1,
        5.0,
        if is_hovered(plus_rect) { theme::rgb(0x22, 0x22, 0x28) } else { theme::BG_CHIP },
        1.0,
        theme::LINE,
        clip_id,
    );
    draw_icon(
        scene,
        Icon::Plus,
        plus_x0 + (plus_s - 9.5) * 0.5,
        plus_y0 + (plus_s - 9.5) * 0.5,
        9.5,
        theme::INK_2,
        clip_id,
    );
    scene.push_hit_clipped(plus_rect, clip_id, HitAction::NewTab);
    scene.push_hover_clipped(
        plus_rect,
        clip_id,
        HoverKind::Tooltip {
            title: "New workspace".to_owned(),
            subtitle: Some("Cmd+T".to_owned()),
        },
    );

    let footer_h = 26.0;
    let list_bottom = (y1 - footer_h - 4.0).max(y0 + hdr_h + 24.0);
    let mut cur_y = y0 + hdr_h + 2.0;

    let tab_row_h = 28.0;
    let branch_row_h = 23.0;
    let can_close = tabs.len() > 1;

    for tab in tabs {
        if cur_y + tab_row_h > list_bottom {
            break;
        }
        let row_rect = HitRect::new(6.0, cur_y, sidebar_w - 6.0, cur_y + tab_row_h);
        let hovered = is_hovered(row_rect);
        if tab.active {
            scene.round_rect(
                row_rect.x0,
                row_rect.y0,
                row_rect.x1,
                row_rect.y1,
                6.0,
                theme::rgb(0x1b, 0x1b, 0x21),
                1.0,
                theme::rgb(0x2c, 0x2c, 0x34),
                clip_id,
            );
            // Subtle emerald active indicator bar on the left edge of the workspace pill.
            scene.round_rect(
                row_rect.x0 + 1.5,
                row_rect.y0 + 5.0,
                row_rect.x0 + 4.0,
                row_rect.y1 - 5.0,
                1.25,
                theme::OK,
                0.0,
                [0.0; 4],
                clip_id,
            );
        } else if hovered {
            scene.round_rect(
                row_rect.x0,
                row_rect.y0,
                row_rect.x1,
                row_rect.y1,
                6.0,
                theme::rgba(0x1a, 0x1a, 0x20, 0.8),
                0.0,
                [0.0; 4],
                clip_id,
            );
        }

        let ty = cur_y + (tab_row_h - chrome_ch) * 0.5;
        let mut rx = row_rect.x1 - 7.0;

        // Hover close button `×` when >1 workspace tab exists.
        let mut close_hit: Option<HitRect> = None;
        if can_close && hovered {
            let cs = 16.0;
            let cx0 = rx - cs;
            let cy0 = cur_y + (tab_row_h - cs) * 0.5;
            let cr = HitRect::new(cx0, cy0, cx0 + cs, cy0 + cs);
            if is_hovered(cr) {
                scene.round_rect(cr.x0, cr.y0, cr.x1, cr.y1, 4.0, theme::rgba(0xff, 0x71, 0x68, 0.20), 0.0, [0.0; 4], clip_id);
            }
            draw_icon(
                scene,
                Icon::Close,
                cx0 + (cs - 8.5) * 0.5,
                cy0 + (cs - 8.5) * 0.5,
                8.5,
                if is_hovered(cr) { theme::DANGER } else { theme::INK_3 },
                clip_id,
            );
            close_hit = Some(cr);
            rx = cx0 - 4.0;
        } else if let Some(proc_tag) = tab.process_tag {
            let tag = truncate_chars(proc_tag, 10);
            let tw = m.measure(&tag, TextFace::UiSmall);
            let dot_w = 8.0;
            rx -= tw;
            scene.ui_small(rx, cur_y + (tab_row_h - small_ch) * 0.5, &tag, theme::rgb(0x7c, 0xd4, 0xa8), false, false, clip_id);
            scene.over_circle(rx - 4.5, cur_y + tab_row_h * 0.5, 2.3, theme::OK, clip_id);
            rx -= dot_w + 4.0;
        } else if tab.pane_count > 1 {
            let tag = format!("{}p", tab.pane_count);
            let tw = m.measure(&tag, TextFace::UiSmall);
            rx -= tw;
            scene.ui_small(rx, cur_y + (tab_row_h - small_ch) * 0.5, &tag, theme::INK_4, false, false, clip_id);
            rx -= 5.0;
        }

        let mut tx = row_rect.x0 + 9.0;
        let slot_str = format!("{}", tab.slot);
        let slot_w = m.measure(&slot_str, TextFace::UiSmall);
        scene.ui_small(
            tx,
            cur_y + (tab_row_h - small_ch) * 0.5,
            &slot_str,
            if tab.active { theme::INK_3 } else { theme::INK_4 },
            false,
            false,
            clip_id,
        );
        tx += slot_w.max(9.0) + 6.0;

        let sym_h = 13.0f32;
        if let Some(sym) = tab.symbol {
            let sw = draw_symbol_or_emoji(
                scene,
                tx,
                cur_y + (tab_row_h - sym_h) * 0.5,
                sym_h,
                sym,
                Some(if tab.active { theme::INK_1 } else { theme::INK_2 }),
                if tab.active { 1.0 } else { 0.78 },
                clip_id,
            );
            tx += sw + 5.0;
        } else {
            scene.svg_icon_alpha(
                tx,
                cur_y + (tab_row_h - sym_h) * 0.5,
                sym_h,
                "folder_macos",
                if tab.active { 1.0 } else { 0.72 },
                clip_id,
            );
            tx += sym_h + 5.0;
        }

        let avail_w = (rx - tx - 2.0).max(20.0);
        let max_ch = (avail_w / m.measure("n", TextFace::Chrome).max(5.5)).floor().max(4.0) as usize;
        let shown = truncate_chars(tab.title, max_ch);
        scene.chrome_text(
            tx,
            ty,
            &shown,
            if tab.active { theme::INK_1 } else { theme::INK_2 },
            tab.active,
            clip_id,
        );

        // Push tab hit first, then close hit on top (hit_test resolves last-pushed first!).
        scene.push_hit_clipped(row_rect, clip_id, HitAction::SwitchTab(tab.id));
        if let Some(cr) = close_hit {
            scene.push_hit_clipped(cr, clip_id, HitAction::CloseTab(tab.id));
        }
        cur_y += tab_row_h + 2.0;

        // Render the workspace's CoW fork hierarchy tree when it has forks (or for the active tab
        // so the fork tree & quick-fork affordance are immediately discoverable!).
        let show_tree = tab.branches.len() > 1 || (tab.active && !tab.branches.is_empty());
        if show_tree {
            let indent_step = 13.0f32;
            let tree_base_x = 18.0f32;
            for node in &tab.branches {
                if cur_y + branch_row_h > list_bottom {
                    break;
                }
                let b_rect = HitRect::new(10.0, cur_y, sidebar_w - 6.0, cur_y + branch_row_h);
                let b_hov = is_hovered(b_rect);
                let is_active_node = tab.active && node.active;
                if is_active_node {
                    scene.round_rect(
                        b_rect.x0,
                        b_rect.y0,
                        b_rect.x1,
                        b_rect.y1,
                        5.0,
                        theme::rgba(0x4c, 0xc2, 0x7a, 0.09),
                        0.0,
                        [0.0; 4],
                        clip_id,
                    );
                } else if b_hov {
                    scene.round_rect(
                        b_rect.x0,
                        b_rect.y0,
                        b_rect.x1,
                        b_rect.y1,
                        5.0,
                        theme::rgba(0x20, 0x20, 0x26, 0.85),
                        0.0,
                        [0.0; 4],
                        clip_id,
                    );
                }

                let cy = cur_y + branch_row_h * 0.5;
                let b_ty = cur_y + (branch_row_h - small_ch) * 0.5;
                let rail_col = theme::LINE_STRONG;

                // Continuous vertical ancestor continuation rails.
                for anc_d in 0..node.depth.saturating_sub(1).min(8) {
                    if (node.ancestor_cont_mask & (1u32 << anc_d)) != 0 {
                        let ax = tree_base_x + anc_d as f32 * indent_step + 4.0;
                        scene.line(ax, cur_y, ax, cur_y + branch_row_h, 1.15, rail_col, clip_id);
                    }
                }
                // L / T vector connector from parent rail into this fork node.
                if node.depth > 0 {
                    let parent_x = tree_base_x + (node.depth - 1).min(8) as f32 * indent_step + 4.0;
                    let target_x = tree_base_x + node.depth.min(8) as f32 * indent_step;
                    let y_end = if node.is_last { cy } else { cur_y + branch_row_h };
                    scene.line(parent_x, cur_y, parent_x, y_end, 1.15, rail_col, clip_id);
                    scene.line(parent_x, cy, target_x - 2.0, cy, 1.15, rail_col, clip_id);
                }

                let nx = tree_base_x + node.depth.min(8) as f32 * indent_step;
                if node.depth == 0 {
                    // Root branch dot (`main`).
                    scene.over_circle(
                        nx + 4.0,
                        cy,
                        if is_active_node { 3.1 } else { 2.4 },
                        if is_active_node { theme::OK } else { theme::INK_4 },
                        clip_id,
                    );
                } else {
                    // Fork branch glyph (`Icon::Branch`).
                    let bs = 10.5;
                    draw_icon(
                        scene,
                        Icon::Branch,
                        nx,
                        cy - bs * 0.5,
                        bs,
                        if is_active_node { theme::OK } else { theme::rgb(0x74, 0xb9, 0xff) },
                        clip_id,
                    );
                }

                // Right-aligned block count (or subtle hover `fork` button on active single-branch root).
                let mut br_rx = b_rect.x1 - 7.0;
                let mut fork_hit: Option<HitRect> = None;
                if is_active_node && b_hov {
                    let fl = "+fork";
                    let fw = m.measure(fl, TextFace::UiSmall) + 8.0;
                    let fr = HitRect::new(br_rx - fw, cur_y + 3.0, br_rx, cur_y + branch_row_h - 3.0);
                    scene.round_rect(
                        fr.x0,
                        fr.y0,
                        fr.x1,
                        fr.y1,
                        3.5,
                        theme::rgba(0x4c, 0xc2, 0x7a, 0.16),
                        0.0,
                        [0.0; 4],
                        clip_id,
                    );
                    scene.ui_small(fr.x0 + 4.0, b_ty, fl, theme::OK, true, false, clip_id);
                    fork_hit = Some(fr);
                    br_rx = fr.x0 - 4.0;
                } else if node.block_count > 0 {
                    let cnt = format!("{}", node.block_count);
                    let cw = m.measure(&cnt, TextFace::UiSmall);
                    br_rx -= cw;
                    scene.ui_small(br_rx, b_ty, &cnt, theme::INK_4, false, false, clip_id);
                    br_rx -= 5.0;
                }

                let name_x = nx + 13.0;
                let avail_name_w = (br_rx - name_x).max(16.0);
                let name_col = if is_active_node {
                    theme::INK_1
                } else if node.active {
                    theme::INK_2
                } else {
                    theme::INK_3
                };
                let max_name_c = (avail_name_w / m.measure("n", TextFace::UiSmall).max(5.0))
                    .floor()
                    .max(4.0) as usize;
                let shown_name = truncate_chars(&node.name, max_name_c);
                scene.ui_small(name_x, b_ty, &shown_name, name_col, is_active_node, false, clip_id);

                // Subtle fork-origin command hint right after the fork name when space permits.
                if let Some(cmd) = &node.fork_cmd {
                    let nw = m.measure(&shown_name, TextFace::UiSmall);
                    let cmd_x = name_x + nw + 5.0;
                    let rem_w = br_rx - cmd_x;
                    if rem_w >= 28.0 {
                        let rem_c = (rem_w / m.measure("n", TextFace::UiSmall).max(5.0)).floor() as usize;
                        if rem_c >= 4 {
                            let hint = truncate_chars(cmd, rem_c);
                            scene.ui_small(cmd_x, b_ty, &hint, theme::INK_4, false, true, clip_id);
                        }
                    }
                }

                scene.push_hit_clipped(
                    b_rect,
                    clip_id,
                    HitAction::SwitchTabBranch { tab: tab.id, branch: node.id },
                );
                if let Some(fr) = fork_hit {
                    scene.push_hit_clipped(fr, clip_id, HitAction::ForkActiveBranch);
                }
                cur_y += branch_row_h;
            }
            cur_y += 4.0;
        } else {
            cur_y += 2.0;
        }
    }

    // Compact footer bar with keyboard shortcut hints.
    let fy0 = y1 - footer_h;
    scene.line(0.0, fy0, sidebar_w - 1.0, fy0, 1.0, theme::LINE_FAINT, clip_id);
    scene.ui_small(
        pad_x,
        fy0 + (footer_h - small_ch) * 0.5,
        "⌘T new   ⌘B fork   ⌘S hide",
        theme::INK_4,
        false,
        false,
        clip_id,
    );
}

/// Draw crisp 1px hairline split dividers between tiled panes (highlighted while actively dragged).
pub fn draw_split_dividers(scene: &mut Scene, handles: &[SplitHandle], dragging: Option<SplitId>) {
    for h in handles {
        let is_drag = dragging == Some(h.split);
        let col = if is_drag { theme::INK_2 } else { theme::LINE };
        let thick = if is_drag { 2.0 } else { 1.0 };
        match h.axis {
            SplitAxis::Vertical => {
                let mx = (h.rect.x0 + h.rect.x1) * 0.5;
                scene.over_round_rect(mx - thick * 0.5, h.rect.y0, mx + thick * 0.5, h.rect.y1, 0.0, col, 0);
            }
            SplitAxis::Horizontal => {
                let my = (h.rect.y0 + h.rect.y1) * 0.5;
                scene.over_round_rect(h.rect.x0, my - thick * 0.5, h.rect.x1, my + thick * 0.5, 0.0, col, 0);
            }
        }
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
    let cap_h = m.ch + 6.0;
    let ty = y + 3.0;
    let pad_x = (m.cw * 1.1).round().max(8.0);
    let sep_pad = (m.cw * 1.05).round().max(8.0);
    // Uniform font-relative icon scale so EVERY icon inside the capsule matches text height at ANY zoom!
    let folder_s = m.icon_md();
    let branch_s = m.icon_sm();
    let file_s = m.icon_md();
    let tool_s = m.icon_md();

    // Consistent calm capsule styling across both live & past prompts (never flashes/dims on Enter!).
    let _ = is_live;
    let (cap_bg, cap_bd, sep_col, dir_col, branch_col, meta_col, icon_alpha) = (
        theme::rgb(0x16, 0x16, 0x18),
        theme::rgb(0x28, 0x28, 0x2c),
        theme::rgb(0x26, 0x26, 0x2a),
        theme::INK_1,
        theme::INK_2,
        theme::INK_3,
        1.0f32,
    );

    // Reserve outer capsule rect slot so we can patch exact width after laying out segments.
    let cap_idx = scene.rects.len();
    scene.round_rect(x0, y, x0 + 60.0, y + cap_h, 6.0, cap_bg, 1.0, cap_bd, clip_id);

    let mut cx = x0 + pad_x;
    let draw_divider = |scene: &mut Scene, x: f32| {
        scene.line(x, y + 4.0, x, y + cap_h - 4.0, 1.0, sep_col, clip_id);
    };

    // Segment 0: Host icon/custom emoji (`~/.config/blaze-shell/config.toml`) + optional hostname.
    let show_host = !prompt.host.is_empty() || prompt.host_symbol.is_some();
    if show_host {
        let seg0_x0 = cx - 4.0;
        let host_s = m.icon_md();
        let host_col = if prompt.is_remote { theme::INK_1 } else { theme::INK_2 };
        let host_label_raw = prompt
            .host_label_override
            .as_deref()
            .unwrap_or(prompt.host.as_str());
        if let Some(sym) = prompt.host_symbol.as_deref() {
            let sym_h = (host_s + 1.5).min(m.ch);
            let sw = draw_symbol_or_emoji(
                scene,
                cx,
                ty + (m.ch - sym_h) * 0.5,
                sym_h,
                sym,
                Some(host_col),
                icon_alpha,
                clip_id,
            );
            cx += sw;
            if !host_label_raw.is_empty() {
                cx += 5.0;
            }
        } else {
            let host_svg = if prompt.is_remote { "server_rack" } else { "macbook_pro" };
            scene.svg_icon_alpha(
                cx,
                ty + (m.ch - host_s) * 0.5,
                host_s,
                host_svg,
                icon_alpha,
                clip_id,
            );
            cx += host_s;
            if !host_label_raw.is_empty() {
                cx += 6.0;
            }
        }
        if !host_label_raw.is_empty() {
            let host_lbl = truncate_chars(host_label_raw, 16);
            scene.text(cx, ty, &host_lbl, host_col, prompt.is_remote, false, clip_id);
            cx += host_lbl.chars().count() as f32 * m.cw;
        }
        scene.push_hover_clipped(
            HitRect::new(seg0_x0, y, cx + 4.0, y + cap_h),
            clip_id,
            HoverKind::Tooltip {
                title: if prompt.is_remote {
                    format!("remote server · {}", prompt.host)
                } else {
                    format!("local machine · {}", prompt.host)
                },
                subtitle: Some(if prompt.is_remote {
                    "connected via SSH / BSH1".to_owned()
                } else {
                    format!("{} {} · ~/.config/blaze-shell/config.toml", std::env::consts::OS, std::env::consts::ARCH)
                }),
            },
        );
        cx += sep_pad;
        draw_divider(scene, cx);
        cx += sep_pad;
    }

    // Segment 1: Location (custom project emoji or macOS folder SVG + quiet parent path + crisp leaf dir).
    let seg1_x0 = cx - 4.0;
    let raw_dir_lbl = match prompt.project_label_override.as_deref() {
        Some(custom) => custom,
        None => {
            if prompt.dir_short.is_empty() {
                &prompt.cwd_display
            } else {
                &prompt.dir_short
            }
        }
    };
    let eff_dir_col = prompt.project_color.unwrap_or(dir_col);
    if let Some(sym) = prompt.project_symbol.as_deref() {
        let sym_h = (folder_s + 1.5).min(m.ch);
        let sw = draw_symbol_or_emoji(
            scene,
            cx,
            ty + (m.ch - sym_h) * 0.5,
            sym_h,
            sym,
            Some(eff_dir_col),
            icon_alpha,
            clip_id,
        );
        cx += sw;
        if !raw_dir_lbl.is_empty() {
            cx += 6.0;
        }
    } else {
        scene.svg_icon_alpha(cx, ty + (m.ch - folder_s) * 0.5, folder_s, "folder_macos", icon_alpha, clip_id);
        cx += folder_s + 6.0;
    }

    if !raw_dir_lbl.is_empty() {
        let max_dir = ((max_w * 0.58) / m.cw).floor().max(16.0) as usize;
        let dir_lbl = truncate_chars(raw_dir_lbl, max_dir);
        if let Some(slash) = dir_lbl.rfind('/')
            && slash + 1 < dir_lbl.len()
        {
            let parent = &dir_lbl[..=slash];
            let leaf = &dir_lbl[slash + 1..];
            scene.text(cx, ty, parent, meta_col, false, false, clip_id);
            cx += parent.chars().count() as f32 * m.cw;
            scene.text(cx, ty, leaf, eff_dir_col, true, false, clip_id);
            cx += leaf.chars().count() as f32 * m.cw;
        } else {
            scene.text(cx, ty, &dir_lbl, eff_dir_col, true, false, clip_id);
            cx += dir_lbl.chars().count() as f32 * m.cw;
        }
    }
    if let Some(badge) = prompt.project_badge.as_deref()
        && !badge.is_empty()
    {
        cx += 6.0;
        let bw = m.measure(badge, TextFace::MonoSmall) + 10.0;
        let bh = m.ch - 2.0;
        let by = ty + 1.0;
        let bc = prompt.project_color.unwrap_or(theme::SERIES_BLUE);
        scene.round_rect(
            cx,
            by,
            cx + bw,
            by + bh,
            bh * 0.5,
            [bc[0], bc[1], bc[2], 0.16],
            1.0,
            [bc[0], bc[1], bc[2], 0.42],
            clip_id,
        );
        scene.mono_small(cx + 5.0, ty + 1.0, badge, bc, true, false, clip_id);
        cx += bw;
    }
    if !prompt.cwd.is_empty() {
        scene.push_hover_clipped(
            HitRect::new(seg1_x0, y, cx + 4.0, y + cap_h),
            clip_id,
            HoverKind::Directory { cwd: prompt.cwd.clone() },
        );
    }

    // Segment 2: Git branch + in-progress state (hover/click opens interactive branch switcher!).
    if let Some(branch) = &prompt.git_branch {
        cx += sep_pad;
        draw_divider(scene, cx);
        cx += sep_pad;
        let seg2_x0 = cx - 4.0;

        let branch_icon_col = if prompt.git_detached { theme::FIRE } else { theme::INK_3 };
        draw_icon(scene, Icon::Branch, cx, ty + (m.ch - branch_s) * 0.5, branch_s, branch_icon_col, clip_id);
        cx += branch_s + 5.0;

        if prompt.git_detached {
            let branch_lbl = format!("@{branch}");
            scene.text(cx, ty, &branch_lbl, branch_col, false, false, clip_id);
            cx += branch_lbl.chars().count() as f32 * m.cw;
        } else {
            cx += draw_styled_branch_name(scene, cx, ty, branch, is_live, false, m, clip_id);
        }

        if let Some(st) = &prompt.git_status {
            if let Some(state_lbl) = &st.state_label {
                cx += 6.0;
                scene.text(cx, ty, state_lbl, theme::FIRE, true, false, clip_id);
                cx += state_lbl.chars().count() as f32 * m.cw;
            }
            // Compact inline divergence indicator + mini visual bar vs `main` / `origin/main`
            // right on the branch segment (hovering opens the full visual commit graph popover!).
            if is_live
                && let Some(rel) = &st.branch_relation
                && (rel.ahead > 0 || rel.behind > 0)
            {
                cx += 7.0;
                let a_col = theme::rgb(0x57, 0xc7, 0x85);
                let b_col = if rel.behind >= 10 {
                    theme::rgb(0xf0, 0x71, 0x78)
                } else {
                    theme::rgb(0xe8, 0xa8, 0x48)
                };
                if rel.ahead > 0 {
                    let s = format!("↑{}", rel.ahead);
                    scene.text(cx, ty, &s, a_col, false, false, clip_id);
                    cx += s.chars().count() as f32 * m.cw;
                    if rel.behind > 0 {
                        cx += 4.0;
                    }
                }
                if rel.behind > 0 {
                    let s = format!("↓{}", rel.behind);
                    scene.text(cx, ty, &s, b_col, rel.behind >= 10, false, clip_id);
                    cx += s.chars().count() as f32 * m.cw;
                }
                // Proportional mini ahead/behind bar (`20px × 3.5px`) so divergence reads at a glance.
                cx += 5.0;
                let bar_w = 20.0f32;
                let bar_h = 3.5f32;
                let bar_y0 = ty + (m.ch - bar_h) * 0.5;
                scene.round_rect(
                    cx,
                    bar_y0,
                    cx + bar_w,
                    bar_y0 + bar_h,
                    1.75,
                    theme::BG_TRACK,
                    0.0,
                    [0.0; 4],
                    clip_id,
                );
                let total_d = (rel.ahead + rel.behind) as f32;
                let fill_scale = ((total_d + 1.0).ln() / 20.0f32.ln()).clamp(0.28, 1.0);
                let fill_w = (bar_w * fill_scale).max(4.0);
                if rel.ahead > 0 && rel.behind > 0 {
                    let gap = 1.0;
                    let avail = (fill_w - gap).max(3.0);
                    let aw = ((rel.ahead as f32 / total_d) * avail).clamp(2.0, avail - 2.0);
                    scene.round_rect(cx, bar_y0, cx + aw, bar_y0 + bar_h, 1.5, a_col, 0.0, [0.0; 4], clip_id);
                    scene.round_rect(cx + aw + gap, bar_y0, cx + fill_w, bar_y0 + bar_h, 1.5, b_col, 0.0, [0.0; 4], clip_id);
                } else if rel.ahead > 0 {
                    scene.round_rect(cx, bar_y0, cx + fill_w, bar_y0 + bar_h, 1.5, a_col, 0.0, [0.0; 4], clip_id);
                } else {
                    scene.round_rect(cx, bar_y0, cx + fill_w, bar_y0 + bar_h, 1.5, b_col, 0.0, [0.0; 4], clip_id);
                }
                cx += bar_w;
            }
        }
        let branch_rect = HitRect::new(seg2_x0, y, cx + 4.0, y + cap_h);
        let branches = prompt
            .git_status
            .as_ref()
            .map(|s| s.branches.clone())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| vec![branch.clone()]);
        let relation = prompt
            .git_status
            .as_ref()
            .and_then(|s| s.branch_relation.clone());
        scene.push_hover_clipped(
            branch_rect,
            clip_id,
            HoverKind::GitBranch { current: branch.clone(), branches, relation },
        );
        scene.push_hit_clipped(branch_rect, clip_id, HitAction::OpenBranchPicker);

        if let Some(st) = &prompt.git_status {
            // Segment 3: Uncommitted files (hover lists exact files; click a file to insert path!).
            let total_dirty = st.conflicted + st.staged + st.modified + st.deleted + st.untracked;
            if !st.changed_icons.is_empty() {
                cx += sep_pad;
                draw_divider(scene, cx);
                cx += sep_pad;
                let seg3_x0 = cx - 4.0;

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
                    } else {
                        theme::INK_1
                    };
                    cx += 2.0;
                    scene.text(cx, ty, &cnt, cnt_col, true, false, clip_id);
                    cx += cnt.len() as f32 * m.cw;
                }
                scene.push_hover_clipped(
                    HitRect::new(seg3_x0, y, cx + 4.0, y + cap_h),
                    clip_id,
                    HoverKind::GitChanges {
                        files: st.changed_files.clone(),
                        details: st.changed_details.clone(),
                        total: total_dirty,
                    },
                );
            } else if st.staged > 0 || st.conflicted > 0 {
                cx += sep_pad;
                draw_divider(scene, cx);
                cx += sep_pad;
                let seg3_x0 = cx - 4.0;
                let dot_col = if st.conflicted > 0 { theme::DANGER } else { theme::OK };
                scene.over_circle(cx + 3.0, ty + m.ch * 0.5, 2.6, dot_col, clip_id);
                cx += 9.0;
                let n = if st.conflicted > 0 { st.conflicted } else { st.staged };
                let cnt = format!("{n}");
                scene.text(cx, ty, &cnt, meta_col, false, false, clip_id);
                cx += cnt.len() as f32 * m.cw;
                scene.push_hover_clipped(
                    HitRect::new(seg3_x0, y, cx + 4.0, y + cap_h),
                    clip_id,
                    HoverKind::Tooltip {
                        title: if st.conflicted > 0 {
                            format!("{} merge conflict(s)", st.conflicted)
                        } else {
                            format!("{} staged change(s)", st.staged)
                        },
                        subtitle: Some("ready to commit".to_owned()),
                    },
                );
            }

            // Segment 4: Upstream ahead/behind (only when distinct from Segment 2's base relation) & stash count.
            let show_upstream_ab = (st.ahead > 0 || st.behind > 0)
                && !st
                    .branch_relation
                    .as_ref()
                    .is_some_and(|r| is_live && r.ahead == st.ahead && r.behind == st.behind);
            if show_upstream_ab || st.stashed > 0 {
                cx += sep_pad;
                draw_divider(scene, cx);
                cx += sep_pad;

                let mut need_gap = false;
                if show_upstream_ab {
                    let ab_x0 = cx - 4.0;
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
                    scene.push_hover_clipped(
                        HitRect::new(ab_x0, y, cx + 4.0, y + cap_h),
                        clip_id,
                        HoverKind::Tooltip {
                            title: format!("{} ahead · {} behind upstream", st.ahead, st.behind),
                            subtitle: Some("computed from .git/objects/info/commit-graph".to_owned()),
                        },
                    );
                }
                if st.stashed > 0 {
                    if need_gap {
                        cx += 6.0;
                    }
                    let stash_x0 = cx - 4.0;
                    let st_s = m.icon_sm();
                    draw_icon(scene, Icon::Stash, cx, ty + (m.ch - st_s) * 0.5, st_s, meta_col, clip_id);
                    cx += st_s + 3.5;
                    let s = format!("{}", st.stashed);
                    scene.text(cx, ty, &s, meta_col, false, false, clip_id);
                    cx += s.len() as f32 * m.cw;
                    scene.push_hover_clipped(
                        HitRect::new(stash_x0, y, cx + 4.0, y + cap_h),
                        clip_id,
                        HoverKind::GitStashes {
                            stashes: st.stashes.clone(),
                            entries: st.stash_entries.clone(),
                            total: st.stashed,
                        },
                    );
                }
            }
        }
    }

    // Segment 5: Detected project toolchain SVG icons (hover explains detected markers!).
    if !prompt.hide_toolchains && !prompt.project_icons.is_empty() {
        cx += sep_pad;
        draw_divider(scene, cx);
        cx += sep_pad;
        let seg5_x0 = cx - 4.0;
        let t_iy = ty + (m.ch - tool_s) * 0.5;
        for (i, &icon_name) in prompt.project_icons.iter().take(3).enumerate() {
            if i > 0 {
                cx += 4.0;
            }
            scene.svg_icon_alpha(cx, t_iy, tool_s, icon_name, icon_alpha * 0.9, clip_id);
            cx += tool_s;
        }
        if !prompt.project_details.is_empty() {
            scene.push_hover_clipped(
                HitRect::new(seg5_x0, y, cx + 4.0, y + cap_h),
                clip_id,
                HoverKind::Toolchains { items: prompt.project_details.clone() },
            );
        }
    }

    // Segment 6: Live Folder Processes & Hot Indicator — compact bundle of distinct process icons + count
    // (mirrors Segment 3 uncommitted file icons; hover reveals full CPU/MEM/ancestry/Kill details!).
    if is_live && !prompt.folder_procs.items.is_empty() {
        let fp = &prompt.folder_procs;
        let count = fp.items.len();
        cx += sep_pad;
        draw_divider(scene, cx);
        cx += sep_pad;
        let seg6_x0 = cx - 4.0;
        let pill_fg = if fp.is_hot {
            theme::rgb(0xff, 0xa8, 0x5c)
        } else {
            theme::rgb(0x7c, 0xd4, 0xa8)
        };
        let dot_col = if fp.is_hot {
            theme::rgb(0xff, 0x6b, 0x4a)
        } else {
            theme::OK
        };

        // Collect up to 4 distinct process icons in priority order (macOS app icons + SVG runtime badges).
        let mut distinct_icons: Vec<ProcBadgeIcon<'_>> = Vec::with_capacity(4);
        for item in &fp.items {
            let ic = proc_badge_icon(item);
            if !distinct_icons.contains(&ic) {
                distinct_icons.push(ic);
                if distinct_icons.len() >= 4 {
                    break;
                }
            }
        }
        let proc_s = file_s;
        let cnt_lbl = if fp.is_hot {
            let hot_pct = fp.total_cpu_pct.max(fp.total_gpu_pct).round() as u32;
            format!("{hot_pct}% · {count}")
        } else {
            format!("{count}")
        };
        let cnt_w = cnt_lbl.chars().count() as f32 * m.cw;

        // Reserve pill background first, then patch its exact right edge after laying out icons, bar & count!
        let hot_pill_idx = if fp.is_hot {
            let idx = scene.rects.len();
            scene.round_rect(
                seg6_x0 + 1.0,
                y + 2.5,
                cx + 40.0,
                y + cap_h - 2.5,
                (cap_h - 5.0) * 0.5,
                theme::rgba(0xe8, 0x6a, 0x35, 0.16),
                0.75,
                theme::rgba(0xff, 0x8a, 0x4c, 0.48),
                clip_id,
            );
            cx += 3.0;
            scene.text(cx, ty, "🔥", pill_fg, true, false, clip_id);
            cx += m.cw + 5.0;
            Some(idx)
        } else {
            scene.over_circle(cx + 3.0, ty + m.ch * 0.5, 2.7, dot_col, clip_id);
            cx += 9.5;
            None
        };

        let p_iy = ty + (m.ch - proc_s) * 0.5;
        for ic in &distinct_icons {
            match *ic {
                ProcBadgeIcon::Image(path) => {
                    scene.image(cx, p_iy, proc_s, proc_s, path.to_owned(), clip_id);
                }
                ProcBadgeIcon::Svg(name) => {
                    scene.svg_icon_alpha(cx, p_iy, proc_s, name, icon_alpha, clip_id);
                }
            }
            cx += proc_s + 3.0;
        }
        // Compact inline CPU heat bar (`24px × 4px`) when active/hot so load reads visually at a glance!
        let peak_pct = fp.total_cpu_pct.max(fp.total_gpu_pct);
        if fp.is_hot || peak_pct >= 2.0 {
            let bar_w = 24.0f32;
            let bar_h = 4.0f32;
            let bar_y0 = ty + (m.ch - bar_h) * 0.5;
            let frac = (peak_pct / 100.0).clamp(0.08, 1.0);
            let bar_col = if peak_pct >= 60.0 {
                theme::rgb(0xff, 0x6b, 0x4a)
            } else if peak_pct >= 20.0 {
                theme::rgb(0xe8, 0xa8, 0x48)
            } else {
                theme::OK
            };
            scene.round_rect(cx, bar_y0, cx + bar_w, bar_y0 + bar_h, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
            scene.round_rect(cx, bar_y0, cx + (bar_w * frac).max(3.0), bar_y0 + bar_h, 2.0, bar_col, 0.0, [0.0; 4], clip_id);
            cx += bar_w + 5.0;
        }
        scene.text(cx, ty, &cnt_lbl, pill_fg, fp.is_hot, false, clip_id);
        cx += cnt_w + 2.0;
        if let Some(idx) = hot_pill_idx {
            cx += 3.0;
            scene.rects[idx].rect[2] = cx.min(x0 + max_w - pad_x + 4.0);
        }
        scene.push_hover_clipped(
            HitRect::new(seg6_x0, y, cx + 4.0, y + cap_h),
            clip_id,
            HoverKind::FolderProcs { summary: fp.clone() },
        );
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
    pty_focused: bool,
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
    let fork_hit = HitRect::new(fx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0);
    draw_icon(scene, Icon::Branch, fx0, act_iy, act_s, theme::INK_4, clip_id);
    scene.push_hit_clipped(fork_hit, clip_id, HitAction::ForkBlock(block.id));
    scene.push_hover_clipped(
        fork_hit,
        clip_id,
        HoverKind::Tooltip {
            title: "Fork session from this block".to_owned(),
            subtitle: Some("O(1) copy-on-write state snapshot".to_owned()),
        },
    );
    rx = fx0 - 12.0;

    // Quiet icon-only table/grid view toggle on file-entry tables (`ls`, `ls -l`, `ls | sort-by ...`).
    if let BlockBody::Value(Value::Table(t)) = &block.body
        && is_file_table(t)
    {
        let in_grid = block.is_grid_view(t.default_grid);
        let toggle_icon = if in_grid { Icon::Table } else { Icon::Grid };
        let mx0 = rx - act_s;
        let toggle_hit = HitRect::new(mx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0);
        draw_icon(scene, toggle_icon, mx0, act_iy, act_s, theme::INK_4, clip_id);
        scene.push_hit_clipped(toggle_hit, clip_id, HitAction::ToggleTableView(block.id));
        scene.push_hover_clipped(
            toggle_hit,
            clip_id,
            HoverKind::Tooltip {
                title: if in_grid {
                    "Switch to detailed table".to_owned()
                } else {
                    "Switch to compact grid".to_owned()
                },
                subtitle: None,
            },
        );
        rx = mx0 - 12.0;
    }

    let running_pid: Option<u32> = match &block.body {
        BlockBody::Running { pid, .. } => {
            let p = pid.load(std::sync::atomic::Ordering::Relaxed);
            if p > 0 { Some(p) } else { None }
        }
        BlockBody::Terminal { pid, running: true, .. } => *pid,
        _ => None,
    };
    if matches!(&block.body, BlockBody::Running { .. } | BlockBody::Terminal { running: true, .. }) {
        if matches!(&block.body, BlockBody::Terminal { running: true, .. }) {
            let fs_lbl = "⤢ ⌘⇧F";
            let fsw = fs_lbl.chars().count() as f32 * m.cw + 10.0;
            let fx0 = rx - fsw;
            scene.round_rect(
                fx0,
                cur_y - 1.0,
                rx,
                cur_y + m.ch + 1.0,
                4.0,
                theme::BG_CHIP,
                1.0,
                theme::LINE_FAINT,
                clip_id,
            );
            scene.text(fx0 + 5.0, cur_y, fs_lbl, theme::INK_3, false, false, clip_id);
            let frect = HitRect::new(fx0, cur_y - 2.0, rx, cur_y + m.ch + 2.0);
            scene.push_hit_clipped(frect, clip_id, HitAction::TogglePtyFullscreen);
            scene.push_hover_clipped(
                frect,
                clip_id,
                HoverKind::Tooltip {
                    title: "Toggle fullscreen terminal (⌘⇧F)".to_owned(),
                    subtitle: Some("Inline by default so scrollback stays visible".to_owned()),
                },
            );
            rx = fx0 - 6.0;

            let (foc_lbl, foc_fg, foc_bg, foc_border) = if pty_focused {
                (
                    "● focused",
                    theme::rgb(0x8c, 0xc2, 0xff),
                    theme::rgba(0x58, 0x9d, 0xf6, 0.14),
                    theme::rgba(0x58, 0x9d, 0xf6, 0.35),
                )
            } else {
                ("click to type", theme::INK_3, theme::BG_CHIP, theme::LINE_FAINT)
            };
            let foc_w = foc_lbl.chars().count() as f32 * m.cw + 10.0;
            let foc_x0 = rx - foc_w;
            scene.round_rect(
                foc_x0,
                cur_y - 1.0,
                rx,
                cur_y + m.ch + 1.0,
                4.0,
                foc_bg,
                1.0,
                foc_border,
                clip_id,
            );
            scene.text(foc_x0 + 5.0, cur_y, foc_lbl, foc_fg, false, false, clip_id);
            let foc_rect = HitRect::new(foc_x0, cur_y - 2.0, rx, cur_y + m.ch + 2.0);
            if !pty_focused {
                scene.push_hit_clipped(foc_rect, clip_id, HitAction::FocusInlinePty(block.id));
            }
            scene.push_hover_clipped(
                foc_rect,
                clip_id,
                HoverKind::Tooltip {
                    title: if pty_focused {
                        "Inline terminal focused (⌘⎋ or click below to type in shell)".to_owned()
                    } else {
                        "Click to focus inline terminal".to_owned()
                    },
                    subtitle: None,
                },
            );
            rx = foc_x0 - 10.0;
        }
        let elapsed_ms = (now_ns.max(0) as u64 / 1_000_000).saturating_sub(block.started_ms);
        let dur = format_duration_ms(elapsed_ms);
        let lbl = match running_pid {
            Some(p) => format!("pid {p} · {dur}"),
            None => dur,
        };
        let w = lbl.chars().count() as f32 * m.cw;
        scene.text(rx - w, cur_y, lbl, theme::FIRE, false, false, clip_id);
        scene.over_circle(rx - w - 9.0, cur_y + m.ch * 0.5, 3.0, theme::FIRE, clip_id);
        rx -= w + 20.0;
    } else {
        let eff_ns = if block.duration_ns > 0 {
            block.duration_ns
        } else {
            block.duration_ms.saturating_mul(1_000_000)
        };
        let dur_lbl = format_duration_ns(eff_ns);
        let dur_w = dur_lbl.chars().count() as f32 * m.cw;
        let dx0 = rx - dur_w;
        scene.text(dx0, cur_y, &dur_lbl, theme::INK_4, false, false, clip_id);
        let mut ns_commas = String::new();
        let _ = blaze_sh::format_u64_commas(eff_ns, &mut ns_commas);
        scene.push_hover_clipped(
            HitRect::new(dx0 - 3.0, cur_y - 2.0, rx + 3.0, cur_y + m.ch + 2.0),
            clip_id,
            HoverKind::Tooltip {
                title: format!("{ns_commas} ns"),
                subtitle: Some(format!("command wall time ({dur_lbl})")),
            },
        );
        rx -= dur_w + 12.0;

        if let BlockBody::Terminal { pid: Some(p), .. } = &block.body {
            let pid_lbl = format!("pid {p}");
            let pw = pid_lbl.len() as f32 * m.cw;
            scene.text(rx - pw, cur_y, pid_lbl, theme::INK_4, false, false, clip_id);
            rx -= pw + 12.0;
        }

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
        None,
        None,
        None,
        false,
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
            BlockBody::Terminal { term, running, .. } => layout_inline_terminal(
                scene,
                block.id,
                term,
                *running,
                *running && pty_focused,
                cursor_visible,
                x0,
                cur_y,
                body_w,
                m,
                clip_id,
            ),
            BlockBody::Running { label, pid } => {
                scene.over_circle(x0 + 5.0, cur_y + m.ch * 0.5, 3.0, theme::FIRE, clip_id);
                let p = pid.load(std::sync::atomic::Ordering::Relaxed);
                let base = if label.is_empty() { "running…" } else { label.as_str() };
                let txt = if p > 0 { format!("pid {p} · {base}") } else { base.to_owned() };
                scene.text(x0 + 15.0, cur_y, txt, theme::INK_3, false, false, clip_id);
                m.ch + 2.0
            }
            BlockBody::SysMon(state) => crate::sysmon_view::layout_sysmon_block(
                scene,
                block.id,
                state,
                x0,
                cur_y,
                x1,
                None,
                m,
                clip_id,
                false,
            ),
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
    abbrevs: Option<&std::collections::BTreeMap<String, String>>,
    cwd: Option<&str>,
    live_cursor: Option<usize>,
    has_live_completions: bool,
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
        let col = if sp.kind == blaze_sh::lex::HighlightKind::Command && s < e {
            let tok = &source[s..e];
            match crate::editor::classify_command_token(tok, abbrevs) {
                crate::editor::CommandHighlightKind::Builtin => theme::CMD_BUILTIN,
                crate::editor::CommandHighlightKind::Abbrev => theme::CMD_ABBREV,
                crate::editor::CommandHighlightKind::External => theme::CMD_EXTERNAL,
                crate::editor::CommandHighlightKind::DataFile => {
                    theme::highlight_color(blaze_sh::lex::HighlightKind::String)
                }
                crate::editor::CommandHighlightKind::Unknown => {
                    // While actively typing a token at the cursor that has live completions
                    // or matches a known command/data-file prefix, keep it clean neutral ink
                    // instead of screaming red mid-word (e.g. `Car` -> `Cargo.lock` / `cargo`).
                    let editing_here = live_cursor.is_some_and(|c| c >= s && c <= e);
                    if editing_here
                        && (has_live_completions
                            || crate::editor::is_known_command_prefix(tok, cwd, abbrevs))
                    {
                        theme::INK_1
                    } else {
                        theme::CMD_UNKNOWN
                    }
                }
            }
        } else {
            theme::highlight_color(sp.kind)
        };
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

    // Record hoverable command-token spans for live man-page / `--help` explain-on-hover!
    let cmd_tokens = blaze_cli_spec::explain::tokenize_command_spans(
        source,
        blaze_own::Alloc::global(),
    );
    if !cmd_tokens.is_empty() {
        // Compute (line_y, col) for every byte index in `source` matching the wrap layout above.
        let mut byte_pos_xy: Vec<Option<(f32, f32)>> = vec![None; source.len()];
        let mut ly = y0;
        let mut b_off = 0usize;
        for raw_line in source.split('\n') {
            let mut col = 0usize;
            for (rel_idx, _ch) in raw_line.char_indices() {
                if col >= max_cols {
                    ly += line_step;
                    col = 0;
                }
                if let Some(slot) = byte_pos_xy.get_mut(b_off + rel_idx) {
                    *slot = Some((x0 + col as f32 * m.cw, ly));
                }
                col += 1;
            }
            b_off = (b_off + raw_line.len() + 1).min(source.len());
            ly += line_step;
        }
        for tok in cmd_tokens.iter() {
            if tok.start >= tok.end || tok.end > source.len() {
                continue;
            }
            if matches!(tok.role, blaze_cli_spec::explain::TokenRole::Arg)
                && tok.for_flag.is_none()
            {
                continue;
            }
            let Some((tx0, ty0)) = byte_pos_xy.get(tok.start).copied().flatten() else {
                continue;
            };
            // Find last char inside [tok.start, tok.end) on the same wrapped line.
            let mut tx1 = tx0 + m.cw;
            for pos in byte_pos_xy[tok.start..tok.end].iter().flatten() {
                if (pos.1 - ty0).abs() < 0.5 {
                    tx1 = tx1.max(pos.0 + m.cw);
                }
            }
            scene.push_hover_clipped(
                HitRect::new(tx0, ty0, tx1, ty0 + m.ch),
                clip_id,
                HoverKind::CommandToken {
                    source: source.to_owned(),
                    token_start: tok.start,
                    token_end: tok.end,
                },
            );
        }
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
        Value::Record(r) if depth == 0 && crate::schema_view::is_schema_record(r) => {
            crate::schema_view::layout_schema_card(scene, r, x0, y0, width, m, clip_id)
        }
        Value::Record(r) => layout_record(scene, block, r, x0, y0, width, m, clip_id, depth, now_ns),
        Value::List(items) => layout_list(scene, block, items, x0, y0, width, m, clip_id, depth, now_ns),
        Value::Error(err) => {
            layout_error_panel(scene, &block.source, err, x0, y0, width, m, clip_id)
        }
        Value::String(s) if depth == 0 && open_target_filename(&block.source).is_some() => {
            let fname = open_target_filename(&block.source).unwrap_or("file");
            let target_path = std::path::Path::new(&block.prompt_snapshot.cwd).join(fname);
            let base_dir = target_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(&block.prompt_snapshot.cwd));
            let max_h = if block.folded { 520.0 } else { 16000.0 };
            with_cached_open_preview(block.id.0, fname, s.as_ref().as_bytes(), |doc| {
                crate::yazi::render_rich_preview(
                    scene,
                    doc,
                    Some(base_dir),
                    Some(&target_path),
                    x0,
                    y0,
                    width,
                    y0 + max_h,
                    m,
                    clip_id,
                )
                .max(m.ch)
            })
        }
        Value::Binary(b) if depth == 0 && open_target_filename(&block.source).is_some() => {
            let fname = open_target_filename(&block.source).unwrap_or("file");
            let target_path = std::path::Path::new(&block.prompt_snapshot.cwd).join(fname);
            let base_dir = target_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(&block.prompt_snapshot.cwd));
            with_cached_open_preview(block.id.0, fname, b.as_ref(), |doc| {
                crate::yazi::render_rich_preview(
                    scene,
                    doc,
                    Some(base_dir),
                    Some(&target_path),
                    x0,
                    y0,
                    width,
                    y0 + 540.0,
                    m,
                    clip_id,
                )
                .max(m.ch)
            })
        }
        Value::Output(out) => {
            layout_command_output(scene, block.id.0, &block.source, out, x0, y0, width, m, clip_id)
        }
        scalar => layout_scalar(scene, scalar, x0, y0, width, m, clip_id, now_ns),
    }
}

/// Cache parsed `blaze_preview::PreviewDoc` per `(block_id, filename, byte_len)` on the UI thread
/// so scrolling and live animations never re-parse Markdown, HTML, or syntax highlighting per frame!
fn with_cached_open_preview<R>(
    block_id: u64,
    fname: &str,
    bytes: &[u8],
    f: impl FnOnce(&blaze_preview::PreviewDoc<'static>) -> R,
) -> R {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static CACHE: RefCell<HashMap<(u64, String, usize), blaze_preview::PreviewDoc<'static>>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cell| {
        let key = (block_id, fname.to_owned(), bytes.len());
        let mut map = cell.borrow_mut();
        if !map.contains_key(&key) {
            if map.len() >= 32 {
                map.clear();
            }
            let open_limits = blaze_preview::PreviewLimits {
                max_rows: 64,
                max_cols: 8,
                max_lines: 240,
                max_cell_chars: 96,
            };
            let doc = blaze_preview::build_preview(
                fname,
                bytes,
                bytes.len() as u64,
                open_limits,
                Alloc::global(),
            );
            map.insert(key.clone(), doc);
        }
        let doc = map.get(&key).expect("just inserted");
        f(doc)
    })
}

/// Extract `<file>` when a command line is a direct `open <file>` (without pipes), so `open`
/// shares the exact same `blaze-preview` display engine as `y` and hover previews!
fn open_target_filename(source: &str) -> Option<&str> {
    let trimmed = source.trim();
    if trimmed.contains('|') || trimmed.contains(';') {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    if parts.next()? != "open" {
        return None;
    }
    for tok in parts {
        if tok.starts_with('-') {
            continue;
        }
        return Some(tok.trim_matches(|c| c == '"' || c == '\''));
    }
    None
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

fn full_path_for_entry(block_cwd: &str, block_source: &str, entry_name: &str) -> String {
    let rel = drag_path_for_entry(block_source, entry_name);
    if rel.starts_with('/') {
        return rel;
    }
    if let Some(rest) = rel.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{}/{rest}", home.trim_end_matches('/'));
    }
    if block_cwd.is_empty() {
        rel
    } else {
        format!("{}/{}", block_cwd.trim_end_matches('/'), rel)
    }
}

fn resolve_ls_target_dir(block_cwd: &str, block_source: &str) -> std::path::PathBuf {
    if let Some(prefix) = resolve_block_dir_prefix(block_source) {
        if prefix.starts_with('/') {
            return std::path::PathBuf::from(prefix);
        }
        if let Some(rest) = prefix.strip_prefix("~/")
            && let Ok(home) = std::env::var("HOME")
        {
            return std::path::PathBuf::from(home).join(rest);
        }
        return std::path::PathBuf::from(block_cwd).join(prefix);
    }
    std::path::PathBuf::from(block_cwd)
}

fn cached_ls_git_statuses(
    block: &Block,
    entries: &[(String, bool)],
) -> Option<Vec<GitFileStatus>> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    thread_local! {
        static CACHE: RefCell<HashMap<(u64, usize), (Instant, Option<Vec<GitFileStatus>>)>> =
            RefCell::new(HashMap::new());
    }
    let key = (block.id.0, entries.len());
    let cached = CACHE.with(|c| {
        c.borrow()
            .get(&key)
            .filter(|(t, _)| t.elapsed() < Duration::from_millis(750))
            .map(|(_, v)| v.clone())
    });
    if let Some(hit) = cached {
        return hit;
    }
    let dir = resolve_ls_target_dir(block.state_after.cwd.as_ref(), &block.source);
    let computed =
        crate::prompt::git_ls_entry_statuses(&dir, entries, block.prompt_snapshot.git_status.as_ref());
    CACHE.with(|c| {
        let mut m = c.borrow_mut();
        if m.len() > 128 {
            m.clear();
        }
        m.insert(key, (Instant::now(), computed.clone()));
    });
    computed
}

fn git_status_style(st: GitFileStatus, is_dir: bool) -> (f32, [f32; 4], Option<(&'static str, [f32; 4])>, bool) {
    match st {
        // Ignored / build artifacts: strongly faded, zero badge clutter.
        GitFileStatus::Ignored => (0.40, theme::INK_4, None, false),
        // Clean tracked: normal bright text, NO dot clutter (IntelliJ VCS style).
        GitFileStatus::Tracked => (
            1.0,
            if is_dir { theme::INK_1 } else { theme::INK_2 },
            None,
            false,
        ),
        // Tracked with uncommitted edits: IntelliJ VCS modified sky-blue + `M`.
        GitFileStatus::Modified => (
            1.0,
            theme::rgb(0x7c, 0xc0, 0xff),
            Some(("M", theme::rgb(0x58, 0x9d, 0xf6))),
            false,
        ),
        // Staged / added: IntelliJ VCS added emerald + `+`.
        GitFileStatus::Staged => (
            1.0,
            theme::rgb(0x88, 0xe0, 0x9e),
            Some(("+", theme::rgb(0x57, 0xc7, 0x85))),
            false,
        ),
        // Untracked (not in git yet): visibly faded warm-rust (IntelliJ VCS untracked),
        // NO `?` glued right after `/` so folders like `.blaze/` stay crisp and clean.
        GitFileStatus::Untracked => (
            0.76,
            theme::rgb(0xc8, 0x89, 0x76),
            None,
            false,
        ),
        GitFileStatus::Deleted => (
            0.75,
            theme::rgb(0xf0, 0x71, 0x78),
            Some(("D", theme::rgb(0xf0, 0x71, 0x78))),
            false,
        ),
        GitFileStatus::Conflicted => (
            1.0,
            theme::rgb(0xff, 0x88, 0x88),
            Some(("!", theme::rgb(0xf8, 0x51, 0x49))),
            false,
        ),
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

    let row_h = m.ch + 2.0;
    let pane_h = scene
        .clips
        .get(clip_id as usize)
        .map(|c| (c.rect[3] - c.rect[1]).max(220.0))
        .unwrap_or(720.0);
    let max_grid_rows = ((pane_h / 3.0 - 24.0) / row_h).floor().clamp(4.0, 9.0) as usize;
    // Cap initial scan for optimal column packing; after packing we enforce `<= 1/3` pane height.
    let grid_scan_cap = (max_grid_rows * 6).min(54);
    let visible_scan = if block.folded && total > grid_scan_cap {
        grid_scan_cap
    } else {
        total
    };

    let icon_s = m.icon_md();
    let icon_gap = (m.cw * 0.7).round().max(5.0);
    let col_gap = (m.cw * 1.8).round().max(14.0);

    // Pre-collect display labels + SVG icon names + optional git tracked/status badges.
    struct GridItem {
        raw: String,
        label: String,
        icon: &'static str,
        is_dir: bool,
        git: Option<GitFileStatus>,
        w_px: f32,
    }
    let mut raw_entries: Vec<(String, bool)> = Vec::with_capacity(visible_scan);
    let mut items: Vec<GridItem> = Vec::with_capacity(visible_scan);
    for r in 0..visible_scan {
        let raw = table
            .cell(r, name_col)
            .and_then(Value::as_str)
            .unwrap_or("");
        let kind = type_col.and_then(|ti| table.cell(r, ti)).and_then(Value::as_str);
        let is_dir = kind.is_some_and(blaze_sniff::is_dir_label) || raw.ends_with('/');
        let label = if is_dir && !raw.ends_with('/') {
            format!("{raw}/")
        } else {
            raw.to_owned()
        };
        let icon = icon_name_for_file(raw, kind);
        raw_entries.push((raw.to_owned(), is_dir));
        items.push(GridItem { raw: raw.to_owned(), label, icon, is_dir, git: None, w_px: 0.0 });
    }
    let git_statuses = cached_ls_git_statuses(block, &raw_entries);
    let has_any_badge = git_statuses
        .as_ref()
        .is_some_and(|v| v.iter().any(|st| git_status_style(*st, false).2.is_some()));
    let badge_pad = if has_any_badge { m.cw * 1.45 } else { 0.0 };
    for (idx, item) in items.iter_mut().enumerate() {
        item.git = git_statuses.as_ref().and_then(|v| v.get(idx).copied());
        let label_chars = item.label.chars().count().min(32);
        item.w_px = icon_s + icon_gap + label_chars as f32 * m.cw + badge_pad;
    }
    // In default `ls` grids inside a git repo, place active non-ignored entries first and group
    // `.gitignore`d build artifacts (`target/`, `node_modules/`, etc.) cleanly at the end.
    if git_statuses.is_some() && !block.source.contains("sort-by") {
        items.sort_by_key(|it| it.git == Some(GitFileStatus::Ignored));
    }

    // Pick the largest column count `ncols` (1..=6) whose column-major widths fit in `width`
    // (exact `eza` optimal grid packing!).
    let max_cols = visible_scan.min(6);
    let mut best_cols = 1usize;
    let mut best_col_widths = vec![width];
    for nc in (1..=max_cols).rev() {
        let nrows = visible_scan.div_ceil(nc);
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

    // Enforce <= 1/3 pane height in folded mode so narrow panes don't expand to 30+ rows.
    let grid_fold_items = (best_cols * max_grid_rows).max(4);
    let visible = if block.folded && total > grid_fold_items {
        items.truncate(grid_fold_items);
        grid_fold_items
    } else {
        visible_scan
    };

    let nrows = visible.div_ceil(best_cols);
    let mut cur_y = y0;

    for r in 0..nrows {
        let ty = cur_y + 1.0;
        let iy = ty + (m.ch - icon_s) * 0.5;
        let mut cx = x0;
        for c in 0..best_cols {
            let idx = c * nrows + r;
            let cw = best_col_widths[c];
            if let Some(item) = items.get(idx) {
                let max_ch = ((cw - icon_s - icon_gap - badge_pad).max(m.cw * 3.0) / m.cw).floor() as usize;
                let full_p = full_path_for_entry(block.state_after.cwd.as_ref(), &block.source, &item.raw);
                let entity = crate::term_server::lookup_global_path_entity(std::path::Path::new(&full_p));
                let eff_label = entity
                    .as_ref()
                    .and_then(|e| e.display_name.as_deref())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&item.label);
                let txt = truncate_chars(eff_label, max_ch.max(3));
                let txt_w = txt.chars().count() as f32 * m.cw;
                let (icon_alpha, mut col, badge, tracked_dot) = match item.git {
                    Some(st) => git_status_style(st, item.is_dir),
                    None => (1.0, if item.is_dir { theme::INK_1 } else { theme::INK_2 }, None, false),
                };
                if let Some(c) = entity.as_ref().and_then(|e| e.color) {
                    col = rgba_u8_to_f32(c);
                }
                let drawn_icon_w = if let Some(sym) = entity.as_ref().and_then(|e| e.symbol.as_deref()) {
                    draw_symbol_or_emoji(scene, cx, iy, icon_s, sym, Some(col), icon_alpha, clip_id)
                } else {
                    scene.svg_icon_alpha(cx, iy, icon_s, item.icon, icon_alpha, clip_id);
                    icon_s
                };
                draw_shaded_filename(
                    scene,
                    cx + drawn_icon_w + icon_gap,
                    ty,
                    &txt,
                    item.is_dir,
                    col,
                    m,
                    clip_id,
                );
                let mut end_x = cx + drawn_icon_w + icon_gap + txt_w;
                if let Some((tag, tag_col)) = badge {
                    let bx = end_x + (m.cw * 0.45).max(3.5);
                    scene.mono_small(bx, ty + 1.0, tag, tag_col, true, false, clip_id);
                    end_x = bx + m.cw;
                } else if tracked_dot {
                    let dx = end_x + (m.cw * 0.55).max(4.5);
                    scene.over_circle(
                        dx,
                        ty + m.ch * 0.52,
                        1.9,
                        theme::rgba(0x3f, 0xb9, 0x50, 0.52),
                        clip_id,
                    );
                    end_x = dx + 4.0;
                }
                let item_rect = HitRect::new(cx - 3.0, cur_y, end_x + 4.0, cur_y + row_h);
                scene.push_drag_source_clipped(
                    item_rect,
                    clip_id,
                    DragPayload {
                        label: item.label.clone(),
                        insert_text: drag_path_for_entry(&block.source, &item.raw),
                        icon: Some(item.icon),
                    },
                );
                scene.push_hover_clipped(
                    item_rect,
                    clip_id,
                    HoverKind::FileEntry {
                        full_path: full_p,
                        name: item.label.clone(),
                        icon: Some(item.icon),
                        is_dir: item.is_dir,
                    },
                );
            }
            cx += cw + col_gap;
        }
        cur_y += row_h;
    }

    if total > grid_fold_items {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!("+ {} more entries", total - grid_fold_items)
        } else {
            format!("− collapse to {grid_fold_items} entries")
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

    let total_rows = table.nrows();
    let row_h_est = m.ch + 7.0;
    let pane_h = scene
        .clips
        .get(clip_id as usize)
        .map(|c| (c.rect[3] - c.rect[1]).max(240.0))
        .unwrap_or(720.0);
    // Keep default folded tables compact: never more than ~1/3 of the pane height (4..=TABLE_FOLD_ROWS).
    let fold_rows = ((pane_h / 3.0 - (m.ch + 28.0)) / row_h_est)
        .floor()
        .clamp(4.0, TABLE_FOLD_ROWS as f32) as usize;
    let is_scrollable = depth <= 1 && block.folded && total_rows > fold_rows;
    let visible_rows = if is_scrollable { fold_rows } else { total_rows };
    let type_col_idx = table.col_index("type");
    let sample_rows = total_rows.min(256);

    // Pre-compute column displays & non-blocking `blaze-dust` folder sizes BEFORE sorting so
    // clicking `size ↓` / `size ↑` sorts folders by their real progressive disk usage (`43.9 MiB`)
    // instead of the raw directory inode `st_size` (`96 B`)!
    let mut col_displays: Vec<ColDisplay> = Vec::with_capacity(ncols);
    for c in 0..ncols {
        col_displays.push(effective_col_display(table, c, sample_rows));
    }
    let file_entry_col = col_displays
        .iter()
        .position(|&d| matches!(d, ColDisplay::FileEntry | ColDisplay::TreeEntry));
    let tree_prefix_col = table.col_index("tree_prefix");
    let tree_path_col = table.col_index("path");
    let size_col_idx = col_displays.iter().position(|&d| d == ColDisplay::FileSize);
    let time_col_idx = col_displays.iter().position(|&d| d == ColDisplay::HumanTime);
    let mut all_row_dust: Vec<Option<blaze_dust::DustProgress>> = vec![None; total_rows];
    let mut dust_max_bytes: u64 = 0;
    if (size_col_idx.is_some() || time_col_idx.is_some())
        && let Some(fc) = file_entry_col
    {
        // Always scan up to 512 rows when sorting by size or modified so folder order is exact!
        let dust_scan_rows = if block
            .table_sort
            .is_some_and(|(sc, _)| Some(sc) == size_col_idx || Some(sc) == time_col_idx)
        {
            total_rows.min(512)
        } else {
            total_rows.min(64)
        };
        for r in 0..dust_scan_rows {
            let fk = type_col_idx.and_then(|ti| table.cell(r, ti)).and_then(Value::as_str);
            let raw = tree_path_col
                .and_then(|pc| table.cell(r, pc))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    table
                        .cell(r, fc)
                        .map(|c| format_value_cell_at(c, Some(now_ns)))
                        .unwrap_or_default()
                });
            let is_dir = fk.is_some_and(blaze_sniff::is_dir_label) || raw.ends_with('/');
            if is_dir && !raw.is_empty() {
                let full_p = full_path_for_entry(block.state_after.cwd.as_ref(), &block.source, &raw);
                if let Some(prog) = crate::dust_driver::request_dir_size(std::path::Path::new(&full_p))
                {
                    if prog.bytes > dust_max_bytes {
                        dust_max_bytes = prog.bytes;
                    }
                    all_row_dust[r] = Some(prog);
                }
            }
        }
    }

    // Non-mutating row permutation when the user clicks a column header to sort!
    let mut row_order: Vec<usize> = (0..total_rows).collect();
    if depth <= 1
        && let Some((sort_col, desc)) = block.table_sort
        && sort_col < ncols
    {
        let sort_is_file_size = col_displays.get(sort_col) == Some(&ColDisplay::FileSize);
        let sort_is_human_time = col_displays.get(sort_col) == Some(&ColDisplay::HumanTime);
        row_order.sort_by(|&a, &b| {
            let ord = if sort_is_file_size {
                let eff_bytes = |row_idx: usize| -> i64 {
                    if let Some(prog) = all_row_dust.get(row_idx).copied().flatten() {
                        return prog.bytes.min(i64::MAX as u64) as i64;
                    }
                    match table.cell(row_idx, sort_col) {
                        Some(Value::Size(Bytes(n))) => *n,
                        Some(Value::Int(n)) => *n,
                        _ => -1,
                    }
                };
                eff_bytes(a).cmp(&eff_bytes(b))
            } else if sort_is_human_time {
                let eff_ns = |row_idx: usize| -> i64 {
                    let base = match table.cell(row_idx, sort_col) {
                        Some(Value::Date(dt)) => dt.utc_ns,
                        _ => i64::MIN,
                    };
                    if let Some(prog) = all_row_dust.get(row_idx).copied().flatten()
                        && prog.max_mtime_ns > base
                    {
                        prog.max_mtime_ns
                    } else {
                        base
                    }
                };
                eff_ns(a).cmp(&eff_ns(b))
            } else {
                let va = table.cell(a, sort_col);
                let vb = table.cell(b, sort_col);
                match (va, vb) {
                    (Some(x), Some(y)) => x.compare(y).unwrap_or_else(|| {
                        format_value_cell_at(x, Some(now_ns))
                            .cmp(&format_value_cell_at(y, Some(now_ns)))
                    }),
                    (Some(_), None) => core::cmp::Ordering::Greater,
                    (None, Some(_)) => core::cmp::Ordering::Less,
                    (None, None) => core::cmp::Ordering::Equal,
                }
            };
            if desc { ord.reverse() } else { ord }
        });
    }

    let mut hidden_col_names: Vec<&str> = (0..ncols)
        .filter(|&c| table.is_col_hidden(c))
        .filter_map(|c| table.columns.get(c).map(|s| s.as_ref()))
        .collect();
    let mut has_hidden_toggle = depth <= 1 && !hidden_col_names.is_empty();
    let mut col_visible: Vec<bool> = (0..ncols)
        .map(|c| block.show_hidden_cols || !table.is_col_hidden(c))
        .collect();

    let icon_s = m.icon_md();
    let icon_pad = icon_s + (m.cw * 0.85).round().max(6.0);

    let idx_chars = total_rows.max(1).to_string().len().max(2);
    let idx_w = idx_chars as f32 * m.cw + 16.0;
    let col_gap = 22.0;
    let scrollbar_reserve = if is_scrollable { 12.0 } else { 0.0 };

    let mut col_chars: Vec<usize> = Vec::with_capacity(ncols);
    let mut col_right_align: Vec<bool> = Vec::with_capacity(ncols);
    let mut col_extra_px: Vec<f32> = Vec::with_capacity(ncols);
    let mut col_max_num: Vec<f64> = Vec::with_capacity(ncols);

    for c in 0..ncols {
        let disp = col_displays[c];

        let hdr = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
        let is_icon_only_bool = hdr == "readonly"
            || (disp == ColDisplay::Status
                && (0..sample_rows)
                    .filter_map(|r| table.cell(r, c))
                    .all(|v| matches!(v, Value::Bool(_) | Value::Nothing)));
        let mut max_c = if is_icon_only_bool {
            2
        } else {
            (hdr.chars().count() + 2).max(3)
        };
        let mut numeric_votes = 0usize;
        let mut non_empty = 0usize;
        let mut max_num = 0.0f64;

        for r in 0..sample_rows {
            if let Some(cell) = table.cell(r, c) {
                let row_is_dir = type_col_idx
                    .and_then(|ti| table.cell(r, ti))
                    .and_then(Value::as_str)
                    .is_some_and(blaze_sniff::is_dir_label);
                if !is_icon_only_bool {
                    let mut txt = if disp == ColDisplay::FileSize
                        && row_is_dir
                        && let Some(prog) = all_row_dust.get(r).copied().flatten()
                    {
                        if !prog.done && prog.bytes == 0 {
                            "…".to_owned()
                        } else {
                            let base = crate::dust_driver::format_bytes_compact(prog.bytes);
                            if !prog.done { format!("~{base}") } else { base }
                        }
                    } else if disp == ColDisplay::HumanTime
                        && row_is_dir
                        && let Value::Date(dt) = cell
                        && let Some(prog) = all_row_dust.get(r).copied().flatten()
                        && prog.max_mtime_ns > dt.utc_ns
                    {
                        format_value_cell_at(
                            &Value::Date(blaze_sh::DateTime {
                                utc_ns: prog.max_mtime_ns,
                                offset_secs: dt.offset_secs,
                            }),
                            Some(now_ns),
                        )
                    } else {
                        format_value_cell_at(cell, Some(now_ns))
                    };
                    if matches!(disp, ColDisplay::FileEntry | ColDisplay::TreeEntry)
                        && row_is_dir
                        && !txt.ends_with('/')
                    {
                        txt.push('/');
                    }
                    let extra_prefix_c = if disp == ColDisplay::TreeEntry {
                        tree_prefix_col
                            .and_then(|pc| table.cell(r, pc))
                            .and_then(Value::as_str)
                            .map_or(0, |s| {
                                let (segs, _, _) = parse_tree_prefix(s);
                                if segs == 0 {
                                    0
                                } else {
                                    ((segs as f32 * DATAFRAME_TREE_STEP + 4.0) / m.cw.max(1.0))
                                        .ceil() as usize
                                }
                            })
                    } else {
                        0
                    };
                    max_c = max_c.max(txt.chars().count() + extra_prefix_c);
                }
                if !matches!(cell, Value::Nothing) {
                    non_empty += 1;
                    if theme::is_right_aligned(cell) {
                        numeric_votes += 1;
                    }
                    match cell {
                        Value::Size(Bytes(b)) if !row_is_dir => {
                            max_num = max_num.max((*b).max(0) as f64)
                        }
                        Value::Float(f) if disp == ColDisplay::Percent => {
                            max_num = max_num.max(f.max(0.0))
                        }
                        _ => {}
                    }
                }
            }
        }
        let has_dust_dirs = all_row_dust.iter().any(Option::is_some);
        // Reserve full IEC size + bar width up front (including while `blaze-dust` is scanning)
        // so `20.0 KiB` / `~999.9 MiB` never clips and columns never jump when scans complete.
        if disp == ColDisplay::FileSize && (max_num > 0.0 || dust_max_bytes > 0 || has_dust_dirs) {
            max_c = max_c.max(10);
        }
        let col_cap = if matches!(hdr, "code" | "message" | "subject" | "summary") {
            110
        } else if disp == ColDisplay::TreeEntry {
            64
        } else {
            40
        };
        col_chars.push(max_c.min(col_cap));
        col_right_align.push(
            matches!(disp, ColDisplay::FileSize | ColDisplay::Percent | ColDisplay::HumanTime)
                || (!is_icon_only_bool && non_empty > 0 && numeric_votes * 2 >= non_empty),
        );
        let extra = if is_icon_only_bool {
            10.0
        } else {
            match disp {
                ColDisplay::FileEntry
                | ColDisplay::TreeEntry
                | ColDisplay::Crate
                | ColDisplay::Process
                | ColDisplay::GitAuthor => icon_pad + 10.0,
                ColDisplay::Status => 14.0,
                ColDisplay::FileSize if max_num > 0.0 || dust_max_bytes > 0 || has_dust_dirs => {
                    SIZE_BAR_W + 10.0
                }
                ColDisplay::Percent if max_num > 0.0 => SIZE_BAR_W + 10.0,
                _ => 0.0,
            }
        };
        col_extra_px.push(extra);
        col_max_num.push(max_num);
    }

    // Priority-based responsive column widths: hidden columns take 0px; user-dragged overrides win;
    // otherwise preserve intrinsic widths for short fixed columns (`size`, `modified`) & flex paths!
    let mut col_widths: Vec<f32> = (0..ncols)
        .map(|c| {
            if !col_visible[c] {
                0.0
            } else if let Some(&(_, custom_w)) =
                block.col_width_overrides.iter().find(|&&(oc, _)| oc == c)
            {
                custom_w
            } else {
                col_chars[c] as f32 * m.cw + col_extra_px[c] + col_gap
            }
        })
        .collect();

    // Auto-compress overflow columns on wide tables by default (collapsible via the `+N cols` pill):
    // keep the primary columns readable (at most 5 default columns, or fewer if wider than the pane),
    // never auto-hiding the primary file/tree column or the active sort column.
    if depth <= 1 {
        const DEFAULT_MAX_VISIBLE_COLS: usize = 5;
        let sort_col = block.table_sort.map(|(sc, _)| sc);
        let natural_visible: Vec<usize> = (0..ncols).filter(|&c| !table.is_col_hidden(c)).collect();
        let mut kept = 0usize;
        let mut running_w = 0.0f32;
        let toggle_budget = (width - idx_w - scrollbar_reserve - 58.0).max(80.0);
        let mut auto_hidden: Vec<usize> = Vec::new();
        for &c in &natural_visible {
            let cw = if let Some(&(_, custom_w)) =
                block.col_width_overrides.iter().find(|&&(oc, _)| oc == c)
            {
                custom_w
            } else {
                col_chars[c] as f32 * m.cw + col_extra_px[c] + col_gap
            };
            let must_keep = kept < 3 || Some(c) == file_entry_col || Some(c) == sort_col;
            let fits_budget = kept < DEFAULT_MAX_VISIBLE_COLS && running_w + cw <= toggle_budget + 40.0;
            if must_keep || fits_budget {
                kept += 1;
                running_w += cw;
            } else {
                auto_hidden.push(c);
            }
        }
        if !auto_hidden.is_empty() {
            has_hidden_toggle = true;
            for &c in &auto_hidden {
                if let Some(name) = table.columns.get(c).map(|s| s.as_ref())
                    && !hidden_col_names.contains(&name)
                {
                    hidden_col_names.push(name);
                }
                if !block.show_hidden_cols {
                    col_visible[c] = false;
                    col_widths[c] = 0.0;
                }
            }
        }
    }
    let hidden_toggle_reserve = if has_hidden_toggle { 58.0 } else { 0.0 };
    let avail_w = (width - idx_w - scrollbar_reserve - hidden_toggle_reserve).max(80.0);
    let total_w: f32 = col_widths.iter().sum();
    if total_w > avail_w && total_w > 0.0 {
        let is_flex: Vec<bool> = (0..ncols)
            .map(|c| {
                if !col_visible[c] {
                    return false;
                }
                let col_name = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
                col_name != "readonly"
                    && !matches!(
                        col_displays[c],
                        ColDisplay::FileSize
                            | ColDisplay::HumanTime
                            | ColDisplay::Percent
                            | ColDisplay::Status
                    )
            })
            .collect();
        let fixed_sum: f32 = (0..ncols)
            .filter(|&c| col_visible[c] && !is_flex[c])
            .map(|c| col_widths[c])
            .sum();
        let flex_sum: f32 = total_w - fixed_sum;
        let flex_budget = avail_w - fixed_sum;
        if flex_sum > 0.0 && flex_budget >= 110.0 {
            let fscale = flex_budget / flex_sum;
            for c in 0..ncols {
                if is_flex[c] {
                    col_widths[c] = (col_widths[c] * fscale).max(96.0);
                }
            }
        } else {
            // Very narrow pane: compact status column to single-letter badge first before shrinking sizes/times!
            for c in 0..ncols {
                if col_visible[c]
                    && col_displays[c] == ColDisplay::Status
                    && table.columns.get(c).map(|s| s.as_ref()) != Some("readonly")
                {
                    col_widths[c] = (m.cw * 3.2 + col_gap).min(col_widths[c]);
                }
            }
            let new_total: f32 = col_widths.iter().sum();
            if new_total > avail_w && new_total > 0.0 {
                let fsum2: f32 = (0..ncols).filter(|&c| is_flex[c]).map(|c| col_widths[c]).sum();
                let fix2 = new_total - fsum2;
                if fsum2 > 0.0 && avail_w - fix2 >= 90.0 {
                    let fs = (avail_w - fix2) / fsum2;
                    for c in 0..ncols {
                        if is_flex[c] {
                            col_widths[c] = (col_widths[c] * fs).max(86.0);
                        }
                    }
                } else {
                    let scale = avail_w / new_total;
                    for c in 0..ncols {
                        if col_visible[c] {
                            col_widths[c] = (col_widths[c] * scale).max(38.0);
                        }
                    }
                }
            }
        }
    }
    // Progressive git last-commit column for `ls -l` / `ll` inside any git repository
    // (mirrors GitHub's repo tree view: circular GitHub avatar + short SHA + commit subject!).
    let has_status_col = table.col_index("status").is_some();
    let ls_dir_commits = if depth <= 1
        && !has_status_col
        && file_entry_col.is_some()
        && table.col_index("commit").is_none()
    {
        let dir = resolve_ls_target_dir(block.state_after.cwd.as_ref(), &block.source);
        crate::git_meta_driver::request_dir_commits(&dir)
            .filter(|snap| !snap.entries.is_empty())
    } else {
        None
    };
    let base_cols_w: f32 = col_widths.iter().sum();
    let ls_commit_col_w: f32 = if ls_dir_commits.is_some() {
        if let Some(&(_, custom_w)) = block.col_width_overrides.iter().find(|&&(oc, _)| oc == ncols) {
            custom_w
        } else {
            let remaining = avail_w - base_cols_w;
            if remaining >= 140.0 {
                remaining.clamp(160.0, 380.0)
            } else {
                0.0
            }
        }
    } else {
        0.0
    };
    let show_ls_commit_col = ls_dir_commits.is_some() && ls_commit_col_w >= 80.0;
    let table_w = (idx_w + base_cols_w + ls_commit_col_w + hidden_toggle_reserve - col_gap * 0.4)
        .min(width - scrollbar_reserve);
    let x1 = x0 + table_w;

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;

    // Clickable sortable column headers in crisp compact monospace (`MonoSmall`) + hairline underneath.
    scene.mono_small(x0, y0, "#", theme::INK_4, false, false, clip_id);
    let mut cx = x0 + idx_w;
    for c in 0..ncols {
        if !col_visible[c] {
            continue;
        }
        let cw = col_widths[c];
        let content_w = (cw - col_gap).max(m.cw * 2.0);
        let col_raw = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
        let active_sort = if depth <= 1 {
            block.table_sort.and_then(|(sc, desc)| if sc == c { Some(desc) } else { None })
        } else {
            None
        };
        let suffix = match active_sort {
            Some(true) => " ↓",
            Some(false) => " ↑",
            None => "",
        };
        let hdr_col = if active_sort.is_some() { theme::INK_1 } else { theme::INK_3 };
        if col_raw == "readonly" {
            let ls = m.icon_sm();
            draw_icon(scene, Icon::Lock, cx + 2.0, y0 + (m.ch - ls) * 0.5, ls, hdr_col, clip_id);
            if !suffix.is_empty() {
                scene.mono_small(cx + ls + 4.0, y0, suffix.trim(), hdr_col, true, false, clip_id);
            }
        } else {
            let avg_cw = m.measure("n", TextFace::MonoSmall).max(4.5);
            let max_ch = (content_w / avg_cw).floor().max(2.0) as usize;
            let hdr_base = truncate_chars(col_raw, max_ch.saturating_sub(suffix.chars().count()).max(1));
            let hdr = format!("{hdr_base}{suffix}");
            let hdr_w = m.measure(&hdr, TextFace::MonoSmall);
            let tx = if col_right_align[c] {
                cx + content_w - hdr_w
            } else {
                cx
            };
            scene.mono_small(tx, y0, hdr, hdr_col, active_sort.is_some(), false, clip_id);
        }
        if depth <= 1 {
            let resize_x = cx + cw - col_gap * 0.5;
            let hdr_hit = HitRect::new(cx - 4.0, y0 - 2.0, (resize_x - 5.0).max(cx + 8.0), y0 + m.ch + 4.0);
            scene.push_hit_clipped(
                hdr_hit,
                clip_id,
                HitAction::SortTableColumn { block: block.id, col: c },
            );
            scene.push_hover_clipped(
                hdr_hit,
                clip_id,
                HoverKind::Tooltip {
                    title: format!("Sort by `{col_raw}`"),
                    subtitle: Some(match active_sort {
                        Some(true) => "currently descending · click for ascending".to_owned(),
                        Some(false) => "currently ascending · click for descending".to_owned(),
                        None => "click to sort · drag right edge to resize".to_owned(),
                    }),
                },
            );
            // Subtle vertical column resize grip + 10px EW-resize drag hit handle!
            scene.line(resize_x, y0 + 2.0, resize_x, y0 + m.ch - 1.0, 1.0, theme::LINE_FAINT, clip_id);
            let resize_hit = HitRect::new(resize_x - 5.0, y0 - 2.0, resize_x + 5.0, y0 + m.ch + 4.0);
            scene.push_hit_clipped(
                resize_hit,
                clip_id,
                HitAction::ResizeTableColumn {
                    block: block.id,
                    col: c,
                    col_x0: cx.round() as i32,
                },
            );
        }
        cx += cw;
    }

    if show_ls_commit_col {
        scene.mono_small(cx, y0, "commit", theme::INK_3, false, false, clip_id);
        let resize_x = cx + ls_commit_col_w - col_gap * 0.5;
        scene.line(resize_x, y0 + 2.0, resize_x, y0 + m.ch - 1.0, 1.0, theme::LINE_FAINT, clip_id);
        scene.push_hit_clipped(
            HitRect::new(resize_x - 5.0, y0 - 2.0, resize_x + 5.0, y0 + m.ch + 4.0),
            clip_id,
            HitAction::ResizeTableColumn {
                block: block.id,
                col: ncols,
                col_x0: cx.round() as i32,
            },
        );
        cx += ls_commit_col_w;
    }

    // Sleek clickable pill to reveal or re-hide default-hidden columns (e.g. `+2 cols` -> `type, readonly`).
    if has_hidden_toggle {
        let n_hid = hidden_col_names.len();
        let pill_lbl = if block.show_hidden_cols {
            if n_hid == 1 {
                format!("−{}", hidden_col_names[0])
            } else {
                format!("−{n_hid} cols")
            }
        } else if n_hid == 1 {
            format!("+{}", hidden_col_names[0])
        } else {
            format!("+{n_hid} cols")
        };
        let pw = m.measure(&pill_lbl, TextFace::MonoSmall) + 12.0;
        let px0 = (x1 - pw).max(cx - 8.0);
        let py0 = y0 - 1.0;
        let py1 = y0 + m.ch + 1.0;
        scene.round_rect(
            px0,
            py0,
            px0 + pw,
            py1,
            4.0,
            theme::BG_CHIP,
            1.0,
            theme::LINE_FAINT,
            clip_id,
        );
        scene.mono_small(
            px0 + 6.0,
            y0,
            pill_lbl,
            if block.show_hidden_cols { theme::INK_2 } else { theme::INK_4 },
            false,
            false,
            clip_id,
        );
        let pill_rect = HitRect::new(px0, py0 - 2.0, px0 + pw, py1 + 2.0);
        scene.push_hit_clipped(pill_rect, clip_id, HitAction::ToggleHiddenCols(block.id));
        scene.push_hover_clipped(
            pill_rect,
            clip_id,
            HoverKind::Tooltip {
                title: if block.show_hidden_cols {
                    format!("hide default-hidden: {}", hidden_col_names.join(", "))
                } else {
                    format!("show hidden columns: {}", hidden_col_names.join(", "))
                },
                subtitle: None,
            },
        );
    }

    let hdr_line_y = y0 + m.ch + 6.0;
    scene.line(x0, hdr_line_y, x1, hdr_line_y, 1.0, theme::LINE, clip_id);

    let rows_top_y = hdr_line_y + 5.0;
    let mut cur_y = rows_top_y;

    // Snap scroll offset cleanly to exact integer row grid boundaries (never half-clipped rows!).
    let max_first_row = total_rows.saturating_sub(visible_rows);
    let max_scroll_y = max_first_row as f32 * row_h;
    let first_row = if is_scrollable && max_first_row > 0 {
        (block.table_scroll_y / row_h).round().clamp(0.0, max_first_row as f32) as usize
    } else {
        0
    };

    // Pre-compute cached per-row git statuses for `ls -l` / file tables (skipping `git status` tables
    // that already display an explicit `status` column).
    let email_col_idx = table.col_index("email");
    let sha_col_idx = table.col_index("sha");
    let summary_col_idx = table.col_index("summary");
    let table_gh_repo = if col_displays.iter().any(|&d| d == ColDisplay::GitAuthor) {
        crate::git_meta_driver::github_repo_for_dir(std::path::Path::new(&block.prompt_snapshot.cwd))
    } else {
        None
    };
    let table_git_statuses: Option<Vec<GitFileStatus>> = if !has_status_col
        && let Some(fc) = file_entry_col
    {
        let mut raw_entries = Vec::with_capacity(total_rows);
        for r in 0..total_rows {
            let raw = table
                .cell(r, fc)
                .map(|c| format_value_cell_at(c, Some(now_ns)))
                .unwrap_or_default();
            let fk = type_col_idx.and_then(|ti| table.cell(r, ti)).and_then(Value::as_str);
            let is_dir = fk.is_some_and(blaze_sniff::is_dir_label) || raw.ends_with('/');
            raw_entries.push((raw, is_dir));
        }
        cached_ls_git_statuses(block, &raw_entries)
    } else {
        None
    };

    // Ensure scrolled-into-view directories beyond the initial 64-row window also trigger `blaze-dust`.
    if size_col_idx.is_some()
        && let Some(fc) = file_entry_col
    {
        for vis_i in 0..visible_rows {
            let r = row_order[first_row + vis_i];
            if all_row_dust[r].is_some() {
                continue;
            }
            let fk = type_col_idx.and_then(|ti| table.cell(r, ti)).and_then(Value::as_str);
            let raw = table
                .cell(r, fc)
                .map(|c| format_value_cell_at(c, Some(now_ns)))
                .unwrap_or_default();
            let is_dir = fk.is_some_and(blaze_sniff::is_dir_label) || raw.ends_with('/');
            if is_dir && !raw.is_empty() {
                let full_p = full_path_for_entry(&block.prompt_snapshot.cwd, &block.source, &raw);
                if let Some(prog) = crate::dust_driver::request_dir_size(std::path::Path::new(&full_p))
                {
                    if prog.bytes > dust_max_bytes {
                        dust_max_bytes = prog.bytes;
                    }
                    all_row_dust[r] = Some(prog);
                }
            }
        }
    }

    if visible_rows == 0 {
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, "0 rows", theme::INK_4, false, true, clip_id);
        cur_y += row_h;
    } else {
        for vis_i in 0..visible_rows {
            let disp_idx = first_row + vis_i;
            let r = row_order[disp_idx];
            let ty = cur_y + TABLE_ROW_PAD_Y;
            let iy = ty + (m.ch - icon_s) * 0.5;
            scene.text(x0, ty, disp_idx.to_string(), theme::INK_4, false, false, clip_id);

            let row_file_kind = type_col_idx
                .and_then(|ti| table.cell(r, ti))
                .and_then(Value::as_str);

            let mut expanded_for_row: Vec<(usize, &Value<'_>)> = Vec::new();
            let mut rx = x0 + idx_w;
            for c in 0..ncols {
                if !col_visible[c] {
                    continue;
                }
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
                            ColDisplay::TreeEntry => {
                                let leaf_name = format_value_cell_at(cell, Some(now_ns));
                                let rel_path = tree_path_col
                                    .and_then(|pc| table.cell(r, pc))
                                    .and_then(Value::as_str)
                                    .unwrap_or(&leaf_name);
                                let prefix = tree_prefix_col
                                    .and_then(|pc| table.cell(r, pc))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let is_dir = row_file_kind.is_some_and(blaze_sniff::is_dir_label)
                                    || leaf_name.ends_with('/');
                                let icon_name = icon_name_for_file(rel_path, row_file_kind);
                                let display_name = if is_dir && !leaf_name.ends_with('/') {
                                    format!("{leaf_name}/")
                                } else {
                                    leaf_name.clone()
                                };
                                let has_visible_child = is_dir && vis_i + 1 < visible_rows && {
                                    let (cur_segs, _, _) = parse_tree_prefix(prefix);
                                    let next_r = row_order[disp_idx + 1];
                                    let next_prefix = tree_prefix_col
                                        .and_then(|pc| table.cell(next_r, pc))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    let (next_segs, next_mask, next_elbow) =
                                        parse_tree_prefix(next_prefix);
                                    (next_elbow.is_some() && next_segs == cur_segs + 1)
                                        || (cur_segs < 64 && ((next_mask >> cur_segs) & 1) != 0)
                                };
                                let pref_w = draw_dataframe_tree_guides(
                                    scene,
                                    rx,
                                    cur_y,
                                    row_h,
                                    icon_s,
                                    prefix,
                                    has_visible_child,
                                    clip_id,
                                );
                                let item_x = rx + pref_w;
                                let max_ch = ((content_w - pref_w - icon_pad - 6.0) / m.cw)
                                    .floor()
                                    .max(4.0) as usize;
                                let txt = truncate_chars(&display_name, max_ch);
                                let txt_w = txt.chars().count() as f32 * m.cw;
                                let mut name_col = if is_dir { theme::INK_1 } else { theme::INK_2 };
                                if let Some(mc) = table.col_index("modified")
                                    && let Some(Value::Date(dt)) = table.cell(r, mc)
                                {
                                    let dust_ns = if is_dir {
                                        all_row_dust.get(r).copied().flatten().map(|p| p.max_mtime_ns).unwrap_or(0)
                                    } else {
                                        0
                                    };
                                    let eff_ns = dt.utc_ns.max(dust_ns);
                                    let age_secs = (now_ns.saturating_sub(eff_ns)).max(0) as u64 / 1_000_000_000;
                                    if age_secs < 7_200 {
                                        let heat = theme::recency_heat_color(age_secs);
                                        let blend = if age_secs < 600 { 0.42 } else { 0.28 };
                                        name_col = [
                                            name_col[0] + (heat[0] - name_col[0]) * blend,
                                            name_col[1] + (heat[1] - name_col[1]) * blend,
                                            name_col[2] + (heat[2] - name_col[2]) * blend,
                                            name_col[3],
                                        ];
                                    }
                                }
                                let tree_full_path = full_path_for_entry(
                                    block.state_after.cwd.as_ref(),
                                    &block.source,
                                    rel_path,
                                );
                                let tree_entity = crate::term_server::lookup_global_path_entity(
                                    std::path::Path::new(&tree_full_path),
                                );
                                if let Some(c) = tree_entity.as_ref().and_then(|e| e.color) {
                                    name_col = rgba_u8_to_f32(c);
                                }
                                let eff_tree_txt = tree_entity
                                    .as_ref()
                                    .and_then(|e| e.display_name.as_deref())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(&txt);
                                let drawn_icon_pad = if let Some(sym) =
                                    tree_entity.as_ref().and_then(|e| e.symbol.as_deref())
                                {
                                    draw_symbol_or_emoji(
                                        scene,
                                        item_x,
                                        iy,
                                        icon_s,
                                        sym,
                                        Some(name_col),
                                        1.0,
                                        clip_id,
                                    ) + 6.0
                                } else {
                                    scene.svg_icon(item_x, iy, icon_s, icon_name, clip_id);
                                    icon_pad
                                };
                                draw_shaded_filename(
                                    scene,
                                    item_x + drawn_icon_pad,
                                    ty,
                                    eff_tree_txt,
                                    is_dir,
                                    name_col,
                                    m,
                                    clip_id,
                                );
                                let cell_rect = HitRect::new(
                                    item_x - 3.0,
                                    cur_y,
                                    item_x + drawn_icon_pad + txt_w + 6.0,
                                    cur_y + row_h,
                                );
                                scene.push_drag_source_clipped(
                                    cell_rect,
                                    clip_id,
                                    DragPayload {
                                        label: display_name.clone(),
                                        insert_text: drag_path_for_entry(&block.source, rel_path),
                                        icon: Some(icon_name),
                                    },
                                );
                                scene.push_hover_clipped(
                                    cell_rect,
                                    clip_id,
                                    HoverKind::FileEntry {
                                        full_path: tree_full_path,
                                        name: display_name,
                                        icon: Some(icon_name),
                                        is_dir,
                                    },
                                );
                            }
                            ColDisplay::FileEntry => {
                                let raw_name = format_value_cell_at(cell, Some(now_ns));
                                let icon_name = icon_name_for_file(&raw_name, row_file_kind);
                                let is_dir = row_file_kind.is_some_and(blaze_sniff::is_dir_label)
                                    || raw_name.ends_with('/');
                                let display_name = if is_dir && !raw_name.ends_with('/') {
                                    format!("{raw_name}/")
                                } else {
                                    raw_name.clone()
                                };
                                let row_git = table_git_statuses.as_ref().and_then(|v| v.get(r).copied());
                                let (icon_alpha, mut name_col, badge, tracked_dot) = match row_git {
                                    Some(st) => git_status_style(st, is_dir),
                                    None => (1.0, theme::INK_1, None, false),
                                };
                                // Subtle warm recency glow on freshly modified files/folders (< 2h)
                                // when not already colored by git status (`M`, `A`, `?`, ignored).
                                let is_neutral_git =
                                    matches!(row_git, None | Some(GitFileStatus::Tracked));
                                if is_neutral_git
                                    && let Some(mc) = table.col_index("modified")
                                    && let Some(Value::Date(dt)) = table.cell(r, mc)
                                {
                                    let dust_ns = if is_dir {
                                        all_row_dust
                                            .get(r)
                                            .copied()
                                            .flatten()
                                            .map(|p| p.max_mtime_ns)
                                            .unwrap_or(0)
                                    } else {
                                        0
                                    };
                                    let eff_ns = dt.utc_ns.max(dust_ns);
                                    let age_secs =
                                        (now_ns.saturating_sub(eff_ns)).max(0) as u64 / 1_000_000_000;
                                    if age_secs < 7_200 {
                                        let heat = theme::recency_heat_color(age_secs);
                                        // Gentle 32% blend so names stay crisp while recent items glow!
                                        let blend = if age_secs < 600 { 0.42 } else { 0.28 };
                                        name_col = [
                                            name_col[0] + (heat[0] - name_col[0]) * blend,
                                            name_col[1] + (heat[1] - name_col[1]) * blend,
                                            name_col[2] + (heat[2] - name_col[2]) * blend,
                                            name_col[3],
                                        ];
                                    }
                                }
                                let entry_full_path = full_path_for_entry(
                                    &block.prompt_snapshot.cwd,
                                    &block.source,
                                    &raw_name,
                                );
                                let entry_entity = crate::term_server::lookup_global_path_entity(
                                    std::path::Path::new(&entry_full_path),
                                );
                                if let Some(c) = entry_entity.as_ref().and_then(|e| e.color) {
                                    name_col = rgba_u8_to_f32(c);
                                }
                                let shown_name = entry_entity
                                    .as_ref()
                                    .and_then(|e| e.display_name.as_deref())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or(&display_name);
                                let git_pad = if badge.is_some() { m.cw * 1.8 } else { 0.0 };
                                let max_ch = ((content_w - icon_pad - 8.0 - git_pad) / m.cw).floor().max(4.0) as usize;
                                // Middle-truncate nested paths (`crates/…/layout.rs`) so filenames stay visible!
                                let txt = if shown_name.contains('/') {
                                    truncate_middle(shown_name, max_ch)
                                } else {
                                    truncate_chars(shown_name, max_ch)
                                };
                                let txt_w = txt.chars().count() as f32 * m.cw;
                                let drawn_icon_pad = if let Some(sym) =
                                    entry_entity.as_ref().and_then(|e| e.symbol.as_deref())
                                {
                                    symbol_display_width(sym, icon_s) + 6.0
                                } else {
                                    icon_pad
                                };
                                let chip_w = (drawn_icon_pad + txt_w + 10.0).min(content_w + 2.0);
                                let bx0 = rx - 5.0;
                                let by0 = cur_y + 2.0;
                                let by1 = cur_y + row_h - 2.0;

                                // Sleek `ix.dev/blaze` inline chip for folders and `.rs` files.
                                if (is_dir || icon_name == "rust_dark" || entry_entity.is_some())
                                    && row_git != Some(GitFileStatus::Ignored)
                                {
                                    scene.round_rect(bx0, by0, bx0 + chip_w, by1, 4.0, theme::BG_CHIP, 0.0, [0.0; 4], clip_id);
                                }
                                if let Some(sym) = entry_entity.as_ref().and_then(|e| e.symbol.as_deref()) {
                                    draw_symbol_or_emoji(
                                        scene,
                                        rx,
                                        iy,
                                        icon_s,
                                        sym,
                                        Some(name_col),
                                        icon_alpha,
                                        clip_id,
                                    );
                                } else {
                                    scene.svg_icon_alpha(rx, iy, icon_s, icon_name, icon_alpha, clip_id);
                                }
                                draw_shaded_filename(
                                    scene,
                                    rx + drawn_icon_pad,
                                    ty,
                                    &txt,
                                    is_dir,
                                    name_col,
                                    m,
                                    clip_id,
                                );
                                if let Some((tag, tag_col)) = badge {
                                    let gx = (bx0 + chip_w + 6.0).min(rx + content_w - m.cw);
                                    scene.mono_small(gx, ty + 1.0, tag, tag_col, true, false, clip_id);
                                } else if tracked_dot {
                                    let gx = rx + drawn_icon_pad + txt_w + (m.cw * 0.55).max(4.5);
                                    scene.over_circle(
                                        gx,
                                        ty + m.ch * 0.52,
                                        1.9,
                                        theme::rgba(0x3f, 0xb9, 0x50, 0.52),
                                        clip_id,
                                    );
                                }
                                let cell_rect = HitRect::new(bx0, cur_y, bx0 + chip_w, cur_y + row_h);
                                scene.push_drag_source_clipped(
                                    cell_rect,
                                    clip_id,
                                    DragPayload {
                                        label: display_name.clone(),
                                        insert_text: drag_path_for_entry(&block.source, &raw_name),
                                        icon: Some(icon_name),
                                    },
                                );
                                scene.push_hover_clipped(
                                    cell_rect,
                                    clip_id,
                                    HoverKind::FileEntry {
                                        full_path: entry_full_path,
                                        name: display_name,
                                        icon: Some(icon_name),
                                        is_dir,
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
                            ColDisplay::GitAuthor => {
                                let author = format_value_cell_at(cell, Some(now_ns));
                                let email = email_col_idx
                                    .and_then(|ec| table.cell(r, ec))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let full_sha = sha_col_idx
                                    .and_then(|sc| table.cell(r, sc))
                                    .and_then(Value::as_str);
                                let avatar_s = 15.0f32.min(m.ch + 1.0);
                                let av_pad = avatar_s + 7.0;
                                let av = crate::git_meta_driver::request_avatar(
                                    email,
                                    full_sha,
                                    table_gh_repo.as_ref(),
                                );
                                draw_git_avatar(
                                    scene,
                                    rx,
                                    ty + (m.ch - avatar_s) * 0.5,
                                    avatar_s,
                                    &author,
                                    email,
                                    av.image_path.as_deref(),
                                    m,
                                    clip_id,
                                );
                                let max_ch = ((content_w - av_pad) / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&author, max_ch);
                                scene.text(rx + av_pad, ty, txt, theme::INK_2, false, false, clip_id);
                                let title = if let Some(login) = av.github_login.as_deref() {
                                    format!("{author} (@{login})")
                                } else {
                                    author
                                };
                                scene.push_hover_clipped(
                                    HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h),
                                    clip_id,
                                    HoverKind::Tooltip {
                                        title,
                                        subtitle: (!email.is_empty()).then(|| email.to_owned()),
                                    },
                                );
                            }
                            ColDisplay::GitCommit => {
                                let short_sha = format_value_cell_at(cell, Some(now_ns));
                                let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&short_sha, max_ch);
                                scene.text(rx, ty, txt, theme::rgb(0x8a, 0xa8, 0xd4), false, false, clip_id);
                                let cell_rect = HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h);
                                scene.push_hit_clipped(
                                    cell_rect,
                                    clip_id,
                                    HitAction::CopyText(short_sha.clone()),
                                );
                                let summary = summary_col_idx
                                    .and_then(|sc| table.cell(r, sc))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let full_sha = sha_col_idx
                                    .and_then(|sc| table.cell(r, sc))
                                    .and_then(Value::as_str)
                                    .unwrap_or(&short_sha)
                                    .to_owned();
                                scene.push_hover_clipped(
                                    cell_rect,
                                    clip_id,
                                    HoverKind::Tooltip {
                                        title: if summary.is_empty() {
                                            short_sha
                                        } else {
                                            summary.to_owned()
                                        },
                                        subtitle: Some(format!("{full_sha} · click to copy SHA")),
                                    },
                                );
                            }
                            ColDisplay::Status => {
                                let col_name = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
                                if let Value::Bool(flag) = cell {
                                    // Crisp icon-only boolean rendering (zero `○ false` / `● true` noise!).
                                    if col_name == "readonly" {
                                        if *flag {
                                            let ls = m.icon_sm();
                                            draw_icon(scene, Icon::Lock, rx + 2.0, ty + (m.ch - ls) * 0.5, ls, theme::INK_2, clip_id);
                                            scene.push_hover_clipped(
                                                HitRect::new(rx, cur_y, rx + 18.0, cur_y + row_h),
                                                clip_id,
                                                HoverKind::Tooltip {
                                                    title: "read-only".to_owned(),
                                                    subtitle: None,
                                                },
                                            );
                                        } else {
                                            scene.text(rx + 4.0, ty, "·", theme::rgba(0x57, 0x57, 0x5a, 0.45), false, false, clip_id);
                                        }
                                    } else if *flag {
                                        scene.over_circle(rx + 6.0, ty + m.ch * 0.5, 3.0, theme::OK, clip_id);
                                    } else {
                                        scene.text(rx + 4.0, ty, "·", theme::rgba(0x57, 0x57, 0x5a, 0.45), false, false, clip_id);
                                    }
                                } else {
                                    let raw = format_value_cell_at(cell, Some(now_ns));
                                    let is_ok = raw.eq_ignore_ascii_case("running")
                                        || raw.eq_ignore_ascii_case("ok")
                                        || raw.eq_ignore_ascii_case("staged")
                                        || raw.eq_ignore_ascii_case("added")
                                        || raw.eq_ignore_ascii_case("current")
                                        || raw == "R"
                                        || raw == "S";
                                    let is_danger = raw.eq_ignore_ascii_case("conflict")
                                        || raw.eq_ignore_ascii_case("deleted");
                                    let compact_w = content_w < 7.5 * m.cw;
                                    if compact_w {
                                        // Crisp compact single-letter status badge when narrow (`M`, `A`, `D`, `?`, `R`).
                                        let badge = match raw.to_ascii_lowercase().as_str() {
                                            "modified" => "M",
                                            "untracked" => "?",
                                            "staged" | "added" => "A",
                                            "deleted" => "D",
                                            "renamed" => "R",
                                            "conflict" => "!",
                                            _ => raw.get(..1).unwrap_or("·"),
                                        };
                                        let col = if is_ok {
                                            theme::OK
                                        } else if is_danger {
                                            theme::DANGER
                                        } else if badge == "M" {
                                            theme::rgb(0xde, 0xc7, 0x92)
                                        } else {
                                            theme::INK_3
                                        };
                                        scene.round_rect(
                                            rx,
                                            cur_y + 3.0,
                                            rx + 18.0,
                                            cur_y + row_h - 3.0,
                                            4.0,
                                            theme::BG_CHIP,
                                            1.0,
                                            theme::LINE_FAINT,
                                            clip_id,
                                        );
                                        scene.text(rx + 5.0, ty, badge, col, true, false, clip_id);
                                    } else {
                                        let dot_y = ty + m.ch * 0.5;
                                        if is_ok {
                                            scene.over_circle(rx + 4.0, dot_y, 3.0, theme::OK, clip_id);
                                        } else if is_danger {
                                            scene.over_circle(rx + 4.0, dot_y, 3.0, theme::DANGER, clip_id);
                                        } else {
                                            scene.over_ring(rx + 4.0, dot_y, 3.0, 1.1, theme::INK_3, clip_id);
                                        }
                                        let max_ch = ((content_w - 14.0) / m.cw).floor().max(2.0) as usize;
                                        let txt = truncate_chars(&raw, max_ch);
                                        let col = if is_ok { theme::INK_2 } else { theme::INK_3 };
                                        scene.mono_small(rx + 13.0, ty, txt, col, false, false, clip_id);
                                    }
                                }
                            }
                            ColDisplay::FileSize => {
                                let row_is_dir = row_file_kind.is_some_and(blaze_sniff::is_dir_label);
                                let effective_max = col_max_num[c].max(dust_max_bytes as f64);
                                let max_bytes_u64 = effective_max.round().max(1.0) as u64;
                                let bar_on = effective_max > 0.0;
                                let text_area_w = if bar_on {
                                    (content_w - SIZE_BAR_W - 10.0).max(m.cw * 3.0)
                                } else {
                                    content_w
                                };
                                let tier_bar_color = |bytes: u64, scanning: bool| -> [f32; 4] {
                                    if scanning {
                                        return theme::rgba(0xe5, 0xb5, 0x67, 0.50);
                                    }
                                    match blaze_dust::size_tier(bytes) {
                                        blaze_dust::SizeTier::Bytes => theme::rgba(0x88, 0x90, 0x9e, 0.55),
                                        blaze_dust::SizeTier::KiB => theme::rgb(0x58, 0x9d, 0xf6),
                                        blaze_dust::SizeTier::MiB => theme::rgb(0xe5, 0xb5, 0x67),
                                        blaze_dust::SizeTier::GiB => theme::rgb(0xe8, 0x6a, 0x45),
                                    }
                                };
                                if row_is_dir {
                                    if let Some(prog) = all_row_dust.get(r).copied().flatten() {
                                        if !prog.done && prog.bytes == 0 {
                                            let tx = rx + text_area_w - m.cw;
                                            scene.text(tx, ty, "…", theme::INK_4, false, false, clip_id);
                                        } else {
                                            let base = crate::dust_driver::format_bytes_compact(prog.bytes);
                                            let raw = if !prog.done {
                                                format!("~{base}")
                                            } else if prog.capped {
                                                format!("{base}+")
                                            } else {
                                                base
                                            };
                                            let max_ch = (text_area_w / m.cw).floor().max(2.0) as usize;
                                            let txt = truncate_chars(&raw, max_ch);
                                            let tx = rx + text_area_w - txt.chars().count() as f32 * m.cw;
                                            let txt_col = if prog.done { theme::INK_2 } else { theme::INK_3 };
                                            scene.text(tx, ty, txt, txt_col, false, false, clip_id);

                                            if bar_on && prog.bytes > 0 {
                                                let frac = blaze_dust::bar_fraction(prog.bytes, max_bytes_u64);
                                                if frac >= 0.02 {
                                                    let bx0 = rx + text_area_w + 10.0;
                                                    let by0 = ty + m.ch * 0.5 - 2.0;
                                                    let by1 = by0 + 4.0;
                                                    scene.round_rect(bx0, by0, bx0 + SIZE_BAR_W, by1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
                                                    let fill_w = (SIZE_BAR_W * frac).max(3.0);
                                                    let fill_col = tier_bar_color(prog.bytes, !prog.done);
                                                    scene.round_rect(bx0, by0, bx0 + fill_w, by1, 2.0, fill_col, 0.0, [0.0; 4], clip_id);
                                                }
                                            }
                                            push_exact_size_hover(
                                                scene,
                                                HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h),
                                                prog.bytes,
                                                Some(prog),
                                                clip_id,
                                            );
                                        }
                                    } else {
                                        let tx = rx + text_area_w - m.cw;
                                        scene.text(tx, ty, "—", theme::INK_4, false, false, clip_id);
                                    }
                                } else {
                                    let raw = format_value_cell_at(cell, Some(now_ns));
                                    let max_ch = (text_area_w / m.cw).floor().max(2.0) as usize;
                                    let txt = truncate_chars(&raw, max_ch);
                                    let tx = rx + text_area_w - txt.chars().count() as f32 * m.cw;
                                    scene.text(tx, ty, txt, theme::INK_2, false, false, clip_id);

                                    if let Value::Size(Bytes(b)) = cell && *b >= 0 {
                                        let bytes_u64 = *b as u64;
                                        if bar_on && bytes_u64 > 0 {
                                            let frac = blaze_dust::bar_fraction(bytes_u64, max_bytes_u64);
                                            if frac >= 0.02 {
                                                let bx0 = rx + text_area_w + 10.0;
                                                let by0 = ty + m.ch * 0.5 - 2.0;
                                                let by1 = by0 + 4.0;
                                                scene.round_rect(bx0, by0, bx0 + SIZE_BAR_W, by1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
                                                let fill_w = (SIZE_BAR_W * frac).max(3.0);
                                                let fill_col = tier_bar_color(bytes_u64, false);
                                                scene.round_rect(bx0, by0, bx0 + fill_w, by1, 2.0, fill_col, 0.0, [0.0; 4], clip_id);
                                            }
                                        }
                                        push_exact_size_hover(
                                            scene,
                                            HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h),
                                            bytes_u64,
                                            None,
                                            clip_id,
                                        );
                                    }
                                }
                            }
                            ColDisplay::HumanTime => {
                                let row_is_dir = row_file_kind.is_some_and(blaze_sniff::is_dir_label);
                                let dust_mtime = if row_is_dir {
                                    all_row_dust
                                        .get(r)
                                        .copied()
                                        .flatten()
                                        .map(|p| p.max_mtime_ns)
                                        .unwrap_or(0)
                                } else {
                                    0
                                };
                                let (raw, txt_col, hover_dt) = if let Value::Date(dt) = cell {
                                    let eff_ns = dt.utc_ns.max(dust_mtime);
                                    let eff_dt = blaze_sh::DateTime {
                                        utc_ns: eff_ns,
                                        offset_secs: dt.offset_secs,
                                    };
                                    let eff_val = Value::Date(eff_dt);
                                    let age_secs =
                                        (now_ns.saturating_sub(eff_ns)).max(0) as u64 / 1_000_000_000;
                                    (
                                        format_value_cell_at(&eff_val, Some(now_ns)),
                                        theme::recency_heat_color(age_secs),
                                        Some((eff_dt, row_is_dir && dust_mtime > dt.utc_ns)),
                                    )
                                } else {
                                    (format_value_cell_at(cell, Some(now_ns)), theme::INK_3, None)
                                };
                                let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
                                let txt = truncate_chars(&raw, max_ch);
                                let tx = rx + content_w - txt.chars().count() as f32 * m.cw;
                                scene.text(tx, ty, txt, txt_col, false, false, clip_id);
                                if let Some((eff_dt, is_recursive)) = hover_dt {
                                    push_exact_date_hover(
                                        scene,
                                        HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h),
                                        eff_dt,
                                        now_ns,
                                        is_recursive,
                                        clip_id,
                                    );
                                }
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
                                // Render registered PUA custom symbols (`U+E100..=U+EFFF`) live inside
                                // table cells (e.g. `term symbol list`'s `preview` column!).
                                if raw.chars().count() == 1
                                    && let Some(ch) = raw.chars().next()
                                    && let Some(sym) =
                                        crate::term_server::lookup_global_codepoint(ch as u32)
                                {
                                    let sh = m.icon_md();
                                    let sw = (sym.width_cells.max(1) as f32 * sh).max(sh);
                                    let tint = if sym.monochrome { Some(color) } else { None };
                                    scene.custom_symbol(
                                        rx,
                                        ty + (m.ch - sh) * 0.5,
                                        sw,
                                        sh,
                                        sym.name,
                                        tint,
                                        1.0,
                                        clip_id,
                                    );
                                } else {
                                    let tx = if col_right_align[c] {
                                        rx + content_w - txt.chars().count() as f32 * m.cw
                                    } else {
                                        rx
                                    };
                                    scene.text(tx, ty, &txt, color, false, false, clip_id);
                                }
                                let cell_rect = HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h);
                                if !push_exact_value_hover(scene, cell_rect, cell, now_ns, clip_id) && txt != raw {
                                    scene.push_hover_clipped(
                                        cell_rect,
                                        clip_id,
                                        HoverKind::Tooltip {
                                            title: raw,
                                            subtitle: None,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
                rx += cw;
            }
            if show_ls_commit_col
                && let Some(snap) = ls_dir_commits.as_ref()
                && let Some(fc) = file_entry_col
            {
                let content_w = (ls_commit_col_w - col_gap).max(m.cw * 6.0);
                let raw_name = table
                    .cell(r, fc)
                    .map(|c| format_value_cell_at(c, Some(now_ns)))
                    .unwrap_or_default();
                let entry_key = raw_name.trim_end_matches('/');
                if let Some(cm) = snap.entries.get(entry_key) {
                    let avatar_s = 15.0f32.min(m.ch + 1.0);
                    let av_pad = avatar_s + 7.0;
                    let av = crate::git_meta_driver::request_avatar(
                        &cm.author_email,
                        Some(&cm.full_sha),
                        snap.github_repo.as_ref(),
                    );
                    draw_git_avatar(
                        scene,
                        rx,
                        ty + (m.ch - avatar_s) * 0.5,
                        avatar_s,
                        &cm.author_name,
                        &cm.author_email,
                        av.image_path.as_deref(),
                        m,
                        clip_id,
                    );
                    let sha_x = rx + av_pad;
                    let sha_w = cm.short_sha.chars().count() as f32 * m.cw;
                    scene.text(sha_x, ty, &cm.short_sha, theme::rgb(0x8a, 0xa8, 0xd4), false, false, clip_id);
                    let subj_x = sha_x + sha_w + m.cw;
                    let subj_avail = (rx + content_w - subj_x).max(0.0);
                    let subj_max_ch = (subj_avail / m.cw).floor() as usize;
                    if subj_max_ch >= 3 {
                        let subj_txt = truncate_chars(&cm.subject, subj_max_ch);
                        scene.text(subj_x, ty, subj_txt, theme::INK_3, false, false, clip_id);
                    }
                    let cell_rect = HitRect::new(rx, cur_y, rx + content_w, cur_y + row_h);
                    scene.push_hit_clipped(
                        cell_rect,
                        clip_id,
                        HitAction::CopyText(cm.short_sha.clone()),
                    );
                    let mut iso = String::new();
                    if cm.author_time_secs > 0 {
                        let _ = blaze_sh::format_date(
                            blaze_sh::DateTime {
                                utc_ns: cm.author_time_secs.saturating_mul(1_000_000_000),
                                offset_secs: 0,
                            },
                            &mut iso,
                        );
                    }
                    let who = if let Some(login) = av.github_login.as_deref() {
                        format!("{} (@{login})", cm.author_name)
                    } else {
                        cm.author_name.clone()
                    };
                    let sub = if iso.is_empty() {
                        format!("{} · {who} · click to copy SHA", cm.short_sha)
                    } else {
                        format!("{} · {who} · {iso}", cm.short_sha)
                    };
                    scene.push_hover_clipped(
                        cell_rect,
                        clip_id,
                        HoverKind::Tooltip {
                            title: cm.subject.clone(),
                            subtitle: Some(sub),
                        },
                    );
                } else {
                    scene.text(rx, ty, "—", theme::INK_4, false, false, clip_id);
                }
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

    // Crisp macOS-style minimal scrollbar track + thumb when scrolling a large table on the row grid.
    if is_scrollable && max_first_row > 0 {
        let track_h = (cur_y - rows_top_y).max(24.0);
        let sb_x0 = x1 + 5.0;
        let sb_w = 3.5;
        scene.round_rect(
            sb_x0,
            rows_top_y,
            sb_x0 + sb_w,
            rows_top_y + track_h,
            1.75,
            theme::BG_TRACK,
            0.0,
            [0.0; 4],
            clip_id,
        );
        let frac = (visible_rows as f32 / total_rows as f32).clamp(0.12, 1.0);
        let thumb_h = (track_h * frac).max(18.0);
        let pos_frac = first_row as f32 / max_first_row as f32;
        let thumb_y0 = rows_top_y + (track_h - thumb_h) * pos_frac;
        scene.over_round_rect(
            sb_x0,
            thumb_y0,
            sb_x0 + sb_w,
            thumb_y0 + thumb_h,
            1.75,
            theme::INK_4,
            clip_id,
        );
        scene.push_scroll_region(
            HitRect::new(x0, y0, x1 + 14.0, cur_y),
            clip_id,
            block.id,
            max_scroll_y,
            0.0,
        );
    }

    if depth <= 1 && total_rows > fold_rows {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!(
                "rows {}–{} of {} · scroll or show all",
                first_row + 1,
                first_row + visible_rows,
                total_rows
            )
        } else {
            format!("− collapse to {fold_rows}-row scrollable view")
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

const DATAFRAME_TREE_STEP: f32 = 18.0;

/// Parse `blaze-sh`'s 4-char-per-level `tree_prefix` (composed of `"│   "`, `"    "`, `"├── "`,
/// `"└── "`) into `(segments_count, continuing_mask, last_is_elbow_last)`.
fn parse_tree_prefix(prefix: &str) -> (usize, u64, Option<bool>) {
    let mut chars = prefix.chars();
    let mut segments = 0usize;
    let mut continuing_mask = 0u64;
    let mut last_is_elbow_last = None;

    while let Some(first) = chars.next() {
        let lvl = segments;
        for _ in 0..3 {
            if chars.next().is_none() {
                break;
            }
        }
        segments += 1;
        match first {
            '│' => {
                if lvl < 64 {
                    continuing_mask |= 1u64 << lvl;
                }
            }
            '├' => {
                last_is_elbow_last = Some(false);
            }
            '└' => {
                last_is_elbow_last = Some(true);
            }
            _ => {}
        }
    }

    (segments, continuing_mask, last_is_elbow_last)
}

/// Draw crisp, continuous, pixel-snapped 1px GPU vector tree guides, L/T elbows, and
/// parent-folder drop-stems for `ColDisplay::TreeEntry` rows.
///
/// Returns the horizontal pixel offset (`pref_w`) at which the entry's file/folder icon starts.
#[allow(clippy::too_many_arguments)]
fn draw_dataframe_tree_guides(
    scene: &mut Scene,
    rx: f32,
    cur_y: f32,
    row_h: f32,
    icon_s: f32,
    prefix: &str,
    has_visible_child: bool,
    clip_id: u32,
) -> f32 {
    const TREE_STEP: f32 = DATAFRAME_TREE_STEP;
    let (segments_count, continuing_mask, last_is_elbow_last) = parse_tree_prefix(prefix);

    let guide_col = theme::rgba(0xff, 0xff, 0xff, 0.14);
    let elbow_col = theme::rgba(0xff, 0xff, 0xff, 0.19);
    let cy = (cur_y + row_h * 0.5).round();
    let row_y1 = cur_y + row_h;

    let pass_through_end = if last_is_elbow_last.is_some() {
        segments_count.saturating_sub(1)
    } else {
        segments_count
    };

    // Continuous pass-through vertical ancestor rails spanning the exact full row height.
    for lvl in 0..pass_through_end {
        if lvl < 64 && ((continuing_mask >> lvl) & 1) != 0 {
            let sx = (rx + 6.0 + lvl as f32 * TREE_STEP).round();
            scene.round_rect(
                sx - 0.5,
                cur_y,
                sx + 0.5,
                row_y1,
                0.0,
                guide_col,
                0.0,
                [0.0; 4],
                clip_id,
            );
        }
    }

    // Final L (`└── `) or T (`├── `) elbow connecting the ancestor rail to this entry's icon.
    if let Some(is_last) = last_is_elbow_last
        && segments_count > 0
    {
        let elbow_lvl = segments_count - 1;
        let sx = (rx + 6.0 + elbow_lvl as f32 * TREE_STEP).round();
        let branch_x1 = (sx + TREE_STEP - 5.0).round();
        if is_last {
            if cy - 0.5 > cur_y {
                scene.round_rect(
                    sx - 0.5,
                    cur_y,
                    sx + 0.5,
                    cy - 0.5,
                    0.0,
                    elbow_col,
                    0.0,
                    [0.0; 4],
                    clip_id,
                );
            }
            scene.round_rect(
                sx - 0.5,
                cy - 0.5,
                branch_x1,
                cy + 0.5,
                0.0,
                elbow_col,
                0.0,
                [0.0; 4],
                clip_id,
            );
        } else {
            scene.round_rect(
                sx - 0.5,
                cur_y,
                sx + 0.5,
                row_y1,
                0.0,
                guide_col,
                0.0,
                [0.0; 4],
                clip_id,
            );
            scene.round_rect(
                sx + 0.5,
                cy - 0.5,
                branch_x1,
                cy + 0.5,
                0.0,
                elbow_col,
                0.0,
                [0.0; 4],
                clip_id,
            );
        }
    }

    // Parent-folder drop-stem meeting the first child's top edge with zero vertical gap.
    if has_visible_child {
        let drop_x = (rx + 6.0 + segments_count as f32 * TREE_STEP).round();
        let drop_y0 = cy + (icon_s * 0.5).ceil() + 1.0;
        if drop_y0 < row_y1 {
            scene.round_rect(
                drop_x - 0.5,
                drop_y0,
                drop_x + 0.5,
                row_y1,
                0.0,
                guide_col,
                0.0,
                [0.0; 4],
                clip_id,
            );
        }
    }

    if segments_count == 0 {
        0.0
    } else {
        segments_count as f32 * TREE_STEP + 2.0
    }
}

/// Draw a filename with subtle semantic shading between the stem and `.{ext}` (or trailing `/`):
/// - Primary source/manifest files (`Cargo.toml`, `main.rs`, `README.md`): crisp stem + softer `.ext`
/// - Secondary lock/backup/log files (`Cargo.lock`, `blaze.lock`, `*.bak`, `*.tmp`, `*.log`): gently
///   calmer stem + muted `.ext` so primary files naturally pop during visual scanning
/// - Directories (`src/`): crisp folder name + softly muted trailing `/`
#[allow(clippy::too_many_arguments)]
fn draw_shaded_filename(
    scene: &mut Scene,
    x: f32,
    y: f32,
    name: &str,
    is_dir: bool,
    base_col: [f32; 4],
    m: CellMetrics,
    clip_id: u32,
) {
    let scale_alpha = |c: [f32; 4], factor: f32| [c[0], c[1], c[2], (c[3] * factor).clamp(0.15, 1.0)];

    if is_dir {
        if let Some(stem) = name.strip_suffix('/')
            && !stem.is_empty()
        {
            scene.text(x, y, stem, base_col, false, false, clip_id);
            let sx = x + stem.chars().count() as f32 * m.cw;
            scene.text(sx, y, "/", scale_alpha(base_col, 0.50), false, false, clip_id);
            return;
        }
        scene.text(x, y, name, base_col, false, false, clip_id);
        return;
    }

    // Separate optional parent path prefix (`crates/…/foo.rs`) from the leaf filename.
    let (dir_prefix, leaf) = match name.rfind('/') {
        Some(idx) => (&name[..=idx], &name[idx + 1..]),
        None => ("", name),
    };

    let mut cur_x = x;
    if !dir_prefix.is_empty() {
        scene.text(cur_x, y, dir_prefix, scale_alpha(base_col, 0.62), false, false, clip_id);
        cur_x += dir_prefix.chars().count() as f32 * m.cw;
    }

    // Split leaf into `(stem, ext)` without mis-splitting dotfiles like `.gitignore` or `.env`.
    let ext_dot = leaf
        .rfind('.')
        .filter(|&i| i > 0 && i + 1 < leaf.len());
    let Some(dot_idx) = ext_dot else {
        scene.text(cur_x, y, leaf, base_col, false, false, clip_id);
        return;
    };

    let ext = &leaf[dot_idx..]; // includes leading `.`
    let ext_lower = ext.to_ascii_lowercase();
    let is_secondary = matches!(
        ext_lower.as_str(),
        ".lock" | ".bak" | ".orig" | ".tmp" | ".log" | ".swp" | ".pyc" | ".o" | ".d" | ".map"
    );

    // For compound backups like `Cargo.toml.rust-2024.bak`, dim from the secondary suffix onward!
    let split_idx = if is_secondary && ext_lower == ".bak" {
        leaf[..dot_idx].rfind('.').filter(|&i| i > 0).unwrap_or(dot_idx)
    } else {
        dot_idx
    };

    let stem = &leaf[..split_idx];
    let suffix = &leaf[split_idx..];

    let stem_col = if is_secondary {
        scale_alpha(base_col, 0.80)
    } else {
        base_col
    };
    let ext_col = if is_secondary {
        scale_alpha(base_col, 0.50)
    } else {
        scale_alpha(base_col, 0.64)
    };

    scene.text(cur_x, y, stem, stem_col, false, false, clip_id);
    let ex = cur_x + stem.chars().count() as f32 * m.cw;
    scene.text(ex, y, suffix, ext_col, false, false, clip_id);
}

/// Render a structured external-command [`blaze_sh::CommandOutput`] with true time-interleaved
/// `stdout` and `stderr` streams, full ANSI SGR color rendering (16 / 256 / 24-bit TrueColor,
/// bold/dim/italic, backgrounds), subtle coral gutter rail on `stderr` lines, exact per-line
/// arrival timestamp hover tooltips, and explicit `exit N · out · err` metadata pills.
fn with_cached_help_doc<R>(
    block_id: u64,
    cmd_source: &str,
    stdout_len: usize,
    stderr_len: usize,
    lines: &[&str],
    f: impl FnOnce(Option<&crate::help_view::HelpDoc<'static>>) -> R,
) -> R {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static CACHE: RefCell<HashMap<(u64, usize, usize), Option<crate::help_view::HelpDoc<'static>>>> =
            RefCell::new(HashMap::new());
    }

    let key = (block_id, stdout_len, stderr_len);
    CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(&key) {
            if map.len() > 48 {
                map.clear();
            }
            let parsed =
                crate::help_view::parse_help_doc_in(cmd_source, lines, blaze_own::Alloc::global());
            map.insert(key, parsed);
        }
        f(map.get(&key).and_then(Option::as_ref))
    })
}

#[allow(clippy::too_many_arguments)]
fn layout_command_output(
    scene: &mut Scene,
    block_id: u64,
    cmd_source: &str,
    out: &blaze_sh::CommandOutput<'_>,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let stdout_len = out.stdout_len();
    let stderr_len = out.stderr_len();
    let dur_s = format_duration_ns(out.duration.0.max(0) as u64);
    let out_b = crate::dust_driver::format_bytes_compact(stdout_len as u64);
    let err_b = crate::dust_driver::format_bytes_compact(stderr_len as u64);

    // Surface compact stream metadata (`err 552 B`, `out 1.2 KiB · err 552 B`) without repeating
    // the `exit N` badge that already appears in the block header above.
    let streams_lbl = if stderr_len > 0 && stdout_len > 0 {
        format!("out {out_b} · err {err_b}")
    } else if stderr_len > 0 {
        format!("err {err_b}")
    } else if stdout_len > 0 {
        format!("out {out_b}")
    } else {
        format!("exit {}", out.exit_code)
    };
    let pill_lbl = streams_lbl;
    let pill_w = m.measure(&pill_lbl, TextFace::MonoSmall) + 16.0;
    let bx0 = (x0 + width - pill_w).max(x0 + 80.0);
    let (pill_bg, pill_border, pill_fg) = if out.exit_code != 0 {
        (
            theme::rgba(0xe0, 0x6c, 0x75, 0.14),
            theme::rgba(0xe0, 0x6c, 0x75, 0.55),
            theme::DANGER,
        )
    } else if stderr_len > 0 {
        (
            theme::rgba(0xe5, 0xb5, 0x67, 0.10),
            theme::rgba(0xe5, 0xb5, 0x67, 0.45),
            theme::rgb(0xe5, 0xb5, 0x67),
        )
    } else {
        (
            theme::rgba(0x4c, 0xc3, 0x8a, 0.08),
            theme::rgba(0x4c, 0xc3, 0x8a, 0.36),
            theme::rgb(0x7c, 0xd4, 0xa8),
        )
    };
    let draw_meta_pill = |scene: &mut Scene, pill_y: f32, inset_right: f32| {
        let px1 = x0 + width - inset_right;
        let px0 = (px1 - pill_w).max(x0 + 80.0);
        scene.round_rect(
            px0,
            pill_y,
            px1,
            pill_y + m.ch - 1.0,
            (m.ch - 1.0) * 0.5,
            pill_bg,
            1.0,
            pill_border,
            clip_id,
        );
        scene.mono_small(px0 + 8.0, pill_y, &pill_lbl, pill_fg, out.exit_code != 0, false, clip_id);
        scene.push_hover_clipped(
            HitRect::new(px0, pill_y - 2.0, px1, pill_y + m.ch + 1.0),
            clip_id,
            HoverKind::Tooltip {
                title: format!("exit {} · {dur_s} · {out_b} stdout · {err_b} stderr", out.exit_code),
                subtitle: Some("fields: .stdout · .stderr · .exit_code · .duration · .events".to_owned()),
            },
        );
    };

    // Flatten time-ordered chunks into display lines tagged with `(arrival_ns, stream)`,
    // preserving ANSI SGR codes so external CLIs (`rg`, `cargo`, `git`, `gh`, `bun`, `pytest`)
    // render with full terminal colors!
    let mut lines: Vec<(i64, blaze_sh::OutputStream, String)> = Vec::new();
    let mut prev_ended_nl = true;
    for ch in out.chunks.iter() {
        let text = String::from_utf8_lossy(ch.bytes.as_ref());
        let ends_nl = text.ends_with('\n');
        let mut first_seg = true;
        for seg in text.split('\n') {
            let s = seg.trim_end_matches('\r');
            if first_seg && !prev_ended_nl {
                if let Some(last) = lines.last_mut()
                    && last.1 == ch.stream
                {
                    last.2.push_str(s);
                    first_seg = false;
                    continue;
                }
            }
            lines.push((ch.at.0, ch.stream, s.to_owned()));
            first_seg = false;
        }
        if ends_nl && lines.last().is_some_and(|l| l.2.is_empty()) {
            lines.pop();
        }
        prev_ended_nl = ends_nl;
    }
    while lines.last().is_some_and(|l| l.2.is_empty()) {
        lines.pop();
    }

    if lines.is_empty() {
        draw_meta_pill(scene, y0, 0.0);
        let empty_msg = if out.exit_code != 0 {
            format!("(exited with code {}, no stdout/stderr)", out.exit_code)
        } else {
            "(no stdout/stderr)".to_owned()
        };
        scene.text(x0, y0, &empty_msg, theme::INK_4, false, true, clip_id);
        return m.ch;
    }

    // Structured interactive CLI Help / Man-Page rendering (`git --help`, `rg --help`,
    // `blaze --help`, `man ...`, etc.) with syntax-highlighted Usage, runnable Examples,
    // categorized subcommand/flag tables, inline Markdown chips, and one-click drill-down.
    if lines.len() <= 500 {
        let all_refs: Vec<&str> = lines.iter().map(|(_, _, s)| s.as_str()).collect();
        let help_h = with_cached_help_doc(
            block_id,
            cmd_source,
            stdout_len,
            stderr_len,
            &all_refs,
            |doc| {
                doc.map(|d| {
                    let h = crate::help_view::layout_help_doc(
                        scene,
                        d,
                        x0,
                        y0,
                        width,
                        pill_w + 18.0,
                        m,
                        clip_id,
                    );
                    draw_meta_pill(scene, y0 + 7.5, 14.0);
                    h
                })
            },
        );
        if let Some(h) = help_h {
            return h;
        }
    }

    let first_is_diag_card = lines.first().is_some_and(|(_, s, l)| {
        if l.contains('\x1b') {
            return false;
        }
        if *s == blaze_sh::OutputStream::Stderr {
            return true;
        }
        let first_stdout_group: Vec<(i64, &str)> = lines
            .iter()
            .take_while(|(_, st, ln)| *st == blaze_sh::OutputStream::Stdout && !ln.contains('\x1b'))
            .map(|(at, _, ln)| (*at, ln.as_str()))
            .collect();
        crate::diag_view::looks_like_diagnostic_output(&first_stdout_group, out.exit_code)
    });

    let text_w_first = (bx0 - x0 - 12.0).max(m.cw * 16.0);
    let full_cols = ((width - 10.0) / m.cw).floor().max(16.0) as usize;
    let first_cols = ((text_w_first - 10.0) / m.cw).floor().max(16.0) as usize;
    let step_y = m.ch + 2.0;
    let mut cur_y = y0;
    let mut row_idx = 0usize;
    let max_display_lines = 400usize;
    let mut stdout_style = AnsiStyle::default();
    let mut stderr_style = AnsiStyle::default();

    let display_lines = &lines[..lines.len().min(max_display_lines)];
    let mut li = 0usize;
    while li < display_lines.len() {
        let (at_ns, stream, raw_line) = &display_lines[li];
        let is_err = *stream == blaze_sh::OutputStream::Stderr;

        // Contiguous uncolored `stderr` lines — and diagnostic `stdout` groups (such as
        // `blaze check`, `rustc`, `clippy` emitting `note:`, `warning[...]`, `error[...]`,
        // `fix:`, `--> loc`) — render as crisp monospace diagnostic cards with clickable
        // inline Markdown `` `code` `` chips and clean word-boundary wrapping!
        if !raw_line.contains('\x1b') {
            let group_start = li;
            let mut end = li;
            let mut group: Vec<(i64, &str)> = Vec::new();
            while end < display_lines.len()
                && display_lines[end].1 == *stream
                && !display_lines[end].2.contains('\x1b')
            {
                group.push((display_lines[end].0, display_lines[end].2.as_str()));
                end += 1;
            }
            if is_err || crate::diag_view::looks_like_diagnostic_output(&group, out.exit_code) {
                li = end;
                let right_reserve = if row_idx == 0 { pill_w + 14.0 } else { 0.0 };
                let card_y0 = cur_y;
                let h = crate::diag_view::layout_stderr_diagnostic_group(
                    scene,
                    &group,
                    out.exit_code,
                    x0,
                    cur_y,
                    width,
                    right_reserve,
                    m,
                    clip_id,
                );
                cur_y += h + 4.0;
                row_idx += li - group_start;
                let at_lbl = format_duration_ns((*at_ns).max(0) as u64);
                let _ = (card_y0, at_lbl);
                continue;
            }
        }

        let default_fg = if is_err {
            theme::rgb(0xec, 0x9b, 0x82)
        } else {
            theme::INK_1
        };
        let style_state = if is_err { &mut stderr_style } else { &mut stdout_style };
        let spans = parse_ansi_sgr_spans(raw_line, style_state);
        let line_x = if is_err { x0 + 8.0 } else { x0 };
        let block_y0 = cur_y;

        if spans.is_empty() {
            cur_y += step_y;
            row_idx += 1;
        } else {
            let mut col = 0usize;
            let mut max_cols = if row_idx == 0 { first_cols } else { full_cols };
            for sp in &spans {
                let mut fg = sp.style.fg.unwrap_or(default_fg);
                if sp.style.dim {
                    fg[3] *= 0.65;
                }
                let bg = sp.style.bg;
                let mut rest = sp.text.as_str();
                while !rest.is_empty() {
                    if col >= max_cols {
                        cur_y += step_y;
                        row_idx += 1;
                        col = 0;
                        max_cols = full_cols;
                        rest = rest.trim_start_matches(' ');
                        if rest.is_empty() {
                            break;
                        }
                    }
                    let take = max_cols - col;
                    let total_chars = rest.chars().count();
                    let (chunk, next_rest) = if total_chars <= take {
                        (rest, "")
                    } else {
                        // Prefer breaking at the last space within `take` so words never split mid-word!
                        let mut cut_b = rest.len();
                        let mut last_space_b = None;
                        for (ci, (bi, ch)) in rest.char_indices().enumerate() {
                            if ci >= take {
                                cut_b = bi;
                                break;
                            }
                            if ch == ' ' {
                                last_space_b = Some(bi);
                            }
                        }
                        if let Some(sb) = last_space_b
                            && sb > 0
                        {
                            (&rest[..sb], rest[sb + 1..].trim_start_matches(' '))
                        } else {
                            (&rest[..cut_b], &rest[cut_b..])
                        }
                    };
                    let taken = chunk.chars().count();
                    if taken > 0 {
                        let rx = line_x + col as f32 * m.cw;
                        let rw = taken as f32 * m.cw;
                        if let Some(bg_col) = bg {
                            scene.round_rect(
                                rx,
                                cur_y,
                                rx + rw,
                                cur_y + m.ch,
                                2.0,
                                bg_col,
                                0.0,
                                [0.0; 4],
                                clip_id,
                            );
                        }
                        scene.text(rx, cur_y, chunk, fg, sp.style.bold, sp.style.italic, clip_id);
                        col += taken;
                    }
                    rest = next_rest;
                    if !rest.is_empty() {
                        cur_y += step_y;
                        row_idx += 1;
                        col = 0;
                        max_cols = full_cols;
                    }
                }
            }
            cur_y += step_y;
            row_idx += 1;
        }

        if is_err {
            scene.over_round_rect(
                x0,
                block_y0 + 1.0,
                x0 + 2.2,
                (cur_y - 2.0).max(block_y0 + m.ch - 1.0),
                1.0,
                theme::rgba(0xe8, 0x6a, 0x45, 0.78),
                clip_id,
            );
        }
        let at_lbl = format_duration_ns((*at_ns).max(0) as u64);
        let max_line_x1 = if block_y0 == y0 { bx0 - 8.0 } else { x0 + width };
        scene.push_hover_clipped(
            HitRect::new(x0, block_y0, max_line_x1.max(x0 + 40.0), cur_y),
            clip_id,
            HoverKind::Tooltip {
                title: format!("+{at_lbl} · {}", stream.as_str()),
                subtitle: None,
            },
        );
        li += 1;
    }
    if lines.len() > max_display_lines {
        scene.text(
            x0,
            cur_y,
            format!("… {} more lines (inspect via `.events` or `.stdout`)", lines.len() - max_display_lines),
            theme::INK_4,
            false,
            true,
            clip_id,
        );
        cur_y += step_y;
    }

    // Paint stream metadata pill AFTER cards so it sits crisply on top of the first card's header!
    if first_is_diag_card {
        draw_meta_pill(scene, y0 + 7.5, 10.0);
    } else {
        draw_meta_pill(scene, y0, 0.0);
    }

    (cur_y - y0).max(m.ch)
}

#[derive(Clone, Copy, Default)]
struct AnsiStyle {
    fg: Option<[f32; 4]>,
    bg: Option<[f32; 4]>,
    bold: bool,
    dim: bool,
    italic: bool,
}

struct StyledSpan {
    text: String,
    style: AnsiStyle,
}

fn ansi_16_color(idx: u8) -> [f32; 4] {
    match idx {
        0 => theme::rgb(0x48, 0x4f, 0x58),
        1 => theme::rgb(0xe0, 0x6c, 0x75),
        2 => theme::rgb(0x73, 0xd0, 0x89),
        3 => theme::rgb(0xe5, 0xc0, 0x7b),
        4 => theme::rgb(0x61, 0xaf, 0xef),
        5 => theme::rgb(0xc6, 0x78, 0xdd),
        6 => theme::rgb(0x56, 0xb6, 0xc2),
        7 => theme::rgb(0xd4, 0xd9, 0xe1),
        8 => theme::rgb(0x76, 0x83, 0x90),
        9 => theme::rgb(0xff, 0x85, 0x8e),
        10 => theme::rgb(0x8e, 0xe8, 0xa2),
        11 => theme::rgb(0xf6, 0xd4, 0x8c),
        12 => theme::rgb(0x7f, 0xc2, 0xff),
        13 => theme::rgb(0xdc, 0x96, 0xf2),
        14 => theme::rgb(0x73, 0xd6, 0xe0),
        _ => theme::rgb(0xf4, 0xf6, 0xfa),
    }
}

fn ansi_256_color(n: u8) -> [f32; 4] {
    if n < 16 {
        return ansi_16_color(n);
    }
    if n >= 232 {
        let g = 8u8.saturating_add((n - 232).saturating_mul(10));
        return theme::rgb(g, g, g);
    }
    let c = n - 16;
    let r = c / 36;
    let g = (c / 6) % 6;
    let b = c % 6;
    let ch = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    theme::rgb(ch(r), ch(g), ch(b))
}

fn apply_sgr_params(params: &str, style: &mut AnsiStyle) {
    if params.is_empty() {
        *style = AnsiStyle::default();
        return;
    }
    let nums: Vec<u16> = params
        .split(';')
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .collect();
    let mut i = 0usize;
    while i < nums.len() {
        match nums[i] {
            0 => *style = AnsiStyle::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            30..=37 => style.fg = Some(ansi_16_color((nums[i] - 30) as u8)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(ansi_16_color((nums[i] - 40) as u8)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(ansi_16_color((nums[i] - 90 + 8) as u8)),
            100..=107 => style.bg = Some(ansi_16_color((nums[i] - 100 + 8) as u8)),
            38 | 48 => {
                let is_fg = nums[i] == 38;
                if i + 2 < nums.len() && nums[i + 1] == 5 {
                    let col = ansi_256_color(nums[i + 2].min(255) as u8);
                    if is_fg {
                        style.fg = Some(col);
                    } else {
                        style.bg = Some(col);
                    }
                    i += 2;
                } else if i + 4 < nums.len() && nums[i + 1] == 2 {
                    let col = theme::rgb(
                        nums[i + 2].min(255) as u8,
                        nums[i + 3].min(255) as u8,
                        nums[i + 4].min(255) as u8,
                    );
                    if is_fg {
                        style.fg = Some(col);
                    } else {
                        style.bg = Some(col);
                    }
                    i += 4;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Parse one line of external command output into styled ANSI SGR spans, cleanly discarding
/// OSC hyperlinks/titles and non-SGR CSI cursor controls.
fn parse_ansi_sgr_spans(s: &str, style: &mut AnsiStyle) -> Vec<StyledSpan> {
    let mut out: Vec<StyledSpan> = Vec::new();
    let mut cur = String::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    let start = i + 2;
                    let mut j = start;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    if j < bytes.len() {
                        if bytes[j] == b'm' {
                            if !cur.is_empty() {
                                out.push(StyledSpan {
                                    text: core::mem::take(&mut cur),
                                    style: *style,
                                });
                            }
                            if let Ok(params) = core::str::from_utf8(&bytes[start..j]) {
                                apply_sgr_params(params, style);
                            }
                        }
                        i = j + 1;
                    } else {
                        i = bytes.len();
                    }
                    continue;
                }
                b']' => {
                    // Skip OSC sequences (`\x1b]...\x07` or `\x1b]...\x1b\\`).
                    let mut j = i + 2;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            j += 1;
                            break;
                        }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            j += 2;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                _ => {
                    i += 2;
                    continue;
                }
            }
        }
        if let Some(ch) = s[i..].chars().next() {
            if ch == '\t' {
                cur.push_str("    ");
            } else if !ch.is_control() {
                cur.push(ch);
            }
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    if !cur.is_empty() {
        out.push(StyledSpan { text: cur, style: *style });
    }
    out
}

/// Draw a circular Retina GitHub user avatar (when cached on disk) or an instant deterministic
/// colored monogram circle (`AG`) with a subtle 1px border ring.
#[allow(clippy::too_many_arguments)]
fn draw_git_avatar(
    scene: &mut Scene,
    x: f32,
    y: f32,
    size: f32,
    author_name: &str,
    author_email: &str,
    image_path: Option<&str>,
    m: CellMetrics,
    clip_id: u32,
) {
    let cx = x + size * 0.5;
    let cy = y + size * 0.5;
    let r = size * 0.5;
    if let Some(path) = image_path {
        scene.avatar_image(x, y, size, path.to_owned(), clip_id);
        scene.over_ring(cx, cy, r, 0.85, theme::rgba(0xff, 0xff, 0xff, 0.14), clip_id);
    } else {
        let key = if author_email.is_empty() { author_name } else { author_email };
        let [cr, cg, cb] = blaze_git_meta::author_badge_rgb(key);
        let bg = theme::rgba(cr, cg, cb, 0.22);
        let fg = theme::rgb(cr, cg, cb);
        scene.over_circle(cx, cy, r, bg, clip_id);
        scene.over_ring(cx, cy, r, 0.85, theme::rgba(cr, cg, cb, 0.45), clip_id);
        let (init_bytes, len) = blaze_git_meta::author_initials(author_name);
        // Fit cleanly inside the circular badge: measure exact `UiSmall` bold advance horizontally
        // and compensate for `UiSmall`'s mono-baseline vertical nudge (`baseline - cap_h/2 ≈ 0.565 * ch`).
        if len >= 1
            && let Ok(init_str) = core::str::from_utf8(&init_bytes[..1])
        {
            let tw = m.measure_styled(init_str, TextFace::UiSmall, true);
            let tx = cx - tw * 0.5;
            let ty = cy - m.ch * 0.565;
            scene.ui_small(tx, ty, init_str, fg, true, false, clip_id);
        }
    }
}

/// Reusable exact-value hover tooltip for byte sizes (both individual files and recursive
/// `blaze-dust` folder totals). Shows exact comma-grouped bytes + IEC summary / entry counts.
fn push_exact_size_hover(
    scene: &mut Scene,
    rect: HitRect,
    bytes: u64,
    dust: Option<blaze_dust::DustProgress>,
    clip_id: u32,
) {
    let mut b_str = String::new();
    let _ = blaze_sh::format_u64_commas(bytes, &mut b_str);
    let unit = if bytes == 1 { "byte" } else { "bytes" };
    let title = if dust.is_some() {
        format!("{b_str} {unit} on disk")
    } else {
        format!("{b_str} {unit}")
    };
    let subtitle = if let Some(prog) = dust {
        let mut files_s = String::new();
        let mut dirs_s = String::new();
        let _ = blaze_sh::format_u64_commas(prog.files, &mut files_s);
        let _ = blaze_sh::format_u64_commas(prog.dirs, &mut dirs_s);
        let compact = crate::dust_driver::format_bytes_compact(bytes);
        let prefix = if !prog.done { "~" } else { "" };
        let suffix = if !prog.done {
            " · scanning…"
        } else if prog.capped {
            " (capped)"
        } else {
            ""
        };
        Some(format!(
            "{prefix}{compact} · {files_s} {} · {dirs_s} {}{suffix}",
            if prog.files == 1 { "file" } else { "files" },
            if prog.dirs == 1 { "subdir" } else { "subdirs" },
        ))
    } else if bytes >= 1024 {
        Some(format!("{} (exact IEC binary)", crate::dust_driver::format_bytes_compact(bytes)))
    } else {
        None
    };
    scene.push_hover_clipped(rect, clip_id, HoverKind::Tooltip { title, subtitle });
}

/// Reusable exact-value hover tooltip for timestamps (`Value::Date`, including recursive folder
/// mtimes from `blaze-dust`). Shows full ISO 8601 timestamp + exact elapsed time.
fn push_exact_date_hover(
    scene: &mut Scene,
    rect: HitRect,
    dt: blaze_sh::DateTime,
    now_ns: i64,
    is_recursive_folder: bool,
    clip_id: u32,
) {
    let mut iso = String::new();
    let _ = blaze_sh::format_date(dt, &mut iso);
    let diff_secs = (now_ns.saturating_sub(dt.utc_ns)) / 1_000_000_000;
    let mut elapsed = String::new();
    if diff_secs >= 0 {
        let _ = blaze_sh::format_exact_elapsed(diff_secs as u64, &mut elapsed);
    } else {
        let _ = std::fmt::Write::write_fmt(&mut elapsed, format_args!("in {}s", diff_secs.unsigned_abs()));
    }
    let subtitle = if is_recursive_folder {
        format!("{elapsed} · latest edit inside folder")
    } else {
        elapsed
    };
    scene.push_hover_clipped(
        rect,
        clip_id,
        HoverKind::Tooltip {
            title: iso,
            subtitle: Some(subtitle),
        },
    );
}

/// General reusable exact-value hover dispatcher for any formatted `blaze_sh::Value` across
/// tables, records, and lists (`Size`, `Date`, `Duration`, large `Int`, `Float`).
/// Returns `true` if a structured exact tooltip was attached.
fn push_exact_value_hover(
    scene: &mut Scene,
    rect: HitRect,
    val: &Value<'_>,
    now_ns: i64,
    clip_id: u32,
) -> bool {
    match val {
        Value::Size(Bytes(b)) if *b >= 0 => {
            push_exact_size_hover(scene, rect, *b as u64, None, clip_id);
            true
        }
        Value::Date(dt) => {
            push_exact_date_hover(scene, rect, *dt, now_ns, false, clip_id);
            true
        }
        Value::Duration(blaze_sh::Nanos(ns)) => {
            let neg = if *ns < 0 { "-" } else { "" };
            let abs = ns.unsigned_abs();
            let mut ns_str = String::new();
            let _ = blaze_sh::format_u64_commas(abs, &mut ns_str);
            let sub = if abs >= 1_000_000_000 {
                Some(format!("{neg}{:.6} s", abs as f64 / 1e9))
            } else if abs >= 1_000_000 {
                Some(format!("{neg}{:.3} ms", abs as f64 / 1e6))
            } else if abs >= 1_000 {
                Some(format!("{neg}{:.3} µs", abs as f64 / 1e3))
            } else {
                None
            };
            scene.push_hover_clipped(
                rect,
                clip_id,
                HoverKind::Tooltip {
                    title: format!("{neg}{ns_str} ns"),
                    subtitle: sub,
                },
            );
            true
        }
        Value::Int(n) if n.unsigned_abs() >= 10_000 => {
            let neg = if *n < 0 { "-" } else { "" };
            let mut s = String::new();
            let _ = blaze_sh::format_u64_commas(n.unsigned_abs(), &mut s);
            scene.push_hover_clipped(
                rect,
                clip_id,
                HoverKind::Tooltip {
                    title: format!("{neg}{s}"),
                    subtitle: Some(format!("0x{:x}", n.unsigned_abs())),
                },
            );
            true
        }
        Value::Float(f) if f.is_finite() => {
            let exact = format!("{f}");
            scene.push_hover_clipped(
                rect,
                clip_id,
                HoverKind::Tooltip {
                    title: exact,
                    subtitle: Some(format!("{f:.6e}")),
                },
            );
            true
        }
        _ => false,
    }
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
    let icon_s = m.icon_sm();
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
            let row_advance = text_h.max(row_h);
            push_exact_value_hover(
                scene,
                HitRect::new(val_x, start_y, val_x + val_w, start_y + row_advance),
                v,
                now_ns,
                clip_id,
            );
            cur_y += row_advance;
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
    let total_items = items.len();
    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;
    let is_scrollable = depth <= 1 && block.folded && total_items > TABLE_FOLD_ROWS;
    let visible_items = if is_scrollable {
        TABLE_FOLD_ROWS
    } else {
        total_items
    };
    let max_first_item = total_items.saturating_sub(visible_items);
    let max_scroll_y = max_first_item as f32 * row_h;
    let first_item = if is_scrollable {
        ((block.table_scroll_y.clamp(0.0, max_scroll_y) / row_h).round() as usize)
            .min(max_first_item)
    } else {
        0
    };

    let sb_reserve = if is_scrollable && max_first_item > 0 { 14.0 } else { 0.0 };
    let content_w = (width - sb_reserve).max(80.0);
    let idx_w = (total_items.to_string().len().max(2) as f32 * m.cw + 16.0).max(32.0);
    let val_x = x0 + idx_w;
    let max_val_cols = ((content_w - idx_w) / m.cw).floor().max(12.0) as usize;

    let mut cur_y = y0;
    for i in first_item..(first_item + visible_items) {
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, i.to_string(), theme::INK_4, false, false, clip_id);
        if let Some(val) = items.get(i) {
            let raw = format_value_cell_at(val, Some(now_ns));
            let txt = truncate_chars(&raw, max_val_cols);
            scene.text(val_x, cur_y + TABLE_ROW_PAD_Y, &txt, theme::value_color(val), false, false, clip_id);
            let cell_rect = HitRect::new(val_x, cur_y, x0 + content_w, cur_y + row_h);
            if !push_exact_value_hover(scene, cell_rect, val, now_ns, clip_id) && txt != raw {
                scene.push_hover_clipped(
                    cell_rect,
                    clip_id,
                    HoverKind::Tooltip {
                        title: raw,
                        subtitle: None,
                    },
                );
            }
        }
        cur_y += row_h;
    }

    // Crisp macOS-style minimal scrollbar track + thumb when scrolling a folded list in place.
    if is_scrollable && max_first_item > 0 {
        let track_h = (cur_y - y0).max(24.0);
        let sb_x0 = x0 + content_w + 5.0;
        let sb_w = 3.5;
        scene.round_rect(
            sb_x0,
            y0,
            sb_x0 + sb_w,
            y0 + track_h,
            1.75,
            theme::BG_TRACK,
            0.0,
            [0.0; 4],
            clip_id,
        );
        let frac = (visible_items as f32 / total_items as f32).clamp(0.12, 1.0);
        let thumb_h = (track_h * frac).max(18.0);
        let pos_frac = first_item as f32 / max_first_item as f32;
        let thumb_y0 = y0 + (track_h - thumb_h) * pos_frac;
        scene.over_round_rect(
            sb_x0,
            thumb_y0,
            sb_x0 + sb_w,
            thumb_y0 + thumb_h,
            1.75,
            theme::INK_4,
            clip_id,
        );
        scene.push_scroll_region(
            HitRect::new(x0, y0, x0 + width, cur_y),
            clip_id,
            block.id,
            max_scroll_y,
            0.0,
        );
    }

    if depth <= 1 && total_items > TABLE_FOLD_ROWS {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!(
                "items {}–{} of {} · scroll or show all",
                first_item + 1,
                first_item + visible_items,
                total_items
            )
        } else {
            format!("− collapse to {TABLE_FOLD_ROWS}-item scrollable view")
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
    let kind_bw = m.measure(kind_lbl, TextFace::MonoSmall) + 12.0;
    let kind_bh = m.ch - 1.0;
    scene.round_rect(
        kind_x,
        cur_y + 0.5,
        kind_x + kind_bw,
        cur_y + 0.5 + kind_bh,
        kind_bh * 0.5,
        theme::rgba(0xe0, 0x6c, 0x75, 0.18),
        1.0,
        theme::rgba(0xe0, 0x6c, 0x75, 0.45),
        clip_id,
    );
    scene.mono_small(kind_x + 6.0, cur_y + 0.5, kind_lbl, theme::DANGER, true, false, clip_id);

    let inner_w = (x1 - 12.0 - px).max(100.0);
    let msg_indent = (kind_x - px) + kind_bw + 8.0;
    let msg_h = crate::diag_view::layout_inline_markdown(
        scene,
        err.message.as_str(),
        px,
        cur_y,
        inner_w,
        msg_indent,
        0.0,
        theme::INK_1,
        m,
        clip_id,
    );
    cur_y += msg_h + 4.0;

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
        let help_bw = m.measure("help", TextFace::MonoSmall) + 10.0;
        let help_bh = m.ch - 2.0;
        scene.round_rect(
            px,
            cur_y + 1.0,
            px + help_bw,
            cur_y + 1.0 + help_bh,
            help_bh * 0.5,
            theme::rgba(0x4c, 0xc3, 0x8a, 0.16),
            1.0,
            theme::rgba(0x4c, 0xc3, 0x8a, 0.36),
            clip_id,
        );
        scene.mono_small(px + 5.0, cur_y + 1.0, "help", theme::OK, true, false, clip_id);
        let help_h = crate::diag_view::layout_inline_markdown(
            scene,
            help.as_str(),
            px,
            cur_y,
            inner_w,
            help_bw + 8.0,
            0.0,
            theme::INK_2,
            m,
            clip_id,
        );
        cur_y += help_h + 3.0;
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
    block_id: BlockId,
    term: &Terminal<'_>,
    running: bool,
    focused: bool,
    cursor_blink_on: bool,
    x0: f32,
    y0: f32,
    width: f32,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let screen = term.screen();
    let top = screen.pages.active_top();
    let rows = screen.rows();
    let cursor_on = running && (cursor_blink_on || !focused);

    let show_rows = if matches!(term.active(), blaze_vt::terminal::Which::Alternate) {
        // Alternate-screen TUIs (`claude`, `vim`, `htop`) keep a steady inline frame height
        // so redraws never bounce vertically while scrollback stays visible above!
        rows.max(1)
    } else {
        let mut last_non_empty = if running { screen.cursor.y } else { 0 };
        for y in 0..rows {
            let cells = screen.pages.cells(top + y);
            if cells.iter().any(|c| c.codepoint() > 0x20) {
                last_non_empty = last_non_empty.max(y);
            }
        }
        (last_non_empty + 1).min(rows).max(1)
    };
    let pad = 10.0;
    let total_h = show_rows as f32 * m.ch + pad * 2.0;
    let x1 = x0 + width;

    let (border_w, border_col) = if running && focused {
        (1.5, theme::rgba(0x58, 0x9d, 0xf6, 0.58))
    } else if running {
        (1.0, theme::LINE_FAINT)
    } else {
        (1.0, theme::LINE)
    };
    scene.round_rect(
        x0,
        y0,
        x1,
        y0 + total_h,
        8.0,
        theme::BG_SURFACE,
        border_w,
        border_col,
        clip_id,
    );
    if running {
        let card_rect = HitRect::new(x0, y0, x1, y0 + total_h);
        scene.inline_pty_rect = Some(card_rect);
        scene.push_hit_clipped(card_rect, clip_id, HitAction::FocusInlinePty(block_id));
    }
    let inner_clip = scene.push_clip(x0 + 1.0, y0 + 1.0, x1 - 1.0, y0 + total_h - 1.0, 7.0);
    draw_terminal_rows(
        scene,
        term,
        x0 + pad,
        y0 + pad,
        show_rows,
        m,
        cursor_on,
        focused,
        inner_clip,
    );
    total_h
}

/// Lay out a full-screen `blaze-vt` terminal when an interactive program enters alternate screen
/// (`claude`, `vim`, `htop`, etc.), keeping the unified session tab bar live and clickable at top!
#[allow(clippy::too_many_arguments)]
pub fn layout_fullscreen_terminal(
    term: &Terminal<'_>,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    cursor_on: bool,
    zoom_pct: u16,
    inline_titlebar: bool,
) -> Scene {
    layout_fullscreen_terminal_inner(
        term,
        store,
        prompt,
        viewport_w,
        viewport_h,
        m,
        cursor_on,
        zoom_pct,
        inline_titlebar,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn layout_fullscreen_terminal_pane(
    term: &Terminal<'_>,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    cursor_on: bool,
    zoom_pct: u16,
) -> Scene {
    layout_fullscreen_terminal_inner(
        term, store, prompt, viewport_w, viewport_h, m, cursor_on, zoom_pct, false, false,
    )
}

#[allow(clippy::too_many_arguments)]
fn layout_fullscreen_terminal_inner(
    term: &Terminal<'_>,
    store: &BlockStore,
    prompt: &PromptInfo,
    viewport_w: f32,
    viewport_h: f32,
    m: CellMetrics,
    cursor_on: bool,
    zoom_pct: u16,
    inline_titlebar: bool,
    draw_top_bar: bool,
) -> Scene {
    let mut scene = Scene::new();
    let vw = viewport_w.max(220.0);
    let vh = viewport_h.max(160.0);
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
    scene.scroll_viewport_height = (vh - TOP_BAR_H).max(1.0);
    scene.input_top_y = vh;

    // Clip full-screen PTY content strictly below the unified titlebar + tabs.
    let term_clip = scene.push_clip(0.0, TOP_BAR_H, vw, vh, 0.0);
    let pad_x = 10.0;
    let pad_y = TOP_BAR_H + 6.0;
    let rows = term.screen().rows();
    draw_terminal_rows(&mut scene, term, pad_x, pad_y, rows, m, cursor_on, true, term_clip);

    // Keep session tabs, traffic-light clearance, cwd/branch, and fork button live in fullscreen!
    if draw_top_bar {
        layout_top_bar(&mut scene, store, prompt, vw, m, zoom_pct, inline_titlebar);
    }
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
    cursor_focused: bool,
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
                if !cursor_focused {
                    if has_bg || f.inverse {
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
                    let cursor_f = vt_rgbf(cursor_c);
                    scene.round_rect(
                        x0,
                        y0,
                        x1,
                        y0 + m.ch,
                        1.0,
                        [0.0; 4],
                        1.0,
                        [cursor_f[0], cursor_f[1], cursor_f[2], 0.34],
                        clip_id,
                    );
                } else {
                    match screen.cursor.shape {
                        blaze_vt::screen::CursorShape::Bar => {
                            if has_bg || f.inverse {
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
                            scene.over_round_rect(
                                x0,
                                y0 + 1.0,
                                x0 + 2.0,
                                y0 + m.ch - 1.0,
                                1.0,
                                vt_rgbf(cursor_c),
                                clip_id,
                            );
                        }
                        blaze_vt::screen::CursorShape::Underline => {
                            if has_bg || f.inverse {
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
                            scene.over_round_rect(
                                x0,
                                y0 + m.ch - 2.2,
                                x1,
                                y0 + m.ch,
                                0.75,
                                vt_rgbf(cursor_c),
                                clip_id,
                            );
                        }
                        blaze_vt::screen::CursorShape::BlockHollow => {
                            if has_bg || f.inverse {
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
                            scene.round_rect(
                                x0,
                                y0,
                                x1,
                                y0 + m.ch,
                                1.0,
                                [0.0; 4],
                                1.0,
                                vt_rgbf(cursor_c),
                                clip_id,
                            );
                        }
                        blaze_vt::screen::CursorShape::Block => {
                            scene.round_rect(
                                x0,
                                y0,
                                x1,
                                y0 + m.ch,
                                1.0,
                                vt_rgbf(cursor_c),
                                0.0,
                                [0.0; 4],
                                clip_id,
                            );
                            fg = bg0;
                        }
                    }
                }
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
            // Registered Blaze Terminal API custom symbol (`U+E100..=U+EFFF`): renders crisp
            // Retina SVG, in-memory font glyph slice, PNG, or composite text directly in the grid!
            if let Some(sym) = crate::term_server::lookup_global_codepoint(cp) {
                let sym_w = (m.cw * sym.width_cells.max(1) as f32).max(m.cw);
                let sym_h = m.ch;
                let tint = if sym.monochrome { Some(color) } else { None };
                scene.custom_symbol(x0, y0, sym_w, sym_h, sym.name, tint, color[3], clip_id);
                continue;
            }
            // Custom pixel-exact GPU SDF renderer for Unicode block elements (`U+2580..=U+259F`)
            // and box-drawing glyphs (`U+2500..=U+257F`): eliminates CoreText subpixel grid seams
            // in TUI art (Claude Code mascot, progress bars, btop graphs, borders).
            if draw_custom_terminal_glyph(scene, cp, x0, y0, x1, y0 + m.ch, color, clip_id) {
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

/// Custom GPU SDF vector renderer for Unicode block elements (`U+2580..=U+259F`) and box-drawing
/// characters (`U+2500..=U+257F`).
///
/// Bleeds cell-boundary edges by `0.38px` to counter the SDF shader's `±0.5px` linear edge feather
/// so adjacent `█` / quadrant / box cells fuse with zero visible dark grid hairlines.
#[allow(clippy::too_many_arguments)]
fn draw_custom_terminal_glyph(
    scene: &mut Scene,
    cp: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: [f32; 4],
    clip_id: u32,
) -> bool {
    const BLEED: f32 = 0.38;
    let w = x1 - x0;
    let h = y1 - y0;

    // Emit a fractional sub-rectangle `[fx0..fx1, fy0..fy1]` in `[0.0, 1.0]^2`, bleeding only
    // edges that touch the cell boundary so adjacent block cells join with alpha == 1.0.
    let mut emit_frac = |fx0: f32, fy0: f32, fx1: f32, fy1: f32, alpha_scale: f32| {
        let mut c = color;
        c[3] *= alpha_scale;
        let rx0 = x0 + w * fx0 - if fx0 <= 0.001 { BLEED } else { 0.0 };
        let ry0 = y0 + h * fy0 - if fy0 <= 0.001 { BLEED } else { 0.0 };
        let rx1 = x0 + w * fx1 + if fx1 >= 0.999 { BLEED } else { 0.0 };
        let ry1 = y0 + h * fy1 + if fy1 >= 0.999 { BLEED } else { 0.0 };
        scene.over_round_rect(rx0, ry0, rx1, ry1, 0.0, c, clip_id);
    };

    // 1. Block Elements (`U+2580..=U+259F`)
    if (0x2580..=0x259F).contains(&cp) {
        match cp {
            0x2580 => emit_frac(0.0, 0.0, 1.0, 0.5, 1.0),                 // ▀ Upper half
            0x2581..=0x2587 => {
                let frac = (cp - 0x2580) as f32 / 8.0;                    // ▁..▇ Lower 1/8..7/8
                emit_frac(0.0, 1.0 - frac, 1.0, 1.0, 1.0);
            }
            0x2588 => emit_frac(0.0, 0.0, 1.0, 1.0, 1.0),                 // █ Full block
            0x2589..=0x258F => {
                let frac = (0x2590 - cp) as f32 / 8.0;                    // ▉..▏ Left 7/8..1/8
                emit_frac(0.0, 0.0, frac, 1.0, 1.0);
            }
            0x2590 => emit_frac(0.5, 0.0, 1.0, 1.0, 1.0),                 // ▐ Right half
            0x2591 => emit_frac(0.0, 0.0, 1.0, 1.0, 0.25),                // ░ Light shade
            0x2592 => emit_frac(0.0, 0.0, 1.0, 1.0, 0.50),                // ▒ Medium shade
            0x2593 => emit_frac(0.0, 0.0, 1.0, 1.0, 0.75),                // ▓ Dark shade
            0x2594 => emit_frac(0.0, 0.0, 1.0, 0.125, 1.0),               // ▔ Upper 1/8
            0x2595 => emit_frac(0.875, 0.0, 1.0, 1.0, 1.0),               // ▕ Right 1/8
            // Quadrant blocks (`▖`..`▟`, bitmask: UL=1, UR=2, LL=4, LR=8).
            0x2596..=0x259F => {
                let mask: u8 = match cp {
                    0x2596 => 0b0100, // ▖ LL
                    0x2597 => 0b1000, // ▗ LR
                    0x2598 => 0b0001, // ▘ UL
                    0x2599 => 0b1101, // ▙ UL + LL + LR
                    0x259A => 0b1001, // ▚ UL + LR
                    0x259B => 0b0111, // ▛ UL + UR + LL
                    0x259C => 0b1011, // ▜ UL + UR + LR
                    0x259D => 0b0010, // ▝ UR
                    0x259E => 0b0110, // ▞ UR + LL
                    0x259F => 0b1110, // ▟ UR + LL + LR
                    _ => 0,
                };
                // Merge adjacent quadrants into larger single rects when possible to avoid interior seams!
                let ul = (mask & 1) != 0;
                let ur = (mask & 2) != 0;
                let ll = (mask & 4) != 0;
                let lr = (mask & 8) != 0;
                if ul && ur {
                    emit_frac(0.0, 0.0, 1.0, 0.5, 1.0);
                    if ll {
                        emit_frac(0.0, 0.48, 0.5, 1.0, 1.0);
                    }
                    if lr {
                        emit_frac(0.5, 0.48, 1.0, 1.0, 1.0);
                    }
                } else if ll && lr {
                    emit_frac(0.0, 0.5, 1.0, 1.0, 1.0);
                    if ul {
                        emit_frac(0.0, 0.0, 0.5, 0.52, 1.0);
                    }
                    if ur {
                        emit_frac(0.5, 0.0, 1.0, 0.52, 1.0);
                    }
                } else {
                    if ul { emit_frac(0.0, 0.0, 0.5, 0.5, 1.0); }
                    if ur { emit_frac(0.5, 0.0, 1.0, 0.5, 1.0); }
                    if ll { emit_frac(0.0, 0.5, 0.5, 1.0, 1.0); }
                    if lr { emit_frac(0.5, 0.5, 1.0, 1.0, 1.0); }
                }
            }
            _ => return false,
        }
        return true;
    }

    // 2. Box Drawing (`U+2500..=U+257F`): crisp pixel-exact joined strokes.
    // Arm weights: 0 = none, 1 = light, 2 = heavy, 3 = double.
    // Tuple: `(left, right, up, down)`.
    if (0x2500..=0x257F).contains(&cp) {
        let arms: Option<(u8, u8, u8, u8)> = match cp {
            0x2500 => Some((1, 1, 0, 0)), // ─
            0x2501 => Some((2, 2, 0, 0)), // ━
            0x2502 => Some((0, 0, 1, 1)), // │
            0x2503 => Some((0, 0, 2, 2)), // ┃
            0x250C | 0x256D => Some((0, 1, 0, 1)), // ┌ ╭
            0x250D => Some((0, 2, 0, 1)),
            0x250E => Some((0, 1, 0, 2)),
            0x250F => Some((0, 2, 0, 2)), // ┏
            0x2510 | 0x256E => Some((1, 0, 0, 1)), // ┐ ╮
            0x2511 => Some((2, 0, 0, 1)),
            0x2512 => Some((1, 0, 0, 2)),
            0x2513 => Some((2, 0, 0, 2)), // ┓
            0x2514 | 0x2570 => Some((0, 1, 1, 0)), // └ ╰
            0x2515 => Some((0, 2, 1, 0)),
            0x2516 => Some((0, 1, 2, 0)),
            0x2517 => Some((0, 2, 2, 0)), // ┗
            0x2518 | 0x256F => Some((1, 0, 1, 0)), // ┘ ╯
            0x2519 => Some((2, 0, 1, 0)),
            0x251A => Some((1, 0, 2, 0)),
            0x251B => Some((2, 0, 2, 0)), // ┛
            0x251C => Some((0, 1, 1, 1)), // ├
            0x2523 => Some((0, 2, 2, 2)), // ┣
            0x2524 => Some((1, 0, 1, 1)), // ┤
            0x252B => Some((2, 0, 2, 2)), // ┫
            0x252C => Some((1, 1, 0, 1)), // ┬
            0x2533 => Some((2, 2, 0, 2)), // ┳
            0x2534 => Some((1, 1, 1, 0)), // ┴
            0x253B => Some((2, 2, 2, 0)), // ┻
            0x253C => Some((1, 1, 1, 1)), // ┼
            0x254B => Some((2, 2, 2, 2)), // ╋
            0x2550 => Some((3, 3, 0, 0)), // ═
            0x2551 => Some((0, 0, 3, 3)), // ║
            0x2554 => Some((0, 3, 0, 3)), // ╔
            0x2557 => Some((3, 0, 0, 3)), // ╗
            0x255A => Some((0, 3, 3, 0)), // ╚
            0x255D => Some((3, 0, 3, 0)), // ╝
            0x2560 => Some((0, 3, 3, 3)), // ╠
            0x2563 => Some((3, 0, 3, 3)), // ╣
            0x2566 => Some((3, 3, 0, 3)), // ╦
            0x2569 => Some((3, 3, 3, 0)), // ╩
            0x256C => Some((3, 3, 3, 3)), // ╬
            0x2574 => Some((1, 0, 0, 0)), // ╴
            0x2575 => Some((0, 0, 1, 0)), // ╵
            0x2576 => Some((0, 1, 0, 0)), // ╶
            0x2577 => Some((0, 0, 0, 1)), // ╷
            0x2578 => Some((2, 0, 0, 0)), // ╸
            0x2579 => Some((0, 0, 2, 0)), // ╹
            0x257A => Some((0, 2, 0, 0)), // ╺
            0x257B => Some((0, 0, 0, 2)), // ╻
            _ => None,
        };
        if let Some((l, r, u, d)) = arms {
            let cx = x0 + w * 0.5;
            let cy = y0 + h * 0.5;
            let light = (w * 0.13).clamp(1.0, 1.5);
            let heavy = (light * 1.9).clamp(2.0, 3.0);
            let gap = (light * 1.15).clamp(1.2, 2.0);
            let stroke_w = |kind: u8| if kind == 2 { heavy } else { light };

            let mut draw_bar = |bx0: f32, by0: f32, bx1: f32, by1: f32| {
                scene.over_round_rect(bx0, by0, bx1, by1, 0.0, color, clip_id);
            };

            // Horizontal arms
            if l == 3 || r == 3 {
                let hw = light * 0.5;
                let lx0 = if l == 3 { x0 - BLEED } else { cx - gap - hw };
                let rx1 = if r == 3 { x1 + BLEED } else { cx + gap + hw };
                draw_bar(lx0, cy - gap - hw, rx1, cy - gap + hw);
                draw_bar(lx0, cy + gap - hw, rx1, cy + gap + hw);
            } else {
                if l > 0 {
                    let hw = stroke_w(l) * 0.5;
                    let end_x = if r > 0 || u > 0 || d > 0 { cx + hw } else { cx };
                    draw_bar(x0 - BLEED, cy - hw, end_x, cy + hw);
                }
                if r > 0 {
                    let hw = stroke_w(r) * 0.5;
                    let start_x = if l > 0 || u > 0 || d > 0 { cx - hw } else { cx };
                    draw_bar(start_x, cy - hw, x1 + BLEED, cy + hw);
                }
            }

            // Vertical arms
            if u == 3 || d == 3 {
                let hw = light * 0.5;
                let uy0 = if u == 3 { y0 - BLEED } else { cy - gap - hw };
                let dy1 = if d == 3 { y1 + BLEED } else { cy + gap + hw };
                draw_bar(cx - gap - hw, uy0, cx - gap + hw, dy1);
                draw_bar(cx + gap - hw, uy0, cx + gap + hw, dy1);
            } else {
                if u > 0 {
                    let hw = stroke_w(u) * 0.5;
                    let end_y = if d > 0 || l > 0 || r > 0 { cy + hw } else { cy };
                    draw_bar(cx - hw, y0 - BLEED, cx + hw, end_y);
                }
                if d > 0 {
                    let hw = stroke_w(d) * 0.5;
                    let start_y = if u > 0 || l > 0 || r > 0 { cy - hw } else { cy };
                    draw_bar(cx - hw, start_y, cx + hw, y1 + BLEED);
                }
            }
            return true;
        }
    }

    false
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
            VimMode::Visual | VimMode::VisualLine => (theme::BG_SELECTED, theme::INK_1),
            VimMode::Insert => (theme::BG_CHIP, theme::INK_2),
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

    let prompt_col = if prompt.last_exit != 0 {
        theme::DANGER
    } else if vim_mode == VimMode::Normal {
        theme::INK_3
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
        let h = layout_highlighted_source(
            scene,
            &editor.text,
            &spans,
            Some(&editor.abbreviations),
            Some(prompt.cwd.as_str()),
            Some(editor.cursor),
            !editor.completions.is_empty(),
            text_x,
            input_y,
            x1,
            m,
            clip_id,
        );

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

/// Non-modal, zero-Tab live inline hint strip rendered directly beneath the active prompt line
/// as the user types:
/// - `LiveHint::Signature`: 1-line IDE signature (`MonoSmall`) + calm description (SF Pro `UiSmall`)
///   + clickable flag chips + quiet `Tab` cue.
/// - `LiveHint::Candidates`: 1–5 row calm preview card of matching subcommands/flags + descriptions
///   (clickable, never hijacking `Enter`, `↑`/`↓`, or `→` Fish ghost text).
#[allow(clippy::too_many_arguments)]
fn layout_live_hint_strip(
    scene: &mut Scene,
    hint: &LiveHint,
    editor: &Editor,
    x0: f32,
    prompt_y0: f32,
    prompt_y1: f32,
    vh: f32,
    x1: f32,
    m: CellMetrics,
    mouse_pos: Option<[f32; 2]>,
) {
    let max_avail_w = (x1 - x0).max(160.0);
    let glass_bg = theme::rgba(0x16, 0x16, 0x1a, 0.94);

    match hint {
        LiveHint::Signature { anchor_byte: _, signature, about, flags } => {
            let card_h = m.ch + 10.0;
            let pad_x = 10.0;
            let sig_w = m.measure(signature, TextFace::MonoSmall);
            let tab_lbl = "Tab";
            let tab_w = m.measure(tab_lbl, TextFace::MonoSmall) + 10.0;
            let sep_w = m.measure(" · ", TextFace::UiSmall);
            let about_full_w = if about.is_empty() { 0.0 } else { m.measure(about, TextFace::UiSmall) };

            // Measure compact clickable flag chips so the bar hugs its content cleanly.
            let mut chips_total_w = 0.0f32;
            let mut chip_specs: Vec<(&str, &str, f32)> = Vec::with_capacity(flags.len().min(4));
            for (flag, short_doc) in flags.iter().take(4) {
                let fw = m.measure(flag, TextFace::MonoSmall);
                let dw = if short_doc.is_empty() {
                    0.0
                } else {
                    4.0 + m.measure(short_doc, TextFace::UiSmall)
                };
                let w = fw + dw + 10.0;
                chip_specs.push((flag.as_str(), short_doc.as_str(), w));
                chips_total_w += w + 5.0;
            }
            if !chip_specs.is_empty() {
                chips_total_w += 6.0;
            }

            let desired_w = (pad_x * 2.0
                + sig_w
                + if about.is_empty() { 0.0 } else { sep_w + about_full_w }
                + chips_total_w
                + 8.0
                + tab_w)
                .clamp(220.0, max_avail_w);
            let px0 = scene.composer_text_origin[0].clamp(x0, (x1 - desired_w).max(x0));
            let px1 = (px0 + desired_w).min(x1);
            let below_y0 = prompt_y1 + 5.0;
            let py0 = if below_y0 + card_h <= vh - 10.0 {
                below_y0.max(TOP_BAR_H + 8.0)
            } else {
                (prompt_y0 - 6.0 - card_h).max(TOP_BAR_H + 8.0)
            };
            let py1 = py0 + card_h;

            scene.overlay_shadow(px0, py0 + 3.0, px1, py1 + 3.0, 6.0, 12.0, theme::SHADOW_SOFT, 0);
            scene.overlay_round_rect(px0, py0, px1, py1, 6.0, glass_bg, 1.0, theme::LINE, 0);

            let ty = py0 + (card_h - m.ch) * 0.5;
            let mut cx = px0 + pad_x;

            // Right-reserved `Tab` badge (only when there's comfortable room).
            let show_tab = px1 - px0 >= 280.0;
            let right_limit = if show_tab { px1 - pad_x - tab_w - 6.0 } else { px1 - pad_x };

            // 1. Monospace usage signature.
            let max_sig_chars = (((right_limit - cx).max(40.0)) / (m.cw * 0.84)).floor() as usize;
            let sig_txt = truncate_chars(signature, max_sig_chars.max(8));
            let sig_draw_w = m.measure(&sig_txt, TextFace::MonoSmall);
            scene.overlay_mono_small(cx, ty, sig_txt, theme::INK_1, true, false, 0);
            cx += sig_draw_w;

            // 2. Calm SF Pro 1-line description (reserve room for at least 1 flag chip if present).
            let min_chip_reserve = chip_specs.first().map(|c| c.2 + 12.0).unwrap_or(0.0);
            if !about.is_empty() && cx + sep_w + 48.0 < right_limit - min_chip_reserve {
                scene.overlay_ui_small(cx, ty, " · ", theme::INK_4, false, false, 0);
                cx += sep_w;
                let about_budget = (right_limit - min_chip_reserve - cx).max(40.0);
                let max_about_chars = (about_budget / (m.cw * 0.62)).floor() as usize;
                let about_txt = truncate_chars(about, max_about_chars.max(10));
                let about_draw_w = m.measure(&about_txt, TextFace::UiSmall);
                scene.overlay_ui_small(cx, ty, about_txt, theme::INK_2, false, false, 0);
                cx += about_draw_w;
            }

            // 3. Clickable compact flag chips (`MonoSmall` flag + optional short `UiSmall` gloss).
            if !chip_specs.is_empty() {
                cx += 10.0;
                let chip_y0 = py0 + 3.0;
                let chip_y1 = py1 - 3.0;
                for (flag, short_doc, full_chip_w) in chip_specs {
                    let flag_only_w = m.measure(flag, TextFace::MonoSmall) + 10.0;
                    let (chip_w, show_short) = if cx + full_chip_w <= right_limit {
                        (full_chip_w, !short_doc.is_empty())
                    } else if cx + flag_only_w <= right_limit {
                        (flag_only_w, false)
                    } else {
                        break;
                    };
                    let rect = HitRect::new(cx, chip_y0, cx + chip_w, chip_y1);
                    let hovered = mouse_pos.is_some_and(|p| rect.contains(p[0], p[1]));
                    scene.overlay_round_rect(
                        rect.x0,
                        rect.y0,
                        rect.x1,
                        rect.y1,
                        4.0,
                        if hovered { theme::COMPLETION_SEL_BG } else { theme::BG_CHIP },
                        1.0,
                        if hovered { theme::LINE_STRONG } else { theme::LINE },
                        0,
                    );
                    let fx = cx + 5.0;
                    scene.overlay_mono_small(
                        fx,
                        ty,
                        flag,
                        if hovered { theme::INK_1 } else { theme::CMD_BUILTIN },
                        false,
                        false,
                        0,
                    );
                    if show_short {
                        let fw = m.measure(flag, TextFace::MonoSmall);
                        scene.overlay_ui_small(
                            fx + fw + 4.0,
                            ty,
                            short_doc,
                            theme::INK_3,
                            false,
                            false,
                            0,
                        );
                    }
                    scene.push_hit(
                        rect,
                        HitAction::InsertLiveHint {
                            replace_start: editor.cursor,
                            insert: flag.to_owned(),
                        },
                    );
                    cx += chip_w + 5.0;
                }
            }

            // 4. Quiet right-aligned `Tab` hint pill.
            if show_tab {
                let tx0 = px1 - pad_x - tab_w;
                scene.overlay_round_rect(
                    tx0,
                    py0 + 4.0,
                    px1 - pad_x,
                    py1 - 4.0,
                    3.5,
                    theme::rgba(0xff, 0xff, 0xff, 0.04),
                    1.0,
                    theme::LINE,
                    0,
                );
                scene.overlay_mono_small(tx0 + 5.0, ty, tab_lbl, theme::INK_4, false, false, 0);
            }
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
    now_ns: i64,
) {
    let total = editor.completions.len();
    if total == 0 {
        return;
    }
    let show = total.min(COMPLETION_VISIBLE_ROWS);
    let max_scroll = total.saturating_sub(show);
    let win_start = editor.completion_scroll.min(max_scroll);
    let has_sb = total > show;
    let sb_pad = if has_sb { 10.0 } else { 0.0 };
    let row_h = m.ch + 8.0;
    let doc_h = m.ch + 12.0;
    let popup_h = show as f32 * row_h + doc_h + 10.0;

    // Precompute live filesystem recency (`mtime_ns` + recursive `blaze-dust` `max_mtime_ns`),
    // progressive byte sizes, and unified right-hand column widths so every row's recency age,
    // size bar, and size label snap to crisp vertical columns!
    struct RowFsVisual {
        bytes: Option<u64>,
        bytes_done: bool,
        detail_txt: String,
        age_txt: Option<String>,
        heat: Option<([f32; 4], f32)>,
    }
    let format_compact_age = |age_secs: u64| -> String {
        if age_secs < 60 {
            format!("{}s", age_secs.max(1))
        } else if age_secs < 3_600 {
            format!("{}m", age_secs / 60)
        } else if age_secs < 86_400 {
            format!("{}h", age_secs / 3_600)
        } else if age_secs < 86_400 * 30 {
            format!("{}d", age_secs / 86_400)
        } else if age_secs < 86_400 * 365 {
            format!("{}mo", age_secs / (86_400 * 30))
        } else {
            format!("{}y", age_secs / (86_400 * 365))
        }
    };
    let mut row_vis: Vec<RowFsVisual> = Vec::with_capacity(show);
    let mut max_bytes: u64 = 0;
    let mut any_size_bar = false;
    let mut any_age = false;
    let mut max_detail_w = 0.0f32;
    for offset in 0..show {
        let item = &editor.completions[win_start + offset];
        let mut bytes = item.fs_meta.as_ref().and_then(|f| f.size_bytes);
        let mut bytes_done = true;
        let mut bytes_capped = false;
        let mut mtime_ns = item.fs_meta.as_ref().and_then(|f| f.mtime_ns);
        if item.kind == CompletionKind::Dir
            && let Some(fs) = item.fs_meta.as_ref()
            && let Some(prog) = crate::dust_driver::request_dir_size(std::path::Path::new(&fs.path))
        {
            if prog.bytes > 0 {
                bytes = Some(prog.bytes);
                bytes_done = prog.done;
                bytes_capped = prog.capped;
            }
            if prog.max_mtime_ns > 0 {
                mtime_ns = Some(mtime_ns.unwrap_or(0).max(prog.max_mtime_ns));
            }
        }
        if let Some(b) = bytes
            && b > 0
        {
            max_bytes = max_bytes.max(b);
            any_size_bar = true;
        }
        let detail_raw = if item.kind == CompletionKind::Dir
            && let Some(b) = bytes
            && b > 0
        {
            let compact = crate::dust_driver::format_bytes_compact(b);
            let sz_s = if !bytes_done {
                format!("~{compact}")
            } else if bytes_capped {
                format!("{compact}+")
            } else {
                compact
            };
            if matches!(item.detail.as_str(), "dir" | "git dir" | "recent") {
                sz_s
            } else {
                format!("{sz_s} · {}", item.detail)
            }
        } else {
            item.detail.clone()
        };
        let detail_txt = truncate_chars(&detail_raw, 28);
        max_detail_w = max_detail_w.max(m.measure(&detail_txt, TextFace::UiSmall));

        let (age_txt, heat) = if let Some(mt) = mtime_ns
            && mt > 0
            && now_ns > 0
        {
            let age_secs = now_ns.saturating_sub(mt).max(0) as u64 / 1_000_000_000;
            let hcol = theme::recency_heat_color(age_secs);
            let blend = if age_secs < 600 {
                0.42
            } else if age_secs < 7_200 {
                0.28
            } else if age_secs < 86_400 {
                0.15
            } else {
                0.0
            };
            any_age = true;
            (Some(format_compact_age(age_secs)), Some((hcol, blend)))
        } else {
            (None, None)
        };

        row_vis.push(RowFsVisual { bytes, bytes_done, detail_txt, age_txt, heat });
    }
    const COMP_SIZE_BAR_W: f32 = 44.0;
    const COMP_AGE_COL_W: f32 = 28.0;
    let inline_doc_mode = !any_size_bar && !any_age;
    let detail_col_w = if any_size_bar {
        max_detail_w.clamp(58.0, 96.0)
    } else if inline_doc_mode {
        0.0
    } else {
        max_detail_w.min(180.0)
    };
    let icon_s = m.icon_md();
    let max_inline_label_w = if inline_doc_mode {
        (0..show)
            .map(|offset| editor.completions[win_start + offset].label.chars().count() as f32 * m.cw)
            .fold(0.0f32, f32::max)
            .clamp(76.0, 210.0)
    } else {
        0.0
    };
    let popup_w = if inline_doc_mode {
        let max_doc_w = (0..show)
            .map(|offset| m.measure(&editor.completions[win_start + offset].doc, TextFace::UiSmall))
            .fold(0.0f32, f32::max)
            .min(340.0);
        (13.0 + icon_s + 9.0 + max_inline_label_w + 18.0 + max_doc_w + 18.0 + sb_pad)
            .clamp(340.0, (x1 - x0).min(640.0))
    } else {
        (x1 - x0).min(if any_size_bar { 640.0 } else { 560.0 })
    };

    // Anchor horizontally to the start of the active token being completed (rock-steady while typing).
    let tok_start = editor.completion_replace_start.min(editor.text.len());
    let anchor_col = editor.text[..tok_start]
        .rsplit('\n')
        .next()
        .map(|line| line.chars().count())
        .unwrap_or(0);
    let desired_x = scene.composer_text_origin[0] + (anchor_col as f32 * m.cw) - (13.0 + icon_s + 9.0);
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

    let list_y0 = py0 + 5.0;
    let mut ry = list_y0;
    for offset in 0..show {
        let idx = win_start + offset;
        let item = &editor.completions[idx];
        let is_sel = idx == editor.completion_selected;
        if is_sel {
            scene.overlay_round_rect(
                px0 + 5.0,
                ry,
                px1 - 5.0 - sb_pad,
                ry + row_h,
                5.0,
                theme::COMPLETION_SEL_BG,
                0.0,
                [0.0; 4],
                0,
            );
            // Crisp IntelliJ blue left accent rail inside the selected row.
            scene.overlay_round_rect(
                px0 + 5.0,
                ry + 4.0,
                px0 + 7.0,
                ry + row_h - 4.0,
                1.0,
                theme::MATCH_HIGHLIGHT,
                0.0,
                [0.0; 4],
                0,
            );
        }

        let ty = ry + (row_h - m.ch) * 0.5;
        let ix = px0 + 13.0;
        let iy = ry + (row_h - icon_s) * 0.5;

        if let Some(avatar_path) = item.avatar_path.as_deref() {
            scene.overlay_avatar_image(ix, iy, icon_s, avatar_path.to_owned(), 0);
        } else if let Some(svg_name) = item.icon {
            scene.overlay_svg_icon(ix, iy, icon_s, svg_name, 0);
        } else {
            // IntelliJ New UI Dark semantic icons: circular/rounded tinted badges with crisp colored borders & glyphs.
            let (glyph, radius, fill_col, border_col, fg_col) = match item.kind {
                CompletionKind::Variable => (
                    "v",
                    icon_s * 0.5,
                    theme::rgba(0xd1, 0x9a, 0x66, 0.20),
                    theme::rgba(0xd1, 0x9a, 0x66, 0.78),
                    theme::rgb(0xe5, 0xa8, 0x70),
                ),
                CompletionKind::Builtin => (
                    "f",
                    icon_s * 0.5,
                    theme::rgba(0x5c, 0xb8, 0xff, 0.18),
                    theme::rgba(0x5c, 0xb8, 0xff, 0.75),
                    theme::CMD_BUILTIN,
                ),
                CompletionKind::Subcommand => (
                    "s",
                    icon_s * 0.5,
                    theme::rgba(0x56, 0xb6, 0xc2, 0.18),
                    theme::rgba(0x56, 0xb6, 0xc2, 0.75),
                    theme::rgb(0x56, 0xb6, 0xc2),
                ),
                CompletionKind::External => (
                    "›",
                    4.0,
                    theme::rgba(0xe5, 0xc0, 0x7b, 0.16),
                    theme::rgba(0xe5, 0xc0, 0x7b, 0.72),
                    theme::CMD_EXTERNAL,
                ),
                CompletionKind::Abbrev => (
                    "→",
                    icon_s * 0.5,
                    theme::rgba(0xc6, 0x78, 0xdd, 0.20),
                    theme::rgba(0xc6, 0x78, 0xdd, 0.78),
                    theme::CMD_ABBREV,
                ),
                CompletionKind::Def => (
                    "λ",
                    icon_s * 0.5,
                    theme::rgba(0x6a, 0xab, 0x73, 0.20),
                    theme::rgba(0x6a, 0xab, 0x73, 0.78),
                    theme::rgb(0x6a, 0xab, 0x73),
                ),
                CompletionKind::Keyword => (
                    "k",
                    4.0,
                    theme::rgba(0xcf, 0x8e, 0x6d, 0.18),
                    theme::rgba(0xcf, 0x8e, 0x6d, 0.72),
                    theme::rgb(0xcf, 0x8e, 0x6d),
                ),
                CompletionKind::Flag => (
                    "-",
                    4.0,
                    theme::rgba(0x7a, 0xa2, 0xf7, 0.18),
                    theme::rgba(0x7a, 0xa2, 0xf7, 0.70),
                    theme::rgb(0x88, 0xa8, 0xcf),
                ),
                CompletionKind::Column => (
                    "▤",
                    3.5,
                    theme::rgba(0x6a, 0xab, 0x73, 0.18),
                    theme::rgba(0x6a, 0xab, 0x73, 0.70),
                    theme::rgb(0x7c, 0xc9, 0x8b),
                ),
                CompletionKind::Dir => (
                    "d",
                    3.5,
                    theme::rgba(0x39, 0x87, 0xe5, 0.18),
                    theme::rgba(0x39, 0x87, 0xe5, 0.70),
                    theme::SERIES_BLUE,
                ),
                CompletionKind::File => (
                    "·",
                    3.5,
                    theme::BG_TRACK,
                    theme::LINE_STRONG,
                    theme::INK_3,
                ),
                CompletionKind::RemoteRepo => (
                    "⎇",
                    icon_s * 0.5,
                    theme::rgba(0x7c, 0x9c, 0xf5, 0.20),
                    theme::rgba(0x7c, 0x9c, 0xf5, 0.78),
                    theme::rgb(0x9b, 0xb8, 0xff),
                ),
                CompletionKind::RemotePackage => (
                    "◈",
                    4.0,
                    theme::rgba(0xe5, 0xc0, 0x7b, 0.20),
                    theme::rgba(0xe5, 0xc0, 0x7b, 0.78),
                    theme::rgb(0xf0, 0xc8, 0x78),
                ),
                CompletionKind::Hint => (
                    "⌕",
                    icon_s * 0.5,
                    theme::rgba(0x88, 0x96, 0xaa, 0.14),
                    theme::rgba(0x88, 0x96, 0xaa, 0.45),
                    theme::INK_3,
                ),
            };
            scene.overlay_round_rect(ix, iy, ix + icon_s, iy + icon_s, radius, fill_col, 1.0, border_col, 0);
            // Optically center the badge glyph inside `[ix..ix+icon_s, iy..iy+icon_s]` using
            // `UiSmall` bold (sized to breathe cleanly inside the `icon_s` badge) and exact
            // compensation for `UiSmall`'s mono-baseline `y_nudge_px`.
            let cx = ix + icon_s * 0.5;
            let cy = iy + icon_s * 0.5;
            let gw = m.measure_styled(glyph, TextFace::UiSmall, true);
            let gx = cx - gw * 0.5;
            let baseline_offset = match glyph {
                "s" | "v" => 0.625,
                "-" | "·" => 0.645,
                _ => 0.565,
            };
            let gy = cy - m.ch * baseline_offset;
            scene.overlay_ui_small(gx, gy, glyph, fg_col, true, false, 0);
        }

        // Fixed right-hand tabular columns (all baseline-aligned cleanly with the primary label):
        // - In filesystem mode (`any_size_bar || any_age`): `[label] [age] [==== size bar] [size/detail]`
        // - In unified subcommand/flag mode (`inline_doc_mode`): `[label] [inline 1-line description]`
        // Note: `UiSmall` already carries `y_nudge_px = mono_baseline - ui_small_baseline` in
        // `Fonts::rasterize_runs`, so passing `ty` aligns its baseline pixel-for-pixel with `Mono`!
        let vis = &row_vis[offset];
        let ty_small = ty;
        let right_edge = px1 - 14.0 - sb_pad;
        let label_x = ix + icon_s + 9.0;
        let mut col_left;
        if inline_doc_mode {
            let doc_x = label_x + max_inline_label_w + 14.0;
            let show_detail_tag = !vis.detail_txt.is_empty()
                && !matches!(
                    vis.detail_txt.as_str(),
                    "git" | "bgit" | "blaze" | "flag" | "builtin" | "cmd" | "subcommand" | "custom"
                )
                && item.doc != vis.detail_txt;
            let mut doc_right = right_edge;
            if show_detail_tag {
                let dw = m.measure(&vis.detail_txt, TextFace::UiSmall);
                if doc_right - dw > doc_x + 48.0 {
                    scene.overlay_ui_small(
                        doc_right - dw,
                        ty_small,
                        &vis.detail_txt,
                        if is_sel { theme::INK_2 } else { theme::INK_4 },
                        false,
                        false,
                        0,
                    );
                    doc_right -= dw + 10.0;
                }
            }
            if !item.doc.is_empty() && doc_x + 36.0 < doc_right {
                let avg_cw = m.measure("n", TextFace::UiSmall).max(4.5);
                let max_doc_ch = ((doc_right - doc_x) / avg_cw).floor().max(8.0) as usize;
                let doc_txt = truncate_chars(&item.doc, max_doc_ch);
                scene.overlay_ui_small(
                    doc_x,
                    ty_small,
                    doc_txt,
                    if is_sel { theme::INK_2 } else { theme::INK_3 },
                    false,
                    false,
                    0,
                );
            }
            col_left = doc_x - 10.0;
        } else {
            let detail_w = m.measure(&vis.detail_txt, TextFace::UiSmall);
            let detail_x = right_edge - detail_w;
            scene.overlay_ui_small(
                detail_x,
                ty_small,
                &vis.detail_txt,
                if is_sel { theme::INK_2 } else { theme::INK_4 },
                false,
                false,
                0,
            );

            col_left = right_edge - detail_col_w - 10.0;
            if any_size_bar {
                let bar_x0 = col_left - COMP_SIZE_BAR_W;
                col_left = bar_x0 - 12.0;
                if let Some(b) = vis.bytes
                    && b > 0
                    && max_bytes > 0
                {
                    let frac = blaze_dust::bar_fraction(b, max_bytes);
                    let by0 = ry + row_h * 0.5 - 2.0;
                    let by1 = by0 + 4.0;
                    scene.overlay_round_rect(
                        bar_x0,
                        by0,
                        bar_x0 + COMP_SIZE_BAR_W,
                        by1,
                        2.0,
                        theme::BG_TRACK,
                        0.0,
                        [0.0; 4],
                        0,
                    );
                    let fill_w = (COMP_SIZE_BAR_W * frac).max(3.0);
                    let fill_col = if !vis.bytes_done {
                        theme::rgba(0xe5, 0xb5, 0x67, 0.50)
                    } else {
                        match blaze_dust::size_tier(b) {
                            blaze_dust::SizeTier::Bytes => theme::rgba(0x88, 0x90, 0x9e, 0.55),
                            blaze_dust::SizeTier::KiB => theme::rgb(0x58, 0x9d, 0xf6),
                            blaze_dust::SizeTier::MiB => theme::rgb(0xe5, 0xb5, 0x67),
                            blaze_dust::SizeTier::GiB => theme::rgb(0xe8, 0x6a, 0x45),
                        }
                    };
                    scene.overlay_round_rect(
                        bar_x0,
                        by0,
                        bar_x0 + fill_w,
                        by1,
                        2.0,
                        fill_col,
                        0.0,
                        [0.0; 4],
                        0,
                    );
                }
            }

            if any_age {
                if let Some(age_s) = vis.age_txt.as_deref()
                    && let Some((heat_col, _)) = vis.heat
                {
                    let aw = m.measure(age_s, TextFace::UiSmall);
                    scene.overlay_ui_small(col_left - aw, ty_small, age_s, heat_col, false, false, 0);
                }
                col_left -= COMP_AGE_COL_W + 10.0;
            }
        }

        // Label with IntelliJ-style vivid blue matched-character highlighting (`#589DF6`)
        // blended with `ls`-matching recency heat warmth when recently modified.
        let max_label_ch = ((col_left - label_x) / m.cw).floor().max(6.0) as usize;
        let label_txt = truncate_chars(&item.label, max_label_ch);
        let mut base_col = match item.kind {
            CompletionKind::Keyword if !is_sel => theme::rgb(0xd8, 0xa8, 0x8c),
            _ if is_sel => theme::INK_1,
            _ => theme::INK_2,
        };
        if let Some((heat, blend)) = vis.heat
            && blend > 0.0
        {
            base_col = [
                base_col[0] + (heat[0] - base_col[0]) * blend,
                base_col[1] + (heat[1] - base_col[1]) * blend,
                base_col[2] + (heat[2] - base_col[2]) * blend,
                base_col[3],
            ];
        }

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
            HitRect::new(px0 + 5.0, ry, px1 - 5.0 - sb_pad, ry + row_h),
            HitAction::SelectCompletion(idx),
        );
        ry += row_h;
    }

    // Crisp vertical scrollbar thumb when completions exceed visible window.
    if has_sb {
        draw_overlay_scrollbar(scene, px1 - 8.0, list_y0 + 2.0, ry - 2.0, win_start, max_scroll, show, total);
    }

    // Bottom status / Quick-Doc strip in SF Pro + scroll count indicator.
    let div_y = py1 - doc_h;
    scene.overlay_line(px0 + 1.0, div_y, px1 - 1.0, div_y, 1.0, theme::LINE, 0);
    let doc_ty = div_y + (doc_h - m.ch) * 0.5;
    let hint = if has_sb {
        format!("{}/{}  ·  tab complete  ·  ↑↓  ·  esc", editor.completion_selected + 1, total)
    } else {
        "tab complete  ·  ↑↓  ·  esc".to_owned()
    };
    let hint_w = m.measure(&hint, TextFace::UiSmall);
    scene.overlay_ui_small(px1 - 12.0 - hint_w, doc_ty, &hint, theme::INK_4, false, false, 0);

    if !inline_doc_mode {
        if let Some(sel) = editor.completions.get(editor.completion_selected) {
            let avg_cw = m.measure("n", TextFace::UiSmall).max(4.5);
            let max_doc = ((px1 - 24.0 - hint_w - (px0 + 13.0)) / avg_cw).floor().max(8.0) as usize;
            let doc_txt = truncate_chars(&sel.doc, max_doc);
            scene.overlay_ui_small(px0 + 13.0, doc_ty, doc_txt, theme::INK_3, false, false, 0);
        }
    } else if has_sb {
        let rem = total.saturating_sub(show);
        let more_txt = format!("+{rem} more");
        scene.overlay_ui_small(px0 + 13.0, doc_ty, more_txt, theme::INK_4, false, false, 0);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_overlay_scrollbar(
    scene: &mut Scene,
    x: f32,
    y0: f32,
    y1: f32,
    scroll: usize,
    max_scroll: usize,
    visible: usize,
    total: usize,
) {
    let track_h = (y1 - y0).max(16.0);
    let frac = (visible as f32 / total.max(1) as f32).clamp(0.12, 1.0);
    let thumb_h = (track_h * frac).max(18.0).min(track_h);
    let ratio = if max_scroll == 0 {
        0.0
    } else {
        scroll as f32 / max_scroll as f32
    };
    let ty0 = y0 + ratio * (track_h - thumb_h);
    scene.overlay_round_rect(x, y0, x + 3.5, y1, 1.75, theme::rgba(0x28, 0x28, 0x2d, 0.45), 0.0, [0.0; 4], 0);
    scene.overlay_round_rect(x, ty0, x + 3.5, ty0 + thumb_h, 1.75, theme::rgba(0x88, 0x88, 0x92, 0.72), 0.0, [0.0; 4], 0);
}

/// Floating pill rendered under the cursor while dragging a file/folder/crate into the hotbar.
fn layout_drag_overlay(scene: &mut Scene, drag: &ActiveDrag, m: CellMetrics) {
    let icon_s = m.icon_md();
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

/// Native Atuin-class `Ctrl+R` command history palette in serene monochrome dark glass:
/// scored fuzzy search, host/server column, quiet duration/exit column (crimson ONLY on failure),
/// muted timestamps, clickable scope pill (`GLOBAL` / `HOST` / `DIR` / `SESSION`), crisp white
/// match highlights with soft translucent pill, and live command preview.
fn layout_reverse_search_overlay(
    scene: &mut Scene,
    rs: &crate::editor::ReverseSearch,
    vw: f32,
    vh: f32,
    m: CellMetrics,
    now_ns: i64,
) {
    let max_rows = HISTORY_VISIBLE_ROWS;
    let total_matches = rs.results.len();
    let visible = total_matches.min(max_rows).max(1);
    let max_scroll = total_matches.saturating_sub(max_rows);
    let win_start = rs.scroll.min(max_scroll);

    let pad_x = 14.0;
    let header_h = m.ch + 16.0;
    let row_h = m.ch + 7.0;
    let input_h = m.ch + 16.0;
    let preview_h = m.ch + 14.0;
    let list_h = visible as f32 * row_h + 8.0;
    let modal_h = (header_h + list_h + input_h + preview_h).min(vh - TOP_BAR_H - 28.0);
    let modal_w = (vw - 48.0).clamp(240.0, 900.0).min((vw - 24.0).max(200.0));
    let x0 = ((vw - modal_w) * 0.5).max(12.0);
    let x1 = x0 + modal_w;
    let y0 = (TOP_BAR_H + ((vh - TOP_BAR_H - modal_h) * 0.42)).max(TOP_BAR_H + 12.0);
    let y1 = y0 + modal_h;

    // Subtle full-window backdrop dim + elevated serene monochrome dark-glass card.
    scene.overlay_round_rect(0.0, TOP_BAR_H, vw, vh, 0.0, theme::rgba(0x00, 0x00, 0x00, 0.42), 0.0, [0.0; 4], 0);
    scene.overlay_shadow(x0, y0 + 6.0, x1, y1 + 6.0, 10.0, 24.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(x0, y0, x1, y1, 10.0, theme::rgb(0x14, 0x14, 0x17), 1.0, theme::rgb(0x2a, 0x2a, 0x2f), 0);

    // 1. Header bar: SF Pro title, match count, quiet shortcuts.
    let h_ty = y0 + (header_h - m.ch) * 0.5;
    scene.overlay_ui_text(x0 + pad_x, h_ty, "History", theme::INK_1, true, false, 0);
    let count_lbl = format!("  {total_matches} / {}", rs.total_entries);
    let title_w = m.measure("History", TextFace::Ui) + 4.0;
    scene.overlay_ui_small(x0 + pad_x + title_w, h_ty, &count_lbl, theme::INK_4, false, false, 0);
    let hints = "esc exit  ·  tab edit  ·  enter run  ·  ctrl+r scope  ·  scroll";
    let hints_w = m.measure(hints, TextFace::UiSmall);
    if x1 - pad_x - hints_w > x0 + pad_x + title_w + 90.0 {
        scene.overlay_ui_small(x1 - pad_x - hints_w, h_ty, hints, theme::INK_4, false, false, 0);
    }
    scene.overlay_line(x0 + 1.0, y0 + header_h, x1 - 1.0, y0 + header_h, 1.0, theme::LINE, 0);

    // 2. Results list (bottom-up: best/newest match sits directly above the search input!).
    let list_top = y0 + header_h + 4.0;
    let list_bottom = y1 - input_h - preview_h - 4.0;
    if total_matches > max_rows {
        // Bottom-up scroll: `win_start = 0` sits at the bottom, larger `win_start` moves thumb upward.
        let thumb_pos = max_scroll.saturating_sub(win_start);
        draw_overlay_scrollbar(scene, x1 - 7.5, list_top + 2.0, list_bottom - 2.0, thumb_pos, max_scroll, max_rows, total_matches);
    }
    if rs.results.is_empty() {
        let mid_y = (list_top + list_bottom - m.ch) * 0.5;
        scene.overlay_text_run(x0 + pad_x, mid_y, "no matching commands", theme::INK_4, false, true, 0);
    } else {
        let dur_col_w = 7.0 * m.cw;
        let ago_col_w = 9.0 * m.cw;
        let host_col_w = if modal_w > 560.0 { 11.5 * m.cw } else { 0.0 };
        let idx_col_w = 2.4 * m.cw;
        let dir_col_w = if matches!(rs.scope, crate::editor::HistoryScope::Global | crate::editor::HistoryScope::Host)
            && modal_w > 700.0
        {
            15.0 * m.cw
        } else {
            0.0
        };

        let show_count = total_matches.saturating_sub(win_start).min(max_rows);
        for slot in 0..show_count {
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
                    theme::rgb(0x1f, 0x1f, 0x24),
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
                    theme::INK_1,
                    0.0,
                    [0.0; 4],
                    0,
                );
            }

            let mut cx = x0 + pad_x;
            let idx_lbl = if is_sel {
                "❯".to_owned()
            } else if idx > rs.selected && idx - rs.selected <= 9 {
                format!("{}", idx - rs.selected)
            } else {
                String::new()
            };
            if !idx_lbl.is_empty() {
                let col = if is_sel { theme::INK_1 } else { theme::INK_4 };
                scene.overlay_text_run(cx, ty, idx_lbl, col, is_sel, false, 0);
            }
            cx += idx_col_w;

            // Quiet monochrome duration (muted crimson ONLY on non-zero exit code).
            let dur_lbl = format_duration_ms(item.entry.duration_ms);
            let dur_col = if item.entry.exit_code != 0 {
                theme::DANGER
            } else if is_sel {
                theme::INK_2
            } else {
                theme::INK_3
            };
            let dur_x = cx + (dur_col_w - dur_lbl.len() as f32 * m.cw - m.cw).max(0.0);
            scene.overlay_text_run(dur_x, ty, dur_lbl, dur_col, false, false, 0);
            cx += dur_col_w;

            // Quiet relative timestamp in muted neutral grey (no rainbow blue).
            let ago_lbl = if item.entry.timestamp_ns > 0 {
                let mut s = String::new();
                let _ = format_relative_time(
                    DateTime { utc_ns: item.entry.timestamp_ns, offset_secs: 0 },
                    now_ns,
                    &mut s,
                );
                s
            } else {
                "now".to_owned()
            };
            let ago_x = cx + (ago_col_w - ago_lbl.len() as f32 * m.cw - m.cw).max(0.0);
            scene.overlay_text_run(ago_x, ty, ago_lbl, theme::INK_4, false, false, 0);
            cx += ago_col_w;

            // Server / host column (shows which machine ran the command: local Mac vs remote server).
            if host_col_w > 0.0 {
                let host_raw = if item.entry.host.is_empty() { &rs.host } else { &item.entry.host };
                let host_lbl = truncate_chars(host_raw, 10);
                let is_other_host = !rs.host.is_empty() && !host_raw.is_empty() && *host_raw != rs.host;
                let pill_w = (host_lbl.chars().count() as f32 * m.cw + 8.0).min(host_col_w - m.cw);
                if !host_lbl.is_empty() {
                    let pill_bg = if is_other_host {
                        theme::rgb(0x23, 0x23, 0x28)
                    } else {
                        theme::rgb(0x1a, 0x1a, 0x1e)
                    };
                    let pill_fg = if is_other_host { theme::INK_2 } else { theme::INK_4 };
                    scene.overlay_round_rect(
                        cx,
                        ry + 3.0,
                        cx + pill_w,
                        ry + row_h - 3.0,
                        4.0,
                        pill_bg,
                        0.0,
                        [0.0; 4],
                        0,
                    );
                    scene.overlay_text_run(cx + 4.0, ty, host_lbl, pill_fg, false, false, 0);
                }
                cx += host_col_w;
            }

            // Command text with crisp white bold fuzzy-match highlights + subtle soft pill.
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
                let emit_run = |scene: &mut Scene, start: usize, text: String, matched: bool| {
                    let rx = cx + start as f32 * m.cw;
                    if matched {
                        let rw = text.chars().count() as f32 * m.cw;
                        scene.overlay_round_rect(
                            rx - 0.5,
                            ty,
                            rx + rw + 0.5,
                            ty + m.ch,
                            2.5,
                            [1.0, 1.0, 1.0, 0.10],
                            0.0,
                            [0.0; 4],
                            0,
                        );
                    }
                    let col = if matched { [1.0, 1.0, 1.0, 1.0] } else { base_col };
                    scene.overlay_text_run(rx, ty, text, col, matched || is_sel, false, 0);
                };
                for (ci, ch) in shown_cmd.chars().enumerate() {
                    let matched = item.match_indices.contains(&ci);
                    if !run.is_empty() && matched != run_m {
                        emit_run(scene, run_start, core::mem::take(&mut run), run_m);
                        run_start = ci;
                        run_m = matched;
                    } else if run.is_empty() {
                        run_start = ci;
                        run_m = matched;
                    }
                    run.push(ch);
                }
                if !run.is_empty() {
                    emit_run(scene, run_start, run, run_m);
                }
            }

            // Optional right-aligned leaf dir badge in GLOBAL / HOST mode.
            if dir_col_w > 0.0 && !item.entry.cwd.is_empty() {
                let leaf = item.entry.cwd.rsplit('/').next().unwrap_or(&item.entry.cwd);
                let leaf_txt = truncate_chars(leaf, 13);
                let lw = leaf_txt.chars().count() as f32 * m.cw;
                scene.overlay_text_run(x1 - pad_x - lw, ty, leaf_txt, theme::INK_4, false, false, 0);
            }

            scene.push_hit(
                HitRect::new(x0 + 6.0, ry, x1 - 6.0, ry + row_h),
                HitAction::SelectHistory(idx),
            );
        }
    }

    // 3. Search input bar with clickable monochrome `[ GLOBAL | HOST | DIR | SESSION ]` scope pill.
    let in_y0 = y1 - input_h - preview_h;
    scene.overlay_line(x0 + 1.0, in_y0, x1 - 1.0, in_y0, 1.0, theme::LINE, 0);
    let in_ty = in_y0 + (input_h - m.ch) * 0.5;
    let scope_lbl = rs.scope.label();
    let scope_w = scope_lbl.len() as f32 * m.cw + 16.0;
    let sb_x0 = x0 + pad_x;
    let sb_y0 = in_y0 + 5.0;
    let sb_y1 = in_y0 + input_h - 5.0;
    scene.overlay_round_rect(sb_x0, sb_y0, sb_x0 + scope_w, sb_y1, 5.0, theme::rgb(0x1f, 0x1f, 0x24), 1.0, theme::LINE_STRONG, 0);
    scene.overlay_text_run(sb_x0 + 8.0, in_ty, scope_lbl, theme::INK_1, true, false, 0);
    scene.push_hit(HitRect::new(sb_x0, sb_y0, sb_x0 + scope_w, sb_y1), HitAction::CycleHistoryScope);

    let qx = sb_x0 + scope_w + 12.0;
    scene.overlay_text_run(qx, in_ty, "❯", theme::INK_1, true, false, 0);
    let q_txt_x = qx + 1.8 * m.cw;
    if rs.query.is_empty() {
        scene.overlay_text_run(q_txt_x, in_ty, "fuzzy search command history across servers…", theme::INK_4, false, false, 0);
    } else {
        scene.overlay_text_run(q_txt_x, in_ty, &rs.query, theme::INK_1, false, false, 0);
    }
    let cur_x = q_txt_x + rs.query.chars().count() as f32 * m.cw;
    scene.overlay_round_rect(cur_x, in_ty + 1.0, cur_x + 1.8, in_ty + m.ch - 1.0, 0.9, theme::INK_1, 0.0, [0.0; 4], 0);

    // 4. Bottom full-command preview & host/dir/branch metadata strip.
    let pv_y0 = y1 - preview_h;
    scene.overlay_line(x0 + 1.0, pv_y0, x1 - 1.0, pv_y0, 1.0, theme::LINE_FAINT, 0);
    let pv_ty = pv_y0 + (preview_h - m.ch) * 0.5;
    if let Some(sel) = rs.results.get(rs.selected) {
        let mut meta = String::new();
        if !sel.entry.host.is_empty() {
            meta.push_str(&sel.entry.host);
        }
        if !sel.entry.cwd.is_empty() {
            if !meta.is_empty() {
                meta.push_str(" · ");
            }
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
        let meta_w = (meta.chars().count().min(44) as f32) * m.cw;
        let meta_shown = truncate_chars(&meta, 44);
        if !meta_shown.is_empty() {
            scene.overlay_text_run(x1 - pad_x - meta_w, pv_ty, meta_shown, theme::INK_4, false, false, 0);
        }
        let max_pv = ((x1 - pad_x * 2.0 - meta_w - 14.0) / m.cw).floor().max(12.0) as usize;
        let pv_cmd = truncate_chars(&sel.entry.command.replace('\n', " ↵ "), max_pv);
        scene.overlay_text_run(x0 + pad_x, pv_ty, pv_cmd, theme::INK_3, false, false, 0);
    }
}

/// Count visible rows inside the visual branch commit graph card so popover sizing and
/// rendering stay in exact sync.
fn branch_graph_row_count(rel: &crate::prompt::BranchRelation) -> usize {
    if rel.ahead == 0 && rel.behind == 0 {
        let recent = rel.recent_commits.len().min(3);
        if recent > 0 {
            recent
        } else {
            usize::from(!rel.merge_base_short.is_empty())
        }
    } else {
        let shown_ahead = rel.ahead_commits.len().min(5);
        let more_ahead = usize::from(rel.ahead as usize > shown_ahead);
        let shown_behind = rel.behind_commits.len().min(3);
        let more_behind = usize::from(rel.behind as usize > shown_behind);
        let has_mb = usize::from(!rel.merge_base_short.is_empty());
        shown_ahead + more_ahead + shown_behind + more_behind + has_mb
    }
}

/// Floating native Metal popover rendered when hovering or clicking any prompt capsule segment
/// (branch switcher, uncommitted files, stashes, directory breadcrumbs, toolchains) or icon button.
fn layout_active_popover(
    scene: &mut Scene,
    pop: &ActivePopover,
    vw: f32,
    vh: f32,
    m: CellMetrics,
    mouse_pos: Option<[f32; 2]>,
    now_ns: i64,
) {
    let pad_x = 11.0;
    let pad_y = 8.0;
    let row_h = m.ch + 7.0;
    let hdr_h = m.ch + 7.0;
    let icon_s = m.icon_md();
    let gap = 6.0;

    let is_hovered = |r: HitRect| -> bool {
        mouse_pos.is_some_and(|[mx, my]| r.contains(mx, my))
    };

    // Compute exact popover dimensions per variant before placing relative to `pop.anchor`.
    let (pop_w, pop_h): (f32, f32) = match &pop.kind {
        HoverKind::Tooltip { title, subtitle } => {
            let max_c = title
                .chars()
                .count()
                .max(subtitle.as_ref().map_or(0, |s| s.chars().count()))
                .min(48);
            let w = (max_c as f32 * m.cw + pad_x * 2.0).clamp(110.0, 420.0);
            let h = pad_y * 2.0 + m.ch + if subtitle.is_some() { m.ch + 2.0 } else { 0.0 };
            (w, h)
        }
        HoverKind::GitBranch { branches, relation, .. } => {
            if let Some(rel) = relation {
                let commit_row_h = m.ch + 6.0;
                let graph_rows = branch_graph_row_count(rel);
                let graph_hdr_h = m.ch + 10.0;
                let card_h = if graph_rows > 0 {
                    graph_hdr_h + graph_rows as f32 * commit_row_h + 10.0
                } else {
                    m.ch + 16.0
                };
                let max_branches = if graph_rows > 0 { 6 } else { 10 };
                let shown = branches.len().clamp(1, max_branches);
                let w = if graph_rows > 0 {
                    (vw - 32.0).min(540.0).max(410.0)
                } else {
                    let max_br = branches
                        .iter()
                        .take(shown)
                        .map(|b| b.chars().count())
                        .max()
                        .unwrap_or(10)
                        .max(24)
                        .min(48);
                    (max_br as f32 * m.cw + pad_x * 2.0 + 36.0).clamp(300.0, 450.0)
                };
                let h = (hdr_h + card_h + 8.0 + shown as f32 * row_h + pad_y).min(vh - 36.0);
                (w, h)
            } else {
                let shown = branches.len().clamp(1, 10);
                let max_c = branches
                    .iter()
                    .take(shown)
                    .map(|b| b.chars().count())
                    .max()
                    .unwrap_or(10)
                    .max(24)
                    .min(42);
                let w = (max_c as f32 * m.cw + pad_x * 2.0 + 36.0).clamp(220.0, 380.0);
                let h = hdr_h + shown as f32 * row_h + pad_y;
                (w, h)
            }
        }
        HoverKind::GitStashes { stashes, entries, .. } => {
            if !entries.is_empty() {
                let file_row_h = m.ch + 4.0;
                let mut total_h = hdr_h + pad_y;
                let shown_entries = entries.len().min(5);
                for (idx, ent) in entries.iter().take(shown_entries).enumerate() {
                    total_h += row_h; // stash summary header
                    let shown_files = ent.files.len().min(4);
                    total_h += shown_files as f32 * file_row_h;
                    if ent.total_files as usize > shown_files {
                        total_h += file_row_h;
                    }
                    if idx + 1 < shown_entries {
                        total_h += 6.0; // subtle divider gap between stash cards
                    }
                }
                let w = (vw - 32.0).min(540.0).max(360.0);
                (w, total_h.min(vh - 48.0))
            } else {
                let shown = stashes.len().clamp(1, 8);
                let max_c = stashes
                    .iter()
                    .take(shown)
                    .map(|s| s.chars().count() + 11)
                    .max()
                    .unwrap_or(20)
                    .max(22)
                    .min(58);
                let w = (max_c as f32 * m.cw + pad_x * 2.0).clamp(260.0, 520.0);
                let h = hdr_h + shown as f32 * row_h + pad_y;
                (w, h)
            }
        }
        HoverKind::GitChanges { files, details, total } => {
            let shown = details.len().max(files.len()).clamp(1, 12);
            let hdr_chars = format!("uncommitted · {total}").chars().count() + "click to insert".len() + 6;
            let row_chars = if !details.is_empty() {
                details
                    .iter()
                    .take(shown)
                    .map(|d| d.path.chars().count() + d.status_label.len() + 18)
                    .max()
                    .unwrap_or(28)
            } else {
                files
                    .iter()
                    .take(shown)
                    .map(|(p, _, _)| p.chars().count() + 16)
                    .max()
                    .unwrap_or(28)
            };
            let max_c = row_chars.max(hdr_chars).min(68);
            let w = (max_c as f32 * m.cw + pad_x * 2.0 + icon_s + 18.0).clamp(350.0, 580.0);
            let h = hdr_h + shown as f32 * row_h + pad_y;
            (w, h)
        }
        HoverKind::Directory { cwd } => {
            let parts = parent_breadcrumbs(cwd);
            let shown = parts.len().clamp(1, 7);
            let indent_step = 14.0f32;
            let max_row_w = parts
                .iter()
                .take(shown)
                .enumerate()
                .map(|(i, (lbl, _))| {
                    let indent = i as f32 * indent_step;
                    let right_tag_w = if i + 1 == shown { 9.0 * m.cw } else { 5.0 * m.cw };
                    indent + icon_s + 8.0 + lbl.chars().count().min(36) as f32 * m.cw + right_tag_w
                })
                .fold(0.0f32, f32::max);
            let hdr_min_w = ("directory".len() + "click to cd".len() + 6) as f32 * m.cw;
            let w = (max_row_w.max(hdr_min_w) + pad_x * 2.0 + 8.0).clamp(240.0, 440.0);
            let h = hdr_h + shown as f32 * row_h + pad_y;
            (w, h)
        }
        HoverKind::Toolchains { items } => {
            let shown = items.len().clamp(1, 6);
            let hdr_chars = "project toolchains".len() + "detected".len() + 5;
            let row_chars = items
                .iter()
                .take(shown)
                .map(|&(_, name, marker)| name.chars().count() + marker.chars().count() + 6)
                .max()
                .unwrap_or(20);
            let w = ((hdr_chars.max(row_chars) as f32 * m.cw) + pad_x * 2.0 + icon_s + 12.0)
                .clamp(280.0, 460.0);
            let h = hdr_h + shown as f32 * row_h + pad_y;
            (w, h)
        }
        HoverKind::FileEntry { name, .. } => {
            let w = (name.chars().count() as f32 * m.cw + pad_x * 2.0 + 30.0).clamp(180.0, 380.0);
            (w, hdr_h + row_h + pad_y)
        }
        HoverKind::FilePreview { name, meta, lines, truncated, .. } => {
            let shown = lines.len().clamp(1, 16);
            let max_line_c = lines
                .iter()
                .take(shown)
                .map(|l| {
                    let chars: usize = l.spans.iter().map(|s| s.text.chars().count()).sum();
                    chars + if l.line_no.is_some() { 5 } else { 1 }
                })
                .max()
                .unwrap_or(28)
                .max(name.chars().count() + meta.chars().count() + 6)
                .min(72);
            let w = (max_line_c as f32 * m.cw + pad_x * 2.0 + 16.0)
                .clamp(300.0, 580.0)
                .min((vw - 24.0).max(240.0));
            let line_h = m.ch + 2.0;
            let extra = if *truncated { m.ch + 4.0 } else { 0.0 };
            let h = hdr_h + 6.0 + shown as f32 * line_h + extra + pad_y;
            (w, h)
        }
        HoverKind::DirPreview { name, meta, entries, total_items, hidden_count, .. } => {
            let shown = entries.len().clamp(1, 16);
            let max_name_c = entries
                .iter()
                .take(shown)
                .map(|e| e.name.chars().count() + usize::from(e.is_dir))
                .max()
                .unwrap_or(14)
                .clamp(14, 28);
            let has_kind = entries.iter().take(shown).any(|e| !e.kind_label.is_empty());
            let has_size = entries.iter().take(shown).any(|e| !e.size_label.is_empty());
            let row_chars = max_name_c
                + 4
                + if has_kind { 14 } else { 0 }
                + if has_size { 10 } else { 0 };
            let hdr_chars = name.chars().count() + meta.chars().count() + 7;
            let total_c = row_chars.max(hdr_chars).min(64);
            let w = (total_c as f32 * m.cw + pad_x * 2.0 + icon_s + 14.0)
                .clamp(340.0, 540.0)
                .min((vw - 24.0).max(240.0));
            let dir_row_h = m.ch + 5.0;
            let has_footer = *total_items > shown || *hidden_count > 0;
            let extra = if has_footer { m.ch + 10.0 } else { 0.0 };
            let h = hdr_h + 6.0 + shown as f32 * dir_row_h + extra + pad_y;
            (w, h)
        }
        HoverKind::FolderProcs { summary } => {
            let shown = summary.items.len().clamp(1, 10);
            let w = 616.0f32.min((vw - 24.0).max(340.0));
            let proc_row_h = m.ch * 2.0 + 16.0;
            let foot_h = if summary.items.len() > shown { m.ch + 6.0 } else { 0.0 };
            let h = hdr_h + 4.0 + shown as f32 * proc_row_h + foot_h + pad_y;
            (w, h)
        }
        HoverKind::CommandToken { .. } => (200.0, hdr_h + row_h),
        HoverKind::CommandExplain { title, summary, details, .. } => {
            let base_w = 440.0f32.min((vw - 24.0).max(260.0));
            let title_w = m.measure(title, TextFace::Mono) + 160.0;
            let w = base_w.max(title_w.min(520.0)).min((vw - 24.0).max(260.0));
            let wrap_cols = ((w - pad_x * 2.0) / m.cw).floor().max(24.0) as usize;
            let sum_lines = if summary.is_empty() { 0 } else { wrap_lines(summary, wrap_cols).len().min(4) };
            let mut det_lines = 0usize;
            for (tag, txt) in details.iter().take(6) {
                let rem_cols = wrap_cols.saturating_sub(tag.chars().count() + 4).max(16);
                det_lines += wrap_lines(txt, rem_cols).len().min(2);
            }
            let det_pad = if det_lines > 0 { 6.0 } else { 0.0 };
            let h = hdr_h
                + 8.0
                + sum_lines as f32 * (m.ch + 3.0)
                + det_pad
                + det_lines as f32 * (m.ch + 4.0)
                + pad_y;
            (w, h.max(hdr_h + row_h + pad_y))
        }
    };

    // Place file & directory previews on the RIGHT-HAND SIDE (never above/below, so hovering up/down
    // a file list glides smoothly row-by-row!); place prompt-capsule dropdown menus below their anchor.
    let is_side_preview = matches!(
        pop.kind,
        HoverKind::FilePreview { .. } | HoverKind::DirPreview { .. } | HoverKind::FileEntry { .. }
    );
    let (x0, y0) = if is_side_preview {
        let right_x0 = pop.anchor.x1 + 14.0;
        let sx0 = if right_x0 + pop_w <= vw - 12.0 {
            right_x0
        } else {
            (pop.anchor.x0 - 14.0 - pop_w).max(12.0)
        };
        let sy0 = (pop.anchor.y0 - 4.0).clamp(TOP_BAR_H + 8.0, (vh - pop_h - 10.0).max(TOP_BAR_H + 8.0));
        (sx0, sy0)
    } else {
        let sx0 = pop.anchor.x0.clamp(12.0, (vw - pop_w - 12.0).max(12.0));
        let below_y0 = pop.anchor.y1 + gap;
        let sy0 = if below_y0 + pop_h <= vh - 10.0 {
            below_y0
        } else {
            (pop.anchor.y0 - gap - pop_h).max(TOP_BAR_H + 6.0)
        };
        (sx0, sy0)
    };
    let x1 = x0 + pop_w;
    let y1 = y0 + pop_h;

    scene.popover_rect = Some(HitRect::new(x0, y0, x1, y1));

    // Active-anchor highlight indicator on whichever item/button/segment spawned this overlay,
    // plus a crisp 2px left selection rail & hairline bridge for side file/folder previews.
    let ax0 = pop.anchor.x0 - 2.0;
    let ay0 = pop.anchor.y0 - 1.0;
    let ax1 = pop.anchor.x1 + 2.0;
    let ay1 = pop.anchor.y1 + 1.0;
    if ax1 > ax0 && ay1 > ay0 {
        let is_cmd_explain = matches!(pop.kind, HoverKind::CommandExplain { .. });
        let (a_fill, a_border) = if is_cmd_explain {
            (theme::rgba(0x58, 0x9d, 0xf6, 0.22), theme::rgba(0x7c, 0xb7, 0xff, 0.78))
        } else {
            (theme::rgba(0xff, 0xff, 0xff, 0.08), theme::rgba(0xff, 0xff, 0xff, 0.24))
        };
        scene.overlay_round_rect(ax0, ay0, ax1, ay1, 4.5, a_fill, 1.0, a_border, 0);
        if is_cmd_explain {
            scene.overlay_round_rect(
                ax0 + 2.0,
                ay1 - 1.5,
                ax1 - 2.0,
                ay1,
                0.75,
                theme::rgba(0x7c, 0xb7, 0xff, 0.92),
                0.0,
                [0.0; 4],
                0,
            );
        }
        if is_side_preview {
            scene.overlay_round_rect(
                ax0,
                ay0 + 3.0,
                ax0 + 2.0,
                ay1 - 3.0,
                1.0,
                theme::INK_1,
                0.0,
                [0.0; 4],
                0,
            );
            let mid_y = ((ay0 + ay1) * 0.5).clamp(y0 + 10.0, (y1 - 10.0).max(y0 + 10.0));
            if x0 >= ax1 + 2.0 {
                scene.overlay_line(
                    ax1 + 1.0,
                    mid_y,
                    x0 - 1.0,
                    mid_y,
                    1.0,
                    theme::rgba(0xff, 0xff, 0xff, 0.20),
                    0,
                );
            } else if x1 + 2.0 <= ax0 {
                scene.overlay_line(
                    x1 + 1.0,
                    mid_y,
                    ax0 - 1.0,
                    mid_y,
                    1.0,
                    theme::rgba(0xff, 0xff, 0xff, 0.20),
                    0,
                );
            }
        }
    }

    // Soft shadow + dark glass body + 1px hairline border.
    scene.overlay_round_rect(
        x0 - 2.0,
        y0 + 3.0,
        x1 + 2.0,
        y1 + 5.0,
        10.0,
        theme::rgba(0, 0, 0, 0.42),
        0.0,
        [0.0; 4],
        0,
    );
    scene.overlay_round_rect(
        x0,
        y0,
        x1,
        y1,
        8.0,
        theme::rgb(0x14, 0x14, 0x17),
        1.0,
        theme::rgb(0x2a, 0x2a, 0x2f),
        0,
    );

    let draw_header = |scene: &mut Scene, left: &str, right: &str| -> f32 {
        let hy = y0 + (hdr_h - m.ch) * 0.5;
        scene.overlay_text_run(x0 + pad_x, hy, left, theme::INK_3, true, false, 0);
        if !right.is_empty() {
            let rw = right.chars().count() as f32 * m.cw;
            scene.overlay_text_run(x1 - pad_x - rw, hy, right, theme::INK_4, false, false, 0);
        }
        scene.overlay_line(x0 + 1.0, y0 + hdr_h, x1 - 1.0, y0 + hdr_h, 1.0, theme::LINE_FAINT, 0);
        y0 + hdr_h + 3.0
    };

    match &pop.kind {
        HoverKind::Tooltip { title, subtitle } => {
            let max_c = ((pop_w - pad_x * 2.0) / m.cw).floor().max(8.0) as usize;
            scene.overlay_text_run(x0 + pad_x, y0 + pad_y, truncate_chars(title, max_c), theme::INK_1, false, false, 0);
            if let Some(sub) = subtitle {
                scene.overlay_text_run(
                    x0 + pad_x,
                    y0 + pad_y + m.ch + 2.0,
                    truncate_chars(sub, max_c),
                    theme::INK_3,
                    false,
                    false,
                    0,
                );
            }
        }
        HoverKind::GitBranch { current, branches, relation } => {
            let mut ry = draw_header(scene, "git branches", "click to switch");
            let mut max_branches = 10usize;
            if let Some(rel) = relation {
                let commit_row_h = m.ch + 6.0;
                let graph_rows = branch_graph_row_count(rel);
                if graph_rows > 0 {
                    max_branches = 6;
                }
                let graph_hdr_h = m.ch + 10.0;
                let card_x0 = x0 + 8.0;
                let card_x1 = x1 - 8.0;
                let card_bg = theme::rgb(0x14, 0x14, 0x19);
                let card_h = if graph_rows > 0 {
                    graph_hdr_h + graph_rows as f32 * commit_row_h + 10.0
                } else {
                    m.ch + 16.0
                };
                scene.overlay_round_rect(
                    card_x0,
                    ry,
                    card_x1,
                    ry + card_h,
                    6.0,
                    card_bg,
                    1.0,
                    theme::LINE,
                    0,
                );

                let add_col = theme::rgb(0x57, 0xc7, 0x85);
                let del_col = theme::rgb(0xf0, 0x71, 0x78);
                let behind_col = theme::rgb(0xe8, 0xa8, 0x48);
                let sha_col = theme::rgb(0x74, 0xb9, 0xff);

                // Card header: `vs <base_ref>` on the left + total `+A -D` & ahead/behind pills on the right.
                let inner_x0 = card_x0 + 9.0;
                let inner_x1 = card_x1 - 9.0;
                let row1_y = ry + 6.0;
                let pill_h = m.ch + 2.0;
                let pill_y0 = row1_y - 1.0;

                let mut rx = inner_x1;
                let draw_badge = |scene: &mut Scene,
                                  rx: &mut f32,
                                  label: &str,
                                  fg: [f32; 4],
                                  bg: [f32; 4]| {
                    let bw = label.chars().count() as f32 * m.cw + 10.0;
                    let bx0 = *rx - bw;
                    scene.overlay_round_rect(bx0, pill_y0, *rx, pill_y0 + pill_h, 4.0, bg, 0.0, [0.0; 4], 0);
                    scene.overlay_text_run(bx0 + 5.0, row1_y, label, fg, true, false, 0);
                    *rx = bx0 - 5.0;
                };

                if rel.ahead == 0 && rel.behind == 0 {
                    draw_badge(
                        scene,
                        &mut rx,
                        "up to date",
                        theme::OK,
                        theme::rgba(0x46, 0xc9, 0x7a, 0.14),
                    );
                } else {
                    if rel.behind > 0 {
                        let lbl = if rel.ahead == 0 {
                            format!("↓{} behind", rel.behind)
                        } else {
                            format!("↓{}", rel.behind)
                        };
                        draw_badge(
                            scene,
                            &mut rx,
                            &lbl,
                            behind_col,
                            theme::rgba(0xe5, 0x9b, 0x45, 0.15),
                        );
                    }
                    if rel.ahead > 0 {
                        let lbl = if rel.behind == 0 {
                            format!("↑{} ahead", rel.ahead)
                        } else {
                            format!("↑{}", rel.ahead)
                        };
                        draw_badge(
                            scene,
                            &mut rx,
                            &lbl,
                            theme::OK,
                            theme::rgba(0x46, 0xc9, 0x7a, 0.15),
                        );
                    }
                }

                // Aggregate branch diff `+total_added -total_deleted` right beside the divergence badge.
                if rel.total_deleted > 0 {
                    let s = format!("-{}", rel.total_deleted);
                    let sw = s.len() as f32 * m.cw;
                    rx -= sw + 2.0;
                    scene.overlay_text_run(rx, row1_y, &s, del_col, false, false, 0);
                    rx -= 4.0;
                }
                if rel.total_added > 0 {
                    let s = format!("+{}", rel.total_added);
                    let sw = s.len() as f32 * m.cw;
                    rx -= sw;
                    scene.overlay_text_run(rx, row1_y, &s, add_col, false, false, 0);
                    rx -= 8.0;
                }

                scene.overlay_text_run(inner_x0, row1_y, "vs ", theme::INK_4, false, false, 0);
                let base_x = inner_x0 + 3.0 * m.cw;
                let max_base_c = ((rx - base_x - 4.0) / m.cw).floor().max(6.0) as usize;
                scene.overlay_text_run(
                    base_x,
                    row1_y,
                    truncate_chars(&rel.base_ref, max_base_c),
                    theme::INK_1,
                    true,
                    false,
                    0,
                );

                // Visual GPU vector commit DAG inside the card.
                if graph_rows > 0 {
                    let sep_y = ry + graph_hdr_h;
                    scene.overlay_round_rect(
                        card_x0 + 1.0,
                        sep_y,
                        card_x1 - 1.0,
                        sep_y + 1.0,
                        0.0,
                        theme::LINE_FAINT,
                        0.0,
                        [0.0; 4],
                        0,
                    );

                    let g_top = sep_y + 5.0;
                    let draw_diff_stats = |scene: &mut Scene,
                                           right_x: f32,
                                           ty: f32,
                                           cy: f32,
                                           added: u32,
                                           deleted: u32| -> f32 {
                        let mut cur_rx = right_x;
                        if added == 0 && deleted == 0 {
                            return cur_rx;
                        }
                        // Compact 2-color proportional `+/-` bar (`20px × 3.5px`).
                        let bar_w = 20.0f32;
                        let bar_h = 3.5f32;
                        let bar_x0 = cur_rx - bar_w;
                        let bar_y0 = cy - bar_h * 0.5;
                        let total = (added + deleted) as f32;
                        if added > 0 && deleted > 0 {
                            let gap = 1.0;
                            let avail = bar_w - gap;
                            let aw = ((added as f32 / total) * avail).clamp(2.5, avail - 2.5);
                            scene.overlay_round_rect(bar_x0, bar_y0, bar_x0 + aw, bar_y0 + bar_h, 1.5, add_col, 0.0, [0.0; 4], 0);
                            scene.overlay_round_rect(bar_x0 + aw + gap, bar_y0, cur_rx, bar_y0 + bar_h, 1.5, del_col, 0.0, [0.0; 4], 0);
                        } else if added > 0 {
                            scene.overlay_round_rect(bar_x0, bar_y0, cur_rx, bar_y0 + bar_h, 1.5, add_col, 0.0, [0.0; 4], 0);
                        } else {
                            scene.overlay_round_rect(bar_x0, bar_y0, cur_rx, bar_y0 + bar_h, 1.5, del_col, 0.0, [0.0; 4], 0);
                        }
                        cur_rx = bar_x0 - 6.0;

                        if deleted > 0 {
                            let ds = format!("-{deleted}");
                            let dw = ds.len() as f32 * m.cw;
                            cur_rx -= dw;
                            scene.overlay_text_run(cur_rx, ty, &ds, del_col, false, false, 0);
                            cur_rx -= 5.0;
                        }
                        if added > 0 {
                            let as_ = format!("+{added}");
                            let aw = as_.len() as f32 * m.cw;
                            cur_rx -= aw;
                            scene.overlay_text_run(cur_rx, ty, &as_, add_col, false, false, 0);
                            cur_rx -= 6.0;
                        }
                        cur_rx
                    };

                    let draw_sha_chip = |scene: &mut Scene,
                                         sx: f32,
                                         ty: f32,
                                         sha: &str,
                                         accent: [f32; 4]| -> f32 {
                        let short: String = sha.chars().take(7).collect();
                        let sw = short.chars().count() as f32 * m.cw;
                        let r = HitRect::new(sx - 2.0, ty - 1.0, sx + sw + 2.0, ty + m.ch + 1.0);
                        let bg = if is_hovered(r) {
                            [accent[0], accent[1], accent[2], 0.24]
                        } else {
                            [accent[0], accent[1], accent[2], 0.10]
                        };
                        scene.overlay_round_rect(r.x0, r.y0, r.x1, r.y1, 3.0, bg, 0.0, [0.0; 4], 0);
                        scene.overlay_text_run(sx, ty, &short, accent, false, false, 0);
                        scene.push_hit(r, HitAction::CopyText(sha.to_owned()));
                        sx + sw + 7.0
                    };

                    let draw_dot = |scene: &mut Scene,
                                    cx: f32,
                                    cy: f32,
                                    r: f32,
                                    fill: [f32; 4],
                                    border_w: f32,
                                    border: [f32; 4]| {
                        scene.overlay_round_rect(
                            cx - r,
                            cy - r,
                            cx + r,
                            cy + r,
                            r,
                            fill,
                            border_w,
                            border,
                            0,
                        );
                    };

                    if rel.ahead == 0 && rel.behind == 0 {
                        // Single-trunk recent commits (HEAD == base).
                        let lx0 = inner_x0 + 6.0;
                        let text_x0 = lx0 + 13.0;
                        let count = rel.recent_commits.len().min(3);
                        if count > 0 {
                            if count > 1 {
                                let y_start = g_top + commit_row_h * 0.5;
                                let y_end = g_top + (count - 1) as f32 * commit_row_h + commit_row_h * 0.5;
                                scene.overlay_line(lx0, y_start, lx0, y_end, 1.5, theme::LINE_STRONG, 0);
                            }
                            for (idx, c) in rel.recent_commits.iter().take(count).enumerate() {
                                let row_y = g_top + idx as f32 * commit_row_h;
                                let ty = row_y + (commit_row_h - m.ch) * 0.5;
                                let cy = row_y + commit_row_h * 0.5;
                                if idx == 0 {
                                    draw_dot(scene, lx0, cy, 5.2, theme::rgba(0x4c, 0xc2, 0x7a, 0.24), 0.0, [0.0; 4]);
                                    draw_dot(scene, lx0, cy, 3.1, theme::OK, 0.0, [0.0; 4]);
                                } else {
                                    draw_dot(scene, lx0, cy, 2.7, theme::INK_3, 1.0, card_bg);
                                }
                                let sx = draw_sha_chip(
                                    scene,
                                    text_x0,
                                    ty,
                                    &c.short_sha,
                                    if idx == 0 { theme::OK } else { sha_col },
                                );
                                let right_limit = draw_diff_stats(scene, inner_x1, ty, cy, c.added_lines, c.deleted_lines);
                                let rem_c = ((right_limit - sx - 2.0) / m.cw).floor().max(0.0) as usize;
                                if rem_c >= 3 {
                                    scene.overlay_text_run(
                                        sx,
                                        ty,
                                        truncate_chars(&c.subject, rem_c),
                                        if idx == 0 { theme::INK_1 } else { theme::INK_3 },
                                        idx == 0,
                                        false,
                                        0,
                                    );
                                }
                            }
                        } else if !rel.merge_base_short.is_empty() {
                            let row_y = g_top;
                            let ty = row_y + (commit_row_h - m.ch) * 0.5;
                            let cy = row_y + commit_row_h * 0.5;
                            draw_dot(scene, lx0, cy, 3.4, card_bg, 1.8, sha_col);
                            let sx = draw_sha_chip(scene, text_x0, ty, &rel.merge_base_short, sha_col);
                            let rem_c = ((inner_x1 - sx) / m.cw).floor().max(0.0) as usize;
                            if rem_c >= 3 {
                                scene.overlay_text_run(sx, ty, truncate_chars(&rel.merge_base_subject, rem_c), theme::INK_3, false, false, 0);
                            }
                        }
                    } else {
                        // Diverged or ahead/behind vector DAG:
                        // Lane 0 (`lx0`): base branch (`main`/`master`)
                        // Lane 1 (`lx1`): current `HEAD` branch (when `ahead > 0`)
                        let two_lanes = rel.ahead > 0;
                        let lx0 = inner_x0 + 6.0;
                        let lx1 = if two_lanes { lx0 + 13.0 } else { lx0 };
                        let text_x0 = lx1 + 12.0;

                        let shown_ahead = rel.ahead_commits.len().min(5);
                        let more_ahead = rel.ahead.saturating_sub(shown_ahead as u32);
                        let ahead_rows = shown_ahead + usize::from(more_ahead > 0);

                        let shown_behind = rel.behind_commits.len().min(3);
                        let more_behind = rel.behind.saturating_sub(shown_behind as u32);
                        let behind_rows = shown_behind + usize::from(more_behind > 0);

                        let has_mb = !rel.merge_base_short.is_empty();
                        let mb_idx = ahead_rows + behind_rows;
                        let row_cy = |i: usize| g_top + i as f32 * commit_row_h + commit_row_h * 0.5;

                        // 1. Draw vector rails first so circular commit dots render crisply on top.
                        if two_lanes && ahead_rows > 0 {
                            let y_first = row_cy(0);
                            let y_last_branch = row_cy(ahead_rows + behind_rows - 1);
                            if ahead_rows + behind_rows > 1 {
                                scene.overlay_line(lx1, y_first, lx1, y_last_branch, 1.75, theme::OK, 0);
                            }
                            if has_mb {
                                // Smooth diagonal fork edge from Lane 1 (HEAD) into Lane 0 (merge-base).
                                scene.overlay_line(lx1, y_last_branch, lx0, row_cy(mb_idx), 1.75, theme::OK, 0);
                            }
                        }
                        if behind_rows > 0 {
                            let y_b0 = row_cy(ahead_rows);
                            let y_b_end = if has_mb {
                                row_cy(mb_idx)
                            } else {
                                row_cy(ahead_rows + behind_rows - 1)
                            };
                            if y_b_end > y_b0 {
                                scene.overlay_line(lx0, y_b0, lx0, y_b_end, 1.75, behind_col, 0);
                            }
                        }

                        // 2. Render `ahead` commits on Lane 1 (`HEAD`, emerald).
                        let mut cur_row = 0usize;
                        for (idx, c) in rel.ahead_commits.iter().take(shown_ahead).enumerate() {
                            let row_y = g_top + cur_row as f32 * commit_row_h;
                            let ty = row_y + (commit_row_h - m.ch) * 0.5;
                            let cy = row_y + commit_row_h * 0.5;
                            if idx == 0 {
                                draw_dot(scene, lx1, cy, 5.2, theme::rgba(0x4c, 0xc2, 0x7a, 0.25), 0.0, [0.0; 4]);
                                draw_dot(scene, lx1, cy, 3.2, theme::OK, 0.0, [0.0; 4]);
                            } else {
                                draw_dot(scene, lx1, cy, 2.9, theme::OK, 1.0, card_bg);
                            }
                            let sx = draw_sha_chip(scene, text_x0, ty, &c.short_sha, theme::OK);
                            let right_limit = draw_diff_stats(scene, inner_x1, ty, cy, c.added_lines, c.deleted_lines);
                            let rem_c = ((right_limit - sx - 2.0) / m.cw).floor().max(0.0) as usize;
                            if rem_c >= 3 {
                                scene.overlay_text_run(
                                    sx,
                                    ty,
                                    truncate_chars(&c.subject, rem_c),
                                    if idx == 0 { theme::INK_1 } else { theme::INK_2 },
                                    idx == 0,
                                    false,
                                    0,
                                );
                            }
                            cur_row += 1;
                        }
                        if more_ahead > 0 {
                            let row_y = g_top + cur_row as f32 * commit_row_h;
                            let ty = row_y + (commit_row_h - m.ch) * 0.5;
                            let cy = row_y + commit_row_h * 0.5;
                            draw_dot(scene, lx1, cy, 2.5, card_bg, 1.3, theme::OK);
                            let more_lbl = if more_ahead == 1 {
                                "+1 more commit on HEAD".to_owned()
                            } else {
                                format!("+{more_ahead} more commits on HEAD")
                            };
                            scene.overlay_text_run(text_x0, ty, &more_lbl, theme::INK_4, false, true, 0);
                            cur_row += 1;
                        }

                        // 3. Render `behind` commits on Lane 0 (`<base_ref>`, amber).
                        for (idx, c) in rel.behind_commits.iter().take(shown_behind).enumerate() {
                            let row_y = g_top + cur_row as f32 * commit_row_h;
                            let ty = row_y + (commit_row_h - m.ch) * 0.5;
                            let cy = row_y + commit_row_h * 0.5;
                            if idx == 0 {
                                draw_dot(scene, lx0, cy, 5.0, theme::rgba(0xe8, 0xa8, 0x48, 0.24), 0.0, [0.0; 4]);
                                draw_dot(scene, lx0, cy, 3.1, behind_col, 0.0, [0.0; 4]);
                            } else {
                                draw_dot(scene, lx0, cy, 2.8, behind_col, 1.0, card_bg);
                            }
                            let sx = draw_sha_chip(scene, text_x0, ty, &c.short_sha, behind_col);
                            let right_limit = draw_diff_stats(scene, inner_x1, ty, cy, c.added_lines, c.deleted_lines);
                            let rem_c = ((right_limit - sx - 2.0) / m.cw).floor().max(0.0) as usize;
                            if rem_c >= 3 {
                                scene.overlay_text_run(
                                    sx,
                                    ty,
                                    truncate_chars(&c.subject, rem_c),
                                    theme::INK_3,
                                    false,
                                    false,
                                    0,
                                );
                            }
                            cur_row += 1;
                        }
                        if more_behind > 0 {
                            let row_y = g_top + cur_row as f32 * commit_row_h;
                            let ty = row_y + (commit_row_h - m.ch) * 0.5;
                            let cy = row_y + commit_row_h * 0.5;
                            draw_dot(scene, lx0, cy, 2.5, card_bg, 1.3, behind_col);
                            let more_lbl = if more_behind == 1 {
                                format!("+1 more commit on {}", rel.base_ref)
                            } else {
                                format!("+{more_behind} more commits on {}", rel.base_ref)
                            };
                            scene.overlay_text_run(text_x0, ty, &more_lbl, theme::INK_4, false, true, 0);
                            cur_row += 1;
                        }

                        // 4. Render common ancestor (`merge-base`) anchor node on Lane 0.
                        if has_mb {
                            let row_y = g_top + cur_row as f32 * commit_row_h;
                            let ty = row_y + (commit_row_h - m.ch) * 0.5;
                            let cy = row_y + commit_row_h * 0.5;
                            let (node_col, tag_lbl) = if rel.ahead == 0 {
                                (theme::OK, "HEAD")
                            } else if rel.behind == 0 {
                                (sha_col, rel.base_ref.as_str())
                            } else {
                                (sha_col, "base")
                            };
                            draw_dot(scene, lx0, cy, 3.6, card_bg, 1.8, node_col);

                            // Right-aligned anchor pill (`[main]` / `[base]` / `[HEAD]`).
                            let tag_shown = truncate_chars(tag_lbl, 14);
                            let tag_w = tag_shown.chars().count() as f32 * m.cw + 8.0;
                            let tag_x0 = inner_x1 - tag_w;
                            scene.overlay_round_rect(
                                tag_x0,
                                ty - 1.0,
                                inner_x1,
                                ty + m.ch + 1.0,
                                3.5,
                                [node_col[0], node_col[1], node_col[2], 0.14],
                                0.0,
                                [0.0; 4],
                                0,
                            );
                            scene.overlay_text_run(tag_x0 + 4.0, ty, &tag_shown, node_col, true, false, 0);

                            let mut cx = draw_sha_chip(scene, text_x0, ty, &rel.merge_base_short, sha_col);
                            if rel.merge_base_time_secs > 0 && now_ns > 0 {
                                let mut age = String::new();
                                let _ = format_relative_time(
                                    DateTime {
                                        utc_ns: rel.merge_base_time_secs.saturating_mul(1_000_000_000),
                                        offset_secs: 0,
                                    },
                                    now_ns,
                                    &mut age,
                                );
                                let age_lbl = format!("{age} · ");
                                let age_w = age_lbl.chars().count() as f32 * m.cw;
                                if cx + age_w + 6.0 * m.cw < tag_x0 {
                                    scene.overlay_text_run(cx, ty, &age_lbl, theme::INK_4, false, false, 0);
                                    cx += age_w;
                                }
                            }
                            let rem_c = ((tag_x0 - cx - 6.0) / m.cw).floor().max(0.0) as usize;
                            if rem_c >= 3 && !rel.merge_base_subject.is_empty() {
                                scene.overlay_text_run(
                                    cx,
                                    ty,
                                    truncate_chars(&rel.merge_base_subject, rem_c),
                                    theme::INK_3,
                                    false,
                                    false,
                                    0,
                                );
                            }
                        }
                    }
                }

                ry += card_h + 8.0;
            }
            for br in branches.iter().take(max_branches) {
                if ry + row_h > y1 - 4.0 {
                    break;
                }
                let row_rect = HitRect::new(x0 + 4.0, ry, x1 - 4.0, ry + row_h);
                let is_cur = br == current;
                let is_base = !is_cur && relation.as_ref().is_some_and(|r| r.base_ref == *br);
                if is_hovered(row_rect) {
                    scene.overlay_round_rect(
                        row_rect.x0,
                        row_rect.y0,
                        row_rect.x1,
                        row_rect.y1,
                        5.0,
                        theme::rgb(0x20, 0x20, 0x25),
                        0.0,
                        [0.0; 4],
                        0,
                    );
                }
                let ty = ry + (row_h - m.ch) * 0.5;
                if is_cur {
                    scene.overlay_round_rect(
                        x0 + pad_x + 1.0,
                        ty + m.ch * 0.5 - 2.5,
                        x0 + pad_x + 6.0,
                        ty + m.ch * 0.5 + 2.5,
                        2.5,
                        theme::OK,
                        0.0,
                        [0.0; 4],
                        0,
                    );
                }
                let right_tag = if is_cur {
                    Some("current")
                } else if is_base {
                    Some("base")
                } else {
                    None
                };
                let right_reserve = right_tag.map_or(0.0, |t| (t.len() as f32 + 2.0) * m.cw);
                let max_c = ((pop_w - pad_x * 2.0 - 18.0 - right_reserve) / m.cw)
                    .floor()
                    .max(8.0) as usize;
                let shown_br = truncate_chars(br, max_c);
                draw_styled_branch_name(scene, x0 + pad_x + 14.0, ty, &shown_br, is_cur, true, m, 0);
                if let Some(tag) = right_tag {
                    let tw = tag.len() as f32 * m.cw;
                    scene.overlay_text_run(x1 - pad_x - tw, ty, tag, theme::INK_4, false, false, 0);
                }
                if !is_cur {
                    scene.push_hit(row_rect, HitAction::SwitchGitBranch(br.clone()));
                }
                ry += row_h;
            }
        }
        HoverKind::GitStashes { stashes, entries, total } => {
            let mut ry = draw_header(scene, &format!("git stashes · {total}"), "click to inspect");
            if !entries.is_empty() {
                let file_row_h = m.ch + 4.0;
                let file_icon_s = m.icon_sm();
                let add_col = theme::rgb(0x57, 0xc7, 0x85);
                let del_col = theme::rgb(0xf0, 0x71, 0x78);
                let shown_entries = entries.len().min(5);
                for (ei, ent) in entries.iter().take(shown_entries).enumerate() {
                    if ry + row_h > y1 - 4.0 {
                        break;
                    }
                    let row_rect = HitRect::new(x0 + 4.0, ry, x1 - 4.0, ry + row_h);
                    if is_hovered(row_rect) {
                        scene.overlay_round_rect(
                            row_rect.x0,
                            row_rect.y0,
                            row_rect.x1,
                            row_rect.y1,
                            5.0,
                            theme::rgb(0x20, 0x22, 0x2a),
                            0.0,
                            [0.0; 4],
                            0,
                        );
                    }
                    let ty = ry + (row_h - m.ch) * 0.5;
                    let idx_lbl = format!("@{{{}}}", ent.index);
                    let idx_w = idx_lbl.len() as f32 * m.cw;
                    scene.overlay_round_rect(
                        x0 + pad_x - 2.0,
                        ty,
                        x0 + pad_x + idx_w + 4.0,
                        ty + m.ch,
                        3.5,
                        theme::BG_TRACK,
                        0.0,
                        [0.0; 4],
                        0,
                    );
                    scene.overlay_text_run(x0 + pad_x + 1.0, ty, &idx_lbl, theme::PROMPT_BRANCH, true, false, 0);

                    // Right-aligned total diffstat (`+N -M`).
                    let mut stat_x = x1 - pad_x;
                    if ent.total_removed > 0 {
                        let del_s = format!("-{}", ent.total_removed);
                        let dw = del_s.len() as f32 * m.cw;
                        stat_x -= dw;
                        scene.overlay_text_run(stat_x, ty, del_s, del_col, true, false, 0);
                        stat_x -= 6.0;
                    }
                    if ent.total_added > 0 {
                        let add_s = format!("+{}", ent.total_added);
                        let aw = add_s.len() as f32 * m.cw;
                        stat_x -= aw;
                        scene.overlay_text_run(stat_x, ty, add_s, add_col, true, false, 0);
                        stat_x -= 8.0;
                    }
                    if ent.total_files > 0 {
                        let fc = format!("{}f", ent.total_files);
                        let fw = fc.len() as f32 * m.cw;
                        stat_x -= fw;
                        scene.overlay_text_run(stat_x, ty, fc, theme::INK_4, false, false, 0);
                        stat_x -= 8.0;
                    }

                    let mut tx = x0 + pad_x + idx_w + 10.0;
                    if !ent.branch.is_empty() {
                        let br = truncate_chars(&ent.branch, 16);
                        let bw = br.chars().count() as f32 * m.cw;
                        if tx + bw + 24.0 < stat_x {
                            scene.overlay_text_run(tx, ty, &br, theme::INK_3, false, false, 0);
                            tx += bw + 7.0;
                        }
                    }
                    let max_sum = ((stat_x - tx - 6.0) / m.cw).floor().max(6.0) as usize;
                    scene.overlay_text_run(
                        tx,
                        ty,
                        truncate_chars(&ent.summary, max_sum),
                        theme::INK_1,
                        true,
                        false,
                        0,
                    );
                    scene.push_hit(
                        row_rect,
                        HitAction::InsertToken(format!("git stash show -p stash@{{{}}}", ent.index)),
                    );
                    ry += row_h;

                    // Indented changed files with SVG file icons and green `+N` / red `-N`.
                    let shown_files = ent.files.len().min(4);
                    for f in ent.files.iter().take(shown_files) {
                        if ry + file_row_h > y1 - 4.0 {
                            break;
                        }
                        let fty = ry + (file_row_h - m.ch) * 0.5;
                        let fx = x0 + pad_x + 14.0;
                        scene.overlay_svg_icon(fx, fty + (m.ch - file_icon_s) * 0.5, file_icon_s, f.icon, 0);

                        let mut rx = x1 - pad_x;
                        if f.is_binary {
                            let b = "bin";
                            let bw = b.len() as f32 * m.cw;
                            rx -= bw;
                            scene.overlay_text_run(rx, fty, b, theme::INK_4, false, false, 0);
                            rx -= 8.0;
                        } else {
                            if f.removed > 0 {
                                let rs = format!("-{}", f.removed);
                                let rw = rs.len() as f32 * m.cw;
                                rx -= rw;
                                scene.overlay_text_run(rx, fty, rs, del_col, false, false, 0);
                                rx -= 6.0;
                            }
                            if f.added > 0 {
                                let as_ = format!("+{}", f.added);
                                let aw = as_.len() as f32 * m.cw;
                                rx -= aw;
                                scene.overlay_text_run(rx, fty, as_, add_col, false, false, 0);
                                rx -= 8.0;
                            }
                        }
                        let name_x = fx + file_icon_s + 6.0;
                        let max_fc = ((rx - name_x - 4.0) / m.cw).floor().max(8.0) as usize;
                        scene.overlay_text_run(
                            name_x,
                            fty,
                            truncate_middle(&f.path, max_fc),
                            theme::INK_2,
                            false,
                            false,
                            0,
                        );
                        ry += file_row_h;
                    }
                    if ent.total_files as usize > shown_files && ry + file_row_h <= y1 - 4.0 {
                        let rem = ent.total_files as usize - shown_files;
                        let fty = ry + (file_row_h - m.ch) * 0.5;
                        scene.overlay_text_run(
                            x0 + pad_x + 14.0 + file_icon_s + 6.0,
                            fty,
                            format!("+{rem} more file{}", if rem == 1 { "" } else { "s" }),
                            theme::INK_4,
                            false,
                            true,
                            0,
                        );
                        ry += file_row_h;
                    }
                    if ei + 1 < shown_entries && ry + 4.0 < y1 - 4.0 {
                        scene.overlay_line(x0 + pad_x, ry + 2.5, x1 - pad_x, ry + 2.5, 1.0, theme::LINE_FAINT, 0);
                        ry += 6.0;
                    }
                }
            } else if stashes.is_empty() {
                let ty = ry + (row_h - m.ch) * 0.5;
                scene.overlay_text_run(x0 + pad_x, ty, "no stash descriptions", theme::INK_4, false, true, 0);
            } else {
                for (i, msg) in stashes.iter().take(8).enumerate() {
                    let ty = ry + (row_h - m.ch) * 0.5;
                    let idx_lbl = format!("@{{{i}}}");
                    let idx_w = idx_lbl.len() as f32 * m.cw + 8.0;
                    scene.overlay_text_run(x0 + pad_x, ty, idx_lbl, theme::INK_3, false, false, 0);
                    let max_c = ((pop_w - pad_x * 2.0 - idx_w) / m.cw).floor().max(8.0) as usize;
                    scene.overlay_text_run(
                        x0 + pad_x + idx_w,
                        ty,
                        truncate_chars(msg, max_c),
                        theme::INK_1,
                        false,
                        false,
                        0,
                    );
                    ry += row_h;
                }
            }
        }
        HoverKind::GitChanges { files, details, total } => {
            let mut ry = draw_header(scene, &format!("uncommitted · {total}"), "click to insert");
            if !details.is_empty() {
                for d in details.iter().take(12) {
                    let row_rect = HitRect::new(x0 + 4.0, ry, x1 - 4.0, ry + row_h);
                    if is_hovered(row_rect) {
                        scene.overlay_round_rect(
                            row_rect.x0,
                            row_rect.y0,
                            row_rect.x1,
                            row_rect.y1,
                            5.0,
                            theme::rgb(0x20, 0x20, 0x25),
                            0.0,
                            [0.0; 4],
                            0,
                        );
                    }
                    let ty = ry + (row_h - m.ch) * 0.5;
                    scene.overlay_svg_icon(x0 + pad_x, ty + (m.ch - icon_s) * 0.5, icon_s, d.icon, 0);

                    // Right-aligned status badge (`untracked`, `modified`, `staged`, `deleted`, `conflict`)
                    // plus rich diff stats (`+14 -3`) or file size + kind (`4.2 KiB · Env`)!
                    let (st_fg, st_bg) = match d.tag {
                        "!" => (theme::DANGER, theme::rgba(0xe8, 0x5d, 0x5d, 0.16)),
                        "+" => (theme::OK, theme::rgba(0x57, 0xab, 0x5a, 0.16)),
                        "M" => (theme::rgb(0xe5, 0xb5, 0x67), theme::rgba(0xe5, 0xb5, 0x67, 0.15)),
                        "D" => (theme::DANGER, theme::rgba(0xe8, 0x5d, 0x5d, 0.14)),
                        _ => (theme::rgb(0x7c, 0xa8, 0xe8), theme::rgba(0x58, 0x9d, 0xf6, 0.15)),
                    };
                    let badge_w = m.measure(d.status_label, TextFace::UiSmall) + 10.0;
                    let badge_x0 = x1 - pad_x - badge_w;
                    scene.overlay_round_rect(
                        badge_x0,
                        ry + 3.0,
                        x1 - pad_x,
                        ry + row_h - 3.0,
                        4.0,
                        st_bg,
                        0.0,
                        [0.0; 4],
                        0,
                    );
                    scene.overlay_ui_small(badge_x0 + 5.0, ty, d.status_label, st_fg, false, false, 0);

                    let mut right_edge = badge_x0 - 10.0;
                    if let (Some(add), Some(del)) = (d.added_lines, d.deleted_lines) {
                        if del > 0 {
                            let del_s = format!("-{del}");
                            let dw = del_s.len() as f32 * m.cw;
                            right_edge -= dw;
                            scene.overlay_text_run(right_edge, ty, del_s, theme::DANGER, false, false, 0);
                            right_edge -= 5.0;
                        }
                        if add > 0 {
                            let add_s = format!("+{add}");
                            let aw = add_s.len() as f32 * m.cw;
                            right_edge -= aw;
                            scene.overlay_text_run(right_edge, ty, add_s, theme::OK, false, false, 0);
                            right_edge -= 8.0;
                        }
                    } else {
                        let meta_s = match (d.size_bytes, d.kind_label) {
                            (Some(sz), k) if !k.is_empty() && k != "File" => {
                                format!("{} · {k}", crate::dust_driver::format_bytes_compact(sz))
                            }
                            (Some(sz), _) => crate::dust_driver::format_bytes_compact(sz),
                            (None, k) if !k.is_empty() => k.to_owned(),
                            _ => String::new(),
                        };
                        if !meta_s.is_empty() {
                            let mw = m.measure(&meta_s, TextFace::UiSmall);
                            right_edge -= mw;
                            scene.overlay_ui_small(right_edge, ty, meta_s, theme::INK_4, false, false, 0);
                            right_edge -= 8.0;
                        }
                    }

                    let name_x = x0 + pad_x + icon_s + 7.0;
                    let max_c = ((right_edge - name_x) / m.cw).floor().max(8.0) as usize;
                    let shown_p = if d.path.contains('/') {
                        truncate_middle(&d.path, max_c)
                    } else {
                        truncate_chars(&d.path, max_c)
                    };
                    scene.overlay_text_run(name_x, ty, shown_p, theme::INK_2, false, false, 0);
                    scene.push_hit(row_rect, HitAction::InsertToken(d.path.clone()));
                    ry += row_h;
                }
            } else {
                let max_c = ((pop_w - pad_x * 2.0 - icon_s - 28.0) / m.cw).floor().max(8.0) as usize;
                for (path, icon, tag) in files.iter().take(12) {
                    let row_rect = HitRect::new(x0 + 4.0, ry, x1 - 4.0, ry + row_h);
                    if is_hovered(row_rect) {
                        scene.overlay_round_rect(
                            row_rect.x0,
                            row_rect.y0,
                            row_rect.x1,
                            row_rect.y1,
                            5.0,
                            theme::rgb(0x20, 0x20, 0x25),
                            0.0,
                            [0.0; 4],
                            0,
                        );
                    }
                    let ty = ry + (row_h - m.ch) * 0.5;
                    scene.overlay_svg_icon(x0 + pad_x, ty + (m.ch - icon_s) * 0.5, icon_s, icon, 0);
                    scene.overlay_text_run(
                        x0 + pad_x + icon_s + 7.0,
                        ty,
                        truncate_chars(path, max_c),
                        theme::INK_2,
                        false,
                        false,
                        0,
                    );
                    let tag_col = match *tag {
                        "!" => theme::DANGER,
                        "+" => theme::OK,
                        "M" | "D" => theme::FIRE,
                        _ => theme::INK_3,
                    };
                    scene.overlay_text_run(x1 - pad_x - m.cw, ty, *tag, tag_col, true, false, 0);
                    scene.push_hit(row_rect, HitAction::InsertToken(path.clone()));
                    ry += row_h;
                }
            }
        }
        HoverKind::Directory { cwd } => {
            let mut ry = draw_header(scene, "directory", "click to cd");
            let parts = parent_breadcrumbs(cwd);
            let total = parts.len();
            let indent_step = 14.0f32;
            for (i, (label, full_path)) in parts.iter().enumerate() {
                let is_cur = i + 1 == total;
                let row_rect = HitRect::new(x0 + 4.0, ry, x1 - 4.0, ry + row_h);
                if is_hovered(row_rect) {
                    scene.overlay_round_rect(
                        row_rect.x0,
                        row_rect.y0,
                        row_rect.x1,
                        row_rect.y1,
                        5.0,
                        theme::rgb(0x20, 0x20, 0x25),
                        0.0,
                        [0.0; 4],
                        0,
                    );
                }
                let ty = ry + (row_h - m.ch) * 0.5;
                let cy = ry + row_h * 0.5;
                let ix = x0 + pad_x + i as f32 * indent_step;

                // Subtle GPU vector L-connector linking each nested folder to its parent icon.
                if i > 0 {
                    let parent_cx = x0 + pad_x + (i - 1) as f32 * indent_step + icon_s * 0.5;
                    let top_y = ry - row_h * 0.5 + icon_s * 0.5 + 1.0;
                    scene.overlay_line(parent_cx, top_y, parent_cx, cy, 1.25, theme::LINE_STRONG, 0);
                    scene.overlay_line(parent_cx, cy, ix - 3.0, cy, 1.25, theme::LINE_STRONG, 0);
                }

                scene.overlay_svg_icon(ix, ty + (m.ch - icon_s) * 0.5, icon_s, "folder_macos", 0);
                let depth_up = total.saturating_sub(1 + i);
                let right_tag = if is_cur {
                    "copy".to_owned()
                } else if depth_up == 1 {
                    "..".to_owned()
                } else {
                    format!("↑{depth_up}")
                };
                let tag_w = right_tag.chars().count() as f32 * m.cw;
                let tx = ix + icon_s + 7.0;
                let max_c = ((x1 - pad_x - tag_w - 10.0 - tx) / m.cw).floor().max(6.0) as usize;
                let col = if is_cur { theme::INK_1 } else { theme::INK_2 };
                scene.overlay_text_run(
                    tx,
                    ty,
                    truncate_chars(label, max_c),
                    col,
                    is_cur,
                    false,
                    0,
                );
                let tag_col = if is_cur && is_hovered(row_rect) {
                    theme::INK_2
                } else {
                    theme::INK_4
                };
                scene.overlay_text_run(x1 - pad_x - tag_w, ty, &right_tag, tag_col, false, false, 0);
                if is_cur {
                    scene.push_hit(row_rect, HitAction::CopyText(full_path.clone()));
                } else {
                    scene.push_hit(row_rect, HitAction::ChangeDir(full_path.clone()));
                }
                ry += row_h;
            }
        }
        HoverKind::Toolchains { items } => {
            let mut ry = draw_header(scene, "project toolchains", "detected");
            for &(icon, name, marker) in items.iter().take(6) {
                let ty = ry + (row_h - m.ch) * 0.5;
                scene.overlay_svg_icon(x0 + pad_x, ty + (m.ch - icon_s) * 0.5, icon_s, icon, 0);
                scene.overlay_text_run(x0 + pad_x + icon_s + 7.0, ty, name, theme::INK_1, false, false, 0);
                let mw = marker.len() as f32 * m.cw;
                scene.overlay_text_run(x1 - pad_x - mw, ty, marker, theme::INK_4, false, false, 0);
                ry += row_h;
            }
        }
        HoverKind::FileEntry { name, .. } => {
            let ry = draw_header(scene, name, "loading preview…");
            scene.overlay_text_run(x0 + pad_x, ry + 2.0, "reading file…", theme::INK_4, false, true, 0);
        }
        HoverKind::FilePreview { name, icon, meta, lines, truncated, .. } => {
            let hy = y0 + (hdr_h - m.ch) * 0.5;
            let mut hx = x0 + pad_x;
            if let Some(ic) = icon {
                scene.overlay_svg_icon(hx, hy + (m.ch - icon_s) * 0.5, icon_s, ic, 0);
                hx += icon_s + 7.0;
            }
            let mw = meta.chars().count() as f32 * m.cw;
            let max_name = ((x1 - pad_x - mw - 12.0 - hx) / m.cw).floor().max(8.0) as usize;
            scene.overlay_text_run(hx, hy, truncate_chars(name, max_name), theme::INK_1, true, false, 0);
            if !meta.is_empty() {
                scene.overlay_text_run(x1 - pad_x - mw, hy, meta, theme::INK_4, false, false, 0);
            }
            scene.overlay_line(x0 + 1.0, y0 + hdr_h, x1 - 1.0, y0 + hdr_h, 1.0, theme::LINE_FAINT, 0);

            let mut ry = y0 + hdr_h + 6.0;
            let line_h = m.ch + 2.0;
            let has_gutter = lines.iter().any(|l| l.line_no.is_some());
            let gutter_w = if has_gutter { 3.5 * m.cw } else { 0.0 };
            let max_c = ((pop_w - pad_x * 2.0 - gutter_w) / m.cw).floor().max(12.0) as usize;
            for pl in lines.iter().take(16) {
                if let Some(ln) = pl.line_no {
                    scene.overlay_text_run(x0 + pad_x, ry, format!("{ln:>2}"), theme::INK_4, false, false, 0);
                }
                let mut cx = x0 + pad_x + gutter_w;
                let mut rem = max_c;
                for sp in &pl.spans {
                    if rem == 0 {
                        break;
                    }
                    let txt = truncate_chars(&sp.text, rem);
                    let len = txt.chars().count();
                    scene.overlay_text_run(cx, ry, txt, sp.color, sp.bold, sp.italic, 0);
                    cx += len as f32 * m.cw;
                    rem = rem.saturating_sub(len);
                }
                ry += line_h;
            }
            if *truncated {
                scene.overlay_ui_small(x0 + pad_x + gutter_w, ry + 1.0, "… more lines", theme::INK_4, false, true, 0);
            }
        }
        HoverKind::DirPreview { full_path, name, icon, meta, entries, total_items, hidden_count } => {
            let hy = y0 + (hdr_h - m.ch) * 0.5;
            let mut hx = x0 + pad_x;
            if let Some(ic) = icon {
                scene.overlay_svg_icon(hx, hy + (m.ch - icon_s) * 0.5, icon_s, ic, 0);
                hx += icon_s + 7.0;
            }
            let dir_total_lbl = crate::dust_driver::request_dir_size(std::path::Path::new(full_path))
                .filter(|p| p.bytes > 0)
                .map(|p| {
                    let s = crate::dust_driver::format_bytes_compact(p.bytes);
                    if !p.done { format!("~{s}") } else if p.capped { format!("{s}+") } else { s }
                });
            let header_meta = match (meta.is_empty(), dir_total_lbl) {
                (false, Some(sz)) => format!("{meta} · {sz}"),
                (true, Some(sz)) => sz,
                (_, None) => meta.clone(),
            };
            let mw = m.measure(&header_meta, TextFace::UiSmall);
            let avg_ui = m.measure("n", TextFace::Ui).max(4.5);
            let max_name = ((x1 - pad_x - mw - 12.0 - hx) / avg_ui).floor().max(8.0) as usize;
            scene.overlay_ui_text(hx, hy, truncate_chars(name, max_name), theme::INK_1, true, false, 0);
            if !header_meta.is_empty() {
                scene.overlay_ui_small(x1 - pad_x - mw, hy, &header_meta, theme::INK_4, false, false, 0);
            }
            scene.overlay_line(x0 + 1.0, y0 + hdr_h, x1 - 1.0, y0 + hdr_h, 1.0, theme::LINE_FAINT, 0);

            let dir_row_h = m.ch + 5.0;
            let mut ry = y0 + hdr_h + 6.0;
            if entries.is_empty() {
                scene.overlay_ui_small(
                    x0 + pad_x,
                    ry + (dir_row_h - m.ch) * 0.5,
                    "empty directory",
                    theme::INK_4,
                    false,
                    true,
                    0,
                );
                ry += dir_row_h;
            } else {
                let inner_w = pop_w - pad_x * 2.0;
                // Always reserve the live IEC size column inside `DirPreview` so progressive
                // `blaze-dust` folder sizes stream smoothly right beside every subfolder!
                let size_w = 10.5 * m.cw;
                let show_kind = inner_w >= 320.0 && entries.iter().take(16).any(|e| !e.kind_label.is_empty());
                let kind_w = if show_kind { 11.5 * m.cw } else { 0.0 };
                let name_x = x0 + pad_x + icon_s + 8.0;
                let name_max_c = ((x1 - pad_x - size_w - kind_w - 8.0 - name_x) / m.cw)
                    .floor()
                    .max(10.0) as usize;
                let base_dir = std::path::Path::new(full_path);

                for ent in entries.iter().take(16) {
                    let ty = ry + (dir_row_h - m.ch) * 0.5;
                    scene.overlay_svg_icon(
                        x0 + pad_x,
                        ty + (m.ch - icon_s) * 0.5,
                        icon_s,
                        ent.icon,
                        0,
                    );
                    let lbl = if ent.is_dir { format!("{}/", ent.name) } else { ent.name.clone() };
                    let col = if ent.muted {
                        theme::INK_4
                    } else if ent.is_dir {
                        theme::INK_1
                    } else {
                        theme::INK_2
                    };
                    scene.overlay_text_run(name_x, ty, truncate_chars(&lbl, name_max_c), col, false, false, 0);
                    if show_kind && !ent.kind_label.is_empty() {
                        let kx = x1 - pad_x - size_w - kind_w;
                        scene.overlay_ui_small(
                            kx,
                            ty,
                            truncate_chars(ent.kind_label, 14),
                            theme::INK_4,
                            false,
                            false,
                            0,
                        );
                    }
                    let (sz_txt, sz_col) = if ent.is_dir {
                        let sub_p = base_dir.join(&ent.name);
                        if let Some(prog) = crate::dust_driver::request_dir_size(&sub_p) {
                            if !prog.done && prog.bytes == 0 {
                                ("…".to_owned(), theme::INK_4)
                            } else {
                                let base = crate::dust_driver::format_bytes_compact(prog.bytes);
                                let txt = if !prog.done {
                                    format!("~{base}")
                                } else if prog.capped {
                                    format!("{base}+")
                                } else {
                                    base
                                };
                                let col = match blaze_dust::size_tier(prog.bytes) {
                                    blaze_dust::SizeTier::Bytes => theme::INK_3,
                                    blaze_dust::SizeTier::KiB => theme::rgb(0x7c, 0xb2, 0xf8),
                                    blaze_dust::SizeTier::MiB => theme::rgb(0xe5, 0xb5, 0x67),
                                    blaze_dust::SizeTier::GiB => theme::rgb(0xe8, 0x6a, 0x45),
                                };
                                (txt, col)
                            }
                        } else {
                            (String::new(), theme::INK_4)
                        }
                    } else {
                        (ent.size_label.clone(), theme::INK_3)
                    };
                    if !sz_txt.is_empty() {
                        let sw = sz_txt.chars().count() as f32 * m.cw;
                        scene.overlay_text_run(x1 - pad_x - sw, ty, &sz_txt, sz_col, false, false, 0);
                    }
                    ry += dir_row_h;
                }
            }
            let shown = entries.len().min(16);
            let more = total_items.saturating_sub(shown);
            if more > 0 || *hidden_count > 0 {
                let foot = match (more, *hidden_count) {
                    (m_n, 0) => format!("… {m_n} more"),
                    (0, h_n) => format!("{h_n} hidden"),
                    (m_n, h_n) => format!("… {m_n} more · {h_n} hidden"),
                };
                scene.overlay_ui_small(x0 + pad_x + icon_s + 8.0, ry + 2.0, foot, theme::INK_4, false, true, 0);
            }
        }
        HoverKind::FolderProcs { summary } => {
            let folder_name = std::path::Path::new(&summary.folder)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&summary.folder);
            let left_hdr = if summary.is_hot {
                format!("🔥 running in {folder_name}/ · {}", summary.items.len())
            } else {
                format!("running in {folder_name}/ · {}", summary.items.len())
            };
            let right_hdr = format!(
                "{:.0}% CPU · {}",
                summary.total_cpu_pct,
                crate::sysmon_view::format_iec_bytes(summary.total_mem_bytes),
            );
            let mut ry = draw_header(scene, &left_hdr, &right_hdr) + 2.0;
            let proc_row_h = m.ch * 2.0 + 16.0;
            let main_icon = 20.0;
            for item in summary.items.iter().take(10) {
                let row_rect = HitRect::new(x0 + 4.0, ry, x1 - 4.0, ry + proc_row_h);
                if is_hovered(row_rect) {
                    scene.overlay_round_rect(
                        row_rect.x0,
                        row_rect.y0,
                        row_rect.x1,
                        row_rect.y1,
                        5.0,
                        theme::rgb(0x20, 0x22, 0x2a),
                        0.0,
                        [0.0; 4],
                        0,
                    );
                }
                let ty1 = ry + 6.5;
                let ty2 = ty1 + m.ch + 3.0;
                let ix = x0 + pad_x;
                let iy = ry + (proc_row_h - main_icon) * 0.5;
                let mut used_origin_as_main = false;
                if let Some(app_ic) = item.app_icon_path.as_deref() {
                    scene.overlay_image(ix, iy, main_icon, main_icon, app_ic.to_owned(), 0);
                } else if is_shell_name(&item.name)
                    && let Some(orig_ic) = item.origin_app_icon_path.as_deref()
                {
                    scene.overlay_image(ix, iy, main_icon, main_icon, orig_ic.to_owned(), 0);
                    used_origin_as_main = true;
                } else {
                    let lc = item.name.to_ascii_lowercase();
                    let svg = if lc.contains("rust") || lc.contains("cargo") || lc.contains("blaze") {
                        if lc == "blaze-shell" {
                            "shell_dark"
                        } else {
                            "rust_dark"
                        }
                    } else if lc.contains("node") || lc.contains("bun") || lc.contains("deno") {
                        "javaScript_dark"
                    } else if lc.contains("python") || lc == "pytest" || lc == "uv" {
                        "python_dark"
                    } else if lc == "go" || lc == "gopls" {
                        "go_dark"
                    } else if lc.contains("docker") || lc == "containerd" || lc == "orbstack" {
                        "docker_dark"
                    } else if lc.starts_with("swift") || lc == "sourcekit-lsp" {
                        "swiftLang_dark"
                    } else {
                        "shell_dark"
                    };
                    scene.overlay_svg_icon(ix, iy, main_icon, svg, 0);
                }

                // Line 1 right side: crimson `Kill` button (SIGTERM; Shift+click SIGKILL) +
                // clickable action pill (`Pane N · TUI ↗` or `top ↗`), MEM, GPU%, CPU%, Uptime!
                let kill_lbl = "Kill";
                let kill_w = m.measure(kill_lbl, TextFace::UiSmall) + 12.0;
                let kill_x1 = x1 - pad_x;
                let kill_x0 = kill_x1 - kill_w;
                let kill_rect = HitRect::new(kill_x0, ty1 - 2.0, kill_x1, ty1 + m.ch + 2.0);
                let kill_hovered = is_hovered(kill_rect);
                scene.overlay_round_rect(
                    kill_x0,
                    ty1 - 1.5,
                    kill_x1,
                    ty1 + m.ch + 1.5,
                    4.5,
                    if kill_hovered {
                        theme::rgba(0xe0, 0x55, 0x55, 0.30)
                    } else {
                        theme::rgba(0xe0, 0x55, 0x55, 0.14)
                    },
                    0.75,
                    if kill_hovered {
                        theme::rgba(0xf0, 0x6b, 0x6b, 0.65)
                    } else {
                        theme::rgba(0xe0, 0x55, 0x55, 0.36)
                    },
                    0,
                );
                scene.overlay_ui_small(kill_x0 + 6.0, ty1, kill_lbl, theme::DANGER, true, false, 0);

                let (act_lbl, act_fg, act_bg, action) = if let Some(sp) = item.shell_pane {
                    let lbl = if sp.is_alt_screen {
                        format!("Pane {} · TUI ↗", sp.pane_number)
                    } else {
                        format!("Pane {} ↗", sp.pane_number)
                    };
                    (
                        lbl,
                        theme::rgb(0x8c, 0xc2, 0xff),
                        theme::rgba(0x58, 0x9d, 0xf6, 0.18),
                        HitAction::FocusPaneBlock {
                            pane: crate::tiling::PaneId(sp.pane_id as u32),
                            block: crate::blocks::BlockId(sp.block_id),
                        },
                    )
                } else {
                    (
                        "top ↗".to_owned(),
                        theme::INK_3,
                        theme::rgba(0xff, 0xff, 0xff, 0.06),
                        HitAction::InspectProcessPid(item.pid),
                    )
                };
                let act_w = m.measure(&act_lbl, TextFace::UiSmall) + 12.0;
                let act_x1 = kill_x0 - 6.0;
                let act_x0 = act_x1 - act_w;
                scene.overlay_round_rect(
                    act_x0,
                    ty1 - 1.5,
                    act_x1,
                    ty1 + m.ch + 1.5,
                    4.5,
                    act_bg,
                    0.0,
                    [0.0; 4],
                    0,
                );
                scene.overlay_ui_small(act_x0 + 6.0, ty1, &act_lbl, act_fg, item.shell_pane.is_some(), false, 0);

                let mut rx = act_x0 - 8.0;
                let max_mem = summary
                    .items
                    .iter()
                    .map(|p| p.mem_bytes)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let bar_h = 4.0f32;
                let bar_y0 = ty1 + (m.ch - bar_h) * 0.5;

                // Fixed-width Memory column: right-aligned label + proportional blue/cyan bar.
                let mem_col_w = 50.0f32;
                let mem_s = crate::sysmon_view::format_iec_bytes(item.mem_bytes);
                let mem_w = m.measure(&mem_s, TextFace::UiSmall);
                scene.overlay_ui_small(rx - mem_w, ty1, &mem_s, theme::INK_2, false, false, 0);
                rx -= mem_col_w + 4.0;
                let mem_bar_w = 30.0f32;
                let mem_bx0 = rx - mem_bar_w;
                let mem_frac = (item.mem_bytes as f32 / max_mem as f32).clamp(0.0, 1.0);
                scene.overlay_round_rect(mem_bx0, bar_y0, rx, bar_y0 + bar_h, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], 0);
                if item.mem_bytes > 0 {
                    let mw = (mem_bar_w * mem_frac).max(2.5);
                    let m_col = if mem_frac >= 0.65 {
                        theme::rgb(0x6e, 0xb5, 0xff)
                    } else {
                        theme::rgba(0x6e, 0xb5, 0xff, 0.65)
                    };
                    scene.overlay_round_rect(mem_bx0, bar_y0, mem_bx0 + mw, bar_y0 + bar_h, 2.0, m_col, 0.0, [0.0; 4], 0);
                }
                rx = mem_bx0 - 10.0;

                if item.gpu_pct >= 0.5 {
                    let gpu_s = format!("G:{:.0}%", item.gpu_pct);
                    let gw = m.measure(&gpu_s, TextFace::UiSmall);
                    rx -= gw;
                    scene.overlay_ui_small(rx, ty1, &gpu_s, theme::rgb(0x56, 0xd8, 0xc8), true, false, 0);
                    rx -= 8.0;
                }

                // Fixed-width CPU column: right-aligned % + heat-colored proportional CPU bar.
                let cpu_col_w = 36.0f32;
                let cpu_s = if item.is_hot {
                    format!("{:.0}%", item.cpu_pct)
                } else {
                    format!("{:.1}%", item.cpu_pct)
                };
                let cpu_col = if item.is_hot {
                    theme::rgb(0xff, 0x7c, 0x4c)
                } else if item.cpu_pct >= 15.0 {
                    theme::rgb(0xe8, 0xa8, 0x48)
                } else if item.cpu_pct >= 2.0 {
                    theme::OK
                } else {
                    theme::INK_3
                };
                let cpu_w = m.measure(&cpu_s, TextFace::UiSmall);
                scene.overlay_ui_small(rx - cpu_w, ty1, &cpu_s, cpu_col, item.is_hot, false, 0);
                rx -= cpu_col_w + 4.0;
                let cpu_bar_w = 30.0f32;
                let cpu_bx0 = rx - cpu_bar_w;
                let cpu_frac = (item.cpu_pct / 100.0).clamp(0.0, 1.0);
                scene.overlay_round_rect(cpu_bx0, bar_y0, rx, bar_y0 + bar_h, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], 0);
                if item.cpu_pct >= 0.5 {
                    let cw = (cpu_bar_w * cpu_frac).max(2.5);
                    scene.overlay_round_rect(cpu_bx0, bar_y0, cpu_bx0 + cw, bar_y0 + bar_h, 2.0, cpu_col, 0.0, [0.0; 4], 0);
                }
                rx = cpu_bx0 - 10.0;

                if item.uptime_secs > 0 {
                    let up_s = format!("up {}", crate::sysmon_view::format_uptime(item.uptime_secs));
                    let up_w = m.measure(&up_s, TextFace::UiSmall);
                    rx -= up_w;
                    scene.overlay_ui_small(rx, ty1, &up_s, theme::INK_4, false, false, 0);
                    rx -= 10.0;
                }

                // Line 1 left side: bold process name + subtle origin pill when space permits.
                let name_x = ix + main_icon + 9.0;
                let origin_pill = if item.shell_pane.is_none()
                    && !item.origin_app_name.is_empty()
                    && !item.origin_app_name.eq_ignore_ascii_case(&item.name)
                {
                    let app_short = truncate_chars(&item.origin_app_name, 16);
                    if !used_origin_as_main
                        && let Some(orig_ic) = item.origin_app_icon_path.as_deref()
                    {
                        let tw = m.measure(&app_short, TextFace::UiSmall);
                        Some((Some(orig_ic), app_short, 5.0 + 12.0 + 4.0 + tw + 6.0))
                    } else {
                        let lbl = format!("in {app_short}");
                        let tw = m.measure(&lbl, TextFace::UiSmall);
                        Some((None, lbl, 6.0 + tw + 6.0))
                    }
                } else {
                    None
                };
                let pill_gap = 7.0;
                let total_avail_w = (rx - name_x - 4.0).max(m.cw * 6.0);
                let reserve_for_pill = origin_pill
                    .as_ref()
                    .filter(|(_, _, pw)| total_avail_w >= pw + pill_gap + m.cw * 6.0)
                    .map_or(0.0, |(_, _, pw)| pw + pill_gap);
                let name_avail_w = (total_avail_w - reserve_for_pill).max(m.cw * 6.0);
                let avail_ch = (name_avail_w / m.cw).floor().max(6.0) as usize;
                let shown_name = truncate_chars(&item.name, avail_ch);
                scene.overlay_text_run(name_x, ty1, &shown_name, theme::INK_1, true, false, 0);

                let shown_name_w = m.measure(&shown_name, TextFace::Mono);
                if let Some((pill_icon, pill_lbl, pill_w)) = origin_pill {
                    let pill_x0 = name_x + shown_name_w + pill_gap;
                    if pill_x0 + pill_w <= rx - 2.0 {
                        let pill_y0 = ty1 - 1.0;
                        let pill_y1 = ty1 + m.ch + 1.0;
                        let pill_h = pill_y1 - pill_y0;
                        scene.overlay_round_rect(
                            pill_x0,
                            pill_y0,
                            pill_x0 + pill_w,
                            pill_y1,
                            pill_h * 0.5,
                            theme::rgba(0xff, 0xff, 0xff, 0.06),
                            0.75,
                            theme::rgba(0xff, 0xff, 0xff, 0.12),
                            0,
                        );
                        let mut px = pill_x0;
                        if let Some(orig_ic) = pill_icon {
                            px += 5.0;
                            let ic_s = 12.0;
                            let ic_y = pill_y0 + (pill_h - ic_s) * 0.5;
                            scene.overlay_image(px, ic_y, ic_s, ic_s, orig_ic.to_owned(), 0);
                            px += ic_s + 4.0;
                        } else {
                            px += 6.0;
                        }
                        scene.overlay_ui_small(px, ty1, &pill_lbl, theme::INK_3, false, false, 0);
                    }
                }

                // Line 2 subtitle: multi-hop origin chain + cmdline + subfolder + pid + thread count!
                let mut sub_parts: Vec<String> = Vec::new();
                if !item.origin_chain.is_empty() && item.origin_chain.contains('›') {
                    sub_parts.push(truncate_chars(&item.origin_chain, 38));
                }
                if !item.cmdline.is_empty() && item.cmdline != item.name {
                    sub_parts.push(truncate_chars(&item.cmdline, 44));
                }
                if item.rel_dir != "." && !item.rel_dir.is_empty() {
                    sub_parts.push(format!("in {}/", item.rel_dir));
                }
                sub_parts.push(format!("pid {}", item.pid));
                if item.threads > 0 {
                    sub_parts.push(format!("{} thr", item.threads));
                }
                let sub_line = sub_parts.join(" · ");
                let max_sub_ch = ((x1 - pad_x - name_x) / (m.cw * 0.85)).floor().max(16.0) as usize;
                let shown_sub = truncate_chars(&sub_line, max_sub_ch);
                scene.overlay_ui_small(name_x, ty2, &shown_sub, theme::INK_4, false, false, 0);

                scene.push_hit(row_rect, action);
                // Push Kill hit rect AFTER row_rect so top-most `.rev()` hit-test picks Kill cleanly!
                scene.push_hit(kill_rect, HitAction::KillProcessPid(item.pid));
                ry += proc_row_h;
            }
            if summary.items.len() > 10 {
                scene.overlay_ui_small(
                    x0 + pad_x,
                    ry + 2.0,
                    format!("… {} more processes (run `top` for full tree)", summary.items.len() - 10),
                    theme::INK_4,
                    false,
                    true,
                    0,
                );
            }
        }
        HoverKind::CommandToken { .. } => {}
        HoverKind::CommandExplain { badge, title, summary, details, source_note, .. } => {
            let (b_fg, b_bg) = match *badge {
                "BUILTIN" => (theme::CMD_BUILTIN, theme::rgba(0x5c, 0xb8, 0xff, 0.16)),
                "COMMAND" => (theme::CMD_EXTERNAL, theme::rgba(0xe5, 0xc0, 0x7b, 0.16)),
                "SUBCOMMAND" => (theme::rgb(0x56, 0xb6, 0xc2), theme::rgba(0x56, 0xb6, 0xc2, 0.16)),
                "FLAG" => (theme::rgb(0x88, 0xb4, 0xff), theme::rgba(0x58, 0x9d, 0xf6, 0.18)),
                "PIPE" | "REDIR" => (theme::rgb(0xc6, 0x78, 0xdd), theme::rgba(0xc6, 0x78, 0xdd, 0.16)),
                "VAR" => (theme::rgb(0xd1, 0x9a, 0x66), theme::rgba(0xd1, 0x9a, 0x66, 0.16)),
                _ => (theme::INK_2, theme::rgba(0xff, 0xff, 0xff, 0.08)),
            };
            // Header with colored role pill + monospace token title.
            let hy = y0 + (hdr_h - m.ch) * 0.5;
            let bw = m.measure(badge, TextFace::UiSmall) + 10.0;
            scene.overlay_round_rect(
                x0 + pad_x,
                y0 + 5.0,
                x0 + pad_x + bw,
                y0 + hdr_h - 5.0,
                4.0,
                b_bg,
                0.0,
                [0.0; 4],
                0,
            );
            scene.overlay_ui_small(x0 + pad_x + 5.0, hy, *badge, b_fg, true, false, 0);
            scene.overlay_text_run(x0 + pad_x + bw + 8.0, hy, title, theme::INK_1, true, false, 0);
            if !source_note.is_empty() {
                let sw = m.measure(source_note, TextFace::UiSmall);
                scene.overlay_ui_small(x1 - pad_x - sw, hy, source_note, theme::INK_4, false, false, 0);
            }
            scene.overlay_line(x0 + 1.0, y0 + hdr_h, x1 - 1.0, y0 + hdr_h, 1.0, theme::LINE_FAINT, 0);

            let mut ry = y0 + hdr_h + 6.0;
            let wrap_cols = ((x1 - x0 - pad_x * 2.0) / m.cw).floor().max(24.0) as usize;
            if !summary.is_empty() {
                for line in wrap_lines(summary, wrap_cols).into_iter().take(4) {
                    scene.overlay_ui_small(x0 + pad_x, ry, line, theme::INK_2, false, false, 0);
                    ry += m.ch + 3.0;
                }
            }
            if !details.is_empty() {
                ry += 4.0;
                for (tag, txt) in details.iter().take(6) {
                    let tag_w = (tag.chars().count() as f32 * m.cw + 10.0).max(34.0);
                    scene.overlay_text_run(x0 + pad_x, ry, tag, b_fg, true, false, 0);
                    let rem_cols = ((x1 - x0 - pad_x * 2.0 - tag_w) / m.cw).floor().max(16.0) as usize;
                    for (li, line) in wrap_lines(txt, rem_cols).into_iter().take(2).enumerate() {
                        scene.overlay_ui_small(x0 + pad_x + tag_w, ry, line, theme::INK_3, false, false, 0);
                        if li == 0 {
                            ry += m.ch + 4.0;
                        } else {
                            ry += m.ch + 2.0;
                        }
                    }
                }
            }
        }
    }
}

/// Monochrome dark-glass `Cmd+Shift+P` Command Palette rendered in Apple SF Pro with
/// right-aligned monospace shortcut keycaps, fuzzy matching, & trackpad scrolling.
pub fn layout_command_palette(
    scene: &mut Scene,
    palette: &CommandPalette,
    vw: f32,
    vh: f32,
    m: CellMetrics,
) {
    let total = palette.matches.len();
    let show = total.min(PALETTE_VISIBLE_ROWS).max(1);
    let max_scroll = total.saturating_sub(PALETTE_VISIBLE_ROWS);
    let win_start = palette.scroll.min(max_scroll);
    let has_sb = total > PALETTE_VISIBLE_ROWS;
    let sb_pad = if has_sb { 10.0 } else { 0.0 };

    let pad_x = 14.0;
    let input_h = m.ch + 20.0;
    let row_h = m.ch + 10.0;
    let footer_h = m.ch + 12.0;
    let list_h = show as f32 * row_h + 8.0;
    let modal_w = (vw - 48.0).clamp(320.0, 580.0).min((vw - 24.0).max(240.0));
    let modal_h = (input_h + list_h + footer_h).min(vh - TOP_BAR_H - 28.0);
    let x0 = ((vw - modal_w) * 0.5).max(12.0);
    let x1 = x0 + modal_w;
    let y0 = (TOP_BAR_H + ((vh - TOP_BAR_H - modal_h) * 0.24)).max(TOP_BAR_H + 14.0);
    let y1 = y0 + modal_h;

    scene.overlay_round_rect(0.0, TOP_BAR_H, vw, vh, 0.0, theme::rgba(0x00, 0x00, 0x00, 0.42), 0.0, [0.0; 4], 0);
    scene.overlay_shadow(x0, y0 + 6.0, x1, y1 + 6.0, 10.0, 26.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(x0, y0, x1, y1, 10.0, theme::rgb(0x14, 0x14, 0x17), 1.0, theme::rgb(0x2d, 0x2d, 0x33), 0);

    // 1. Search input bar (`>` + SF Pro query or placeholder).
    let in_ty = y0 + (input_h - m.ch) * 0.5;
    scene.overlay_text_run(x0 + pad_x, in_ty, "›", theme::INK_2, true, false, 0);
    let qx = x0 + pad_x + 16.0;
    if palette.query.is_empty() {
        scene.overlay_ui_text(qx, in_ty, "Type a command…", theme::INK_4, false, false, 0);
    } else {
        scene.overlay_ui_text(qx, in_ty, &palette.query, theme::INK_1, false, false, 0);
    }
    let count_s = format!("{total}");
    let cw = m.measure(&count_s, TextFace::UiSmall);
    scene.overlay_ui_small(x1 - pad_x - cw, in_ty, &count_s, theme::INK_4, false, false, 0);
    scene.overlay_line(x0 + 1.0, y0 + input_h, x1 - 1.0, y0 + input_h, 1.0, theme::LINE, 0);

    // 2. Scrollable command list.
    let list_y0 = y0 + input_h + 4.0;
    let mut ry = list_y0;
    if palette.matches.is_empty() {
        scene.overlay_ui_small(x0 + pad_x, ry + 6.0, "No matching commands", theme::INK_4, false, true, 0);
    } else {
        for offset in 0..show {
            let idx = win_start + offset;
            let (cmd, _) = &palette.matches[idx];
            let is_sel = idx == palette.selected;
            let ty = ry + (row_h - m.ch) * 0.5;

            if is_sel {
                scene.overlay_round_rect(
                    x0 + 6.0,
                    ry,
                    x1 - 6.0 - sb_pad,
                    ry + row_h,
                    6.0,
                    theme::rgb(0x21, 0x21, 0x26),
                    0.0,
                    [0.0; 4],
                    0,
                );
                scene.overlay_round_rect(
                    x0 + 6.0,
                    ry + 5.0,
                    x0 + 8.0,
                    ry + row_h - 5.0,
                    1.0,
                    theme::INK_1,
                    0.0,
                    [0.0; 4],
                    0,
                );
            }

            // Right-aligned monospace shortcut keycap pill (`⌘D`, `⌘⇧↵`, `⌘T`).
            let sc_w = cmd.shortcut.chars().count() as f32 * m.cw + 12.0;
            let sc_x1 = x1 - pad_x - sb_pad;
            let sc_x0 = sc_x1 - sc_w;
            scene.overlay_round_rect(
                sc_x0,
                ry + 4.0,
                sc_x1,
                ry + row_h - 4.0,
                4.5,
                theme::rgb(0x1b, 0x1b, 0x1f),
                1.0,
                theme::LINE,
                0,
            );
            scene.overlay_text_run(sc_x0 + 6.0, ty, cmd.shortcut, theme::INK_2, false, false, 0);

            // Category prefix + SF Pro command title.
            let cat_w = m.measure(cmd.category, TextFace::UiSmall) + 12.0;
            scene.overlay_ui_small(x0 + pad_x + 4.0, ty, cmd.category, theme::INK_4, false, false, 0);
            let title_x = x0 + pad_x + 4.0 + cat_w.max(66.0);
            let avg_ui = m.measure("n", TextFace::Ui).max(4.5);
            let max_c = ((sc_x0 - 10.0 - title_x) / avg_ui).floor().max(8.0) as usize;
            scene.overlay_ui_text(
                title_x,
                ty,
                truncate_chars(cmd.title, max_c),
                if is_sel { theme::INK_1 } else { theme::INK_2 },
                is_sel,
                false,
                0,
            );

            scene.push_hit(
                HitRect::new(x0 + 6.0, ry, x1 - 6.0 - sb_pad, ry + row_h),
                HitAction::SelectPalette(idx),
            );
            ry += row_h;
        }
        if has_sb {
            draw_overlay_scrollbar(scene, x1 - 8.0, list_y0 + 2.0, ry - 2.0, win_start, max_scroll, PALETTE_VISIBLE_ROWS, total);
        }
    }

    // 3. Footer bar.
    let fy0 = y1 - footer_h;
    scene.overlay_line(x0 + 1.0, fy0, x1 - 1.0, fy0, 1.0, theme::LINE_FAINT, 0);
    let fty = fy0 + (footer_h - m.ch) * 0.5;
    scene.overlay_ui_small(x0 + pad_x, fty, "Command Palette", theme::INK_3, false, false, 0);
    let fh = "↑↓ navigate  ·  scroll  ·  enter run  ·  esc close";
    let fhw = m.measure(fh, TextFace::UiSmall);
    scene.overlay_ui_small(x1 - pad_x - fhw, fty, fh, theme::INK_4, false, false, 0);
}

/// Highlight all `Cmd+F` matches across the entire scrollback stream and draw the
/// floating dark-glass top-right Find Bar (`🔍 query |  3/14  [Aa] [↑] [↓] [×]`).
///
/// Returns the currently focused [`crate::find::FindMatch`] (if any) so the caller can
/// smoothly auto-scroll off-screen matches into view when navigating.
#[allow(clippy::too_many_arguments)]
pub fn apply_find_overlay(
    scene: &mut Scene,
    fb: &mut crate::find::FindBar,
    vw: f32,
    feed_top: f32,
    feed_bottom: f32,
    m: CellMetrics,
    cursor_visible: bool,
    mouse_pos: Option<[f32; 2]>,
) -> Option<crate::find::FindMatch> {
    // 1. Scan all stream text runs (skipping top chrome runs at clip_id == 0).
    let stream_runs: Vec<crate::view::TextRun> = scene
        .text_runs
        .iter()
        .filter(|r| r.clip_id != 0)
        .cloned()
        .collect();
    let vlines = extract_visual_lines(&stream_runs, m, -1.0e6, 1.0e6);
    let matches = crate::find::find_matches_in_lines(&vlines, m, &fb.query, fb.case_sensitive);
    fb.clamp_current(matches.len());

    // 2. Paint amber match highlights (clipped to the visible scrollback viewport).
    let feed_clip = scene.push_clip(0.0, feed_top, vw, feed_bottom, 0.0);
    let active_match = if matches.is_empty() {
        None
    } else {
        Some(matches[fb.current])
    };
    for (idx, fm) in matches.iter().enumerate() {
        if fm.y + fm.h < feed_top || fm.y > feed_bottom {
            continue;
        }
        let is_cur = idx == fb.current;
        if is_cur {
            scene.over_round_rect(
                fm.x - 1.5,
                fm.y - 1.0,
                fm.x + fm.w + 1.5,
                fm.y + fm.h + 1.0,
                3.0,
                theme::rgba(0xe8, 0x80, 0x3c, 0.56),
                feed_clip,
            );
            scene.overlay_round_rect(
                fm.x - 1.5,
                fm.y - 1.0,
                fm.x + fm.w + 1.5,
                fm.y + fm.h + 1.0,
                3.0,
                theme::rgba(0, 0, 0, 0.0),
                1.2,
                theme::rgba(0xff, 0xb8, 0x5c, 0.92),
                feed_clip,
            );
        } else {
            scene.over_round_rect(
                fm.x - 1.0,
                fm.y - 0.5,
                fm.x + fm.w + 1.0,
                fm.y + fm.h + 0.5,
                2.5,
                theme::rgba(0xe5, 0xb5, 0x67, 0.28),
                feed_clip,
            );
        }
    }

    // 3. Floating dark-glass Find Bar anchored top-right right under the titlebar.
    let bar_w = 390.0f32.min((vw - 24.0).max(240.0));
    let bar_h = (m.ch + 16.0).max(34.0);
    let x1 = (vw - 14.0).max(bar_w + 12.0);
    let x0 = x1 - bar_w;
    let y0 = feed_top + 8.0;
    let y1 = y0 + bar_h;

    scene.overlay_shadow(x0, y0, x1, y1, 9.0, 14.0, [0.0, 0.0, 0.0, 0.45], 0);
    scene.overlay_round_rect(
        x0,
        y0,
        x1,
        y1,
        9.0,
        theme::rgba(0x16, 0x18, 0x1d, 0.96),
        1.0,
        theme::LINE_STRONG,
        0,
    );

    let ty = y0 + (bar_h - m.ch) * 0.5;
    // Right-side compact controls: [Aa] [↑] [↓] [×]
    let btn_h = (bar_h - 10.0).max(20.0);
    let btn_y0 = y0 + (bar_h - btn_h) * 0.5;
    let btn_y1 = btn_y0 + btn_h;
    let close_x1 = x1 - 7.0;
    let close_x0 = close_x1 - 22.0;
    let next_x1 = close_x0 - 3.0;
    let next_x0 = next_x1 - 22.0;
    let prev_x1 = next_x0 - 3.0;
    let prev_x0 = prev_x1 - 22.0;
    let case_x1 = prev_x0 - 5.0;
    let case_x0 = case_x1 - 28.0;

    let draw_btn = |scene: &mut Scene, bx0: f32, bx1: f32, label: &str, active: bool, action: HitAction| {
        let hovered = mouse_pos.is_some_and(|[mx, my]| mx >= bx0 && mx <= bx1 && my >= btn_y0 && my <= btn_y1);
        let bg = if active {
            theme::rgba(0x3a, 0x62, 0xa8, 0.40)
        } else if hovered {
            theme::rgba(0xff, 0xff, 0xff, 0.09)
        } else {
            theme::rgba(0, 0, 0, 0.0)
        };
        let border = if active {
            theme::rgba(0x64, 0x98, 0xf2, 0.72)
        } else {
            theme::rgba(0, 0, 0, 0.0)
        };
        if active || hovered {
            scene.overlay_round_rect(bx0, btn_y0, bx1, btn_y1, 5.0, bg, 1.0, border, 0);
        }
        let lw = m.measure(label, TextFace::UiSmall);
        let lx = bx0 + ((bx1 - bx0) - lw) * 0.5;
        let col = if active {
            theme::rgb(0x96, 0xc2, 0xff)
        } else if hovered {
            theme::INK_1
        } else {
            theme::INK_3
        };
        scene.overlay_ui_small(lx, ty, label, col, active, false, 0);
        scene.push_hit(HitRect::new(bx0, btn_y0, bx1, btn_y1), action);
    };

    draw_btn(scene, case_x0, case_x1, "Aa", fb.case_sensitive, HitAction::FindToggleCase);
    draw_btn(scene, prev_x0, prev_x1, "↑", false, HitAction::FindPrev);
    draw_btn(scene, next_x0, next_x1, "↓", false, HitAction::FindNext);
    draw_btn(scene, close_x0, close_x1, "×", false, HitAction::FindClose);

    // Match counter (`3/14` or `0/0` in muted coral when query has no matches).
    let counter = fb.counter_label();
    let counter_w = if counter.is_empty() {
        0.0
    } else {
        m.measure(&counter, TextFace::UiSmall)
    };
    let counter_x = case_x0 - 8.0 - counter_w;
    if !counter.is_empty() {
        let ccol = if fb.total_matches == 0 {
            theme::rgb(0xe0, 0x6c, 0x75)
        } else {
            theme::INK_3
        };
        scene.overlay_ui_small(counter_x, ty, &counter, ccol, false, false, 0);
    }

    // Left search glyph + query input text + crisp beam cursor.
    let qx = x0 + 11.0;
    scene.overlay_text_run(qx, ty, "⌕", theme::INK_3, false, false, 0);
    let text_x = qx + m.cw + 7.0;
    let avail_w = (counter_x - 8.0 - text_x).max(40.0);
    let max_ch = (avail_w / m.cw).floor().max(4.0) as usize;
    if fb.query.is_empty() {
        scene.overlay_ui_small(text_x, ty, "Find in output…  (⌘G next)", theme::INK_4, false, false, 0);
        if cursor_visible {
            scene.overlay_round_rect(text_x, ty + 1.0, text_x + 1.5, ty + m.ch - 1.0, 0.5, theme::INK_1, 0.0, [0.0; 4], 0);
        }
    } else {
        let display_q = truncate_chars(&fb.query, max_ch);
        scene.overlay_text_run(text_x, ty, &display_q, theme::INK_1, false, false, 0);
        if cursor_visible {
            let cur_cols = fb.query[..fb.cursor.min(fb.query.len())].chars().count().min(max_ch);
            let cx = text_x + cur_cols as f32 * m.cw;
            scene.overlay_round_rect(cx, ty + 1.0, cx + 1.5, ty + m.ch - 1.0, 0.5, theme::INK_1, 0.0, [0.0; 4], 0);
        }
    }

    active_match
}

fn is_shell_name(name: &str) -> bool {
    let base = name.trim_start_matches('-');
    matches!(
        base.to_ascii_lowercase().as_str(),
        "nu" | "zsh"
            | "bash"
            | "fish"
            | "sh"
            | "dash"
            | "tcsh"
            | "csh"
            | "elvish"
            | "xonsh"
            | "pwsh"
    )
}

/// Render a git branch name with subtle semantic syntax highlighting on `<prefix>/` namespaces
/// (`feat/`, `fix/`, `perf/`, `refactor/`, `chore/`, `docs/`) so the important leaf name after
/// the slash stands out cleanly in bright primary ink. Returns logical width consumed.
#[allow(clippy::too_many_arguments)]
fn draw_styled_branch_name(
    scene: &mut Scene,
    x: f32,
    y: f32,
    branch: &str,
    live: bool,
    overlay: bool,
    m: CellMetrics,
    clip_id: u32,
) -> f32 {
    let mut emit = |cx: f32, txt: &str, col: [f32; 4], bold: bool| {
        if overlay {
            scene.overlay_text_run(cx, y, txt, col, bold, false, clip_id);
        } else {
            scene.text(cx, y, txt, col, bold, false, clip_id);
        }
    };
    if let Some(slash) = branch.rfind('/')
        && slash > 0
        && slash + 1 < branch.len()
    {
        let prefix = &branch[..slash];
        let leaf = &branch[slash + 1..];
        let head = prefix.split('/').next().unwrap_or(prefix);
        let prefix_col = if !live {
            theme::INK_4
        } else if head.eq_ignore_ascii_case("feat") || head.eq_ignore_ascii_case("feature") {
            theme::rgb(0x58, 0xb9, 0x80)
        } else if head.eq_ignore_ascii_case("fix")
            || head.eq_ignore_ascii_case("hotfix")
            || head.eq_ignore_ascii_case("bugfix")
        {
            theme::rgb(0xe0, 0x75, 0x7c)
        } else if head.eq_ignore_ascii_case("perf") || head.eq_ignore_ascii_case("bench") {
            theme::rgb(0xe2, 0xa8, 0x52)
        } else if head.eq_ignore_ascii_case("refactor")
            || head.eq_ignore_ascii_case("chore")
            || head.eq_ignore_ascii_case("ci")
            || head.eq_ignore_ascii_case("docs")
            || head.eq_ignore_ascii_case("test")
        {
            theme::rgb(0x7f, 0xa8, 0xe8)
        } else {
            theme::INK_3
        };
        let slash_col = theme::INK_4;
        let leaf_col = if live { theme::INK_1 } else { theme::INK_3 };
        let pw = prefix.chars().count() as f32 * m.cw;
        let lw = leaf.chars().count() as f32 * m.cw;
        emit(x, prefix, prefix_col, false);
        emit(x + pw, "/", slash_col, false);
        emit(x + pw + m.cw, leaf, leaf_col, false);
        return pw + m.cw + lw;
    }
    let col = if live { theme::INK_1 } else { theme::INK_3 };
    emit(x, branch, col, false);
    branch.chars().count() as f32 * m.cw
}

/// Resolved icon source for a running folder process (native macOS app icon or semantic SVG badge).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcBadgeIcon<'a> {
    Image(&'a str),
    Svg(&'static str),
}

fn proc_badge_icon(item: &crate::sysmon_driver::FolderProcItem) -> ProcBadgeIcon<'_> {
    if let Some(app_ic) = item.app_icon_path.as_deref() {
        return ProcBadgeIcon::Image(app_ic);
    }
    if is_shell_name(&item.name)
        && let Some(orig_ic) = item.origin_app_icon_path.as_deref()
    {
        return ProcBadgeIcon::Image(orig_ic);
    }
    let lc = item.name.to_ascii_lowercase();
    let svg = if lc.contains("rust") || lc.contains("cargo") || lc.contains("blaze") {
        if lc == "blaze-shell" { "shell_dark" } else { "rust_dark" }
    } else if lc.contains("node") || lc.contains("bun") || lc.contains("deno") {
        "javaScript_dark"
    } else if lc.contains("python") || lc == "pytest" || lc == "uv" {
        "python_dark"
    } else if lc == "go" || lc == "gopls" {
        "go_dark"
    } else if lc.contains("docker") || lc == "containerd" || lc == "orbstack" {
        "docker_dark"
    } else if lc.starts_with("swift") || lc == "sourcekit-lsp" {
        "swiftLang_dark"
    } else {
        "shell_dark"
    };
    ProcBadgeIcon::Svg(svg)
}

fn parent_breadcrumbs(cwd: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() {
        out.push(("/".to_owned(), "/".to_owned()));
        return out;
    }
    let mut acc = String::new();
    for seg in trimmed.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(seg);
        let label = if out.is_empty() {
            format!("/{seg}")
        } else {
            seg.to_owned()
        };
        out.push((label, acc.clone()));
    }
    if out.len() > 6 {
        out = out.split_off(out.len() - 6);
        if let Some((first_lbl, _)) = out.first_mut() {
            *first_lbl = format!("…/{}", first_lbl.trim_start_matches('/'));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_tree_prefix;

    #[test]
    fn parse_tree_prefix_handles_standard_segments() {
        assert_eq!(parse_tree_prefix(""), (0, 0, None));
        assert_eq!(parse_tree_prefix("├── "), (1, 0, Some(false)));
        assert_eq!(parse_tree_prefix("└── "), (1, 0, Some(true)));
        assert_eq!(parse_tree_prefix("│   ├── "), (2, 0b1, Some(false)));
        assert_eq!(parse_tree_prefix("│   │   └── "), (3, 0b11, Some(true)));
        assert_eq!(parse_tree_prefix("    └── "), (2, 0b0, Some(true)));
        assert_eq!(parse_tree_prefix("    │   ├── "), (3, 0b10, Some(false)));
    }
}
