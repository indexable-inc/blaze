//! Session tree and output blocks with O(1) copy-on-write state forks.
//!
//! Every executed command produces a [`Block`] holding its source, prompt snapshot, O(1) forked
//! [`EngineState`] snapshots (`state_before` and `state_after`), timing, and typed [`BlockBody`].
//! Clicking **Fork** on any historical block creates a sibling [`Branch`] in O(1) time without
//! copying tables, variables, or `def`s.

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use blaze_sh::{EngineState, ShellError, Value};
use blaze_vt::terminal::{Terminal, Which};

use crate::prompt::PromptInfo;

/// Default maximum rows shown in a folded table viewport (~1/3 screen height on standard windows).
pub const TABLE_FOLD_ROWS: usize = 8;

/// Unique identifier for a block in [`BlockStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

/// Unique identifier for a session branch in [`BlockStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BranchId(pub u64);

/// Rich typed body of an executed shell block.
pub enum BlockBody {
    /// A structured `blaze-sh` value (table, record, list, scalar, etc.).
    Value(Value<'static>),
    /// Streamed output lines (e.g. `print`) paired with a final structured value.
    StreamAndValue { lines: Vec<String>, value: Value<'static> },
    /// A typed diagnostic error carrying source span and optional help note.
    Error { err: ShellError<'static> },
    /// An external program running on a PTY rendered through `blaze-vt`.
    Terminal {
        term: Terminal<'static>,
        pid: Option<u32>,
        running: bool,
        exit_code: Option<i32>,
    },
    /// A live `rust { ... }` / `:rs` cell compiled & executed by `blaze run`.
    RustCell {
        code: String,
        compile_ms: u64,
        run_ms: u64,
        stdout: String,
        value: Value<'static>,
    },
    /// A command running asynchronously while the prompt is already live on the next line.
    Running {
        label: String,
        /// Live OS child PID (0 until child process spawns or for in-process tasks).
        pid: Arc<AtomicU32>,
    },
    /// Native Metal btop + macOS Activity & GPU Monitor (`top` / `btop` / `htop` / `sysmon`).
    SysMon(Box<crate::sysmon_view::SysMonState>),
}

/// One submitted command and its structured output card.
pub struct Block {
    pub id: BlockId,
    pub branch_id: BranchId,
    pub source: String,
    pub prompt_snapshot: PromptInfo,
    /// O(1) CoW engine state before this block ran.
    pub state_before: EngineState<'static>,
    /// O(1) CoW engine state after this block committed (used when forking from this block).
    pub state_after: EngineState<'static>,
    pub started_ms: u64,
    pub duration_ms: u64,
    /// Exact nanosecond execution duration for crisp `ns` / `µs` / `ms` / `s` display.
    pub duration_ns: u64,
    pub exit_code: i32,
    /// Whether large tables (> 25 rows) are currently folded.
    pub folded: bool,
    /// Optional user override for compact grid vs detailed table view on file tables.
    pub grid_override: Option<bool>,
    /// When true, reveals columns marked hidden by default on the table (e.g. `type` in `ls`).
    pub show_hidden_cols: bool,
    /// Smooth per-table vertical scroll offset in logical pixels (snapped to row grid in layout).
    pub table_scroll_y: f32,
    /// Smooth per-table horizontal scroll offset in logical pixels (for wide tables).
    pub table_scroll_x: f32,
    /// Optional interactive UI sort override `(col_idx, descending)` toggled by clicking table headers.
    pub table_sort: Option<(usize, bool)>,
    /// User-dragged custom widths `(col_idx, width_px)` for interactive column resizing.
    pub col_width_overrides: Vec<(usize, f32)>,
    /// `(row, col)` table cells currently expanded inline (for nested lists/records/tables).
    pub expanded_cells: Vec<(usize, usize)>,
    pub body: BlockBody,
}

impl Block {
    /// True when this block is a live pinned view (running inline terminal / alternate-screen TUI,
    /// live `top` monitor, or in-flight command) that `Cmd+K` / `clear` should keep visible.
    pub fn is_pinned_live(&self) -> bool {
        match &self.body {
            BlockBody::Terminal { running: true, .. } | BlockBody::Running { .. } => true,
            BlockBody::SysMon(sm) => sm.running,
            _ => false,
        }
    }

    pub fn is_grid_view(&self, default_grid: bool) -> bool {
        self.grid_override.unwrap_or(default_grid)
    }

    pub fn is_cell_expanded(&self, row: usize, col: usize) -> bool {
        self.expanded_cells.contains(&(row, col))
    }

    pub fn toggle_cell_expanded(&mut self, row: usize, col: usize) {
        if let Some(pos) = self.expanded_cells.iter().position(|&rc| rc == (row, col)) {
            self.expanded_cells.remove(pos);
        } else {
            self.expanded_cells.push((row, col));
        }
    }
}

/// One branch in the session tree.
pub struct Branch {
    pub id: BranchId,
    pub name: String,
    pub parent_branch: Option<BranchId>,
    pub parent_block: Option<BlockId>,
    /// Live persistent CoW engine state at the tip of this branch.
    pub state: EngineState<'static>,
    /// Ordered blocks visible on this branch (may share historical prefix `BlockId`s with parent).
    pub blocks: Vec<BlockId>,
}

/// Session tree store managing all branches and blocks.
pub struct BlockStore {
    branches: Vec<Branch>,
    blocks: Vec<Block>,
    active_branch: BranchId,
    next_block_id: u64,
    next_branch_id: u64,
}

impl BlockStore {
    pub fn new(initial_state: EngineState<'static>) -> Self {
        let main_id = BranchId(1);
        let main_branch = Branch {
            id: main_id,
            name: "main".to_owned(),
            parent_branch: None,
            parent_block: None,
            state: initial_state,
            blocks: Vec::new(),
        };
        Self {
            branches: vec![main_branch],
            blocks: Vec::new(),
            active_branch: main_id,
            next_block_id: 1,
            next_branch_id: 2,
        }
    }

    pub fn active_branch_id(&self) -> BranchId {
        self.active_branch
    }

    pub fn branches(&self) -> &[Branch] {
        &self.branches
    }

    pub fn active_branch(&self) -> &Branch {
        self.branches
            .iter()
            .find(|b| b.id == self.active_branch)
            .unwrap_or(&self.branches[0])
    }

    pub fn active_branch_mut(&mut self) -> &mut Branch {
        let id = self.active_branch;
        let idx = self.branches.iter().position(|b| b.id == id).unwrap_or(0);
        &mut self.branches[idx]
    }

    pub fn active_state(&self) -> &EngineState<'static> {
        &self.active_branch().state
    }

    pub fn set_active_state(&mut self, state: EngineState<'static>) {
        self.active_branch_mut().state = state;
    }

    /// Allocate the next [`BlockId`] and append `block` to the active branch.
    pub fn push_block(&mut self, mut block: Block) -> BlockId {
        let id = BlockId(self.next_block_id);
        self.next_block_id += 1;
        block.id = id;
        block.branch_id = self.active_branch;
        self.blocks.push(block);
        self.active_branch_mut().blocks.push(id);
        id
    }

    /// Helper to allocate a fresh `Block` with default fold state and push it.
    #[allow(clippy::too_many_arguments)]
    pub fn record_block(
        &mut self,
        source: String,
        prompt_snapshot: PromptInfo,
        state_before: EngineState<'static>,
        state_after: EngineState<'static>,
        started_ms: u64,
        duration_ms: u64,
        exit_code: i32,
        body: BlockBody,
    ) -> BlockId {
        let (folded, initial_sort) = match &body {
            BlockBody::Value(Value::Table(t))
            | BlockBody::StreamAndValue { value: Value::Table(t), .. }
            | BlockBody::RustCell { value: Value::Table(t), .. } => {
                // Default large tables to folded so layout can cap them at <= 1/3 pane height
                // (layout only folds when `nrows` actually exceeds the viewport-derived cap).
                (t.nrows() > 4, t.initial_sort)
            }
            BlockBody::Value(Value::List(l))
            | BlockBody::StreamAndValue { value: Value::List(l), .. }
            | BlockBody::RustCell { value: Value::List(l), .. } => {
                (l.len() > TABLE_FOLD_ROWS, None)
            }
            _ => (false, None),
        };
        let block = Block {
            id: BlockId(0),
            branch_id: self.active_branch,
            source,
            prompt_snapshot,
            state_before,
            state_after,
            started_ms,
            duration_ms,
            duration_ns: duration_ms.saturating_mul(1_000_000),
            exit_code,
            folded,
            grid_override: None,
            show_hidden_cols: false,
            table_scroll_y: 0.0,
            table_scroll_x: 0.0,
            table_sort: initial_sort,
            col_width_overrides: Vec::new(),
            expanded_cells: Vec::new(),
            body,
        };
        self.push_block(block)
    }

    /// Toggle compact grid vs detailed table view for a file-table block.
    pub fn toggle_table_view(&mut self, id: BlockId) -> bool {
        if let Some(blk) = self.block_mut(id) {
            let default_grid = match &blk.body {
                BlockBody::Value(Value::Table(t))
                | BlockBody::StreamAndValue { value: Value::Table(t), .. }
                | BlockBody::RustCell { value: Value::Table(t), .. } => t.default_grid,
                _ => false,
            };
            let cur = blk.grid_override.unwrap_or(default_grid);
            blk.grid_override = Some(!cur);
            true
        } else {
            false
        }
    }

    /// Toggle revealing default-hidden columns (e.g. `type` in `ls`) for a table block.
    pub fn toggle_hidden_cols(&mut self, id: BlockId) -> bool {
        if let Some(blk) = self.block_mut(id) {
            blk.show_hidden_cols = !blk.show_hidden_cols;
            true
        } else {
            false
        }
    }

    /// Set or update an interactive dragged width (clamped `48..=680` px) for column `col`.
    pub fn set_table_col_width(&mut self, id: BlockId, col: usize, width_px: f32) -> bool {
        let clamped = width_px.clamp(48.0, 680.0);
        if let Some(blk) = self.block_mut(id) {
            if let Some((_, w)) = blk.col_width_overrides.iter_mut().find(|(c, _)| *c == col) {
                if (*w - clamped).abs() < 0.5 {
                    return false;
                }
                *w = clamped;
            } else {
                blk.col_width_overrides.push((col, clamped));
            }
            true
        } else {
            false
        }
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }

    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.blocks.iter_mut().find(|b| b.id == id)
    }

    /// Ordered slice of blocks on the active branch.
    pub fn active_blocks(&self) -> Vec<&Block> {
        let ids = &self.active_branch().blocks;
        ids.iter().filter_map(|&id| self.block(id)).collect()
    }

    /// Switch the active branch to `branch_id`. Returns `true` if the branch exists.
    pub fn switch_branch(&mut self, branch_id: BranchId) -> bool {
        if self.branches.iter().any(|b| b.id == branch_id) {
            self.active_branch = branch_id;
            true
        } else {
            false
        }
    }

    /// Fork a new branch in O(1) from the exact historical state of `block_id`.
    ///
    /// Copies the block prefix up to and including `block_id` from the owning/active branch,
    /// forks `block.state_after` in O(1) (falling back to `state_before` if the block erred),
    /// and switches the active branch to the new fork.
    pub fn fork_from_block(&mut self, block_id: BlockId, label: Option<&str>) -> Option<BranchId> {
        let blk = self.block(block_id)?;
        let forked_state = if blk.exit_code == 0 {
            blk.state_after.fork()
        } else {
            blk.state_before.fork()
        };

        // Find the prefix of blocks containing `block_id` (prefer active branch, then owning branch).
        let source_branch = if self.active_branch().blocks.contains(&block_id) {
            self.active_branch()
        } else {
            self.branches
                .iter()
                .find(|b| b.blocks.contains(&block_id))
                .unwrap_or_else(|| self.active_branch())
        };
        let parent_branch = source_branch.id;
        let prefix: Vec<BlockId> = match source_branch.blocks.iter().position(|&id| id == block_id) {
            Some(idx) => source_branch.blocks[..=idx].to_vec(),
            None => vec![block_id],
        };

        let id = BranchId(self.next_branch_id);
        self.next_branch_id += 1;
        let name = match label {
            Some(l) if !l.trim().is_empty() => l.trim().to_owned(),
            _ => format!("fork-{}", id.0 - 1),
        };
        self.branches.push(Branch {
            id,
            name,
            parent_branch: Some(parent_branch),
            parent_block: Some(block_id),
            state: forked_state,
            blocks: prefix,
        });
        self.active_branch = id;
        Some(id)
    }

    /// Fork the tip of the currently active branch in O(1).
    pub fn fork_active_branch(&mut self, label: Option<&str>) -> BranchId {
        let parent_branch = self.active_branch;
        let cur = self.active_branch();
        let forked_state = cur.state.fork();
        let prefix = cur.blocks.clone();
        let parent_block = prefix.last().copied();

        let id = BranchId(self.next_branch_id);
        self.next_branch_id += 1;
        let name = match label {
            Some(l) if !l.trim().is_empty() => l.trim().to_owned(),
            _ => format!("fork-{}", id.0 - 1),
        };
        self.branches.push(Branch {
            id,
            name,
            parent_branch: Some(parent_branch),
            parent_block,
            state: forked_state,
            blocks: prefix,
        });
        self.active_branch = id;
        id
    }

    pub fn toggle_fold(&mut self, block_id: BlockId) -> bool {
        if let Some(b) = self.block_mut(block_id) {
            b.folded = !b.folded;
            b.table_scroll_y = 0.0;
            true
        } else {
            false
        }
    }

    /// Toggle interactive column sort on a table block (`size`/`date`/numeric start descending;
    /// strings start ascending; clicking the same column again flips direction).
    pub fn toggle_table_sort(&mut self, block_id: BlockId, col: usize) -> bool {
        let Some(blk) = self.block_mut(block_id) else {
            return false;
        };
        let desc_first = match &blk.body {
            BlockBody::Value(Value::Table(t)) => matches!(
                t.col_display.get(col).copied(),
                Some(blaze_sh::ColDisplay::FileSize | blaze_sh::ColDisplay::HumanTime)
            ) || matches!(
                t.cell(0, col),
                Some(
                    Value::Int(_)
                        | Value::Float(_)
                        | Value::Size(_)
                        | Value::Duration(_)
                        | Value::Date(_)
                )
            ),
            _ => false,
        };
        blk.table_sort = match blk.table_sort {
            Some((c, desc)) if c == col => Some((col, !desc)),
            _ => Some((col, desc_first)),
        };
        blk.table_scroll_y = 0.0;
        true
    }

    /// Scroll a table block vertically/horizontally within its clamped bounds; returns `true` if
    /// the scroll offset actually moved (so trackpad scroll chains cleanly to the main feed at edges).
    pub fn scroll_table(
        &mut self,
        block_id: BlockId,
        dy: f32,
        dx: f32,
        max_y: f32,
        max_x: f32,
    ) -> bool {
        let Some(blk) = self.block_mut(block_id) else {
            return false;
        };
        let prev_y = blk.table_scroll_y;
        let prev_x = blk.table_scroll_x;
        if max_y > 0.0 {
            blk.table_scroll_y = (blk.table_scroll_y - dy).clamp(0.0, max_y);
        } else {
            blk.table_scroll_y = 0.0;
        }
        if max_x > 0.0 {
            blk.table_scroll_x = (blk.table_scroll_x - dx).clamp(0.0, max_x);
        } else {
            blk.table_scroll_x = 0.0;
        }
        (blk.table_scroll_y - prev_y).abs() > 0.05 || (blk.table_scroll_x - prev_x).abs() > 0.05
    }

    pub fn toggle_expand_cell(&mut self, block_id: BlockId, row: usize, col: usize) -> bool {
        if let Some(b) = self.block_mut(block_id) {
            b.toggle_cell_expanded(row, col);
            true
        } else {
            false
        }
    }

    /// Clear all non-pinned blocks on the active branch while preserving its live `EngineState`
    /// and keeping live pinned blocks (running inline terminals / alternate-screen TUIs, live
    /// `top` monitors, and in-flight async commands) intact.
    pub fn clear_active_branch(&mut self) {
        let pinned: Vec<BlockId> = self
            .active_branch()
            .blocks
            .iter()
            .copied()
            .filter(|&id| self.block(id).is_some_and(Block::is_pinned_live))
            .collect();
        self.active_branch_mut().blocks = pinned;
    }

    /// Return the `BlockId` of the currently running interactive terminal block on the active branch.
    pub fn active_running_terminal(&self) -> Option<BlockId> {
        let ids = &self.active_branch().blocks;
        for &id in ids.iter().rev() {
            if let Some(b) = self.block(id)
                && matches!(b.body, BlockBody::Terminal { running: true, .. })
            {
                return Some(id);
            }
        }
        None
    }

    /// True if the active running terminal has switched to the alternate screen (vim, htop, less).
    pub fn is_alternate_screen(&self) -> bool {
        let Some(id) = self.active_running_terminal() else {
            return false;
        };
        if let Some(Block { body: BlockBody::Terminal { term, running: true, .. }, .. }) =
            self.block(id)
        {
            term.active() == Which::Alternate
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_own::Alloc;
    use blaze_vt::terminal::{Opts as VtOpts, Terminal};

    #[test]
    fn clear_active_branch_preserves_pinned_live_blocks() {
        let alloc = Alloc::global();
        let state = EngineState::new("/tmp", &[], alloc);
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 0);
        let mut store = BlockStore::new(state.fork());

        let done_id = store.record_block(
            "echo hi".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            0,
            1,
            0,
            BlockBody::Value(Value::Int(42)),
        );
        let exited_pty = store.record_block(
            "vim old".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            0,
            2,
            0,
            BlockBody::Terminal {
                term: Terminal::new_in(VtOpts::new(80, 24, 1 << 16), alloc).unwrap(),
                running: false,
                exit_code: Some(0),
                pid: None,
            },
        );
        let live_pty = store.record_block(
            "claude".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            0,
            0,
            0,
            BlockBody::Terminal {
                term: Terminal::new_in(VtOpts::new(80, 24, 1 << 16), alloc).unwrap(),
                running: true,
                exit_code: None,
                pid: Some(4242),
            },
        );
        let live_job = store.record_block(
            "sleep 10".to_owned(),
            prompt,
            state.fork(),
            state,
            0,
            0,
            0,
            BlockBody::Running {
                label: "sleep 10".to_owned(),
                pid: Arc::new(AtomicU32::new(4243)),
            },
        );
        assert_eq!(store.active_branch().blocks, vec![done_id, exited_pty, live_pty, live_job]);

        store.clear_active_branch();
        assert_eq!(store.active_branch().blocks, vec![live_pty, live_job]);
    }
}
