//! GPU-independent minimal layout engine turning `(BlockStore, Editor, PromptInfo, viewport)`
//! into a layered [`Scene`] of hairlines, subtle surfaces, clips, text runs, and clickable
//! [`HitRegion`]s. Borderless editorial typography over a pitch-dark canvas — zero noisy
//! nested boxes or pill clutter.

use blaze_own::Alloc;
use blaze_sh::lex::{HighlightSpan, highlight};
use blaze_sh::{Record, ShellError, Table, Value, line_col};
use blaze_vt::color::Colors;
use blaze_vt::page::Wide;
use blaze_vt::style::{Color as VtColor, Rgb as VtRgb, Underline as VtUnderline};
use blaze_vt::terminal::{Mode as VtMode, Terminal};

use crate::blocks::{Block, BlockBody, BlockStore, TABLE_FOLD_ROWS};
use crate::editor::{CompletionKind, Editor};
use crate::prompt::PromptInfo;
use crate::theme;
use crate::view::{
    CellMetrics, HitAction, HitRect, Scene, format_duration_ms, format_value_cell,
    is_expandable_cell, truncate_chars, wrap_lines,
};

const TOP_BAR_H: f32 = 34.0;
const OUTER_PAD_X: f32 = 28.0;
const BLOCK_PAD_Y: f32 = 16.0;
const TABLE_ROW_PAD_Y: f32 = 4.0;
const MAX_POPUP_ROWS: usize = 8;

/// All inputs required to lay out one frame of `blaze-shell`.
pub struct LayoutInput<'a> {
    pub store: &'a BlockStore,
    pub editor: &'a Editor,
    pub prompt: &'a PromptInfo,
    pub viewport_w: f32,
    pub viewport_h: f32,
    pub metrics: CellMetrics,
    pub scroll_y: f32,
    pub cursor_visible: bool,
}

/// Lay out the full minimal shell interface: hairline branch strip, open borderless block
/// stream separated by hairlines, sleek docked bottom composer, and floating overlays.
pub fn build_scene(input: &LayoutInput<'_>) -> Scene {
    let mut scene = Scene::new();
    let vw = input.viewport_w.max(320.0);
    let vh = input.viewport_h.max(240.0);
    let m = input.metrics;

    // 1. Docked bottom composer height.
    let editor_lines: Vec<&str> = if input.editor.text.is_empty() {
        vec![""]
    } else {
        input.editor.text.split('\n').collect()
    };
    let visible_input_lines = editor_lines.len().clamp(1, 8);
    let composer_h = (28.0 + visible_input_lines as f32 * (m.ch + 4.0) + 22.0).clamp(74.0, 240.0);
    let input_top_y = (vh - composer_h).max(TOP_BAR_H + 40.0);
    scene.input_top_y = input_top_y;

    // 2. Scrollable block feed clip region (clip_id = 1).
    let feed_top = TOP_BAR_H;
    let feed_bottom = input_top_y.max(feed_top + 1.0);
    let feed_clip = scene.push_clip(0.0, feed_top, vw, feed_bottom, 0.0);
    scene.scroll_viewport_height = (feed_bottom - feed_top).max(1.0);

    // 3. Open borderless block stream.
    let col_x0 = OUTER_PAD_X;
    let col_x1 = (vw - OUTER_PAD_X).max(col_x0 + 200.0);
    let blocks = input.store.active_blocks();

    let mut content_y = 14.0f32;
    if blocks.is_empty() {
        let h = layout_welcome(
            &mut scene,
            col_x0,
            feed_top + content_y - input.scroll_y,
            m,
            feed_clip,
        );
        content_y += h + 16.0;
    } else {
        let mut prev_cwd: Option<&str> = None;
        for (idx, blk) in blocks.iter().enumerate() {
            if idx > 0 {
                let sep_y = feed_top + content_y - input.scroll_y;
                scene.line(col_x0, sep_y, col_x1, sep_y, 1.0, theme::HAIRLINE_FAINT, feed_clip);
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
            );
            content_y += h + BLOCK_PAD_Y;
        }
    }
    scene.total_content_height = content_y + 12.0;

    // 4. Minimal top branch strip (clip 0).
    layout_top_bar(&mut scene, input.store, vw, m);

    // 5. Sleek docked bottom composer (clip 0).
    layout_composer(
        &mut scene,
        input.editor,
        input.prompt,
        vw,
        input_top_y,
        vh,
        m,
        input.cursor_visible,
    );

    // 6. Floating overlays (completion popup and Ctrl+R reverse search).
    if let Some(rs) = &input.editor.reverse_search {
        layout_reverse_search_overlay(&mut scene, rs, col_x0, input_top_y - 10.0, col_x1, m);
    } else if !input.editor.completions.is_empty() {
        layout_completion_popup(&mut scene, input.editor, col_x0, input_top_y - 10.0, col_x1, m);
    }

    scene
}

fn layout_top_bar(scene: &mut Scene, store: &BlockStore, vw: f32, m: CellMetrics) {
    scene.round_rect(0.0, 0.0, vw, TOP_BAR_H, 0.0, theme::BG_TOPBAR, 0.0, [0.0; 4], 0);
    scene.line(0.0, TOP_BAR_H - 0.5, vw, TOP_BAR_H - 0.5, 1.0, theme::HAIRLINE, 0);

    let mid_y = (TOP_BAR_H - m.ch) * 0.5;
    let mut x = OUTER_PAD_X;

    // Right-aligned minimal `+ fork` text action (no heavy box).
    let fork_label = "+ fork";
    let fork_w = fork_label.len() as f32 * m.cw + 12.0;
    let fork_x1 = vw - OUTER_PAD_X;
    let fork_x0 = (fork_x1 - fork_w).max(x + 20.0);
    scene.text(fork_x0 + 6.0, mid_y, fork_label, theme::FG_MUTED, false, false, 0);
    scene.push_hit(HitRect::new(fork_x0, 2.0, fork_x1, TOP_BAR_H - 2.0), HitAction::ForkActiveBranch);

    // Clean minimal underline tabs for session branches.
    let active_id = store.active_branch_id();
    for branch in store.branches() {
        let is_active = branch.id == active_id;
        let name = branch.name.as_str();
        let count_str = format!(" {}", branch.blocks.len());
        let name_w = name.chars().count() as f32 * m.cw;
        let count_w = count_str.chars().count() as f32 * m.cw;
        let tab_w = name_w + count_w + 6.0;
        if x + tab_w > fork_x0 - 16.0 {
            break;
        }
        let name_col = if is_active { theme::FG_BRIGHT } else { theme::FG_MUTED };
        let count_col = if is_active { theme::FG_MUTED } else { theme::FG_DIM };
        scene.text(x, mid_y, name, name_col, is_active, false, 0);
        scene.text(x + name_w, mid_y, count_str, count_col, false, false, 0);
        if is_active {
            // Crisp 1.5px pure-white active indicator sitting on the bottom hairline.
            scene.over_round_rect(
                x,
                TOP_BAR_H - 1.5,
                x + name_w + count_w,
                TOP_BAR_H,
                0.75,
                theme::ACCENT_WHITE,
                0,
            );
        }
        scene.push_hit(
            HitRect::new(x - 4.0, 0.0, x + tab_w + 4.0, TOP_BAR_H),
            HitAction::SwitchBranch(branch.id),
        );
        x += tab_w + 20.0;
    }
}

fn layout_welcome(scene: &mut Scene, x0: f32, y0: f32, m: CellMetrics, clip_id: u32) -> f32 {
    scene.text(x0, y0 + 8.0, "blaze-shell", theme::FG_BRIGHT, true, false, clip_id);
    scene.text(
        x0,
        y0 + 8.0 + m.ch + 8.0,
        "ls | sort-by size -r | take 8    ·    sys    ·    rust { (1..=10).sum::<i64>() }",
        theme::FG_MUTED,
        false,
        false,
        clip_id,
    );
    scene.text(
        x0,
        y0 + 8.0 + (m.ch + 8.0) * 2.0,
        "tab complete  ·  ctrl+r history  ·  cmd+b fork  ·  cmd+k clear",
        theme::FG_DIM,
        false,
        false,
        clip_id,
    );
    (m.ch + 8.0) * 3.0 + 12.0
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
) -> f32 {
    let mut cur_y = y0;

    // Optional quiet context line (only shown on first block or when cwd changes).
    if show_context {
        let mut cx = x0;
        let max_cwd = (((x1 - x0) * 0.55) / m.cw).floor().max(16.0) as usize;
        let cwd_lbl = truncate_chars(&block.prompt_snapshot.cwd_display, max_cwd);
        scene.text(cx, cur_y, &cwd_lbl, theme::FG_DIM, false, false, clip_id);
        cx += cwd_lbl.chars().count() as f32 * m.cw;

        if let Some(branch) = &block.prompt_snapshot.git_branch {
            let dirty = block.prompt_snapshot.git_dirty.map(|d| d.dirty).unwrap_or(false);
            let sep = "  ";
            cx += sep.len() as f32 * m.cw;
            let dot_c = if dirty { theme::ACCENT_SAND } else { theme::ACCENT_SAGE };
            scene.circle(cx + 3.0, cur_y + m.ch * 0.5, 2.2, dot_c, clip_id);
            cx += 10.0;
            let git_lbl = if dirty { format!("{branch}*") } else { branch.clone() };
            scene.text(cx, cur_y, git_lbl, theme::FG_MUTED, false, false, clip_id);
        }
        cur_y += m.ch + 6.0;
    }

    // Right-aligned quiet inline metadata on the command row: duration, optional exit, `fork`.
    let mut rx = x1;
    let fork_lbl = "fork";
    let fork_w = fork_lbl.len() as f32 * m.cw;
    let fx0 = rx - fork_w;
    scene.text(fx0, cur_y, fork_lbl, theme::FG_DIM, false, false, clip_id);
    scene.push_hit_clipped(
        HitRect::new(fx0 - 4.0, cur_y - 2.0, rx + 4.0, cur_y + m.ch + 2.0),
        clip_id,
        HitAction::ForkBlock(block.id),
    );
    rx = fx0 - 14.0;

    let dur_lbl = format_duration_ms(block.duration_ms);
    let dur_w = dur_lbl.len() as f32 * m.cw;
    scene.text(rx - dur_w, cur_y, dur_lbl, theme::FG_DIM, false, false, clip_id);
    rx -= dur_w + 12.0;

    if matches!(&block.body, BlockBody::Terminal { running: true, .. }) {
        let lbl = "running";
        let w = lbl.len() as f32 * m.cw;
        scene.text(rx - w, cur_y, lbl, theme::ACCENT_SAND, false, false, clip_id);
        rx -= w + 12.0;
    } else if block.exit_code != 0 {
        let lbl = format!("exit {}", block.exit_code);
        let w = lbl.len() as f32 * m.cw;
        scene.text(rx - w, cur_y, lbl, theme::ACCENT_CORAL, false, false, clip_id);
        rx -= w + 12.0;
    }

    // Command line: subtle chevron + syntax-highlighted source.
    let chev_color = match &block.body {
        BlockBody::Error { .. } | BlockBody::Value(Value::Error(_)) => theme::ACCENT_CORAL,
        BlockBody::RustCell { .. } => theme::ACCENT_COPPER,
        _ if block.exit_code != 0 => theme::ACCENT_CORAL,
        _ => theme::FG_MUTED,
    };
    scene.text(x0, cur_y, "❯", chev_color, false, false, clip_id);

    let cmd_x = x0 + 2.2 * m.cw;
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

    // Body aligned directly under `cmd_x` for razor-clean vertical grid alignment.
    let has_body = !matches!(&block.body, BlockBody::Value(Value::Nothing));
    if has_body {
        cur_y += 10.0;
        let body_w = (x1 - cmd_x).max(120.0);
        let body_h = match &block.body {
            BlockBody::Value(val) => {
                layout_value(scene, block, val, cmd_x, cur_y, body_w, m, clip_id, 0)
            }
            BlockBody::StreamAndValue { lines, value } => {
                let mut sy = cur_y;
                let max_cols = (body_w / m.cw).floor().max(20.0) as usize;
                for line in lines {
                    for wrapped in wrap_lines(line, max_cols) {
                        scene.text(cmd_x, sy, wrapped, theme::FG_PRIMARY, false, false, clip_id);
                        sy += m.ch + 2.0;
                    }
                }
                if !matches!(value, Value::Nothing) {
                    sy += 6.0;
                    sy += layout_value(scene, block, value, cmd_x, sy, body_w, m, clip_id, 0);
                }
                sy - cur_y
            }
            BlockBody::Error { err } => {
                layout_error_panel(scene, &block.source, err, cmd_x, cur_y, body_w, m, clip_id)
            }
            BlockBody::RustCell { compile_ms, run_ms, stdout, value, .. } => {
                layout_rust_cell_body(
                    scene,
                    block,
                    *compile_ms,
                    *run_ms,
                    stdout,
                    value,
                    cmd_x,
                    cur_y,
                    body_w,
                    m,
                    clip_id,
                )
            }
            BlockBody::Terminal { term, running, .. } => {
                layout_inline_terminal(
                    scene,
                    term,
                    *running && cursor_visible,
                    cmd_x,
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

/// Draw syntax-highlighted source text across one or more lines, returning total height.
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

    let mut byte_colors: Vec<[f32; 4]> = vec![theme::FG_PRIMARY; source.len()];
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
        let mut run_color = theme::FG_PRIMARY;
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
            let c = byte_colors
                .get(line_start + rel_idx)
                .copied()
                .unwrap_or(theme::FG_PRIMARY);
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
) -> f32 {
    match val {
        Value::Nothing => 0.0,
        Value::Table(t) => layout_table(scene, block, t, x0, y0, width, m, clip_id, depth),
        Value::Record(r) => layout_record(scene, block, r, x0, y0, width, m, clip_id, depth),
        Value::List(items) => layout_list(scene, block, items, x0, y0, width, m, clip_id, depth),
        Value::Error(err) => {
            layout_error_panel(scene, &block.source, err, x0, y0, width, m, clip_id)
        }
        scalar => layout_scalar(scene, scalar, x0, y0, width, m, clip_id),
    }
}

/// Editorial borderless table: clean muted header + single bottom hairline, airy columns,
/// zero outer box clutter.
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
) -> f32 {
    let ncols = table.ncols();
    if ncols == 0 {
        scene.text(x0, y0, "empty table", theme::FG_DIM, false, true, clip_id);
        return m.ch;
    }

    let visible_rows = if depth == 0 && block.folded && table.nrows() > TABLE_FOLD_ROWS {
        TABLE_FOLD_ROWS
    } else {
        table.nrows()
    };

    let idx_chars = table.nrows().max(1).to_string().len().max(2);
    let idx_w = idx_chars as f32 * m.cw + 18.0;
    let col_gap = 20.0;
    let avail_w = (width - idx_w).max(80.0);

    let mut col_chars: Vec<usize> = Vec::with_capacity(ncols);
    let mut col_right_align: Vec<bool> = Vec::with_capacity(ncols);

    for c in 0..ncols {
        let hdr = table.columns.get(c).map(|s| s.as_ref()).unwrap_or("");
        let mut max_c = hdr.chars().count().max(3);
        let mut numeric_votes = 0usize;
        let mut non_empty = 0usize;
        for r in 0..visible_rows {
            if let Some(cell) = table.cell(r, c) {
                let txt = format_value_cell(cell);
                max_c = max_c.max(txt.chars().count());
                if !matches!(cell, Value::Nothing) {
                    non_empty += 1;
                    if theme::is_right_aligned(cell) {
                        numeric_votes += 1;
                    }
                }
            }
        }
        col_chars.push(max_c.min(44));
        col_right_align.push(non_empty > 0 && numeric_votes * 2 >= non_empty);
    }

    let mut col_widths: Vec<f32> = col_chars
        .iter()
        .map(|&ch| ch as f32 * m.cw + col_gap)
        .collect();
    let total_w: f32 = col_widths.iter().sum();
    if total_w > avail_w && total_w > 0.0 {
        let scale = avail_w / total_w;
        for w in &mut col_widths {
            *w = (*w * scale).max(48.0);
        }
    }
    let table_w = (idx_w + col_widths.iter().sum::<f32>() - col_gap * 0.5).min(width);
    let x1 = x0 + table_w;

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;

    // Minimal column headers + one crisp hairline underneath.
    scene.text(x0, y0, "#", theme::FG_DIM, false, false, clip_id);
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
        scene.text(tx, y0, hdr, theme::FG_MUTED, false, false, clip_id);
        cx += cw;
    }

    let hdr_line_y = y0 + m.ch + 6.0;
    scene.line(x0, hdr_line_y, x1, hdr_line_y, 1.0, theme::HAIRLINE, clip_id);

    let mut cur_y = hdr_line_y + 5.0;

    if visible_rows == 0 {
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, "0 rows", theme::FG_DIM, false, true, clip_id);
        cur_y += row_h;
    } else {
        for r in 0..visible_rows {
            let ty = cur_y + TABLE_ROW_PAD_Y;
            scene.text(x0, ty, r.to_string(), theme::FG_DIM, false, false, clip_id);

            let mut expanded_for_row: Vec<(usize, &Value<'_>)> = Vec::new();
            let mut rx = x0 + idx_w;
            for c in 0..ncols {
                let cw = col_widths[c];
                let content_w = (cw - col_gap).max(m.cw * 2.0);
                let max_ch = (content_w / m.cw).floor().max(2.0) as usize;
                if let Some(cell) = table.cell(r, c) {
                    let raw = format_value_cell(cell);
                    let txt = truncate_chars(&raw, max_ch);
                    if depth == 0 && is_expandable_cell(cell) {
                        let expanded = block.is_cell_expanded(r, c);
                        let badge_w = (txt.chars().count() as f32 * m.cw + 12.0).min(content_w + 4.0);
                        let bx0 = rx - 4.0;
                        let by0 = cur_y + 2.0;
                        let by1 = cur_y + row_h - 2.0;
                        let bg = if expanded { theme::BG_SELECTED } else { theme::BG_CHIP };
                        scene.round_rect(bx0, by0, bx0 + badge_w, by1, 4.0, bg, 0.0, [0.0; 4], clip_id);
                        scene.text(bx0 + 6.0, ty, txt, theme::ACCENT_ICE, false, false, clip_id);
                        scene.push_hit_clipped(
                            HitRect::new(bx0, by0, bx0 + badge_w, by1),
                            clip_id,
                            HitAction::ToggleExpandCell { block: block.id, row: r, col: c },
                        );
                        if expanded {
                            expanded_for_row.push((c, cell));
                        }
                    } else {
                        // Subtle tonal distinction: strings in crisp bright white, secondary text calm.
                        let color = if c == 0 && matches!(cell, Value::String(_)) {
                            theme::FG_BRIGHT
                        } else if matches!(cell, Value::String(_)) {
                            theme::FG_MUTED
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
                rx += cw;
            }
            cur_y += row_h;

            // Inline expansion drawer for clicked nested cells.
            for (col_idx, nested) in expanded_for_row {
                let col_name = table.columns.get(col_idx).map(|s| s.as_ref()).unwrap_or("cell");
                let drawer_x0 = x0 + idx_w;
                let drawer_w = (width - idx_w).max(120.0);
                let rail_top = cur_y + 2.0;
                scene.text(
                    drawer_x0 + 10.0,
                    cur_y + 2.0,
                    format!("{col_name}"),
                    theme::FG_MUTED,
                    false,
                    false,
                    clip_id,
                );
                cur_y += m.ch + 6.0;
                let sub_h = layout_value(
                    scene,
                    block,
                    nested,
                    drawer_x0 + 10.0,
                    cur_y,
                    (drawer_w - 10.0).max(80.0),
                    m,
                    clip_id,
                    depth + 1,
                );
                cur_y += sub_h + 6.0;
                scene.over_round_rect(
                    drawer_x0,
                    rail_top,
                    drawer_x0 + 1.5,
                    cur_y - 4.0,
                    0.75,
                    theme::HAIRLINE_STRONG,
                    clip_id,
                );
            }
        }
    }

    // Minimal inline fold toggle link.
    if depth == 0 && table.nrows() > TABLE_FOLD_ROWS {
        cur_y += 4.0;
        let lbl = if block.folded {
            format!("+ {} more rows", table.nrows() - TABLE_FOLD_ROWS)
        } else {
            format!("− collapse to {TABLE_FOLD_ROWS} rows")
        };
        let pw = lbl.chars().count() as f32 * m.cw + 8.0;
        scene.text(x0 + idx_w, cur_y, lbl, theme::FG_MUTED, false, false, clip_id);
        scene.push_hit_clipped(
            HitRect::new(x0 + idx_w - 4.0, cur_y - 2.0, x0 + idx_w + pw, cur_y + m.ch + 2.0),
            clip_id,
            HitAction::ToggleFold(block.id),
        );
        cur_y += m.ch + 4.0;
    }

    cur_y - y0
}

/// Editorial borderless two-column record.
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
) -> f32 {
    if rec.is_empty() {
        scene.text(x0, y0, "{}", theme::FG_DIM, false, false, clip_id);
        return m.ch;
    }
    let max_key_chars = rec
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 28);
    let key_col_w = (max_key_chars as f32 * m.cw + 24.0).min(width * 0.36);
    let val_x = x0 + key_col_w;
    let val_w = (width - key_col_w).max(80.0);
    let max_val_cols = (val_w / m.cw).floor().max(12.0) as usize;

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;
    let mut cur_y = y0;
    for (k, v) in rec.iter() {
        let start_y = cur_y;
        let is_nested = depth < 2 && matches!(v, Value::Table(_) | Value::Record(_) | Value::List(_));
        let k_lbl = truncate_chars(k, max_key_chars);
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, k_lbl, theme::FG_MUTED, false, false, clip_id);
        if is_nested {
            let sub_h = layout_value(scene, block, v, val_x, cur_y + 2.0, val_w, m, clip_id, depth + 1);
            cur_y += (sub_h + 8.0).max(row_h);
        } else {
            let formatted = format_value_cell(v);
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
            let text_h = lines.len() as f32 * (m.ch + 2.0) + TABLE_ROW_PAD_Y * 2.0 - 2.0;
            cur_y += text_h.max(row_h);
        }
    }
    cur_y - y0
}

/// Editorial borderless indexed list.
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
) -> f32 {
    if items.is_empty() {
        scene.text(x0, y0, "[]", theme::FG_DIM, false, false, clip_id);
        return m.ch;
    }
    let show = if depth == 0 && block.folded && items.len() > TABLE_FOLD_ROWS {
        TABLE_FOLD_ROWS
    } else {
        items.len()
    };
    let idx_w = (items.len().to_string().len().max(2) as f32 * m.cw + 18.0).max(32.0);
    let val_x = x0 + idx_w;
    let max_val_cols = ((width - idx_w) / m.cw).floor().max(12.0) as usize;

    let row_h = m.ch + TABLE_ROW_PAD_Y * 2.0;
    let mut cur_y = y0;
    for i in 0..show {
        scene.text(x0, cur_y + TABLE_ROW_PAD_Y, i.to_string(), theme::FG_DIM, false, false, clip_id);
        if let Some(val) = items.get(i) {
            let txt = truncate_chars(&format_value_cell(val), max_val_cols);
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
        scene.text(val_x, cur_y, lbl, theme::FG_MUTED, false, false, clip_id);
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
) -> f32 {
    let type_lbl = val.type_name();
    let type_w = type_lbl.len() as f32 * m.cw;
    let text_w = (width - type_w - 20.0).max(80.0);
    let max_cols = (text_w / m.cw).floor().max(10.0) as usize;
    let formatted = format_value_cell(val);
    let lines = wrap_lines(&formatted, max_cols);
    let color = theme::value_color(val);

    // Whisper-quiet right-aligned type hint (no bordered pill box).
    scene.text(x0 + width - type_w, y0, type_lbl, theme::FG_DIM, false, false, clip_id);

    let mut cur_y = y0;
    for line in &lines {
        scene.text(x0, cur_y, line, color, false, false, clip_id);
        cur_y += m.ch + 2.0;
    }
    (cur_y - y0).max(m.ch)
}

/// Minimal crimson-rail error block with exact span underline (zero bulky red boxes).
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
    scene.round_rect(x0, y0, x1, y0 + 24.0, 4.0, theme::ACCENT_CORAL_FAINT, 0.0, [0.0; 4], clip_id);
    let rail_idx = scene.rects.len();
    scene.round_rect(x0, y0, x0 + 2.0, y0 + 24.0, 1.0, theme::ACCENT_CORAL, 0.0, [0.0; 4], clip_id);

    let px = x0 + 14.0;
    let mut cur_y = y0 + 8.0;

    // Clean inline error label + crisp message.
    let kind_lbl = err.kind.as_str();
    scene.text(px, cur_y, kind_lbl, theme::ACCENT_CORAL, true, false, clip_id);
    let msg_x = px + (kind_lbl.len() as f32 + 2.0) * m.cw;
    let msg_cols = ((x1 - 12.0 - msg_x) / m.cw).floor().max(14.0) as usize;
    let msg_lines = wrap_lines(err.message.as_str(), msg_cols);
    for (i, line) in msg_lines.iter().enumerate() {
        let tx = if i == 0 { msg_x } else { px };
        scene.text(tx, cur_y, line, theme::FG_BRIGHT, false, false, clip_id);
        cur_y += m.ch + 3.0;
    }

    // Offending source line with crisp crimson underline directly beneath `err.span`.
    if !source.is_empty() {
        cur_y += 3.0;
        let lc = line_col(source, err.span.start);
        let line_text = &source[lc.line_start..lc.line_end];
        let gutter = format!("{} │ ", lc.line);
        let gutter_w = gutter.chars().count() as f32 * m.cw;

        scene.text(px, cur_y, gutter, theme::FG_DIM, false, false, clip_id);
        scene.text(px + gutter_w, cur_y, line_text, theme::FG_MUTED, false, false, clip_id);

        let start_b = (err.span.start as usize).clamp(lc.line_start, lc.line_end);
        let end_b = (err.span.end as usize).clamp(start_b, lc.line_end);
        let col0 = source[lc.line_start..start_b].chars().count();
        let span_chars = source[start_b..end_b].chars().count().max(1);
        let ux0 = px + gutter_w + col0 as f32 * m.cw;
        let ux1 = (ux0 + span_chars as f32 * m.cw).min(x1 - 12.0);
        let uy = cur_y + m.ch + 1.0;
        scene.over_round_rect(ux0, uy, ux1.max(ux0 + m.cw), uy + 2.0, 1.0, theme::ACCENT_CORAL, clip_id);

        cur_y += m.ch + 8.0;
    }

    if let Some(help) = &err.help {
        let help_lbl = "help  ";
        scene.text(px, cur_y, help_lbl, theme::FG_MUTED, false, false, clip_id);
        let hx = px + help_lbl.len() as f32 * m.cw;
        let h_cols = ((x1 - 12.0 - hx) / m.cw).floor().max(16.0) as usize;
        for (i, line) in wrap_lines(help.as_str(), h_cols).iter().enumerate() {
            scene.text(if i == 0 { hx } else { px }, cur_y, line, theme::FG_PRIMARY, false, false, clip_id);
            cur_y += m.ch + 2.0;
        }
    }

    cur_y += 6.0;
    scene.rects[bg_idx].rect[3] = cur_y;
    scene.rects[rail_idx].rect[3] = cur_y;
    cur_y - y0
}

/// Minimal copper-rail Rust REPL cell output.
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
) -> f32 {
    let rail_idx = scene.rects.len();
    scene.round_rect(x0, y0, x0 + 2.0, y0 + 20.0, 1.0, theme::ACCENT_COPPER, 0.0, [0.0; 4], clip_id);

    let px = x0 + 14.0;
    let inner_w = (width - 14.0).max(80.0);
    let mut cur_y = y0;

    let timing = if run_ms > 0 {
        format!(
            "rust · blaze run · {} / {}",
            format_duration_ms(compile_ms),
            format_duration_ms(run_ms)
        )
    } else {
        format!("rust · blaze run · {}", format_duration_ms(compile_ms))
    };
    scene.text(px, cur_y, timing, theme::FG_DIM, false, false, clip_id);
    cur_y += m.ch + 6.0;

    if !stdout.is_empty() {
        let max_cols = (inner_w / m.cw).floor().max(16.0) as usize;
        for line in wrap_lines(stdout, max_cols) {
            scene.text(px, cur_y, line, theme::FG_MUTED, false, false, clip_id);
            cur_y += m.ch + 2.0;
        }
        cur_y += 4.0;
    }

    if !matches!(value, Value::Nothing) {
        cur_y += layout_value(scene, block, value, px, cur_y, inner_w, m, clip_id, 0);
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

    scene.round_rect(x0, y0, x1, y0 + total_h, 6.0, theme::BG_WELL, 1.0, theme::HAIRLINE, clip_id);
    let inner_clip = scene.push_clip(x0 + 1.0, y0 + 1.0, x1 - 1.0, y0 + total_h - 1.0, 5.0);
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
        .unwrap_or(VtRgb { r: 0x09, g: 0x09, b: 0x0b });
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
        .unwrap_or(VtRgb { r: 0xd4, g: 0xd4, b: 0xdc });
    let bg0 = colors
        .background
        .get()
        .unwrap_or(VtRgb { r: 0x0d, g: 0x0d, b: 0x10 });
    let cursor_c = colors.cursor.get().unwrap_or(VtRgb { r: 0xf4, g: 0xf4, b: 0xf7 });
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

/// Sleek full-width docked bottom composer separated by a single razor hairline.
#[allow(clippy::too_many_arguments)]
fn layout_composer(
    scene: &mut Scene,
    editor: &Editor,
    prompt: &PromptInfo,
    vw: f32,
    y0: f32,
    y1: f32,
    m: CellMetrics,
    cursor_visible: bool,
) {
    let trimmed = editor.text.trim_start();
    let is_rust_mode = trimmed.starts_with("rust") || trimmed.starts_with(":rs");

    scene.round_rect(0.0, y0, vw, y1, 0.0, theme::BG_COMPOSER, 0.0, [0.0; 4], 0);
    let top_line_col = if is_rust_mode {
        theme::rgba(0xcf, 0x97, 0x72, 0.35)
    } else {
        theme::HAIRLINE
    };
    scene.line(0.0, y0 + 0.5, vw, y0 + 0.5, 1.0, top_line_col, 0);

    let px = OUTER_PAD_X;
    let x1 = (vw - OUTER_PAD_X).max(px + 160.0);
    let prompt_y = y0 + 11.0;

    // Quiet prompt context row: path + git branch (no boxes).
    let mut cx = px;
    let max_cwd = (((x1 - px) * 0.55) / m.cw).floor().max(16.0) as usize;
    let cwd_lbl = truncate_chars(&prompt.cwd_display, max_cwd);
    scene.text(cx, prompt_y, &cwd_lbl, theme::FG_MUTED, false, false, 0);
    cx += cwd_lbl.chars().count() as f32 * m.cw;

    if let Some(branch) = &prompt.git_branch {
        let dirty = prompt.git_dirty.map(|d| d.dirty).unwrap_or(false);
        cx += 16.0;
        let dot_c = if dirty { theme::ACCENT_SAND } else { theme::ACCENT_SAGE };
        scene.circle(cx + 3.0, prompt_y + m.ch * 0.5, 2.5, dot_c, 0);
        cx += 11.0;
        let lbl = if prompt.git_detached {
            format!("@{branch}")
        } else if dirty {
            format!("{branch}*")
        } else {
            branch.clone()
        };
        let txt_c = if dirty { theme::ACCENT_SAND } else { theme::FG_MUTED };
        scene.text(cx, prompt_y, lbl, txt_c, false, false, 0);
    }

    // Quiet right-aligned status.
    let right_txt = if is_rust_mode {
        "rust · blaze run".to_owned()
    } else if prompt.last_exit != 0 {
        format!("exit {}  {}", prompt.last_exit, format_duration_ms(prompt.last_duration_ms))
    } else if prompt.last_duration_ms > 0 {
        format_duration_ms(prompt.last_duration_ms)
    } else {
        String::new()
    };
    if !right_txt.is_empty() {
        let right_col = if is_rust_mode {
            theme::ACCENT_COPPER
        } else if prompt.last_exit != 0 {
            theme::ACCENT_CORAL
        } else {
            theme::FG_DIM
        };
        let rw = right_txt.chars().count() as f32 * m.cw;
        scene.text(x1 - rw, prompt_y, right_txt, right_col, false, false, 0);
    }

    // Active command input row.
    let input_y = y0 + 36.0;
    let chev_col = if is_rust_mode {
        theme::ACCENT_COPPER
    } else if prompt.last_exit != 0 {
        theme::ACCENT_CORAL
    } else {
        theme::FG_BRIGHT
    };
    scene.text(px, input_y, "❯", chev_col, false, false, 0);

    let text_x = px + 2.2 * m.cw;
    let text_clip = scene.push_clip(text_x - 2.0, input_y - 2.0, x1, y1 - 6.0, 0.0);
    let line_step = m.ch + 4.0;

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
                scene.round_rect(sx0, sy0, sx1, sy0 + m.ch, 2.0, theme::BG_SELECTION, 0.0, [0.0; 4], text_clip);
            }
            line_start_b = line_end_b + 1;
        }
    }

    if editor.text.is_empty() {
        scene.text(
            text_x,
            input_y,
            "command, pipeline, or rust { ... }",
            theme::FG_DIM,
            false,
            false,
            text_clip,
        );
    } else {
        let spans = editor.highlight_spans();
        layout_highlighted_source(scene, &editor.text, &spans, text_x, input_y, x1, m, text_clip);
    }

    // Crisp 1.5px pure-white vertical beam cursor.
    if cursor_visible {
        let (cline, ccol) = editor.cursor_line_col();
        let cur_x = text_x + ccol as f32 * m.cw;
        let cur_y = input_y + cline as f32 * line_step;
        scene.over_round_rect(cur_x, cur_y + 1.0, cur_x + 1.5, cur_y + m.ch - 1.0, 0.75, theme::ACCENT_WHITE, text_clip);
    }
}

fn layout_completion_popup(
    scene: &mut Scene,
    editor: &Editor,
    x0: f32,
    anchor_bottom_y: f32,
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
    let popup_h = show as f32 * row_h + 12.0;
    let popup_w = (x1 - x0).min(520.0);
    let py0 = (anchor_bottom_y - popup_h).max(TOP_BAR_H + 8.0);
    let py1 = py0 + popup_h;

    scene.overlay_shadow(x0, py0 + 4.0, x0 + popup_w, py1 + 4.0, 8.0, 16.0, theme::SHADOW_SOFT, 0);
    scene.overlay_round_rect(
        x0,
        py0,
        x0 + popup_w,
        py1,
        8.0,
        theme::BG_POPUP,
        1.0,
        theme::HAIRLINE_STRONG,
        0,
    );

    let mut ry = py0 + 6.0;
    for offset in 0..show {
        let idx = win_start + offset;
        let item = &editor.completions[idx];
        let is_sel = idx == editor.completion_selected;
        if is_sel {
            scene.overlay_round_rect(
                x0 + 5.0,
                ry,
                x0 + popup_w - 5.0,
                ry + row_h,
                5.0,
                theme::BG_SELECTED,
                0.0,
                [0.0; 4],
                0,
            );
        }
        let (tag, tag_col) = match item.kind {
            CompletionKind::Builtin => ("cmd", theme::FG_MUTED),
            CompletionKind::Def => ("def", theme::ACCENT_ICE),
            CompletionKind::Keyword => ("kw ", theme::FG_MUTED),
            CompletionKind::Flag => ("flg", theme::ACCENT_SAND),
            CompletionKind::Variable => ("var", theme::ACCENT_ICE),
            CompletionKind::Dir => ("dir", theme::ACCENT_SAGE),
            CompletionKind::File => ("fil", theme::FG_DIM),
        };
        let ty = ry + (row_h - m.ch) * 0.5;
        scene.overlay_text_run(x0 + 12.0, ty, tag, tag_col, false, false, 0);
        let label_x = x0 + 12.0 + 4.5 * m.cw;
        let label_txt = truncate_chars(&item.label, 24);
        scene.overlay_text_run(
            label_x,
            ty,
            &label_txt,
            if is_sel { theme::FG_BRIGHT } else { theme::FG_PRIMARY },
            is_sel,
            false,
            0,
        );

        let detail_x = x0 + 12.0 + 29.0 * m.cw;
        let detail_max = ((x0 + popup_w - 12.0 - detail_x) / m.cw).floor().max(6.0) as usize;
        let detail_txt = truncate_chars(&item.detail, detail_max);
        scene.overlay_text_run(detail_x, ty, detail_txt, theme::FG_DIM, false, false, 0);

        scene.push_hit(
            HitRect::new(x0 + 5.0, ry, x0 + popup_w - 5.0, ry + row_h),
            HitAction::SelectCompletion(idx),
        );
        ry += row_h;
    }
}

fn layout_reverse_search_overlay(
    scene: &mut Scene,
    rs: &crate::editor::ReverseSearch,
    x0: f32,
    anchor_bottom_y: f32,
    x1: f32,
    m: CellMetrics,
) {
    let h = 36.0;
    let py0 = (anchor_bottom_y - h).max(TOP_BAR_H + 8.0);
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
        theme::HAIRLINE_STRONG,
        0,
    );
    let ty = py0 + (h - m.ch) * 0.5;
    let prefix = format!("history  {}  │  ", rs.query);
    scene.overlay_text_run(x0 + 14.0, ty, &prefix, theme::FG_MUTED, false, false, 0);
    let mx = x0 + 14.0 + prefix.chars().count() as f32 * m.cw;
    let max_m = ((x1 - 14.0 - mx) / m.cw).floor().max(8.0) as usize;
    match &rs.matched_text {
        Some(matched) => {
            scene.overlay_text_run(mx, ty, truncate_chars(matched, max_m), theme::FG_BRIGHT, false, false, 0);
        }
        None => {
            scene.overlay_text_run(mx, ty, "no match", theme::FG_DIM, false, true, 0);
        }
    }
}
