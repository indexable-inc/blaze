//! GPU-independent minimal layout engine following the `ix.dev/blaze` aesthetic:
//! warm charcoal surfaces (`#101011`), monochrome-first typography (`#ebeae6`..`#57575a`),
//! `--fire` (`#e0461a`) as the one brand accent, crisp GPU SDF icons next to every semantic
//! entity, custom typed dataframe column renderers (file/folder icons, proportional size bars,
//! human relative times, status dots), and an inline `$ ...` recessed composer.

use blaze_own::Alloc;
use blaze_sh::lex::{HighlightSpan, highlight};
use blaze_sh::{Bytes, ColDisplay, Record, ShellError, Table, Value, line_col};
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
    ActiveDrag, CellMetrics, DragPayload, HitAction, HitRect, Scene, format_duration_ms,
    format_value_cell_at, is_expandable_cell, truncate_chars, wrap_lines,
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
    let mut prev_cwd: Option<&str> = None;
    for (idx, blk) in blocks.iter().enumerate() {
        if idx > 0 {
            let sep_y = feed_top + content_y - input.scroll_y;
            scene.line(col_x0, sep_y, col_x1, sep_y, 1.0, theme::LINE_FAINT, feed_clip);
            content_y += BLOCK_PAD_Y;
        }
        let show_context = prev_cwd != Some(blk.prompt_snapshot.cwd_display.as_str());
        prev_cwd = Some(blk.prompt_snapshot.cwd_display.as_str());

        let draw_y = feed_top + content_y - input.scroll_y;
        let h = layout_block(
            &mut scene,
            blk,
            show_context,
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
    );
    let prompt_y1 = prompt_y0 + prompt_h;
    content_y += prompt_h + 14.0;
    scene.total_content_height = content_y;

    // 4. Unified inline macOS titlebar (branch tabs, cwd/git pill, zoom, fork button).
    layout_top_bar(
        &mut scene,
        input.store,
        input.prompt,
        vw,
        m,
        input.zoom_pct,
        input.inline_titlebar,
    );

    // 5. Floating overlays anchored directly below (or flipped above) the live inline prompt.
    if let Some(rs) = &input.editor.reverse_search {
        layout_reverse_search_overlay(&mut scene, rs, col_x0, prompt_y0, prompt_y1, vh, col_x1, m);
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

    // Right-aligned inline controls inside the titlebar: `fork`, optional zoom badge, cwd/git chip.
    let mut rx = vw - 16.0;
    let fork_label = "fork";
    let fork_w = icon_s + 6.0 + fork_label.len() as f32 * m.cw + 16.0;
    let fork_x0 = (rx - fork_w).max(x + 20.0);
    let btn_y0 = 7.0;
    let btn_y1 = TOP_BAR_H - 7.0;
    scene.round_rect(fork_x0, btn_y0, rx, btn_y1, 6.0, theme::BG_CHIP, 1.0, theme::LINE, 0);
    draw_icon(scene, Icon::Branch, fork_x0 + 8.0, icon_y, icon_s, theme::INK_3, 0);
    scene.text(fork_x0 + 8.0 + icon_s + 5.0, mid_y, fork_label, theme::INK_2, false, false, 0);
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

    // Inline repo/directory + branch status badge tucked cleanly on the right of the titlebar.
    let dir_txt = truncate_chars(&prompt.dir_short, 24);
    let branch_part = prompt.git_branch.as_deref().unwrap_or("");
    let badge_part = prompt
        .git_status
        .as_ref()
        .map(|s| s.status_badge.as_str())
        .unwrap_or("");
    let dir_icon_s = 13.0;
    let mut info_w = dir_icon_s + 6.0 + dir_txt.chars().count() as f32 * m.cw;
    if !branch_part.is_empty() {
        info_w += 14.0 + icon_s + 5.0 + branch_part.chars().count() as f32 * m.cw;
        if !badge_part.is_empty() {
            info_w += 6.0 + badge_part.chars().count() as f32 * m.cw;
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
            draw_icon(scene, Icon::Branch, ix, icon_y, icon_s, theme::PROMPT_BRANCH, 0);
            ix += icon_s + 5.0;
            scene.text(ix, mid_y, branch_part, theme::PROMPT_BRANCH, false, false, 0);
            ix += branch_part.chars().count() as f32 * m.cw;
            if !badge_part.is_empty() {
                ix += 6.0;
                scene.text(ix, mid_y, badge_part, theme::PROMPT_STATUS, false, false, 0);
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

/// Draw a Starship-style context line (`folder  dir on  branch [?] ✦`) using `blaze-git` status.
#[allow(clippy::too_many_arguments)]
fn layout_starship_line(
    scene: &mut Scene,
    prompt: &PromptInfo,
    x0: f32,
    y: f32,
    max_w: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    let icon_s = 13.5;
    let mut cx = x0;
    let iy = y + (m.ch - icon_s) * 0.5;
    scene.svg_icon(cx, iy, icon_s, "folder_macos", clip_id);
    cx += icon_s + 6.0;

    let max_dir = ((max_w * 0.45) / m.cw).floor().max(12.0) as usize;
    let dir_lbl = truncate_chars(
        if prompt.dir_short.is_empty() { &prompt.cwd_display } else { &prompt.dir_short },
        max_dir,
    );
    scene.text(cx, y, &dir_lbl, theme::PROMPT_DIR, true, false, clip_id);
    cx += dir_lbl.chars().count() as f32 * m.cw;

    if let Some(branch) = &prompt.git_branch {
        scene.text(cx, y, " on ", theme::INK_3, false, false, clip_id);
        cx += 4.0 * m.cw;
        let b_s = 12.0;
        draw_icon(scene, Icon::Branch, cx, y + (m.ch - b_s) * 0.5, b_s, theme::PROMPT_BRANCH, clip_id);
        cx += b_s + 4.0;

        let branch_lbl = if prompt.git_detached {
            format!("@{branch}")
        } else {
            branch.clone()
        };
        scene.text(cx, y, &branch_lbl, theme::PROMPT_BRANCH, true, false, clip_id);
        cx += branch_lbl.chars().count() as f32 * m.cw;

        if let Some(st) = &prompt.git_status {
            if let Some(state_lbl) = &st.state_label {
                let s_txt = format!(" ({state_lbl})");
                scene.text(cx, y, &s_txt, theme::FIRE, true, false, clip_id);
                cx += s_txt.chars().count() as f32 * m.cw;
            }
            if !st.status_badge.is_empty() {
                cx += 0.8 * m.cw;
                scene.text(cx, y, &st.status_badge, theme::PROMPT_STATUS, true, false, clip_id);
                cx += st.status_badge.chars().count() as f32 * m.cw;
            }
        }
    }

    if prompt.in_rust_project || prompt.git_branch.is_some() {
        cx += 0.9 * m.cw;
        scene.text(cx, y, "✦", theme::PROMPT_SPARK, false, false, clip_id);
    }
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

    // Starship context line (`folder dir on  branch [?] ✦`) powered by `blaze-git`.
    if show_context {
        layout_starship_line(scene, &block.prompt_snapshot, x0, cur_y, x1 - x0, m, clip_id);
        cur_y += m.ch + 2.0;
    }

    // Right-aligned quiet inline metadata on the command row: exit, duration, optional grid/table toggle, `fork`.
    let mut rx = x1;
    let fork_lbl = "fork";
    let fork_w = fork_lbl.len() as f32 * m.cw;
    let fx0 = rx - fork_w;
    scene.text(fx0, cur_y, fork_lbl, theme::INK_4, false, false, clip_id);
    scene.push_hit_clipped(
        HitRect::new(fx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0),
        clip_id,
        HitAction::ForkBlock(block.id),
    );
    rx = fx0 - 14.0;

    // Quiet `table` / `grid` toggle on file-entry tables (e.g. `ls`, `ls -l`, `ls | sort-by ...`).
    if let BlockBody::Value(Value::Table(t)) = &block.body
        && is_file_table(t)
    {
        let in_grid = block.is_grid_view(t.default_grid);
        let mode_lbl = if in_grid { "table" } else { "grid" };
        let mw = mode_lbl.len() as f32 * m.cw;
        let mx0 = rx - mw;
        scene.text(mx0, cur_y, mode_lbl, theme::INK_4, false, false, clip_id);
        scene.push_hit_clipped(
            HitRect::new(mx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0),
            clip_id,
            HitAction::ToggleTableView(block.id),
        );
        rx = mx0 - 14.0;
    }

    let dur_lbl = format_duration_ms(block.duration_ms);
    let dur_w = dur_lbl.len() as f32 * m.cw;
    scene.text(rx - dur_w, cur_y, dur_lbl, theme::INK_4, false, false, clip_id);
    rx -= dur_w + 12.0;

    if matches!(&block.body, BlockBody::Terminal { running: true, .. }) {
        let lbl = "running";
        let w = lbl.len() as f32 * m.cw;
        scene.text(rx - w, cur_y, lbl, theme::FIRE, false, false, clip_id);
        rx -= w + 12.0;
    } else if block.exit_code != 0 {
        let lbl = format!("exit {}", block.exit_code);
        let w = lbl.len() as f32 * m.cw;
        scene.text(rx - w, cur_y, lbl, theme::DANGER, false, false, clip_id);
        rx -= w + 12.0;
    }

    // Command row: Starship `❯` prompt + syntax-highlighted command source.
    let prompt_col = match &block.body {
        BlockBody::Error { .. } | BlockBody::Value(Value::Error(_)) => theme::DANGER,
        BlockBody::RustCell { .. } => theme::FIRE,
        _ if block.exit_code != 0 => theme::DANGER,
        _ => theme::PROMPT_OK,
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
) -> f32 {
    let trimmed = editor.text.trim_start();
    let is_rust_mode = trimmed.starts_with("rust") || trimmed.starts_with(":rs");
    let drop_hover = drag.is_some_and(|d| d.over_composer);

    // Line 1: Starship context header (`folder dir on  branch [$3] ✦`) + quiet right metadata.
    layout_starship_line(scene, prompt, x0, y0, (x1 - x0) * 0.72, m, clip_id);

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
        scene.text(x1 - rw, y0, right_txt, right_col, false, false, clip_id);
    }

    // Line 2+: `❯` prompt + editable syntax-highlighted text + Fish ghost text directly inline.
    let input_y = y0 + m.ch + 2.0;
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

    let prompt_col = if is_rust_mode {
        theme::FIRE
    } else if prompt.last_exit != 0 {
        theme::DANGER
    } else {
        theme::PROMPT_OK
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
        if let Some(ghost) = &editor.ghost
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

    if cursor_visible {
        let (cline, ccol) = editor.cursor_line_col();
        let cur_x = text_x + ccol as f32 * m.cw;
        let cur_y = input_y + cline as f32 * line_step;
        let cur_col = if is_rust_mode || drop_hover { theme::FIRE } else { theme::INK_1 };
        scene.over_round_rect(cur_x, cur_y + 1.0, cur_x + 1.8, cur_y + m.ch - 1.0, 0.9, cur_col, clip_id);
    }

    (input_y + rendered_h.max(text_h) - y0) + 2.0
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

#[allow(clippy::too_many_arguments)]
fn layout_reverse_search_overlay(
    scene: &mut Scene,
    rs: &crate::editor::ReverseSearch,
    x0: f32,
    prompt_y0: f32,
    prompt_y1: f32,
    vh: f32,
    x1: f32,
    m: CellMetrics,
) {
    let h = 36.0;
    let below_y0 = prompt_y1 + 6.0;
    let py0 = if below_y0 + h <= vh - 12.0 {
        below_y0.max(TOP_BAR_H + 8.0)
    } else {
        (prompt_y0 - 8.0 - h).max(TOP_BAR_H + 8.0)
    };
    let py1 = py0 + h;
    scene.overlay_shadow(x0, py0 + 4.0, x1, py1 + 4.0, 8.0, 14.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(
        x0,
        py0,
        x1,
        py1,
        8.0,
        theme::BG_POPUP,
        1.0,
        theme::LINE_STRONG,
        0,
    );
    let ty = py0 + (h - m.ch) * 0.5;
    let prefix = format!("history  {}  │  ", rs.query);
    scene.overlay_text_run(x0 + 14.0, ty, &prefix, theme::INK_3, false, false, 0);
    let mx = x0 + 14.0 + prefix.chars().count() as f32 * m.cw;
    let max_m = ((x1 - 14.0 - mx) / m.cw).floor().max(8.0) as usize;
    match &rs.matched_text {
        Some(matched) => {
            scene.overlay_text_run(mx, ty, truncate_chars(matched, max_m), theme::INK_1, false, false, 0);
        }
        None => {
            scene.overlay_text_run(mx, ty, "no match", theme::INK_4, false, true, 0);
        }
    }
}
