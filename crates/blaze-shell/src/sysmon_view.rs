//! Native Metal btop + macOS Activity Monitor GUI renderer & controller for `blaze-shell`.
//!
//! Renders the interactive `top` / `btop` / `sysmon` block directly into [`Scene`] primitives
//! (SDF rounded glass cards, 60-sample shaded sparklines, per-core CPU heat strip, Apple Silicon
//! Metal GPU utilization & VRAM meter, macOS Activity Monitor 4-segment memory pressure bar,
//! real Retina `.app` icons, and a sortable collapsible process tree table) matching
//! `ix.dev/blaze`'s warm-charcoal dark-glass aesthetic.

use blaze_own::Alloc;
use blaze_sysmon::{HistoryRing, ProcessRow, SortColumn, SysMonEngine};

use crate::blocks::BlockId;
use crate::theme;
use crate::view::{CellMetrics, HitAction, HitRect, Scene, TextFace, truncate_chars, truncate_middle};

// =============================================================================
// Layout Geometry & Palette Tokens
// =============================================================================

/// Number of process rows shown by default when rendered as an inline block card.
pub const INLINE_VISIBLE_ROWS: usize = 13;

const HEADER_H: f32 = 30.0;
const HEADER_GAP: f32 = 8.0;
const CARDS_H: f32 = 112.0;
const CARDS_GAP: f32 = 10.0;
const FILTER_H: f32 = 26.0;
const FILTER_GAP: f32 = 6.0;
const TABLE_HEADER_H: f32 = 24.0;
const ROW_H: f32 = 22.0;
const TREE_BASE_X: f32 = 13.0;
const TREE_INDENT: f32 = 18.0;
const TREE_MAX_VISUAL_DEPTH: usize = 10;
const INSPECTOR_GAP: f32 = 6.0;
const INSPECTOR_H: f32 = 42.0;
const FOOTER_GAP: f32 = 6.0;
const FOOTER_H: f32 = 24.0;
const PAD_Y: f32 = 10.0;

// Harmonized telemetry semantic palette over `ix.dev/blaze` warm charcoal (`#101011` / `#141415`).
const CARD_BG: [f32; 4] = theme::rgb(0x18, 0x18, 0x1a);
const PLOT_BG: [f32; 4] = theme::rgba(0x0b, 0x0b, 0x0d, 0.72);
const TABLE_HEAD_BG: [f32; 4] = theme::rgba(0x1b, 0x1b, 0x1e, 0.90);
const ZEBRA_BG: [f32; 4] = theme::rgba(0xff, 0xff, 0xff, 0.016);
const ROW_SELECTED_BG: [f32; 4] = theme::rgba(0x3a, 0x62, 0xa8, 0.24);

const AMBER: [f32; 4] = theme::rgb(0xe8, 0xb0, 0x4b);
const AMBER_BG: [f32; 4] = theme::rgba(0xe8, 0xb0, 0x4b, 0.14);
const AMBER_BORDER: [f32; 4] = theme::rgba(0xe8, 0xb0, 0x4b, 0.36);

const GPU_CYAN: [f32; 4] = theme::rgb(0x4f, 0xd1, 0xc5);
const GPU_CYAN_BRIGHT: [f32; 4] = theme::rgb(0x56, 0xd8, 0xc8);
const GPU_CYAN_BG: [f32; 4] = theme::rgba(0x4f, 0xd1, 0xc5, 0.11);
const GPU_CYAN_BORDER: [f32; 4] = theme::rgba(0x4f, 0xd1, 0xc5, 0.30);
const GPU_VIOLET: [f32; 4] = theme::rgb(0xb7, 0x8a, 0xf8);
const GPU_TILER: [f32; 4] = theme::rgb(0x63, 0xb3, 0xed);

const MEM_BLUE: [f32; 4] = theme::rgb(0x5b, 0x9b, 0xf8);
const MEM_APP: [f32; 4] = theme::rgb(0x4d, 0x90, 0xfe);
const MEM_WIRED: [f32; 4] = theme::rgb(0xf0, 0x82, 0x56);
const MEM_COMP: [f32; 4] = theme::rgb(0xb7, 0x8a, 0xf8);
const MEM_CACHED: [f32; 4] = theme::OK;

const KILL_BG: [f32; 4] = theme::rgba(0xff, 0x71, 0x68, 0.14);
const KILL_BORDER: [f32; 4] = theme::rgba(0xff, 0x71, 0x68, 0.44);
const KILL_CHIP_BG: [f32; 4] = theme::rgba(0xe0, 0x55, 0x55, 0.14);
const KILL_CHIP_BORDER: [f32; 4] = theme::rgba(0xe0, 0x55, 0x55, 0.36);

// =============================================================================
// Interactive Controller State
// =============================================================================

/// Live interactive state for a native Activity & GPU monitor block.
pub struct SysMonState {
    pub engine: SysMonEngine<'static>,
    pub selected_row: usize,
    pub scroll_row: usize,
    pub filter_input: String,
    pub filter_focused: bool,
    pub confirm_kill: Option<(u32, String)>,
    pub status_msg: Option<String>,
    pub running: bool,
    /// When true (default), parent rows in tree mode display cumulative subtree CPU %, GPU %,
    /// and Memory across all descendants (press `c` or click `Σ Cumul` to toggle self-only).
    pub cumulative: bool,
    /// Cached `(pid, cmdline)` for the currently selected row so `KERN_PROCARGS2` is queried
    /// only when the selected PID changes (zero per-frame syscall overhead).
    pub cached_cmdline: Option<(u32, String)>,
}

impl Default for SysMonState {
    fn default() -> Self {
        Self::new()
    }
}

impl SysMonState {
    /// Create a fresh live system monitor backed by the global allocator.
    pub fn new() -> Self {
        Self {
            engine: SysMonEngine::new_in(Alloc::global()),
            selected_row: 0,
            scroll_row: 0,
            filter_input: String::new(),
            filter_focused: false,
            confirm_kill: None,
            status_msg: None,
            running: true,
            cumulative: true,
            cached_cmdline: None,
        }
    }

    /// Lazily refresh the cached command line for the currently selected process.
    ///
    /// Only invokes `read_proc_cmdline` when the selected PID actually changes.
    pub fn refresh_selected_cmdline(&mut self) {
        let Some(pid) = self.engine.rows().get(self.selected_row).map(|r| r.pid) else {
            self.cached_cmdline = None;
            return;
        };
        if self.cached_cmdline.as_ref().is_some_and(|(p, _)| *p == pid) {
            return;
        }
        let cmd = crate::sysmon_driver::read_proc_cmdline(pid);
        self.cached_cmdline = Some((pid, cmd));
    }

    /// Clamp `selected_row` and `scroll_row` to the current visible row list.
    pub fn clamp_selection(&mut self, visible_rows: usize) {
        let total = self.engine.rows().len();
        self.clamp_selection_with_len(visible_rows, total);
        self.refresh_selected_cmdline();
    }

    /// Move selection up/down by `delta` rows, scrolling the table viewport to keep it in view.
    pub fn move_selection(&mut self, delta: isize, visible_rows: usize) {
        let total = self.engine.rows().len();
        self.move_selection_with_len(delta, visible_rows, total);
        self.refresh_selected_cmdline();
    }

    /// Scroll the process table viewport by `delta_rows` while keeping selection within view.
    pub fn scroll_by(&mut self, delta_rows: isize, visible_rows: usize) {
        let total = self.engine.rows().len();
        self.scroll_by_with_len(delta_rows, visible_rows, total);
        self.refresh_selected_cmdline();
    }

    /// Select an absolute row index (e.g. from clicking a process row).
    pub fn select_row(&mut self, row: usize, visible_rows: usize) {
        let total = self.engine.rows().len();
        if total == 0 {
            self.selected_row = 0;
            self.scroll_row = 0;
            self.cached_cmdline = None;
            return;
        }
        self.selected_row = row.min(total - 1);
        self.clamp_selection_with_len(visible_rows, total);
        self.refresh_selected_cmdline();
    }

    /// Toggle collapse/expand on the currently selected process row if it has children.
    pub fn toggle_selected_collapse(&mut self) {
        if let Some(row) = self.engine.rows().get(self.selected_row)
            && row.has_children
        {
            let pid = row.pid;
            self.engine.toggle_collapse(pid);
            self.refresh_selected_cmdline();
        }
    }

    /// Native macOS / Finder / btop `→` / `l` tree navigation:
    /// - If the selected row has children and is collapsed, expand it.
    /// - Else if it has children and is expanded (with a child row immediately below), move
    ///   selection into its first child.
    pub fn expand_or_child(&mut self, visible_rows: usize) {
        if !self.engine.tree_mode() {
            return;
        }
        let Some((pid, has_children, collapsed, depth)) = self
            .engine
            .rows()
            .get(self.selected_row)
            .map(|r| (r.pid, r.has_children, r.collapsed, r.depth))
        else {
            return;
        };
        if has_children && collapsed {
            self.engine.toggle_collapse(pid);
            self.clamp_selection(visible_rows);
        } else if has_children
            && !collapsed
            && self
                .engine
                .rows()
                .get(self.selected_row + 1)
                .is_some_and(|next| next.depth > depth)
        {
            self.move_selection(1, visible_rows);
        }
    }

    /// Native macOS / Finder / btop `←` / `h` tree navigation:
    /// - If the selected row has children and is expanded, collapse it.
    /// - Else if it is a nested child (`depth > 0`), jump selection up to its parent row and
    ///   scroll it into view.
    pub fn collapse_or_parent(&mut self, visible_rows: usize) {
        if !self.engine.tree_mode() {
            return;
        }
        let Some((pid, ppid, has_children, collapsed, depth)) = self
            .engine
            .rows()
            .get(self.selected_row)
            .map(|r| (r.pid, r.ppid, r.has_children, r.collapsed, r.depth))
        else {
            return;
        };
        if has_children && !collapsed {
            self.engine.toggle_collapse(pid);
            self.clamp_selection(visible_rows);
        } else if depth > 0 {
            let parent_idx = self.engine.rows()[..self.selected_row]
                .iter()
                .rposition(|r| r.pid == ppid || r.depth < depth);
            if let Some(idx) = parent_idx {
                self.select_row(idx, visible_rows);
            }
        }
    }

    /// Return `(pid, name)` of the currently selected process row, if any.
    pub fn selected_pid_and_name(&self) -> Option<(u32, String)> {
        let row = self.engine.rows().get(self.selected_row)?;
        Some((row.pid, row.name.as_str().to_owned()))
    }

    /// Enter interactive kill confirmation for the currently selected process.
    pub fn begin_kill_selected(&mut self) {
        if let Some((pid, name)) = self.selected_pid_and_name() {
            self.confirm_kill = Some((pid, name));
            self.status_msg = None;
        }
    }

    /// Cancel pending kill confirmation.
    pub fn cancel_kill(&mut self) {
        self.confirm_kill = None;
    }

    /// Send `SIGTERM` (`sigkill == false`) or `SIGKILL` (`sigkill == true`) to the pending target.
    pub fn confirm_kill_selected(&mut self, sigkill: bool) {
        let Some((pid, name)) = self.confirm_kill.take() else {
            return;
        };
        let sig_label = if sigkill { "SIGKILL" } else { "SIGTERM" };
        match crate::sysmon_driver::send_process_signal(pid, sigkill) {
            Ok(()) => {
                self.status_msg = Some(format!("Sent {sig_label} to {name} (PID {pid})"));
            }
            Err(err) => {
                self.status_msg = Some(format!("Failed {sig_label} {name} (PID {pid}): {err}"));
            }
        }
    }

    /// Update the live filter query and reset selection/scroll to the top of filtered results.
    pub fn set_filter(&mut self, q: String) {
        self.filter_input = q;
        self.engine.set_filter(&self.filter_input);
        self.selected_row = 0;
        self.scroll_row = 0;
        self.refresh_selected_cmdline();
    }

    /// Append a character to the live filter input.
    pub fn push_filter_char(&mut self, ch: char) {
        self.filter_input.push(ch);
        self.engine.set_filter(&self.filter_input);
        self.selected_row = 0;
        self.scroll_row = 0;
        self.refresh_selected_cmdline();
    }

    /// Remove the last character from the live filter input.
    pub fn pop_filter_char(&mut self) {
        self.filter_input.pop();
        self.engine.set_filter(&self.filter_input);
        self.selected_row = 0;
        self.scroll_row = 0;
        self.refresh_selected_cmdline();
    }

    /// Clear the filter input and unfocus the filter bar.
    pub fn clear_filter(&mut self) {
        self.filter_focused = false;
        if !self.filter_input.is_empty() {
            self.set_filter(String::new());
        }
    }

    fn clamp_selection_with_len(&mut self, visible_rows: usize, total: usize) {
        if total == 0 {
            self.selected_row = 0;
            self.scroll_row = 0;
            return;
        }
        let vis = visible_rows.max(1);
        self.selected_row = self.selected_row.min(total - 1);
        let max_scroll = total.saturating_sub(vis);
        self.scroll_row = self.scroll_row.min(max_scroll);

        if self.selected_row < self.scroll_row {
            self.scroll_row = self.selected_row;
        } else if self.selected_row >= self.scroll_row + vis {
            self.scroll_row = self.selected_row + 1 - vis;
        }
    }

    fn move_selection_with_len(&mut self, delta: isize, visible_rows: usize, total: usize) {
        if total == 0 {
            self.selected_row = 0;
            self.scroll_row = 0;
            return;
        }
        let next = (self.selected_row as isize + delta).clamp(0, (total - 1) as isize) as usize;
        self.selected_row = next;
        self.clamp_selection_with_len(visible_rows, total);
    }

    fn scroll_by_with_len(&mut self, delta_rows: isize, visible_rows: usize, total: usize) {
        if total == 0 {
            self.scroll_row = 0;
            self.selected_row = 0;
            return;
        }
        let vis = visible_rows.max(1);
        let max_scroll = total.saturating_sub(vis);
        self.scroll_row =
            (self.scroll_row as isize + delta_rows).clamp(0, max_scroll as isize) as usize;
        let hi = (self.scroll_row + vis - 1).min(total - 1);
        self.selected_row = self.selected_row.clamp(self.scroll_row, hi);
    }
}

// =============================================================================
// Formatting Helpers
// =============================================================================

/// Format a byte count in human-readable IEC units (`B`, `KiB`, `MiB`, `GiB`, `TiB`).
pub fn format_iec_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;

    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MIB {
        let v = b / KIB;
        if v < 10.0 { format!("{v:.1} KiB") } else { format!("{v:.0} KiB") }
    } else if b < GIB {
        let v = b / MIB;
        if v < 10.0 { format!("{v:.1} MiB") } else { format!("{v:.0} MiB") }
    } else if b < TIB {
        let v = b / GIB;
        if v < 100.0 { format!("{v:.1} GiB") } else { format!("{v:.0} GiB") }
    } else {
        let v = b / TIB;
        if v < 10.0 { format!("{v:.2} TiB") } else { format!("{v:.1} TiB") }
    }
}

/// Format system uptime concisely (`45s`, `12m 5s`, `3h 40m`, `4d 12h`).
pub fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 { format!("{m}m") } else { format!("{m}m {s}s") }
    } else if secs < 86_400 {
        let h = secs / 3_600;
        let m = (secs % 3_600) / 60;
        if m == 0 { format!("{h}h") } else { format!("{h}h {m}m") }
    } else {
        let d = secs / 86_400;
        let h = (secs % 86_400) / 3_600;
        if h == 0 { format!("{d}d") } else { format!("{d}d {h}h") }
    }
}

/// Format a count compactly (`312`, `2.1k`, `128k`).
pub fn format_count_compact(n: u32) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 100_000 {
        format!("{:.1}k", n as f32 / 1_000.0)
    } else {
        format!("{}k", n / 1_000)
    }
}

/// Compute the number of process rows that fit in the monitor for a given height budget.
pub fn visible_rows_for_height(
    max_h: Option<f32>,
    fullscreen: bool,
    has_filter_bar: bool,
) -> usize {
    let filter_h = if has_filter_bar { FILTER_H + FILTER_GAP } else { 0.0 };
    let fixed_without_inspector = PAD_Y * 2.0
        + HEADER_H
        + HEADER_GAP
        + CARDS_H
        + CARDS_GAP
        + filter_h
        + TABLE_HEADER_H
        + FOOTER_GAP
        + FOOTER_H;
    let fixed = fixed_without_inspector + INSPECTOR_GAP + INSPECTOR_H;
    let default_h = fixed + INLINE_VISIBLE_ROWS as f32 * ROW_H;
    let target_h = if fullscreen {
        max_h.unwrap_or(default_h).max(fixed_without_inspector + ROW_H)
    } else {
        match max_h {
            Some(h) => default_h.min(h).max(fixed_without_inspector + ROW_H),
            None => default_h,
        }
    };
    let eff_fixed = if target_h >= fixed + ROW_H { fixed } else { fixed_without_inspector };
    (((target_h - eff_fixed) / ROW_H).floor() as usize).max(1)
}

// =============================================================================
// Main Native Metal Renderer (`layout_sysmon_block`)
// =============================================================================

/// Render the complete native Metal btop + macOS Activity Monitor card into `scene`,
/// returning the total logical height consumed.
#[allow(clippy::too_many_arguments)]
pub fn layout_sysmon_block(
    scene: &mut Scene,
    block_id: BlockId,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    max_h: Option<f32>,
    m: CellMetrics,
    clip_id: u32,
    fullscreen: bool,
) -> f32 {
    let width = (x1 - x0).max(320.0);
    let eff_x1 = x0 + width;
    let has_filter_bar = state.filter_focused || !state.filter_input.is_empty();
    let filter_section_h = if has_filter_bar { FILTER_H + FILTER_GAP } else { 0.0 };

    let fixed_without_inspector = PAD_Y * 2.0
        + HEADER_H
        + HEADER_GAP
        + CARDS_H
        + CARDS_GAP
        + filter_section_h
        + TABLE_HEADER_H
        + FOOTER_GAP
        + FOOTER_H;
    let fixed_with_inspector = fixed_without_inspector + INSPECTOR_GAP + INSPECTOR_H;

    let visible_rows = visible_rows_for_height(max_h, fullscreen, has_filter_bar);
    let natural_h = fixed_with_inspector + visible_rows as f32 * ROW_H;
    let total_h = if fullscreen {
        max_h.unwrap_or(natural_h).max(fixed_without_inspector + ROW_H)
    } else {
        match max_h {
            Some(h) => natural_h.min(h).max(fixed_without_inspector + ROW_H),
            None => natural_h,
        }
    };

    // -------------------------------------------------------------------------
    // a. Outer dark-glass frame
    // -------------------------------------------------------------------------
    let (pad_x, outer_radius) = if fullscreen { (14.0, 0.0) } else { (12.0, 10.0) };
    if fullscreen {
        scene.round_rect(x0, y0, eff_x1, y0 + total_h, 0.0, theme::BG_CANVAS, 0.0, [0.0; 4], clip_id);
    } else {
        scene.round_rect(
            x0,
            y0,
            eff_x1,
            y0 + total_h,
            outer_radius,
            theme::BG_SURFACE,
            1.0,
            theme::LINE,
            clip_id,
        );
    }

    let inner_clip = intersect_clip(
        scene,
        clip_id,
        x0 + 1.0,
        y0 + 1.0,
        eff_x1 - 1.0,
        y0 + total_h - 1.0,
        if fullscreen { 0.0 } else { outer_radius - 1.0 },
    );

    let ix0 = x0 + pad_x;
    let ix1 = eff_x1 - pad_x;
    let inner_w = (ix1 - ix0).max(200.0);
    let mut cur_y = y0 + PAD_Y;

    // -------------------------------------------------------------------------
    // b. Header Bar
    // -------------------------------------------------------------------------
    draw_header_bar(scene, block_id, state, ix0, cur_y, ix1, m, inner_clip, fullscreen);
    cur_y += HEADER_H + HEADER_GAP;

    // -------------------------------------------------------------------------
    // c. Top 3 Side-by-Side Telemetry Cards (CPU, GPU, Memory)
    // -------------------------------------------------------------------------
    draw_telemetry_cards(scene, state, ix0, cur_y, inner_w, CARDS_H, m, inner_clip);
    cur_y += CARDS_H + CARDS_GAP;

    // -------------------------------------------------------------------------
    // d. Optional Filter Bar
    // -------------------------------------------------------------------------
    if has_filter_bar {
        draw_filter_bar(scene, state, ix0, cur_y, ix1, FILTER_H, m, inner_clip);
        cur_y += FILTER_H + FILTER_GAP;
    }

    // -------------------------------------------------------------------------
    // e. Sortable Process Tree Table + Selected-Process Inspector Bar
    // -------------------------------------------------------------------------
    let footer_y0 = y0 + total_h - PAD_Y - FOOTER_H;
    let selected_row = state.engine.rows().get(state.selected_row);
    let insp_y1 = footer_y0 - FOOTER_GAP;
    let insp_y0 = insp_y1 - INSPECTOR_H;
    let min_table_y1 = cur_y + TABLE_HEADER_H + ROW_H;
    let show_inspector =
        selected_row.is_some() && insp_y0 - INSPECTOR_GAP >= min_table_y1 && insp_y1 <= footer_y0;

    let table_y1 = if show_inspector {
        insp_y0 - INSPECTOR_GAP
    } else {
        (footer_y0 - FOOTER_GAP).max(min_table_y1)
    };
    let rows_avail = (((table_y1 - cur_y - TABLE_HEADER_H) / ROW_H).floor() as usize).max(1);
    let table_visible_rows = if fullscreen { rows_avail } else { visible_rows.min(rows_avail) };

    draw_process_table(
        scene,
        block_id,
        state,
        ix0,
        cur_y,
        ix1,
        table_y1,
        table_visible_rows,
        m,
        inner_clip,
    );

    // -------------------------------------------------------------------------
    // f. Selected-Process Inspector Bar
    // -------------------------------------------------------------------------
    if show_inspector
        && let Some(row) = selected_row
    {
        draw_selected_inspector(
            scene,
            block_id,
            state,
            row,
            ix0,
            insp_y0,
            ix1,
            insp_y1,
            m,
            inner_clip,
        );
    }

    // -------------------------------------------------------------------------
    // g. Footer / Kill Confirm Bar
    // -------------------------------------------------------------------------
    draw_footer_bar(scene, state, ix0, footer_y0, ix1, FOOTER_H, m, inner_clip);

    total_h
}

// =============================================================================
// Section Renderers
// =============================================================================

#[allow(clippy::too_many_arguments)]
fn draw_header_bar(
    scene: &mut Scene,
    block_id: BlockId,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    m: CellMetrics,
    clip_id: u32,
    fullscreen: bool,
) {
    let mid_y = y0 + HEADER_H * 0.5;
    let pill_h = 22.0;
    let pill_y0 = mid_y - pill_h * 0.5;
    let pill_y1 = pill_y0 + pill_h;
    let pill_gap = 6.0;

    // Right-to-left interactive pills so we know exact remaining width for left summary.
    let mut rx = x1;

    // [×] Close pill
    rx = draw_pill_right(
        scene,
        rx,
        pill_y0,
        pill_y1,
        "×",
        theme::INK_2,
        theme::BG_CHIP,
        theme::LINE,
        m,
        clip_id,
        HitAction::SysMonClose(block_id),
    ) - pill_gap;

    // [⤢ ⌘⇧F] / [⤡ ⌘⇧F] Expand / restore pill
    let fs_label = if fullscreen { "⤡ ⌘⇧F" } else { "⤢ ⌘⇧F" };
    rx = draw_pill_right(
        scene,
        rx,
        pill_y0,
        pill_y1,
        fs_label,
        theme::INK_2,
        theme::BG_CHIP,
        theme::LINE,
        m,
        clip_id,
        HitAction::TogglePtyFullscreen,
    ) - pill_gap;

    // [Live] / [Paused] toggle pill
    let paused = state.engine.paused();
    let (pause_label, pause_fg, pause_bg, pause_border) = if paused {
        ("Paused", AMBER, AMBER_BG, AMBER_BORDER)
    } else {
        ("Live", theme::OK, theme::rgba(0x4c, 0xc2, 0x7a, 0.11), theme::rgba(0x4c, 0xc2, 0x7a, 0.30))
    };
    rx = draw_pill_right(
        scene,
        rx,
        pill_y0,
        pill_y1,
        pause_label,
        pause_fg,
        pause_bg,
        pause_border,
        m,
        clip_id,
        HitAction::SysMonTogglePause(block_id),
    ) - pill_gap;

    // [Tree] / [List] toggle pill
    let tree = state.engine.tree_mode();
    let (tree_label, tree_fg, tree_bg, tree_border) = if tree {
        ("Tree", GPU_CYAN_BRIGHT, GPU_CYAN_BG, GPU_CYAN_BORDER)
    } else {
        ("List", theme::INK_2, theme::BG_CHIP, theme::LINE)
    };
    rx = draw_pill_right(
        scene,
        rx,
        pill_y0,
        pill_y1,
        tree_label,
        tree_fg,
        tree_bg,
        tree_border,
        m,
        clip_id,
        HitAction::SysMonToggleTree(block_id),
    ) - pill_gap;

    // [Σ Cumul] / [Self] toggle pill (shown in tree mode; cumulative by default!)
    if tree {
        let (cum_label, cum_fg, cum_bg, cum_border) = if state.cumulative {
            ("Σ Cumul", theme::rgb(0x96, 0xc2, 0xff), theme::rgba(0x58, 0x9d, 0xf6, 0.14), theme::rgba(0x58, 0x9d, 0xf6, 0.36))
        } else {
            ("Self", theme::INK_3, theme::BG_CHIP, theme::LINE)
        };
        rx = draw_pill_right(
            scene,
            rx,
            pill_y0,
            pill_y1,
            cum_label,
            cum_fg,
            cum_bg,
            cum_border,
            m,
            clip_id,
            HitAction::SysMonToggleCumulative(block_id),
        );
    }

    let left_max_x = (rx - 10.0).max(x0 + 80.0);
    let left_clip = intersect_clip(scene, clip_id, x0, y0, left_max_x, y0 + HEADER_H, 0.0);

    // Pulsing live dot (emerald when live, amber when paused)
    let dot_cx = x0 + 6.0;
    let pulse_even = (state.engine.sample_count() & 1) == 0;
    let (dot_color, halo_color, halo_r) = if paused {
        (AMBER, theme::rgba(0xe8, 0xb0, 0x4b, 0.22), 5.2)
    } else if pulse_even {
        (theme::OK, theme::rgba(0x4c, 0xc2, 0x7a, 0.28), 6.0)
    } else {
        (theme::OK, theme::rgba(0x4c, 0xc2, 0x7a, 0.14), 4.8)
    };
    scene.circle(dot_cx, mid_y, halo_r, halo_color, left_clip);
    scene.circle(dot_cx, mid_y, 3.2, dot_color, left_clip);

    // Bold title
    let mut lx = dot_cx + 10.0;
    let title = "Activity & GPU";
    let title_y = center_text_y(y0, HEADER_H, m.line_h(TextFace::Ui));
    scene.ui_text(lx, title_y, title, theme::INK_1, true, false, left_clip);
    lx += m.measure_styled(title, TextFace::Ui, true) + 9.0;

    // GPU model chip (e.g. "Apple M5 Max · 40c")
    let gpu = state.engine.gpu();
    let gpu_chip = format_gpu_chip(gpu.model.as_str(), gpu.core_count);
    if !gpu_chip.is_empty() {
        let chip_tw = m.measure_styled(&gpu_chip, TextFace::UiSmall, true);
        let chip_w = chip_tw + 14.0;
        if lx + chip_w <= left_max_x {
            let chip_h = 20.0;
            let chip_y0 = mid_y - chip_h * 0.5;
            scene.round_rect(
                lx,
                chip_y0,
                lx + chip_w,
                chip_y0 + chip_h,
                5.0,
                GPU_CYAN_BG,
                1.0,
                GPU_CYAN_BORDER,
                left_clip,
            );
            let chip_ty = center_text_y(chip_y0, chip_h, m.line_h(TextFace::UiSmall));
            scene.ui_small(lx + 7.0, chip_ty, gpu_chip, GPU_CYAN_BRIGHT, true, false, left_clip);
            lx += chip_w + 10.0;
        }
    }

    // Muted summary strip ("312 procs · 2.1k thr · load 2.14 1.98 · up 4d 12h")
    if lx + 48.0 < left_max_x {
        let load = state.engine.load_avg();
        let summary = format!(
            "{} procs · {} thr · load {:.2} {:.2} · up {}",
            state.engine.process_count(),
            format_count_compact(state.engine.thread_count()),
            load[0],
            load[1],
            format_uptime(state.engine.uptime_secs()),
        );
        let max_chars = ((left_max_x - lx) / (m.cw * 0.78).max(4.0)).floor() as usize;
        if max_chars >= 6 {
            let shown = truncate_chars(&summary, max_chars);
            let sum_ty = center_text_y(y0, HEADER_H, m.line_h(TextFace::UiSmall));
            scene.ui_small(lx, sum_ty, shown, theme::INK_3, false, false, left_clip);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pill_right(
    scene: &mut Scene,
    right_x: f32,
    y0: f32,
    y1: f32,
    label: &str,
    fg: [f32; 4],
    bg: [f32; 4],
    border: [f32; 4],
    m: CellMetrics,
    clip_id: u32,
    action: HitAction,
) -> f32 {
    let tw = m.measure_styled(label, TextFace::UiSmall, true);
    let pad_x = if label.chars().count() <= 1 { 7.5 } else { 8.0 };
    let w = (tw + pad_x * 2.0).max(22.0);
    let x0 = right_x - w;
    scene.round_rect(x0, y0, right_x, y1, 5.5, bg, 1.0, border, clip_id);
    let ty = center_text_y(y0, y1 - y0, m.line_h(TextFace::UiSmall));
    scene.ui_small(x0 + (w - tw) * 0.5, ty, label, fg, true, false, clip_id);
    scene.push_hit_clipped(HitRect::new(x0, y0, right_x, y1), clip_id, action);
    x0
}

#[allow(clippy::too_many_arguments)]
fn draw_telemetry_cards(
    scene: &mut Scene,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    total_w: f32,
    h: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    let gap = 8.0;
    let card_w = ((total_w - gap * 2.0) / 3.0).max(96.0);
    let c0_x0 = x0;
    let c1_x0 = c0_x0 + card_w + gap;
    let c2_x0 = c1_x0 + card_w + gap;

    draw_cpu_card(scene, state, c0_x0, y0, c0_x0 + card_w, y0 + h, m, clip_id);
    draw_gpu_card(scene, state, c1_x0, y0, c1_x0 + card_w, y0 + h, m, clip_id);
    draw_mem_card(scene, state, c2_x0, y0, (c2_x0 + card_w).min(x0 + total_w), y0 + h, m, clip_id);
}

#[allow(clippy::too_many_arguments)]
fn draw_cpu_card(
    scene: &mut Scene,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    scene.round_rect(x0, y0, x1, y1, 8.0, CARD_BG, 1.0, theme::LINE, clip_id);
    let cclip = intersect_clip(scene, clip_id, x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, 7.0);

    let pad = 10.0;
    let ix0 = x0 + pad;
    let ix1 = x1 - pad;
    let head_y = y0 + 7.0;

    // Header: "CPU" + bold color-coded total percentage
    let cpu_pct = state.engine.total_cpu_pct().clamp(0.0, 100.0);
    let cpu_color = cpu_heat_color(cpu_pct);
    scene.ui_small(ix0, head_y + 2.0, "CPU", theme::INK_3, true, false, cclip);

    let pct_str = format!("{cpu_pct:.1}%");
    let pct_w = m.measure_styled(&pct_str, TextFace::Ui, true);
    scene.ui_text(ix1 - pct_w, head_y, pct_str, cpu_color, true, false, cclip);

    // 60-sample shaded sparkline
    let spark_y0 = y0 + 28.0;
    let spark_y1 = y1 - 28.0;
    draw_sparkline(
        scene,
        state.engine.cpu_history(),
        100.0,
        ix0,
        spark_y0,
        ix1,
        spark_y1,
        SparkStyle::CpuHeat,
        None,
        [0.0; 4],
        cclip,
    );

    // Bottom strip: compact per-core mini vertical heat bars across all cores
    let strip_y0 = spark_y1 + 6.0;
    let strip_y1 = y1 - 8.0;
    let cores = state.engine.core_utils();
    if cores.is_empty() {
        scene.ui_small(ix0, strip_y0 + 1.0, "Sampling cores…", theme::INK_4, false, false, cclip);
        return;
    }

    let n = cores.len();
    let strip_w = (ix1 - ix0).max(16.0);
    let bar_gap = if n > 24 { 1.5 } else { 2.5 };
    let bar_w = ((strip_w - bar_gap * (n.saturating_sub(1) as f32)) / n as f32).max(2.0);
    let bar_h = (strip_y1 - strip_y0).max(6.0);

    for (i, &util) in cores.iter().enumerate() {
        let bx0 = ix0 + i as f32 * (bar_w + bar_gap);
        let bx1 = (bx0 + bar_w).min(ix1);
        if bx1 <= bx0 {
            break;
        }
        let r = (bar_w * 0.35).min(2.0);
        scene.round_rect(bx0, strip_y0, bx1, strip_y1, r, theme::BG_TRACK, 0.0, [0.0; 4], cclip);
        let frac = (util / 100.0).clamp(0.0, 1.0);
        if frac > 0.01 {
            let fill_h = (bar_h * frac).max(2.0);
            let fy0 = (strip_y1 - fill_h).max(strip_y0);
            scene.round_rect(bx0, fy0, bx1, strip_y1, r, cpu_heat_color(util), 0.0, [0.0; 4], cclip);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_gpu_card(
    scene: &mut Scene,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    scene.round_rect(x0, y0, x1, y1, 8.0, CARD_BG, 1.0, theme::LINE, clip_id);
    let cclip = intersect_clip(scene, clip_id, x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, 7.0);

    let pad = 10.0;
    let ix0 = x0 + pad;
    let ix1 = x1 - pad;
    let head_y = y0 + 7.0;
    let gpu = state.engine.gpu();

    // Header: "GPU" + R:<renderer>% T:<tiler>% badges + large bold device util %
    scene.ui_small(ix0, head_y + 2.0, "GPU", theme::INK_3, true, false, cclip);

    let pct_str = format!("{:.0}%", gpu.device_util_pct.clamp(0.0, 100.0));
    let pct_w = m.measure_styled(&pct_str, TextFace::Ui, true);
    let pct_x = ix1 - pct_w;
    scene.ui_text(pct_x, head_y, pct_str, GPU_CYAN_BRIGHT, true, false, cclip);

    // Compact R:xx% T:xx% badges right next to device util if width permits
    let t_badge = format!("T:{:.0}%", gpu.tiler_util_pct.clamp(0.0, 100.0));
    let r_badge = format!("R:{:.0}%", gpu.renderer_util_pct.clamp(0.0, 100.0));
    let t_w = m.measure(&t_badge, TextFace::UiSmall);
    let r_w = m.measure(&r_badge, TextFace::UiSmall);
    let badges_x0 = pct_x - 8.0 - t_w - 6.0 - r_w;
    if badges_x0 > ix0 + 32.0 {
        scene.ui_small(badges_x0, head_y + 2.0, r_badge, GPU_VIOLET, false, false, cclip);
        scene.ui_small(badges_x0 + r_w + 6.0, head_y + 2.0, t_badge, GPU_TILER, false, false, cclip);
    }

    // 60-sample GPU Device utilization sparkline in cyan/teal + violet Renderer overlay
    let spark_y0 = y0 + 28.0;
    let spark_y1 = y1 - 28.0;
    draw_sparkline(
        scene,
        state.engine.gpu_history(),
        100.0,
        ix0,
        spark_y0,
        ix1,
        spark_y1,
        SparkStyle::Solid(GPU_CYAN),
        Some(state.engine.gpu_renderer_history()),
        GPU_VIOLET,
        cclip,
    );

    // Bottom strip: VRAM bar ("VRAM 1.8 / 11.8 GiB")
    let label_y = spark_y1 + 4.0;
    scene.ui_small(ix0, label_y, "VRAM", theme::INK_4, true, false, cclip);

    let vram_used = gpu.vram_in_use_bytes;
    let vram_total = gpu.vram_alloc_bytes.max(vram_used);
    let vram_label = if vram_total > 0 {
        format!("{} / {}", format_iec_bytes(vram_used), format_iec_bytes(vram_total))
    } else {
        "—".to_owned()
    };
    let vl_w = m.measure(&vram_label, TextFace::UiSmall);
    scene.ui_small(ix1 - vl_w, label_y, vram_label, theme::INK_2, false, false, cclip);

    let bar_y0 = y1 - 11.0;
    let bar_y1 = y1 - 6.0;
    let bar_w = (ix1 - ix0).max(16.0);
    scene.round_rect(ix0, bar_y0, ix1, bar_y1, 2.5, theme::BG_TRACK, 0.0, [0.0; 4], cclip);
    if vram_total > 0 && vram_used > 0 {
        let frac = (vram_used as f64 / vram_total as f64).clamp(0.0, 1.0) as f32;
        let fill_x1 = (ix0 + (bar_w * frac).max(3.0)).min(ix1);
        scene.round_rect(ix0, bar_y0, fill_x1, bar_y1, 2.5, GPU_CYAN, 0.0, [0.0; 4], cclip);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_mem_card(
    scene: &mut Scene,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    scene.round_rect(x0, y0, x1, y1, 8.0, CARD_BG, 1.0, theme::LINE, clip_id);
    let cclip = intersect_clip(scene, clip_id, x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, 7.0);

    let pad = 10.0;
    let ix0 = x0 + pad;
    let ix1 = x1 - pad;
    let head_y = y0 + 7.0;
    let mem = state.engine.mem();

    // Activity Monitor "Memory Used" = App + Wired + Compressed
    let used_bytes = mem.used_bytes();
    let total_bytes = mem.total_bytes;
    let used_gib = used_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let total_gib = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let pct = mem.used_pct();

    // Header: "MEM" + "{used} / {total} GiB" + "%"
    scene.ui_small(ix0, head_y + 2.0, "MEM", theme::INK_3, true, false, cclip);

    let pct_str = format!("{pct:.0}%");
    let pct_color = if pct >= 88.0 {
        theme::DANGER
    } else if pct >= 72.0 {
        AMBER
    } else {
        MEM_BLUE
    };
    let pct_w = m.measure_styled(&pct_str, TextFace::UiSmall, true);
    scene.ui_small(ix1 - pct_w, head_y + 2.0, pct_str, pct_color, true, false, cclip);

    let gib_str = if total_bytes > 0 {
        format!("{used_gib:.1} / {total_gib:.0} GiB")
    } else {
        "—".to_owned()
    };
    let gib_w = m.measure_styled(&gib_str, TextFace::Ui, true);
    let gib_x = ix1 - pct_w - 6.0 - gib_w;
    if gib_x > ix0 + 30.0 {
        scene.ui_text(gib_x, head_y, gib_str, theme::INK_1, true, false, cclip);
    }

    // 60-sample memory sparkline in indigo/blue
    let spark_y0 = y0 + 28.0;
    let spark_y1 = y1 - 30.0;
    draw_sparkline(
        scene,
        state.engine.mem_history(),
        100.0,
        ix0,
        spark_y0,
        ix1,
        spark_y1,
        SparkStyle::Solid(MEM_BLUE),
        None,
        [0.0; 4],
        cclip,
    );

    // macOS Activity Monitor 4-segment bar: App (blue), Wired (coral/amber), Compressed (violet), Cached (emerald)
    let bar_y0 = spark_y1 + 5.0;
    let bar_y1 = bar_y0 + 6.0;
    let bar_w = (ix1 - ix0).max(16.0);
    scene.round_rect(ix0, bar_y0, ix1, bar_y1, 3.0, theme::BG_TRACK, 0.0, [0.0; 4], cclip);
    if total_bytes > 0 {
        let bar_clip = intersect_clip(scene, cclip, ix0, bar_y0, ix1, bar_y1, 3.0);
        let segments: [(u64, [f32; 4]); 4] = [
            (mem.app_bytes, MEM_APP),
            (mem.wired_bytes, MEM_WIRED),
            (mem.compressed_bytes, MEM_COMP),
            (mem.cached_bytes, MEM_CACHED),
        ];
        let mut sx = ix0;
        for (bytes, col) in segments {
            if bytes == 0 || sx >= ix1 {
                continue;
            }
            let sw = (bytes as f64 / total_bytes as f64) as f32 * bar_w;
            let ex = (sx + sw).min(ix1);
            if ex > sx + 0.5 {
                scene.round_rect(sx, bar_y0, ex, bar_y1, 0.0, col, 0.0, [0.0; 4], bar_clip);
                sx = ex;
            }
        }
    }

    // Compact legend + optional Swap badge
    let leg_y = bar_y1 + 4.0;
    let mut leg_limit_x = ix1;
    if mem.swap_used_bytes > 0 {
        let swap_str = format!("Swap {}", format_iec_bytes(mem.swap_used_bytes));
        let sw_w = m.measure_styled(&swap_str, TextFace::UiSmall, true);
        let pill_w = sw_w + 8.0;
        let px0 = ix1 - pill_w;
        if px0 > ix0 + 70.0 {
            scene.round_rect(
                px0,
                leg_y - 1.0,
                ix1,
                leg_y + 13.0,
                3.5,
                AMBER_BG,
                0.75,
                AMBER_BORDER,
                cclip,
            );
            scene.ui_small(px0 + 4.0, leg_y, swap_str, AMBER, true, false, cclip);
            leg_limit_x = px0 - 6.0;
        }
    }

    let legend_items: [(&str, [f32; 4]); 4] = [
        ("App", MEM_APP),
        ("Wired", MEM_WIRED),
        ("Comp", MEM_COMP),
        ("Cache", MEM_CACHED),
    ];
    let mut lx = ix0;
    let dot_cy = leg_y + m.line_h(TextFace::UiSmall) * 0.45;
    for (name, col) in legend_items {
        let tw = m.measure(name, TextFace::UiSmall);
        let item_w = 7.0 + tw;
        if lx + item_w > leg_limit_x {
            break;
        }
        scene.circle(lx + 2.5, dot_cy, 2.4, col, cclip);
        scene.ui_small(lx + 7.0, leg_y, name, theme::INK_3, false, false, cclip);
        lx += item_w + 7.0;
    }
}

#[derive(Clone, Copy)]
enum SparkStyle {
    CpuHeat,
    Solid([f32; 4]),
}

/// Draw a 60-sample shaded vertical bar sparkline right-aligned inside `[x0..x1, y0..y1]`,
/// with an optional secondary ridge overlay (used for GPU Renderer % over Device %).
#[allow(clippy::too_many_arguments)]
fn draw_sparkline(
    scene: &mut Scene,
    history: &HistoryRing<64>,
    max_val: f32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    style: SparkStyle,
    overlay: Option<&HistoryRing<64>>,
    overlay_color: [f32; 4],
    clip_id: u32,
) {
    let w = (x1 - x0).max(8.0);
    let h = (y1 - y0).max(8.0);

    // Subtle dark recessed plot well + faint 50% gridline
    scene.round_rect(x0, y0, x1, y1, 4.0, PLOT_BG, 0.75, theme::LINE_FAINT, clip_id);
    let mid_y = y0 + h * 0.5;
    scene.round_rect(
        x0 + 2.0,
        mid_y,
        x1 - 2.0,
        mid_y + 1.0,
        0.0,
        theme::rgba(0xff, 0xff, 0xff, 0.04),
        0.0,
        [0.0; 4],
        clip_id,
    );

    let plot_clip = intersect_clip(scene, clip_id, x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, 3.0);

    const MAX_SLOTS: usize = 60;
    let mut vals = [0.0f32; MAX_SLOTS];
    let total = history.len();
    let skip = total.saturating_sub(MAX_SLOTS);
    let count = total.min(MAX_SLOTS);
    for (slot, val) in vals.iter_mut().enumerate().take(count) {
        *val = history.get(skip + slot);
    }

    let mut ov_vals = [0.0f32; MAX_SLOTS];
    let mut ov_count = 0usize;
    if let Some(ov) = overlay {
        let ov_total = ov.len();
        let ov_skip = ov_total.saturating_sub(MAX_SLOTS);
        ov_count = ov_total.min(MAX_SLOTS);
        for (slot, val) in ov_vals.iter_mut().enumerate().take(ov_count) {
            *val = ov.get(ov_skip + slot);
        }
    }

    if count == 0 {
        return;
    }

    let pad_x = 3.0;
    let pad_y = 3.0;
    let inner_w = (w - pad_x * 2.0).max(4.0);
    let inner_h = (h - pad_y * 2.0).max(4.0);
    let base_y = y1 - pad_y;
    let slot_w = inner_w / MAX_SLOTS as f32;
    let bar_w = (slot_w - 0.8).max(1.2);
    let denom = max_val.max(0.001);

    // Right-align samples so newest sample sits flush at the right edge.
    let offset = MAX_SLOTS - count;
    for i in 0..count {
        let v = vals[i];
        let frac = (v / denom).clamp(0.0, 1.0);
        let slot_idx = offset + i;
        let bx0 = x0 + pad_x + slot_idx as f32 * slot_w;
        let bx1 = (bx0 + bar_w).min(x1 - pad_x);
        if bx1 <= bx0 {
            continue;
        }

        let base_col = match style {
            SparkStyle::CpuHeat => cpu_heat_color(v),
            SparkStyle::Solid(c) => c,
        };

        if frac > 0.004 {
            let bh = (inner_h * frac).max(2.0);
            let top_y = (base_y - bh).max(y0 + pad_y);
            // Translucent shaded body
            let body_col = [base_col[0], base_col[1], base_col[2], 0.30];
            scene.round_rect(bx0, top_y, bx1, base_y, 1.0, body_col, 0.0, [0.0; 4], plot_clip);
            // Luminous 2px top crest
            let cap_y1 = (top_y + 2.0).min(base_y);
            let cap_col = [base_col[0], base_col[1], base_col[2], 0.94];
            scene.round_rect(bx0, top_y, bx1, cap_y1, 1.0, cap_col, 0.0, [0.0; 4], plot_clip);
        }

        // Optional secondary ridge (e.g. GPU Renderer %)
        if overlay.is_some() {
            let ov_i = i.saturating_add(ov_count).saturating_sub(count);
            if ov_i < ov_count {
                let ov_frac = (ov_vals[ov_i] / denom).clamp(0.0, 1.0);
                if ov_frac > 0.01 {
                    let oh = (inner_h * ov_frac).max(2.0);
                    let oy = (base_y - oh).max(y0 + pad_y);
                    let ov_col = [overlay_color[0], overlay_color[1], overlay_color[2], 0.82];
                    scene.round_rect(
                        bx0,
                        oy,
                        bx1,
                        (oy + 1.8).min(base_y),
                        0.8,
                        ov_col,
                        0.0,
                        [0.0; 4],
                        plot_clip,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_filter_bar(
    scene: &mut Scene,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    h: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    let border = if state.filter_focused { theme::MATCH_HIGHLIGHT } else { theme::LINE };
    scene.round_rect(x0, y0, x1, y0 + h, 6.0, theme::rgb(0x0f, 0x0f, 0x11), 1.0, border, clip_id);

    let fclip = intersect_clip(scene, clip_id, x0 + 2.0, y0 + 1.0, x1 - 2.0, y0 + h - 1.0, 5.0);
    let ty = center_text_y(y0, h, m.line_h(TextFace::UiSmall));

    // Match count badge on right
    let count_str = format!("{} / {} procs", state.engine.rows().len(), state.engine.process_count());
    let count_w = m.measure(&count_str, TextFace::UiSmall);
    let count_x = x1 - 10.0 - count_w;
    scene.ui_small(count_x, ty, count_str, theme::INK_3, false, false, fclip);

    // Search glyph + input or placeholder + focused beam cursor
    let icon_col = if state.filter_focused { theme::MATCH_HIGHLIGHT } else { theme::INK_3 };
    let mut lx = x0 + 9.0;
    scene.ui_small(lx, ty, "⌕", icon_col, true, false, fclip);
    lx += m.measure("⌕", TextFace::UiSmall) + 6.0;

    let text_ty = center_text_y(y0, h, m.line_h(TextFace::Mono));
    if state.filter_input.is_empty() {
        scene.ui_small(lx, ty, "filter…", theme::INK_4, false, false, fclip);
        if state.filter_focused {
            scene.over_round_rect(
                lx,
                y0 + 5.0,
                lx + 1.5,
                y0 + h - 5.0,
                0.75,
                theme::MATCH_HIGHLIGHT,
                fclip,
            );
        }
    } else {
        let max_chars = ((count_x - 10.0 - lx) / m.cw.max(4.0)).floor().max(1.0) as usize;
        let shown = truncate_chars(&state.filter_input, max_chars);
        let shown_w = m.measure(&shown, TextFace::Mono);
        scene.text(lx, text_ty, shown, theme::INK_1, false, false, fclip);
        if state.filter_focused {
            let cx = (lx + shown_w + 1.0).min(count_x - 6.0);
            scene.over_round_rect(
                cx,
                y0 + 5.0,
                cx + 1.5,
                y0 + h - 5.0,
                0.75,
                theme::MATCH_HIGHLIGHT,
                fclip,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_process_table(
    scene: &mut Scene,
    block_id: BlockId,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    visible_rows: usize,
    m: CellMetrics,
    clip_id: u32,
) {
    let rows = state.engine.rows();
    let total_rows = rows.len();
    let show_scrollbar = total_rows > visible_rows;
    let scrollbar_w = if show_scrollbar { 8.0 } else { 0.0 };

    // Table container frame
    scene.round_rect(x0, y0, x1, y1, 8.0, theme::rgb(0x13, 0x13, 0x15), 1.0, theme::LINE, clip_id);
    let tclip = intersect_clip(scene, clip_id, x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, 7.0);

    // Responsive column geometry
    let content_x1 = x1 - scrollbar_w;
    let table_w = (content_x1 - x0).max(200.0);
    let show_bars = table_w >= 540.0;
    let pid_w = 54.0;
    let thr_w = if table_w >= 460.0 { 38.0 } else { 0.0 };
    let mem_w = if show_bars { 124.0 } else { 78.0 };
    let gpu_w = if show_bars { 94.0 } else { 56.0 };
    let cpu_w = if show_bars { 102.0 } else { 62.0 };

    let pid_x1 = content_x1 - 8.0;
    let pid_x0 = pid_x1 - pid_w;
    let thr_x1 = pid_x0;
    let thr_x0 = thr_x1 - thr_w;
    let mem_x1 = thr_x0;
    let mem_x0 = mem_x1 - mem_w;
    let gpu_x1 = mem_x0;
    let gpu_x0 = gpu_x1 - gpu_w;
    let cpu_x1 = gpu_x0;
    let cpu_x0 = cpu_x1 - cpu_w;
    let proc_x0 = x0 + 4.0;
    let proc_x1 = (cpu_x0 - 6.0).max(proc_x0 + 80.0);

    // -------------------------------------------------------------------------
    // Sortable Column Header Row
    // -------------------------------------------------------------------------
    let hy0 = y0;
    let hy1 = y0 + TABLE_HEADER_H;
    scene.round_rect(x0, hy0, x1, hy1, 0.0, TABLE_HEAD_BG, 0.0, [0.0; 4], tclip);
    scene.round_rect(x0, hy1 - 1.0, x1, hy1, 0.0, theme::LINE, 0.0, [0.0; 4], tclip);

    let active_sort = state.engine.sort();
    let asc = state.engine.ascending();
    let head_ty = center_text_y(hy0, TABLE_HEADER_H, m.line_h(TextFace::UiSmall));

    draw_sort_header(
        scene,
        block_id,
        "PROCESS",
        Some(SortColumn::Name),
        active_sort,
        asc,
        proc_x0 + 6.0,
        proc_x1,
        hy0,
        hy1,
        head_ty,
        false,
        m,
        tclip,
    );
    draw_sort_header(
        scene,
        block_id,
        "CPU %",
        Some(SortColumn::Cpu),
        active_sort,
        asc,
        cpu_x0,
        cpu_x1 - 6.0,
        hy0,
        hy1,
        head_ty,
        true,
        m,
        tclip,
    );
    draw_sort_header(
        scene,
        block_id,
        "GPU %",
        Some(SortColumn::Gpu),
        active_sort,
        asc,
        gpu_x0,
        gpu_x1 - 6.0,
        hy0,
        hy1,
        head_ty,
        true,
        m,
        tclip,
    );
    draw_sort_header(
        scene,
        block_id,
        "MEMORY",
        Some(SortColumn::Mem),
        active_sort,
        asc,
        mem_x0,
        mem_x1 - 6.0,
        hy0,
        hy1,
        head_ty,
        true,
        m,
        tclip,
    );
    if thr_w > 0.0 {
        draw_sort_header(
            scene,
            block_id,
            "THR",
            None,
            active_sort,
            asc,
            thr_x0,
            thr_x1 - 6.0,
            hy0,
            hy1,
            head_ty,
            true,
            m,
            tclip,
        );
    }
    draw_sort_header(
        scene,
        block_id,
        "PID",
        Some(SortColumn::Pid),
        active_sort,
        asc,
        pid_x0,
        pid_x1,
        hy0,
        hy1,
        head_ty,
        true,
        m,
        tclip,
    );

    // -------------------------------------------------------------------------
    // Process Rows
    // -------------------------------------------------------------------------
    let rows_y0 = hy1;
    let body_clip = intersect_clip(scene, tclip, x0 + 1.0, rows_y0, x1 - 1.0, y1 - 1.0, 0.0);

    if rows.is_empty() {
        let msg = if !state.filter_input.is_empty() {
            "No processes match filter"
        } else {
            "Collecting telemetry…"
        };
        let mw = m.measure(msg, TextFace::UiSmall);
        let my = center_text_y(rows_y0, (y1 - rows_y0).max(ROW_H), m.line_h(TextFace::UiSmall));
        scene.ui_small(x0 + (x1 - x0 - mw) * 0.5, my, msg, theme::INK_4, false, false, body_clip);
        return;
    }

    let total_mem = state.engine.mem().total_bytes.max(1);
    let tree_mode = state.engine.tree_mode();
    let start_idx = state.scroll_row.min(total_rows.saturating_sub(1));
    let end_idx = (start_idx + visible_rows).min(total_rows);

    for (vis_i, abs_idx) in (start_idx..end_idx).enumerate() {
        let row = &rows[abs_idx];
        let ry0 = rows_y0 + vis_i as f32 * ROW_H;
        let ry1 = ry0 + ROW_H;
        if ry0 >= y1 {
            break;
        }
        let selected = abs_idx == state.selected_row;

        // Row background + selection accent
        if selected {
            scene.round_rect(
                x0 + 2.0,
                ry0 + 0.5,
                content_x1 - 2.0,
                ry1 - 0.5,
                4.0,
                ROW_SELECTED_BG,
                0.0,
                [0.0; 4],
                body_clip,
            );
            scene.round_rect(
                x0 + 2.0,
                ry0 + 2.5,
                x0 + 4.5,
                ry1 - 2.5,
                1.2,
                theme::MATCH_HIGHLIGHT,
                0.0,
                [0.0; 4],
                body_clip,
            );
        } else if vis_i % 2 == 1 {
            scene.round_rect(
                x0 + 1.0,
                ry0,
                content_x1 - 1.0,
                ry1,
                0.0,
                ZEBRA_BG,
                0.0,
                [0.0; 4],
                body_clip,
            );
        }

        // Whole row click target (pushed before chevron so chevron takes priority)
        scene.push_hit_clipped(
            HitRect::new(x0 + 1.0, ry0, content_x1 - 1.0, ry1),
            body_clip,
            HitAction::SysMonSelectRow { block: block_id, row: abs_idx },
        );

        // Process column (tree prefix + collapse chevron + icon + name + collapsed child badge)
        draw_process_cell(
            scene,
            block_id,
            row,
            tree_mode,
            selected,
            proc_x0,
            proc_x1,
            ry0,
            ry1,
            m,
            body_clip,
        );

        // Show cumulative subtree CPU %, GPU %, and Memory on parent rows by default in tree mode
        // (and always when collapsed), so app trees immediately reflect their true combined footprint!
        let use_subtree = tree_mode && row.has_children && (state.cumulative || row.collapsed);
        let cpu_val = if use_subtree { row.subtree_cpu_pct } else { row.cpu_pct };
        let gpu_val = if use_subtree { row.subtree_gpu_pct } else { row.gpu_pct };
        let mem_val = if use_subtree { row.subtree_mem_bytes } else { row.mem_bytes };

        let mono_ty = center_text_y(ry0, ROW_H, m.line_h(TextFace::Mono));

        // CPU % + 32px mini horizontal heat bar
        draw_cpu_cell(scene, cpu_val, cpu_x0, cpu_x1 - 6.0, ry0, mono_ty, show_bars, m, body_clip);

        // GPU % + 28px mini cyan bar
        draw_gpu_cell(scene, gpu_val, gpu_x0, gpu_x1 - 6.0, ry0, mono_ty, show_bars, m, body_clip);

        // MEMORY + subtle blue mini bar proportional to total RAM
        draw_mem_cell(
            scene,
            mem_val,
            total_mem,
            mem_x0,
            mem_x1 - 6.0,
            ry0,
            mono_ty,
            show_bars,
            m,
            body_clip,
        );

        // THR
        if thr_w > 0.0 {
            let thr_val = if use_subtree { row.subtree_threads } else { row.threads };
            let thr_s = format!("{thr_val}");
            let tw = m.measure(&thr_s, TextFace::Mono);
            scene.text(thr_x1 - 6.0 - tw, mono_ty, thr_s, theme::INK_3, false, false, body_clip);
        }

        // PID
        let pid_s = format!("{}", row.pid);
        let pw = m.measure(&pid_s, TextFace::Mono);
        scene.text(pid_x1 - pw, mono_ty, pid_s, theme::INK_4, false, false, body_clip);
    }

    // -------------------------------------------------------------------------
    // Right-Edge Scrollbar Track & Thumb
    // -------------------------------------------------------------------------
    if show_scrollbar {
        let track_x0 = x1 - 6.0;
        let track_x1 = x1 - 2.0;
        let track_y0 = rows_y0 + 3.0;
        let track_y1 = y1 - 3.0;
        let track_h = (track_y1 - track_y0).max(12.0);

        scene.round_rect(
            track_x0,
            track_y0,
            track_x1,
            track_y1,
            2.0,
            theme::rgba(0xff, 0xff, 0xff, 0.05),
            0.0,
            [0.0; 4],
            tclip,
        );

        let vis_frac = (visible_rows as f32 / total_rows as f32).clamp(0.08, 1.0);
        let thumb_h = (track_h * vis_frac).max(16.0).min(track_h);
        let max_scroll = total_rows.saturating_sub(visible_rows).max(1);
        let scroll_frac = (state.scroll_row as f32 / max_scroll as f32).clamp(0.0, 1.0);
        let thumb_y0 = track_y0 + (track_h - thumb_h) * scroll_frac;

        scene.round_rect(
            track_x0,
            thumb_y0,
            track_x1,
            thumb_y0 + thumb_h,
            2.0,
            theme::rgba(0xeb, 0xea, 0xe6, 0.25),
            0.0,
            [0.0; 4],
            tclip,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_sort_header(
    scene: &mut Scene,
    block_id: BlockId,
    label: &str,
    col: Option<SortColumn>,
    active_sort: SortColumn,
    asc: bool,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    ty: f32,
    right_align: bool,
    m: CellMetrics,
    clip_id: u32,
) {
    let is_active = col.is_some_and(|c| c == active_sort);
    let text = if is_active {
        let arrow = if asc { " ↑" } else { " ↓" };
        format!("{label}{arrow}")
    } else {
        label.to_owned()
    };
    let fg = if is_active {
        if col == Some(SortColumn::Gpu) { GPU_CYAN_BRIGHT } else { theme::INK_1 }
    } else {
        theme::INK_3
    };
    let tw = m.measure_styled(&text, TextFace::UiSmall, true);
    let tx = if right_align { (x1 - tw).max(x0) } else { x0 };
    scene.ui_small(tx, ty, text, fg, true, false, clip_id);

    if is_active {
        let accent = if col == Some(SortColumn::Gpu) { GPU_CYAN } else { theme::MATCH_HIGHLIGHT };
        scene.round_rect(tx, y1 - 2.0, (tx + tw).min(x1), y1, 1.0, accent, 0.0, [0.0; 4], clip_id);
    }

    if let Some(sort_col) = col {
        scene.push_hit_clipped(
            HitRect::new(x0, y0, x1, y1),
            clip_id,
            HitAction::SysMonSort { block: block_id, col: sort_col.as_u8() },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_process_cell(
    scene: &mut Scene,
    block_id: BlockId,
    row: &ProcessRow<'static>,
    tree_mode: bool,
    selected: bool,
    x0: f32,
    x1: f32,
    ry0: f32,
    ry1: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    let pclip = intersect_clip(scene, clip_id, x0, ry0, x1, ry1, 0.0);
    let ui_ty = center_text_y(ry0, ROW_H, m.line_h(TextFace::Ui));
    let mut cx;

    // Pixel-snapped GPU vector tree guides + crisp disclosure chevron
    if tree_mode {
        let guide_col = theme::rgba(0xff, 0xff, 0xff, 0.13);
        let active_guide_col = if selected {
            theme::rgba(0x7c, 0xb7, 0xff, 0.42)
        } else {
            theme::rgba(0xff, 0xff, 0xff, 0.17)
        };
        let chev_col = if selected { theme::INK_1 } else { theme::INK_2 };

        let depth = (row.depth as usize).min(TREE_MAX_VISUAL_DEPTH);
        let stem_x = |lvl: usize| (x0 + TREE_BASE_X + lvl as f32 * TREE_INDENT).round();
        let cy = (ry0 + ROW_H * 0.5).round();
        let my_x = stem_x(depth);

        // Pass-through vertical ancestor guides
        for lvl in 0..depth.saturating_sub(1) {
            if (row.ancestor_mask >> lvl) & 1 != 0 {
                let sx = stem_x(lvl);
                scene.round_rect(sx - 0.5, ry0, sx + 0.5, ry1, 0.0, guide_col, 0.0, [0.0; 4], pclip);
            }
        }

        // Incoming L/T connector
        if depth >= 1 {
            let sx = stem_x(depth - 1);
            if row.is_last_sibling {
                scene.round_rect(
                    sx - 0.5,
                    ry0,
                    sx + 0.5,
                    cy + 0.5,
                    0.0,
                    active_guide_col,
                    0.0,
                    [0.0; 4],
                    pclip,
                );
            } else if selected {
                scene.round_rect(
                    sx - 0.5,
                    ry0,
                    sx + 0.5,
                    cy + 0.5,
                    0.0,
                    active_guide_col,
                    0.0,
                    [0.0; 4],
                    pclip,
                );
                scene.round_rect(
                    sx - 0.5,
                    cy + 0.5,
                    sx + 0.5,
                    ry1,
                    0.0,
                    guide_col,
                    0.0,
                    [0.0; 4],
                    pclip,
                );
            } else {
                scene.round_rect(sx - 0.5, ry0, sx + 0.5, ry1, 0.0, guide_col, 0.0, [0.0; 4], pclip);
            }

            let branch_x1 = if row.has_children { my_x - 6.5 } else { my_x + 5.0 };
            if branch_x1 > sx - 0.5 {
                scene.round_rect(
                    sx - 0.5,
                    cy - 0.5,
                    branch_x1,
                    cy + 0.5,
                    0.0,
                    active_guide_col,
                    0.0,
                    [0.0; 4],
                    pclip,
                );
            }
        }

        // Parent-to-child drop connector (meets first child's ry0 with zero vertical gap)
        if row.has_children && !row.collapsed {
            scene.round_rect(
                my_x - 0.5,
                cy + 6.0,
                my_x + 0.5,
                ry1,
                0.0,
                guide_col,
                0.0,
                [0.0; 4],
                pclip,
            );
        }

        // Crisp GPU vector disclosure chevron + hit target
        if row.has_children {
            if row.collapsed {
                scene.line(my_x - 2.0, cy - 3.5, my_x + 2.0, cy, 1.35, chev_col, pclip);
                scene.line(my_x + 2.0, cy, my_x - 2.0, cy + 3.5, 1.35, chev_col, pclip);
            } else {
                scene.line(my_x - 3.5, cy - 1.5, my_x, cy + 2.2, 1.35, chev_col, pclip);
                scene.line(my_x, cy + 2.2, my_x + 3.5, cy - 1.5, 1.35, chev_col, pclip);
            }
            scene.push_hit_clipped(
                HitRect::new((my_x - 8.0).max(x0), ry0, (my_x + 8.0).min(x1), ry1),
                pclip,
                HitAction::SysMonToggleCollapse { block: block_id, pid: row.pid },
            );
        }

        cx = my_x + 9.5;
    } else {
        cx = x0 + 8.0;
    }

    // Icon: real macOS `.app` Retina icon, else IntelliJ SVG icon, else subtle rounded glyph badge
    let icon_size = 16.0;
    let icon_y = ry0 + (ROW_H - icon_size) * 0.5;
    if cx + icon_size < x1 {
        let app_icon = row
            .app_bundle_path
            .as_deref()
            .and_then(crate::sysmon_driver::lookup_or_cache_app_icon);
        if let Some(icon_path) = app_icon {
            scene.image(cx, icon_y, icon_size, icon_size, icon_path, pclip);
        } else if let Some(svg_name) = fallback_svg_icon_for_process(row.name.as_str()) {
            scene.svg_icon(cx, icon_y, icon_size, svg_name, pclip);
        } else {
            draw_process_glyph_badge(scene, row.name.as_str(), cx, icon_y, icon_size, m, pclip);
        }
        cx += icon_size + 6.0;
    }

    // Reserve space for `+N` collapsed child count pill if present
    let child_badge = if tree_mode && row.has_children && row.collapsed && row.child_count > 0 {
        Some(format!("+{}", row.child_count))
    } else {
        None
    };
    let badge_reserve = child_badge
        .as_deref()
        .map_or(0.0, |b| m.measure_styled(b, TextFace::UiSmall, true) + 14.0);

    let name_avail_w = (x1 - cx - badge_reserve - 2.0).max(16.0);
    let avg_char_w = (m.cw * 0.85).max(4.5);
    let max_name_chars = (name_avail_w / avg_char_w).floor().max(1.0) as usize;
    let name_shown = truncate_chars(row.name.as_str(), max_name_chars);
    let is_app = row.app_bundle_path.is_some();
    let name_bold = selected || is_app;
    let name_color = if selected || is_app || row.depth == 0 { theme::INK_1 } else { theme::INK_2 };
    scene.ui_text(cx, ui_ty, &name_shown, name_color, name_bold, false, pclip);
    cx += m.measure_styled(&name_shown, TextFace::Ui, name_bold) + 6.0;

    if let Some(badge) = child_badge {
        let bw = m.measure_styled(&badge, TextFace::UiSmall, true) + 8.0;
        if cx + bw <= x1 {
            let bh = 15.0;
            let by0 = ry0 + (ROW_H - bh) * 0.5;
            scene.round_rect(cx, by0, cx + bw, by0 + bh, 4.0, theme::BG_CHIP, 0.75, theme::LINE, pclip);
            let bty = center_text_y(by0, bh, m.line_h(TextFace::UiSmall));
            scene.ui_small(cx + 4.0, bty, badge, theme::INK_3, true, false, pclip);
        }
    }
}

fn draw_process_glyph_badge(
    scene: &mut Scene,
    name: &str,
    x: f32,
    y: f32,
    size: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    let inset = 0.5;
    scene.round_rect(
        x + inset,
        y + inset,
        x + size - inset,
        y + size - inset,
        3.5,
        theme::BG_CHIP,
        0.75,
        theme::LINE,
        clip_id,
    );
    let ch = name
        .chars()
        .find(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('·');
    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    let tw = m.measure_styled(s, TextFace::UiSmall, true);
    let tx = x + (size - tw) * 0.5;
    let ty = center_text_y(y, size, m.line_h(TextFace::UiSmall));
    scene.ui_small(tx, ty, s, theme::INK_3, true, false, clip_id);
}

#[allow(clippy::too_many_arguments)]
fn draw_cpu_cell(
    scene: &mut Scene,
    cpu_pct: f32,
    x0: f32,
    x1: f32,
    ry0: f32,
    mono_ty: f32,
    show_bar: bool,
    m: CellMetrics,
    clip_id: u32,
) {
    let val_str = format!("{cpu_pct:.1}%");
    let color = if cpu_pct >= 75.0 {
        theme::DANGER
    } else if cpu_pct >= 25.0 {
        AMBER
    } else if cpu_pct >= 0.5 {
        theme::INK_1
    } else {
        theme::INK_3
    };
    let bar_w = if show_bar { 32.0 } else { 0.0 };
    let text_x1 = if show_bar { x1 - bar_w - 6.0 } else { x1 };
    let tw = m.measure(&val_str, TextFace::Mono);
    scene.text((text_x1 - tw).max(x0), mono_ty, val_str, color, cpu_pct >= 25.0, false, clip_id);

    if show_bar {
        let bx0 = x1 - bar_w;
        let by0 = ry0 + (ROW_H - 5.0) * 0.5;
        let by1 = by0 + 5.0;
        scene.round_rect(bx0, by0, x1, by1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
        let frac = (cpu_pct / 100.0).clamp(0.0, 1.0);
        if frac > 0.005 {
            let fx1 = (bx0 + (bar_w * frac).max(2.0)).min(x1);
            scene.round_rect(bx0, by0, fx1, by1, 2.0, cpu_heat_color(cpu_pct), 0.0, [0.0; 4], clip_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_gpu_cell(
    scene: &mut Scene,
    gpu_pct: f32,
    x0: f32,
    x1: f32,
    ry0: f32,
    mono_ty: f32,
    show_bar: bool,
    m: CellMetrics,
    clip_id: u32,
) {
    let active = gpu_pct > 0.05;
    let bar_w = if show_bar { 28.0 } else { 0.0 };
    let text_x1 = if show_bar { x1 - bar_w - 6.0 } else { x1 };

    if active {
        let val_str = format!("{gpu_pct:.1}%");
        let tw = m.measure(&val_str, TextFace::Mono);
        scene.text((text_x1 - tw).max(x0), mono_ty, val_str, GPU_CYAN_BRIGHT, true, false, clip_id);
    } else {
        let tw = m.measure("·", TextFace::Mono);
        scene.text((text_x1 - tw).max(x0), mono_ty, "·", theme::INK_4, false, false, clip_id);
    }

    if show_bar {
        let bx0 = x1 - bar_w;
        let by0 = ry0 + (ROW_H - 5.0) * 0.5;
        let by1 = by0 + 5.0;
        scene.round_rect(bx0, by0, x1, by1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
        if active {
            let frac = (gpu_pct / 100.0).clamp(0.0, 1.0);
            let fx1 = (bx0 + (bar_w * frac).max(2.5)).min(x1);
            scene.round_rect(bx0, by0, fx1, by1, 2.0, GPU_CYAN, 0.0, [0.0; 4], clip_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_mem_cell(
    scene: &mut Scene,
    mem_bytes: u64,
    total_mem_bytes: u64,
    x0: f32,
    x1: f32,
    ry0: f32,
    mono_ty: f32,
    show_bar: bool,
    m: CellMetrics,
    clip_id: u32,
) {
    let val_str = format_iec_bytes(mem_bytes);
    let fg = if mem_bytes >= 512 * 1024 * 1024 { theme::INK_1 } else { theme::INK_2 };
    let bar_w = if show_bar { 30.0 } else { 0.0 };
    let text_x1 = if show_bar { x1 - bar_w - 6.0 } else { x1 };
    let tw = m.measure(&val_str, TextFace::Mono);
    scene.text((text_x1 - tw).max(x0), mono_ty, val_str, fg, false, false, clip_id);

    if show_bar {
        let bx0 = x1 - bar_w;
        let by0 = ry0 + (ROW_H - 5.0) * 0.5;
        let by1 = by0 + 5.0;
        scene.round_rect(bx0, by0, x1, by1, 2.0, theme::BG_TRACK, 0.0, [0.0; 4], clip_id);
        let frac = (mem_bytes as f64 / total_mem_bytes.max(1) as f64).clamp(0.0, 1.0) as f32;
        if frac > 0.001 {
            let fx1 = (bx0 + (bar_w * frac).max(2.0)).min(x1);
            scene.round_rect(
                bx0,
                by0,
                fx1,
                by1,
                2.0,
                theme::rgba(0x5b, 0x9b, 0xf8, 0.80),
                0.0,
                [0.0; 4],
                clip_id,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_selected_inspector(
    scene: &mut Scene,
    block_id: BlockId,
    state: &SysMonState,
    row: &ProcessRow<'static>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    scene.round_rect(x0, y0, x1, y1, 6.0, CARD_BG, 1.0, theme::LINE, clip_id);
    let iclip = intersect_clip(scene, clip_id, x0 + 1.0, y0 + 1.0, x1 - 1.0, y1 - 1.0, 5.0);

    let ix0 = x0 + 9.0;
    let ix1 = x1 - 9.0;
    let line1_y0 = y0 + 2.5;
    let line1_h = 18.5;
    let line2_y0 = y0 + 21.0;
    let line2_h = (y1 - line2_y0 - 2.0).max(16.0);

    // -------------------------------------------------------------------------
    // Line 1: 14px icon + bold name + metadata badges + right-aligned Kill chip
    // -------------------------------------------------------------------------
    let kill_h = 17.0;
    let kill_y0 = line1_y0 + (line1_h - kill_h) * 0.5;
    let kill_x0 = draw_pill_right(
        scene,
        ix1,
        kill_y0,
        kill_y0 + kill_h,
        "Kill",
        theme::DANGER,
        KILL_CHIP_BG,
        KILL_CHIP_BORDER,
        m,
        iclip,
        HitAction::SysMonRequestKill(block_id),
    );
    let line1_max_x = (kill_x0 - 8.0).max(ix0 + 60.0);
    let l1_clip = intersect_clip(scene, iclip, ix0, line1_y0, line1_max_x, line1_y0 + line1_h, 0.0);
    let l1_ty = center_text_y(line1_y0, line1_h, m.line_h(TextFace::UiSmall));

    let mut lx = ix0;
    let icon_size = 14.0;
    let icon_y = line1_y0 + (line1_h - icon_size) * 0.5;
    if lx + icon_size < line1_max_x {
        let app_icon = row
            .app_bundle_path
            .as_deref()
            .and_then(crate::sysmon_driver::lookup_or_cache_app_icon);
        if let Some(icon_path) = app_icon {
            scene.image(lx, icon_y, icon_size, icon_size, icon_path, l1_clip);
        } else if let Some(svg_name) = fallback_svg_icon_for_process(row.name.as_str()) {
            scene.svg_icon(lx, icon_y, icon_size, svg_name, l1_clip);
        } else {
            draw_process_glyph_badge(scene, row.name.as_str(), lx, icon_y, icon_size, m, l1_clip);
        }
        lx += icon_size + 6.0;
    }

    let name_budget_w = (line1_max_x - lx).clamp(40.0, 220.0);
    let max_name_chars = (name_budget_w / (m.cw * 0.80).max(4.5)).floor().max(4.0) as usize;
    let name_shown = truncate_chars(row.name.as_str(), max_name_chars);
    let name_w = m.measure_styled(&name_shown, TextFace::UiSmall, true);
    scene.ui_small(lx, l1_ty, name_shown, theme::INK_1, true, false, l1_clip);
    lx += name_w + 8.0;

    // Metadata badges separated by ` · `
    let mut meta: Vec<(String, [f32; 4], bool)> = Vec::with_capacity(6);
    meta.push((format!("PID {}", row.pid), theme::INK_3, false));

    let wall_now = state.engine.wall_time_secs();
    if row.start_time_secs > 0 && wall_now > 0 {
        let up_secs = wall_now.saturating_sub(row.start_time_secs);
        meta.push((format!("up {}", format_uptime(up_secs)), theme::INK_3, false));
    }

    let tree_mode = state.engine.tree_mode();
    if tree_mode && row.has_children {
        meta.push((format!("self {}", format_iec_bytes(row.mem_bytes)), theme::INK_3, false));
        meta.push((
            format!("Σ {} (+{})", format_iec_bytes(row.subtree_mem_bytes), row.child_count),
            theme::rgb(0x96, 0xc2, 0xff),
            false,
        ));
    }

    let use_subtree = tree_mode && row.has_children && (state.cumulative || row.collapsed);
    let disk_bps = if use_subtree { row.subtree_disk_bps } else { row.disk_bps };
    if disk_bps > 0 {
        let io_col = if disk_bps >= 1024 * 1024 { AMBER } else { theme::INK_2 };
        meta.push((format!("I/O {}/s", format_iec_bytes(disk_bps)), io_col, false));
    }

    if row.gpu_queues > 0 {
        meta.push((format!("{} GPU q", row.gpu_queues), GPU_CYAN_BRIGHT, true));
    }

    let sep = " · ";
    let sep_w = m.measure(sep, TextFace::UiSmall);
    for (idx, (seg, col, bold)) in meta.into_iter().enumerate() {
        let seg_w = m.measure_styled(&seg, TextFace::UiSmall, bold);
        let needed = if idx > 0 { sep_w + seg_w } else { seg_w };
        if lx + needed > line1_max_x {
            break;
        }
        if idx > 0 {
            scene.ui_small(lx, l1_ty, sep, theme::INK_4, false, false, l1_clip);
            lx += sep_w;
        }
        scene.ui_small(lx, l1_ty, seg, col, bold, false, l1_clip);
        lx += seg_w;
    }

    // -------------------------------------------------------------------------
    // Line 2: Cached cmdline (fallback exe_path) + right-aligned cwd (no overlap)
    // -------------------------------------------------------------------------
    let l2_ty = center_text_y(line2_y0, line2_h, m.line_h(TextFace::UiSmall));
    let l2_avail_w = (ix1 - ix0).max(40.0);
    let mut cmd_max_x = ix1;

    if !row.cwd.is_empty() && l2_avail_w >= 240.0 {
        let cwd_budget_w = (l2_avail_w * 0.36).clamp(80.0, 320.0);
        let avg_small_cw = (m.cw * 0.78).max(4.5);
        let prefix = "in ";
        let prefix_w = m.measure(prefix, TextFace::UiSmall);
        let path_budget_w = (cwd_budget_w - prefix_w).max(24.0);
        let max_cwd_chars = (path_budget_w / avg_small_cw).floor().max(8.0) as usize;
        let cwd_trunc = truncate_middle(row.cwd.as_str(), max_cwd_chars);
        let cwd_label = format!("{prefix}{cwd_trunc}");
        let cwd_w = m.measure(&cwd_label, TextFace::UiSmall).min(cwd_budget_w);
        let cwd_x = (ix1 - cwd_w).max(ix0 + l2_avail_w * 0.55);
        let cwd_clip = intersect_clip(scene, iclip, cwd_x, line2_y0, ix1, line2_y0 + line2_h, 0.0);
        scene.ui_small(cwd_x, l2_ty, cwd_label, theme::INK_4, false, false, cwd_clip);
        cmd_max_x = (cwd_x - 10.0).max(ix0 + 40.0);
    }

    let cached_cmd = state
        .cached_cmdline
        .as_ref()
        .and_then(|(p, cmd)| (*p == row.pid && !cmd.is_empty()).then_some(cmd.as_str()));
    let cmd_raw = cached_cmd.unwrap_or_else(|| {
        if !row.exe_path.is_empty() {
            row.exe_path.as_str()
        } else {
            row.name.as_str()
        }
    });
    let cmd_avail_w = (cmd_max_x - ix0).max(24.0);
    let cmd_clip = intersect_clip(scene, iclip, ix0, line2_y0, cmd_max_x, line2_y0 + line2_h, 0.0);
    let cmd_max_chars = ((cmd_avail_w / (m.cw * 0.80).max(4.5)).floor() as usize).max(4);
    let cmd_shown = truncate_chars(cmd_raw, cmd_max_chars);
    scene.ui_small(ix0, l2_ty, cmd_shown, theme::INK_2, false, false, cmd_clip);
}

#[allow(clippy::too_many_arguments)]
fn draw_footer_bar(
    scene: &mut Scene,
    state: &SysMonState,
    x0: f32,
    y0: f32,
    x1: f32,
    h: f32,
    m: CellMetrics,
    clip_id: u32,
) {
    let fclip = intersect_clip(scene, clip_id, x0, y0, x1, y0 + h, 6.0);
    let ty = center_text_y(y0, h, m.line_h(TextFace::UiSmall));

    if let Some((pid, ref name)) = state.confirm_kill {
        scene.round_rect(x0, y0, x1, y0 + h, 6.0, KILL_BG, 1.0, KILL_BORDER, fclip);
        let prompt = format!(
            "Terminate {name} (PID {pid})?  [y] SIGTERM · [K] SIGKILL · [Esc] cancel"
        );
        scene.ui_small(x0 + 10.0, ty, prompt, theme::DANGER, true, false, fclip);
    } else if let Some(ref msg) = state.status_msg {
        scene.round_rect(x0, y0, x1, y0 + h, 6.0, theme::BG_CHIP, 1.0, theme::LINE, fclip);
        scene.ui_small(x0 + 10.0, ty, msg.as_str(), theme::INK_2, false, false, fclip);
    } else {
        let legend =
            "↑↓ select · ←→ fold · t tree · c cumul · / filter · 1-5 sort · x kill · p pause · ⌘⇧F expand · q close";
        let max_chars = ((x1 - x0 - 8.0) / (m.cw * 0.78).max(4.0)).floor().max(8.0) as usize;
        let shown = truncate_chars(legend, max_chars);
        scene.ui_small(x0 + 4.0, ty, shown, theme::INK_4, false, false, fclip);
    }
}

// =============================================================================
// Small Pure Helpers
// =============================================================================

#[inline]
fn cpu_heat_color(pct: f32) -> [f32; 4] {
    if pct < 40.0 {
        theme::OK
    } else if pct < 75.0 {
        AMBER
    } else {
        theme::DANGER
    }
}

#[inline]
fn center_text_y(y0: f32, box_h: f32, line_h: f32) -> f32 {
    y0 + ((box_h - line_h) * 0.5).max(0.0)
}

fn format_gpu_chip(model: &str, cores: u32) -> String {
    let trimmed = model.trim();
    match (trimmed.is_empty(), cores > 0) {
        (false, true) => format!("{trimmed} · {cores}c"),
        (false, false) => trimmed.to_owned(),
        (true, true) => format!("GPU · {cores}c"),
        (true, false) => String::new(),
    }
}

fn fallback_svg_icon_for_process(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("rustc")
        || lower.contains("cargo")
        || lower.contains("blaze")
        || lower.contains("rust-analyzer")
        || lower.contains("clippy")
        || lower.contains("rustfmt")
    {
        if lower == "blaze-shell" {
            return Some("shell_dark");
        }
        return Some("rust_dark");
    }
    if matches!(
        lower.as_str(),
        "zsh" | "bash" | "sh" | "fish" | "nu" | "tmux" | "screen" | "ssh" | "sshd" | "login"
    ) || lower.contains("terminal")
        || lower.contains("shell")
    {
        return Some("shell_dark");
    }
    if lower.starts_with("python") || lower == "pytest" || lower == "uv" {
        return Some("python_dark");
    }
    if matches!(lower.as_str(), "node" | "bun" | "deno" | "npm" | "pnpm" | "yarn") {
        return Some("javaScript_dark");
    }
    if lower == "go" || lower == "gopls" {
        return Some("go_dark");
    }
    if lower.contains("docker") || lower == "containerd" || lower == "orbstack" {
        return Some("docker_dark");
    }
    if lower.starts_with("swift") || lower == "sourcekit-lsp" {
        return Some("swiftLang_dark");
    }
    None
}

fn intersect_clip(
    scene: &mut Scene,
    parent_clip: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
) -> u32 {
    if let Some(parent) = scene.clips.get(parent_clip as usize) {
        let cx0 = x0.max(parent.rect[0]);
        let cy0 = y0.max(parent.rect[1]);
        let cx1 = x1.min(parent.rect[2]).max(cx0);
        let cy1 = y1.min(parent.rect[3]).max(cy0);
        scene.push_clip(cx0, cy0, cx1, cy1, radius)
    } else {
        scene.push_clip(x0, y0, x1, y1, radius)
    }
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_iec_bytes_covers_all_units() {
        assert_eq!(format_iec_bytes(0), "0 B");
        assert_eq!(format_iec_bytes(512), "512 B");
        assert_eq!(format_iec_bytes(1023), "1023 B");
        assert_eq!(format_iec_bytes(1024), "1.0 KiB");
        assert_eq!(format_iec_bytes(15 * 1024), "15 KiB");
        assert_eq!(format_iec_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_iec_bytes(248 * 1024 * 1024), "248 MiB");
        assert_eq!(format_iec_bytes(1_503_238_554), "1.4 GiB");
        assert_eq!(format_iec_bytes(12_670_153_523), "11.8 GiB");
        assert_eq!(format_iec_bytes(128 * 1024 * 1024 * 1024), "128 GiB");
        assert_eq!(format_iec_bytes(1024u64.pow(4)), "1.00 TiB");
    }

    #[test]
    fn format_uptime_covers_seconds_minutes_hours_days() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(45), "45s");
        assert_eq!(format_uptime(60), "1m");
        assert_eq!(format_uptime(125), "2m 5s");
        assert_eq!(format_uptime(3_600), "1h");
        assert_eq!(format_uptime(3 * 3_600 + 40 * 60), "3h 40m");
        assert_eq!(format_uptime(86_400), "1d");
        assert_eq!(format_uptime(4 * 86_400 + 12 * 3_600), "4d 12h");
    }

    #[test]
    fn format_count_compact_formats_thousands() {
        assert_eq!(format_count_compact(312), "312");
        assert_eq!(format_count_compact(999), "999");
        assert_eq!(format_count_compact(2_140), "2.1k");
        assert_eq!(format_count_compact(128_400), "128k");
    }

    #[test]
    fn sysmon_state_move_selection_and_scroll_by() {
        let mut st = SysMonState::new();
        assert!(st.running);
        assert_eq!(st.selected_row, 0);
        assert_eq!(st.scroll_row, 0);

        // Empty table stays safely clamped at 0.
        st.move_selection(5, 10);
        assert_eq!(st.selected_row, 0);
        assert_eq!(st.scroll_row, 0);
        st.scroll_by(5, 10);
        assert_eq!(st.selected_row, 0);
        assert_eq!(st.scroll_row, 0);

        // Simulate 30 rows with a 10-row viewport.
        let total = 30;
        let vis = 10;

        // Moving down inside viewport keeps scroll_row at 0.
        st.move_selection_with_len(9, vis, total);
        assert_eq!(st.selected_row, 9);
        assert_eq!(st.scroll_row, 0);

        // Moving one more down scrolls viewport by 1.
        st.move_selection_with_len(1, vis, total);
        assert_eq!(st.selected_row, 10);
        assert_eq!(st.scroll_row, 1);

        // Jumping past the end clamps cleanly to last row and max_scroll.
        st.move_selection_with_len(100, vis, total);
        assert_eq!(st.selected_row, 29);
        assert_eq!(st.scroll_row, 20);

        // Moving up past top of viewport scrolls viewport up.
        st.move_selection_with_len(-15, vis, total);
        assert_eq!(st.selected_row, 14);
        assert_eq!(st.scroll_row, 14);

        // Trackpad scroll down moves viewport and pulls selection into view.
        st.scroll_by_with_len(10, vis, total);
        assert_eq!(st.scroll_row, 20);
        assert_eq!(st.selected_row, 20);

        // Trackpad scroll up slightly keeps selection if already in view.
        st.scroll_by_with_len(-3, vis, total);
        assert_eq!(st.scroll_row, 17);
        assert_eq!(st.selected_row, 20);

        // Trackpad scroll back to top clamps selection into [0..=9].
        st.scroll_by_with_len(-50, vis, total);
        assert_eq!(st.scroll_row, 0);
        assert_eq!(st.selected_row, 9);

        // Moving up past 0 clamps cleanly to 0.
        st.move_selection_with_len(-50, vis, total);
        assert_eq!(st.selected_row, 0);
        assert_eq!(st.scroll_row, 0);
    }

    #[test]
    fn visible_rows_for_height_matches_inline_and_fullscreen() {
        assert_eq!(visible_rows_for_height(None, false, false), INLINE_VISIBLE_ROWS);
        assert!(visible_rows_for_height(Some(900.0), true, false) > INLINE_VISIBLE_ROWS);
        assert!(visible_rows_for_height(Some(300.0), false, false) >= 1);
    }

    #[test]
    fn layout_sysmon_block_and_selection_with_real_snapshot() {
        use blaze_own::{AString, AVec};
        use blaze_sysmon::{CoreTicks, RawGpuSample, RawMemSample, RawProcess, RawSnapshot};

        let alloc = Alloc::global();
        let make_snap = |ts_ns: u64, tick: u64| {
            let mut cores = AVec::new_in(alloc);
            for i in 0..8 {
                cores.push(CoreTicks {
                    user: 100 * tick + i * 5,
                    system: 40 * tick,
                    idle: 200 * tick,
                    nice: 0,
                });
            }
            let mut procs = AVec::new_in(alloc);
            // Parent app process (pid 100) + 15 child helpers (pid 101..=115)
            procs.push(RawProcess {
                pid: 100,
                ppid: 1,
                uid: 501,
                name: AString::from_str_in("Google Chrome", alloc),
                exe_path: AString::from_str_in(
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                    alloc,
                ),
                cwd: AString::from_str_in("/Volumes/Projects/blaze", alloc),
                app_bundle_path: None,
                cpu_time_ns: 250_000_000 * tick,
                gpu_time_ns: 120_000_000 * tick,
                gpu_queues: 2,
                mem_bytes: 1_500_000_000,
                disk_read_bytes: 2 * 1024 * 1024 * tick,
                disk_write_bytes: 1024 * 1024 * tick,
                threads: 42,
                start_time_secs: 1_700_000_000,
            });
            for idx in 1u32..=15 {
                procs.push(RawProcess {
                    pid: 100 + idx,
                    ppid: 100,
                    uid: 501,
                    name: AString::from_str_in(&format!("rustc-worker-{idx}"), alloc),
                    exe_path: AString::from_str_in("/usr/local/bin/rustc", alloc),
                    cwd: AString::new_in(alloc),
                    app_bundle_path: None,
                    cpu_time_ns: u64::from(idx) * 20_000_000 * tick,
                    gpu_time_ns: 0,
                    gpu_queues: 0,
                    mem_bytes: u64::from(idx) * 64 * 1024 * 1024,
                    disk_read_bytes: 0,
                    disk_write_bytes: 0,
                    threads: 4,
                    start_time_secs: 0,
                });
            }
            RawSnapshot {
                timestamp_ns: ts_ns,
                wall_time_secs: 1_700_003_600,
                cores,
                mem: RawMemSample {
                    total_bytes: 64 * 1024u64.pow(3),
                    app_bytes: 18 * 1024u64.pow(3),
                    wired_bytes: 6 * 1024u64.pow(3),
                    compressed_bytes: 4 * 1024u64.pow(3),
                    cached_bytes: 12 * 1024u64.pow(3),
                    swap_used_bytes: 512 * 1024 * 1024,
                    swap_total_bytes: 2 * 1024u64.pow(3),
                },
                gpu: RawGpuSample {
                    model: AString::from_str_in("Apple M5 Max", alloc),
                    core_count: 40,
                    device_util_pct: 37.0,
                    renderer_util_pct: 28.0,
                    tiler_util_pct: 14.0,
                    vram_in_use_bytes: 2 * 1024u64.pow(3),
                    vram_alloc_bytes: 12 * 1024u64.pow(3),
                },
                processes: procs,
                load_avg: [2.14, 1.98, 1.75],
                uptime_secs: 4 * 86_400 + 12 * 3_600,
            }
        };

        let mut st = SysMonState::new();
        st.engine.ingest(make_snap(1_000_000_000, 1));
        st.engine.ingest(make_snap(2_000_000_000, 2));
        assert_eq!(st.engine.rows().len(), 16);

        // Exercise real move_selection and scroll_by over ingested rows.
        st.move_selection(14, INLINE_VISIBLE_ROWS);
        assert_eq!(st.selected_row, 14);
        assert_eq!(st.scroll_row, 2);
        st.scroll_by(-2, INLINE_VISIBLE_ROWS);
        assert_eq!(st.scroll_row, 0);
        assert_eq!(st.selected_row, INLINE_VISIBLE_ROWS - 1);

        // Select parent row (pid 100) so the inspector displays subtree + GPU + I/O + cwd.
        st.select_row(0, INLINE_VISIBLE_ROWS);
        assert_eq!(st.selected_row, 0);

        // Render inline card and verify height + key hit regions + inspector text.
        let mut scene = Scene::new();
        let bid = BlockId(42);
        let h = layout_sysmon_block(
            &mut scene,
            bid,
            &st,
            16.0,
            40.0,
            960.0,
            None,
            CellMetrics::default(),
            0,
            false,
        );
        assert!((545.0..=590.0).contains(&h), "inline height should be ~568px, got {h}");
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::SysMonToggleTree(bid)));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::SysMonTogglePause(bid)));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::SysMonRequestKill(bid)));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::TogglePtyFullscreen));
        assert!(scene.hit_regions.iter().any(|r| r.action == HitAction::SysMonClose(bid)));
        assert!(scene.hit_regions.iter().any(|r| {
            r.action == HitAction::SysMonSort { block: bid, col: SortColumn::Gpu.as_u8() }
        }));
        assert!(scene.hit_regions.iter().any(|r| {
            r.action == HitAction::SysMonToggleCollapse { block: bid, pid: 100 }
        }));
        assert!(scene.text_runs.iter().any(|t| t.text.contains("Apple M5 Max")));
        assert!(scene.text_runs.iter().any(|t| t.text.contains("Activity & GPU")));
        assert!(scene.text_runs.iter().any(|t| t.text.contains("PID 100")));
        assert!(scene.text_runs.iter().any(|t| t.text.contains("up 1h")));
        assert!(scene.text_runs.iter().any(|t| t.text.contains("2 GPU q")));
        assert!(scene.text_runs.iter().any(|t| t.text == "Kill"));
        assert!(scene.text_runs.iter().any(|t| t.text.contains("in /Volumes/Projects/blaze")));
    }

    #[test]
    fn tree_navigation_expand_collapse_and_parent_jump() {
        use blaze_own::{AString, AVec};
        use blaze_sysmon::{CoreTicks, RawGpuSample, RawMemSample, RawProcess, RawSnapshot};

        let alloc = Alloc::global();
        let make_proc = |pid: u32, ppid: u32, name: &str, cpu_ns: u64| RawProcess {
            pid,
            ppid,
            uid: 501,
            name: AString::from_str_in(name, alloc),
            exe_path: AString::from_str_in("/usr/bin/env", alloc),
            cwd: AString::new_in(alloc),
            app_bundle_path: None,
            cpu_time_ns: cpu_ns,
            gpu_time_ns: 0,
            gpu_queues: 0,
            mem_bytes: 32 * 1024 * 1024,
            disk_read_bytes: 0,
            disk_write_bytes: 0,
            threads: 2,
            start_time_secs: 0,
        };
        let make_snap = |ts_ns: u64, tick: u64| {
            let mut cores = AVec::new_in(alloc);
            cores.push(CoreTicks { user: 10 * tick, system: 5 * tick, idle: 100 * tick, nice: 0 });
            let mut procs = AVec::new_in(alloc);
            // Tree: 100 (root) -> 200 (mid, higher cpu) -> 300 (grandchild)
            //                  -> 201 (sibling leaf)
            procs.push(make_proc(100, 1, "root-app", 400_000_000 * tick));
            procs.push(make_proc(200, 100, "mid-worker", 200_000_000 * tick));
            procs.push(make_proc(300, 200, "leaf-task", 100_000_000 * tick));
            procs.push(make_proc(201, 100, "sibling-leaf", 50_000_000 * tick));
            RawSnapshot {
                timestamp_ns: ts_ns,
                wall_time_secs: 1_700_000_000,
                cores,
                mem: RawMemSample::default(),
                gpu: RawGpuSample::empty_in(alloc),
                processes: procs,
                load_avg: [0.5, 0.4, 0.3],
                uptime_secs: 3600,
            }
        };

        let mut st = SysMonState::new();
        st.engine.ingest(make_snap(1_000_000_000, 1));
        st.engine.ingest(make_snap(2_000_000_000, 2));
        // Expected order in tree mode (sorted by subtree CPU desc): [100, 200, 300, 201]
        assert_eq!(st.engine.rows().len(), 4);
        assert_eq!(st.engine.rows()[0].pid, 100);
        assert_eq!(st.engine.rows()[1].pid, 200);
        assert_eq!(st.engine.rows()[2].pid, 300);
        assert_eq!(st.engine.rows()[3].pid, 201);

        // Start on root (100, expanded): `expand_or_child` descends into first child (200).
        st.select_row(0, 2);
        st.expand_or_child(2);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 200);

        // On expanded mid-worker (200): `expand_or_child` descends into grandchild (300) and scrolls.
        st.expand_or_child(2);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 300);
        assert_eq!(st.scroll_row, 1);

        // On leaf grandchild (300): `expand_or_child` is a no-op.
        st.expand_or_child(2);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 300);

        // On leaf grandchild (300): `collapse_or_parent` jumps up to parent (200).
        st.collapse_or_parent(2);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 200);

        // On expanded mid-worker (200): `collapse_or_parent` collapses it (hiding 300).
        st.collapse_or_parent(2);
        assert_eq!(st.engine.rows().len(), 3);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 200);
        assert!(st.engine.rows()[st.selected_row].collapsed);

        // On now-collapsed mid-worker (200): `collapse_or_parent` jumps up to root (100) and scrolls it into view.
        st.collapse_or_parent(2);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 100);
        assert_eq!(st.scroll_row, 0);

        // On expanded root (100): `collapse_or_parent` collapses root (only 1 row visible).
        st.collapse_or_parent(2);
        assert_eq!(st.engine.rows().len(), 1);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 100);
        assert!(st.engine.rows()[0].collapsed);

        // On collapsed root (100): `expand_or_child` expands root (200 stays collapsed -> 3 rows).
        st.expand_or_child(2);
        assert_eq!(st.engine.rows().len(), 3);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 100);
        assert!(!st.engine.rows()[0].collapsed);

        // Second `expand_or_child` descends into first child (200); third expands (200) revealing (300).
        st.expand_or_child(2);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 200);
        st.expand_or_child(2);
        assert_eq!(st.engine.rows().len(), 4);
        assert_eq!(st.engine.rows()[st.selected_row].pid, 200);
        assert!(!st.engine.rows()[st.selected_row].collapsed);
    }
}
