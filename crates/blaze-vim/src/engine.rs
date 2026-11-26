//! Sans-IO Vim modal editing state machine ([`VimMachine`]).
//!
//! Operates on any [`TextBuffer`] and any [`TextObjectResolver`] with explicit allocator,
//! bounded undo/redo stacks, bounded counts, unnamed register + paste, Visual / VisualLine
//! modes, and `.` dot-repeat.

use blaze_own::{AString, AVec, Alloc, String as OwnString, Vec as OwnVec};

use crate::buffer::{
    TextBuffer, char_at, clamp_boundary, clamp_normal_cursor, first_non_blank, line_bounds,
    line_bounds_with_newline, line_col, motion_match_bracket, motion_paragraph,
    motion_word_backward, motion_word_end, motion_word_end_backward, motion_word_forward,
    move_vertical, next_char, normal_line_last_char, prev_char,
};
use crate::objects::{ObjectScope, TextObjectResolver};
use crate::{
    CursorShape, FindKind, KeyCode, KeyEvent, Mode, MotionKind, Operator, Span,
};

/// Options for [`VimMachine::new`] (rule `options-struct`, rule `bounded-progress`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VimOpts {
    pub initial_mode: Mode,
    pub max_undo: usize,
    pub max_count: usize,
}

impl Default for VimOpts {
    fn default() -> Self {
        Self { initial_mode: Mode::Normal, max_undo: 256, max_count: 10_000 }
    }
}

/// Yanked / deleted text stored in the unnamed Vim register (`"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register<'a> {
    pub text: AString<'a>,
    pub kind: MotionKind,
}

/// High-level outcome of feeding one [`KeyEvent`] into [`VimMachine::feed`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Key was consumed by the Vim engine (buffer or cursor/mode may have changed).
    Handled { mutated: bool, yanked: bool },
    /// Key was NOT handled by Vim (e.g. `Enter`/`Tab`/navigation in Insert mode) so the host
    /// should apply its own behavior (submit command, trigger completion, etc.).
    PassThrough,
    /// User pressed `Enter` in Normal mode (host may submit line or move down).
    NormalEnter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UndoSnap<'a> {
    text: AString<'a>,
    cursor: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InsertEditGroup {
    None,
    Typing,
    Deleting,
}

/// Pending multi-key Normal/Visual sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pending {
    None,
    /// Saw `g` (waiting for `g`, `e`, `E`, `u`, `U`, `~`).
    PrefixG { op: Option<Operator> },
    /// Saw an operator (`d`, `c`, `y`, `>`, `<`, `gu`, `gU`, `g~`) waiting for motion/object.
    Op(Operator),
    /// Saw `i` or `a` after an operator (or in Visual mode) waiting for text-object key.
    Obj { op: Option<Operator>, scope: ObjectScope },
    /// Saw `r` waiting for replacement char.
    ReplaceChar,
    /// Saw `f`/`F`/`t`/`T` waiting for target char.
    Find { op: Option<Operator>, kind: FindKind },
}

/// Recorded last mutating action for `.` (dot repeat).
#[derive(Clone, Debug)]
struct DotRecord<'a> {
    keys: AVec<'a, KeyEvent>,
}

/// Complete sans-IO Vim state machine.
pub struct VimMachine<'a> {
    alloc: Alloc<'a>,
    opts: VimOpts,
    mode: Mode,
    cursor: usize,
    visual_anchor: Option<usize>,
    desired_col: Option<usize>,
    count_buf: usize,
    pending: Pending,
    last_find: Option<(FindKind, char)>,
    register: Register<'a>,
    undo_stack: AVec<'a, UndoSnap<'a>>,
    redo_stack: AVec<'a, UndoSnap<'a>>,
    insert_group: InsertEditGroup,
    /// Keys recorded during the current mutating command (including any Insert session).
    recording_keys: AVec<'a, KeyEvent>,
    is_recording: bool,
    last_dot: Option<DotRecord<'a>>,
    replaying_dot: bool,
}

impl<'a> VimMachine<'a> {
    pub fn new(opts: VimOpts, alloc: Alloc<'a>) -> Self {
        Self {
            alloc,
            opts: VimOpts {
                initial_mode: opts.initial_mode,
                max_undo: opts.max_undo.max(1),
                max_count: opts.max_count.max(1),
            },
            mode: opts.initial_mode,
            cursor: 0,
            visual_anchor: None,
            desired_col: None,
            count_buf: 0,
            pending: Pending::None,
            last_find: None,
            register: Register {
                text: OwnString::new_in(alloc),
                kind: MotionKind::Charwise,
            },
            undo_stack: OwnVec::new_in(alloc),
            redo_stack: OwnVec::new_in(alloc),
            insert_group: InsertEditGroup::None,
            recording_keys: OwnVec::new_in(alloc),
            is_recording: false,
            last_dot: None,
            replaying_dot: false,
        }
    }

    #[inline]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[inline]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[inline]
    pub fn register(&self) -> &Register<'a> {
        &self.register
    }

    /// Recommended visual cursor geometry for the current mode and pending state.
    pub fn cursor_shape(&self) -> CursorShape {
        match self.mode {
            Mode::Insert => CursorShape::Beam,
            Mode::Normal => match self.pending {
                Pending::Op(_) | Pending::Obj { .. } | Pending::ReplaceChar | Pending::Find { .. } => {
                    CursorShape::Underline
                }
                Pending::None | Pending::PrefixG { .. } => CursorShape::Block,
            },
            Mode::Visual | Mode::VisualLine => CursorShape::Block,
        }
    }

    /// Format any pending count/operator keystrokes (e.g. `"2d"`, `"ci"`, `"f"`) for a status badge.
    pub fn format_pending(&self, out: &mut impl core::fmt::Write) -> core::fmt::Result {
        if self.count_buf > 0 {
            write!(out, "{}", self.count_buf)?;
        }
        match self.pending {
            Pending::None => Ok(()),
            Pending::PrefixG { op: None } => out.write_str("g"),
            Pending::PrefixG { op: Some(op) } => {
                out.write_str(op.label())?;
                out.write_str("g")
            }
            Pending::Op(op) => out.write_str(op.label()),
            Pending::Obj { op, scope } => {
                if let Some(op) = op {
                    out.write_str(op.label())?;
                }
                out.write_str(if scope == ObjectScope::Inner { "i" } else { "a" })
            }
            Pending::ReplaceChar => out.write_str("r"),
            Pending::Find { op, kind } => {
                if let Some(op) = op {
                    out.write_str(op.label())?;
                }
                out.write_str(match kind {
                    FindKind::Forward => "f",
                    FindKind::Backward => "F",
                    FindKind::TillForward => "t",
                    FindKind::TillBackward => "T",
                })
            }
        }
    }

    /// Active visual selection byte range `[start, end)` (inclusive of the character under cursor,
    /// or full line spans in `VisualLine`), if currently in a visual mode.
    pub fn visual_selection(&self, text: &str) -> Option<Span> {
        let anchor = self.visual_anchor?;
        if !matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return None;
        }
        let a = clamp_boundary(text, anchor);
        let c = clamp_boundary(text, self.cursor);
        let (lo, hi) = if a <= c { (a, c) } else { (c, a) };
        match self.mode {
            Mode::Visual => {
                let end = if hi < text.len() { next_char(text, hi) } else { hi };
                Some(Span::new(lo, end))
            }
            Mode::VisualLine => {
                let s = line_bounds(text, lo).start;
                let e = line_bounds(text, hi).end;
                Some(Span::new(s, e))
            }
            _ => None,
        }
    }

    /// Synchronize cursor offset after an external host edit (paste, drop, completion, click).
    pub fn set_cursor(&mut self, text: &str, cursor: usize) {
        self.cursor = match self.mode {
            Mode::Insert => clamp_boundary(text, cursor),
            _ => clamp_normal_cursor(text, cursor),
        };
        self.desired_col = None;
    }

    /// Switch mode explicitly (e.g. entering Insert on new prompt or Reset).
    pub fn set_mode(&mut self, text: &str, mode: Mode) {
        self.mode = mode;
        self.pending = Pending::None;
        self.count_buf = 0;
        self.insert_group = InsertEditGroup::None;
        if matches!(mode, Mode::Visual | Mode::VisualLine) {
            self.visual_anchor = Some(clamp_normal_cursor(text, self.cursor));
        } else {
            self.visual_anchor = None;
        }
        if mode == Mode::Normal {
            self.cursor = clamp_normal_cursor(text, self.cursor);
        } else {
            self.cursor = clamp_boundary(text, self.cursor);
        }
    }

    /// Reset transient state for a brand new prompt line (clears undo/redo & pending keys).
    pub fn reset_for_new_buffer(&mut self) {
        self.mode = self.opts.initial_mode;
        self.cursor = 0;
        self.visual_anchor = None;
        self.desired_col = None;
        self.count_buf = 0;
        self.pending = Pending::None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.insert_group = InsertEditGroup::None;
        self.is_recording = false;
        self.recording_keys.clear();
    }

    /// Record an undo snapshot before an external host mutation (paste, drag-drop, completion,
    /// abbreviation expansion, kill-ring) so `u` and `Cmd+Z` undo it seamlessly.
    pub fn checkpoint_undo(&mut self, text: &str) {
        self.insert_group = InsertEditGroup::None;
        self.push_undo_raw(text);
    }

    /// Coalesced undo checkpoint for single-char typing/backspace in Insert mode.
    fn checkpoint_insert_op(&mut self, text: &str, group: InsertEditGroup) {
        if group == InsertEditGroup::None || group != self.insert_group || self.undo_stack.is_empty()
        {
            self.push_undo_raw(text);
        }
        self.insert_group = group;
    }

    fn push_undo_raw(&mut self, text: &str) {
        if let Some(last) = self.undo_stack.last()
            && last.text.as_str() == text
            && last.cursor == self.cursor
        {
            self.redo_stack.clear();
            return;
        }
        if self.undo_stack.len() >= self.opts.max_undo {
            self.undo_stack.remove_shift(0);
        }
        self.undo_stack.push(UndoSnap {
            text: OwnString::from_str_in(text, self.alloc),
            cursor: self.cursor,
        });
        self.redo_stack.clear();
    }

    /// Undo the last edit (`u` in Normal mode or `Cmd+Z` in any mode).
    pub fn undo<B: TextBuffer>(&mut self, buf: &mut B) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        let cur_snap = UndoSnap {
            text: OwnString::from_str_in(buf.text(), self.alloc),
            cursor: self.cursor,
        };
        if self.redo_stack.len() >= self.opts.max_undo {
            self.redo_stack.remove_shift(0);
        }
        self.redo_stack.push(cur_snap);
        let old_len = buf.text().len();
        buf.replace_range(0, old_len, prev.text.as_str());
        self.cursor = if self.mode == Mode::Insert {
            clamp_boundary(buf.text(), prev.cursor)
        } else {
            clamp_normal_cursor(buf.text(), prev.cursor)
        };
        self.visual_anchor = None;
        self.pending = Pending::None;
        self.count_buf = 0;
        self.insert_group = InsertEditGroup::None;
        true
    }

    /// Redo the last undone edit (`Ctrl+R` in Normal mode or `Cmd+Shift+Z` in any mode).
    pub fn redo<B: TextBuffer>(&mut self, buf: &mut B) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        let cur_snap = UndoSnap {
            text: OwnString::from_str_in(buf.text(), self.alloc),
            cursor: self.cursor,
        };
        if self.undo_stack.len() >= self.opts.max_undo {
            self.undo_stack.remove_shift(0);
        }
        self.undo_stack.push(cur_snap);
        let old_len = buf.text().len();
        buf.replace_range(0, old_len, next.text.as_str());
        self.cursor = if self.mode == Mode::Insert {
            clamp_boundary(buf.text(), next.cursor)
        } else {
            clamp_normal_cursor(buf.text(), next.cursor)
        };
        self.visual_anchor = None;
        self.pending = Pending::None;
        self.count_buf = 0;
        self.insert_group = InsertEditGroup::None;
        true
    }

    fn begin_recording(&mut self, first: KeyEvent) {
        if self.replaying_dot {
            return;
        }
        self.recording_keys.clear();
        self.recording_keys.push(first);
        self.is_recording = true;
    }

    fn push_recorded(&mut self, ev: KeyEvent) {
        if self.is_recording && !self.replaying_dot && self.recording_keys.len() < 512 {
            self.recording_keys.push(ev);
        }
    }

    fn finish_recording(&mut self) {
        if self.is_recording && !self.replaying_dot && !self.recording_keys.is_empty() {
            let mut saved = OwnVec::with_capacity_in(self.recording_keys.len(), self.alloc);
            for k in self.recording_keys.iter() {
                saved.push(*k);
            }
            self.last_dot = Some(DotRecord { keys: saved });
        }
        self.is_recording = false;
        self.recording_keys.clear();
    }

    fn cancel_recording(&mut self) {
        self.is_recording = false;
        self.recording_keys.clear();
    }

    fn take_count(&mut self) -> usize {
        let c = if self.count_buf == 0 { 1 } else { self.count_buf };
        self.count_buf = 0;
        c
    }

    fn push_count_digit(&mut self, digit: usize) {
        let next = self.count_buf.saturating_mul(10).saturating_add(digit);
        self.count_buf = next.min(self.opts.max_count);
    }

    /// Feed one normalized [`KeyEvent`] into the Vim state machine.
    pub fn feed<B: TextBuffer, O: TextObjectResolver>(
        &mut self,
        key: KeyEvent,
        buf: &mut B,
        objects: &O,
    ) -> Outcome {
        self.cursor = clamp_boundary(buf.text(), self.cursor);
        match self.mode {
            Mode::Insert => self.feed_insert(key, buf),
            Mode::Normal => self.feed_normal(key, buf, objects),
            Mode::Visual | Mode::VisualLine => self.feed_visual(key, buf, objects),
        }
    }

    // =========================================================================
    // Insert Mode
    // =========================================================================

    fn feed_insert<B: TextBuffer>(&mut self, key: KeyEvent, buf: &mut B) -> Outcome {
        if key.code == KeyCode::Escape || (key.mods.ctrl && key.code == KeyCode::Char('[')) {
            if self.is_recording {
                self.push_recorded(key);
                self.finish_recording();
            }
            self.mode = Mode::Normal;
            self.insert_group = InsertEditGroup::None;
            // Standard Vim: leaving Insert steps back one char unless at column 0.
            let lb = line_bounds(buf.text(), self.cursor);
            if self.cursor > lb.start {
                self.cursor = prev_char(buf.text(), self.cursor);
            }
            self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // During `.` dot-replay, replay typed insert characters directly into `buf`.
        if self.replaying_dot {
            match key.code {
                KeyCode::Char(ch) if !key.mods.ctrl && !key.mods.alt => {
                    let mut tmp = [0u8; 4];
                    let s = ch.encode_utf8(&mut tmp);
                    self.checkpoint_insert_op(buf.text(), InsertEditGroup::Typing);
                    buf.replace_range(self.cursor, self.cursor, s);
                    self.cursor += s.len();
                    return Outcome::Handled { mutated: true, yanked: false };
                }
                KeyCode::Enter => {
                    self.checkpoint_insert_op(buf.text(), InsertEditGroup::None);
                    buf.replace_range(self.cursor, self.cursor, "\n");
                    self.cursor += 1;
                    return Outcome::Handled { mutated: true, yanked: false };
                }
                KeyCode::Backspace => {
                    if self.cursor > 0 {
                        self.checkpoint_insert_op(buf.text(), InsertEditGroup::Deleting);
                        let p = prev_char(buf.text(), self.cursor);
                        buf.replace_range(p, self.cursor, "");
                        self.cursor = p;
                    }
                    return Outcome::Handled { mutated: true, yanked: false };
                }
                _ => return Outcome::Handled { mutated: false, yanked: false },
            }
        }

        // Record insert-mode character/backspace edits for `.` dot-repeat while letting
        // the host handle full shell insert ergonomics (abbreviations, history, completion).
        if self.is_recording {
            match key.code {
                KeyCode::Char(_) if !key.mods.ctrl && !key.mods.alt => self.push_recorded(key),
                KeyCode::Backspace => self.push_recorded(key),
                _ => {}
            }
        }
        Outcome::PassThrough
    }

    // =========================================================================
    // Normal Mode
    // =========================================================================

    fn feed_normal<B: TextBuffer, O: TextObjectResolver>(
        &mut self,
        key: KeyEvent,
        buf: &mut B,
        objects: &O,
    ) -> Outcome {
        if key.code == KeyCode::Escape {
            self.pending = Pending::None;
            self.count_buf = 0;
            self.cancel_recording();
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // 1. Complete pending `r<ch>` single-char replacement.
        if self.pending == Pending::ReplaceChar {
            self.pending = Pending::None;
            let count = self.take_count();
            if let KeyCode::Char(ch) = key.code
                && !key.mods.ctrl
                && !key.mods.alt
            {
                self.push_recorded(key);
                let text = buf.text();
                let lb = line_bounds(text, self.cursor);
                if self.cursor < lb.end {
                    let mut end = self.cursor;
                    let mut chars_replaced = 0usize;
                    while chars_replaced < count && end < lb.end {
                        end = next_char(text, end);
                        chars_replaced += 1;
                    }
                    if chars_replaced > 0 {
                        self.push_undo_raw(text);
                        let mut rep = OwnString::with_capacity_in(chars_replaced * ch.len_utf8(), self.alloc);
                        for _ in 0..chars_replaced {
                            rep.push(ch);
                        }
                        buf.replace_range(self.cursor, end, rep.as_str());
                        self.cursor = prev_char(buf.text(), self.cursor + rep.len());
                        self.finish_recording();
                        return Outcome::Handled { mutated: true, yanked: false };
                    }
                }
            }
            self.cancel_recording();
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // 2. Complete pending `f`/`F`/`t`/`T` character search.
        if let Pending::Find { op, kind } = self.pending {
            self.pending = Pending::None;
            let count = self.take_count();
            if let KeyCode::Char(target) = key.code
                && !key.mods.ctrl
                && !key.mods.alt
            {
                self.last_find = Some((kind, target));
                if let Some(op) = op {
                    self.push_recorded(key);
                    return self.apply_find_operator(op, kind, target, count, buf);
                }
                if let Some(dest) = find_char_on_line(buf.text(), self.cursor, kind, target, count)
                {
                    self.cursor = dest;
                    self.desired_col = None;
                }
            } else {
                self.cancel_recording();
            }
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // 3. Complete pending text object (`i<obj>` / `a<obj>`).
        if let Pending::Obj { op, scope } = self.pending {
            self.pending = Pending::None;
            let count = self.take_count();
            if let KeyCode::Char(obj_key) = key.code
                && !key.mods.ctrl
                && !key.mods.alt
            {
                if let Some(op) = op {
                    self.push_recorded(key);
                    if let Some((span, kind)) =
                        objects.resolve(buf.text(), self.cursor, scope, obj_key, count)
                    {
                        return self.apply_operator(op, span, kind, buf);
                    }
                }
            }
            self.cancel_recording();
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // 4. Complete pending `g` prefix (`gg`, `ge`, `gE`, `gu`, `gU`, `g~`).
        if let Pending::PrefixG { op } = self.pending {
            self.pending = Pending::None;
            if let KeyCode::Char(ch) = key.code
                && !key.mods.ctrl
            {
                match ch {
                    'g' => {
                        let count = self.take_count();
                        let dest = if count <= 1 {
                            first_non_blank(buf.text(), 0)
                        } else {
                            goto_line(buf.text(), count)
                        };
                        if let Some(op) = op {
                            self.push_recorded(key);
                            let span = linewise_span(buf.text(), self.cursor, dest);
                            return self.apply_operator(op, span, MotionKind::Linewise, buf);
                        }
                        self.cursor = dest;
                        self.desired_col = None;
                        return Outcome::Handled { mutated: false, yanked: false };
                    }
                    'e' | 'E' => {
                        let count = self.take_count();
                        let big = ch == 'E';
                        let mut dest = self.cursor;
                        for _ in 0..count {
                            dest = motion_word_end_backward(buf.text(), dest, big);
                        }
                        if let Some(op) = op {
                            self.push_recorded(key);
                            let end = next_char(buf.text(), self.cursor).min(buf.text().len());
                            return self.apply_operator(
                                op,
                                Span::new(dest, end),
                                MotionKind::Charwise,
                                buf,
                            );
                        }
                        self.cursor = clamp_normal_cursor(buf.text(), dest);
                        self.desired_col = None;
                        return Outcome::Handled { mutated: false, yanked: false };
                    }
                    'u' if op.is_none() => {
                        self.begin_recording(KeyEvent::char('g'));
                        self.push_recorded(key);
                        self.pending = Pending::Op(Operator::Lowercase);
                        return Outcome::Handled { mutated: false, yanked: false };
                    }
                    'U' if op.is_none() => {
                        self.begin_recording(KeyEvent::char('g'));
                        self.push_recorded(key);
                        self.pending = Pending::Op(Operator::Uppercase);
                        return Outcome::Handled { mutated: false, yanked: false };
                    }
                    '~' if op.is_none() => {
                        self.begin_recording(KeyEvent::char('g'));
                        self.push_recorded(key);
                        self.pending = Pending::Op(Operator::ToggleCase);
                        return Outcome::Handled { mutated: false, yanked: false };
                    }
                    _ => {}
                }
            }
            self.count_buf = 0;
            self.cancel_recording();
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // 5. Operator-pending state (`d`, `c`, `y`, `>`, `<`, `gu`, `gU`, `g~`).
        if let Pending::Op(op) = self.pending {
            if let KeyCode::Char(ch) = key.code
                && !key.mods.ctrl
            {
                // Count digits after operator (`d2w`, `c3l`).
                if ch.is_ascii_digit() && (ch != '0' || self.count_buf > 0) {
                    self.push_recorded(key);
                    self.push_count_digit((ch as u8 - b'0') as usize);
                    return Outcome::Handled { mutated: false, yanked: false };
                }
                // Text object prefix (`diw`, `ca"`, `di|`).
                if ch == 'i' || ch == 'a' {
                    self.push_recorded(key);
                    self.pending = Pending::Obj {
                        op: Some(op),
                        scope: if ch == 'i' { ObjectScope::Inner } else { ObjectScope::Around },
                    };
                    return Outcome::Handled { mutated: false, yanked: false };
                }
                // Doubled linewise operators (`dd`, `cc`, `yy`, `>>`, `<<`, `guu`, `gUU`).
                let is_doubled = matches!(
                    (op, ch),
                    (Operator::Delete, 'd')
                        | (Operator::Change, 'c')
                        | (Operator::Yank, 'y')
                        | (Operator::Indent, '>')
                        | (Operator::Dedent, '<')
                        | (Operator::Lowercase, 'u')
                        | (Operator::Uppercase, 'U')
                        | (Operator::ToggleCase, '~')
                );
                if is_doubled {
                    self.pending = Pending::None;
                    self.push_recorded(key);
                    let count = self.take_count();
                    let span = multi_line_span(buf.text(), self.cursor, count);
                    return self.apply_operator(op, span, MotionKind::Linewise, buf);
                }
                if ch == 'g' {
                    self.push_recorded(key);
                    self.pending = Pending::PrefixG { op: Some(op) };
                    return Outcome::Handled { mutated: false, yanked: false };
                }
                if let Some(fk) = find_kind_for(ch) {
                    self.push_recorded(key);
                    self.pending = Pending::Find { op: Some(op), kind: fk };
                    return Outcome::Handled { mutated: false, yanked: false };
                }
                // Resolve standard motion for this operator.
                self.pending = Pending::None;
                let count = self.take_count();
                if let Some((span, kind)) =
                    self.resolve_operator_motion(buf.text(), op, ch, count)
                {
                    self.push_recorded(key);
                    return self.apply_operator(op, span, kind, buf);
                }
            }
            self.pending = Pending::None;
            self.count_buf = 0;
            self.cancel_recording();
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // 6. Fresh Normal-mode keystroke.
        if key.mods.ctrl {
            match key.code {
                KeyCode::Char('r') => {
                    let count = self.take_count();
                    let mut did = false;
                    for _ in 0..count {
                        if self.redo(buf) {
                            did = true;
                        }
                    }
                    return Outcome::Handled { mutated: did, yanked: false };
                }
                _ => return Outcome::PassThrough,
            }
        }

        match key.code {
            KeyCode::Enter => {
                self.count_buf = 0;
                Outcome::NormalEnter
            }
            KeyCode::Left => {
                let count = self.take_count();
                self.move_horiz(buf.text(), -(count as isize));
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::Right => {
                let count = self.take_count();
                self.move_horiz(buf.text(), count as isize);
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::Up => {
                let count = self.take_count();
                self.move_vert(buf.text(), -(count as isize));
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::Down => {
                let count = self.take_count();
                self.move_vert(buf.text(), count as isize);
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::Home => {
                self.count_buf = 0;
                self.cursor = line_bounds(buf.text(), self.cursor).start;
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::End => {
                self.count_buf = 0;
                self.cursor = normal_line_last_char(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::Backspace => {
                let count = self.take_count();
                for _ in 0..count {
                    self.cursor = prev_char(buf.text(), self.cursor);
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            KeyCode::Delete => {
                self.begin_recording(KeyEvent::char('x'));
                let count = self.take_count();
                return self.delete_chars_under_cursor(buf, count, false);
            }
            KeyCode::Char(ch) => self.feed_normal_char(ch, key, buf, objects),
            KeyCode::Tab | KeyCode::Escape => Outcome::Handled { mutated: false, yanked: false },
        }
    }

    fn feed_normal_char<B: TextBuffer, O: TextObjectResolver>(
        &mut self,
        ch: char,
        key: KeyEvent,
        buf: &mut B,
        objects: &O,
    ) -> Outcome {
        // Count digits (`1..=9`, or `0` when count already started).
        if ch.is_ascii_digit() && (ch != '0' || self.count_buf > 0) {
            self.push_count_digit((ch as u8 - b'0') as usize);
            return Outcome::Handled { mutated: false, yanked: false };
        }

        match ch {
            // --- Basic & word motions ---
            'h' => {
                let n = self.take_count();
                self.move_horiz(buf.text(), -(n as isize));
                Outcome::Handled { mutated: false, yanked: false }
            }
            'l' | ' ' => {
                let n = self.take_count();
                self.move_horiz(buf.text(), n as isize);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'j' => {
                let n = self.take_count();
                self.move_vert(buf.text(), n as isize);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'k' => {
                let n = self.take_count();
                self.move_vert(buf.text(), -(n as isize));
                Outcome::Handled { mutated: false, yanked: false }
            }
            '0' => {
                self.count_buf = 0;
                self.cursor = line_bounds(buf.text(), self.cursor).start;
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            '^' | '_' => {
                self.count_buf = 0;
                self.cursor = first_non_blank(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            '$' => {
                self.count_buf = 0;
                self.cursor = normal_line_last_char(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'w' | 'W' => {
                let n = self.take_count();
                let big = ch == 'W';
                for _ in 0..n {
                    self.cursor = motion_word_forward(buf.text(), self.cursor, big);
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'b' | 'B' => {
                let n = self.take_count();
                let big = ch == 'B';
                for _ in 0..n {
                    self.cursor = motion_word_backward(buf.text(), self.cursor, big);
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'e' | 'E' => {
                let n = self.take_count();
                let big = ch == 'E';
                for _ in 0..n {
                    self.cursor = motion_word_end(buf.text(), self.cursor, big);
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            '%' => {
                self.count_buf = 0;
                if let Some(dest) = motion_match_bracket(buf.text(), self.cursor) {
                    self.cursor = dest;
                    self.desired_col = None;
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            '{' | '}' => {
                let n = self.take_count();
                let fwd = ch == '}';
                for _ in 0..n {
                    self.cursor = motion_paragraph(buf.text(), self.cursor, fwd);
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'G' => {
                let c = self.count_buf;
                self.count_buf = 0;
                self.cursor = if c == 0 {
                    let last_lb = line_bounds(buf.text(), buf.text().len());
                    first_non_blank(buf.text(), last_lb.start)
                } else {
                    goto_line(buf.text(), c)
                };
                self.desired_col = None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'g' => {
                self.pending = Pending::PrefixG { op: None };
                Outcome::Handled { mutated: false, yanked: false }
            }
            'f' | 'F' | 't' | 'T' => {
                self.pending = Pending::Find { op: None, kind: find_kind_for(ch).unwrap() };
                Outcome::Handled { mutated: false, yanked: false }
            }
            ';' | ',' => {
                let n = self.take_count();
                if let Some((kind, target)) = self.last_find {
                    let eff = if ch == ',' { kind.invert() } else { kind };
                    if let Some(dest) = find_char_on_line(buf.text(), self.cursor, eff, target, n) {
                        self.cursor = dest;
                        self.desired_col = None;
                    }
                }
                Outcome::Handled { mutated: false, yanked: false }
            }

            // --- Insert mode entry ---
            'i' => {
                self.count_buf = 0;
                self.begin_recording(key);
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'I' => {
                self.count_buf = 0;
                self.begin_recording(key);
                self.cursor = first_non_blank(buf.text(), self.cursor);
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'a' => {
                self.count_buf = 0;
                self.begin_recording(key);
                let lb = line_bounds(buf.text(), self.cursor);
                if self.cursor < lb.end {
                    self.cursor = next_char(buf.text(), self.cursor);
                }
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'A' => {
                self.count_buf = 0;
                self.begin_recording(key);
                self.cursor = line_bounds(buf.text(), self.cursor).end;
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'o' => {
                self.count_buf = 0;
                self.begin_recording(key);
                self.push_undo_raw(buf.text());
                let end = line_bounds(buf.text(), self.cursor).end;
                buf.replace_range(end, end, "\n");
                self.cursor = end + 1;
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                Outcome::Handled { mutated: true, yanked: false }
            }
            'O' => {
                self.count_buf = 0;
                self.begin_recording(key);
                self.push_undo_raw(buf.text());
                let start = line_bounds(buf.text(), self.cursor).start;
                buf.replace_range(start, start, "\n");
                self.cursor = start;
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                Outcome::Handled { mutated: true, yanked: false }
            }

            // --- Operators ---
            'd' => {
                self.begin_recording_with_count(key);
                self.pending = Pending::Op(Operator::Delete);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'c' => {
                self.begin_recording_with_count(key);
                self.pending = Pending::Op(Operator::Change);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'y' => {
                self.pending = Pending::Op(Operator::Yank);
                Outcome::Handled { mutated: false, yanked: false }
            }
            '>' => {
                self.begin_recording_with_count(key);
                self.pending = Pending::Op(Operator::Indent);
                Outcome::Handled { mutated: false, yanked: false }
            }
            '<' => {
                self.begin_recording_with_count(key);
                self.pending = Pending::Op(Operator::Dedent);
                Outcome::Handled { mutated: false, yanked: false }
            }

            // --- Shorthand edits ---
            'x' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                self.delete_chars_under_cursor(buf, n, false)
            }
            'X' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                self.delete_chars_under_cursor(buf, n, true)
            }
            's' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                let lb = line_bounds(buf.text(), self.cursor);
                let mut end = self.cursor;
                for _ in 0..n {
                    if end >= lb.end {
                        break;
                    }
                    end = next_char(buf.text(), end);
                }
                self.apply_operator(Operator::Change, Span::new(self.cursor, end), MotionKind::Charwise, buf)
            }
            'S' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                let span = multi_line_span(buf.text(), self.cursor, n);
                self.apply_operator(Operator::Change, span, MotionKind::Linewise, buf)
            }
            'D' => {
                self.begin_recording_with_count(key);
                self.count_buf = 0;
                let end = line_bounds(buf.text(), self.cursor).end;
                self.apply_operator(Operator::Delete, Span::new(self.cursor, end), MotionKind::Charwise, buf)
            }
            'C' => {
                self.begin_recording_with_count(key);
                self.count_buf = 0;
                let end = line_bounds(buf.text(), self.cursor).end;
                self.apply_operator(Operator::Change, Span::new(self.cursor, end), MotionKind::Charwise, buf)
            }
            'Y' => {
                let n = self.take_count();
                let span = multi_line_span(buf.text(), self.cursor, n);
                self.apply_operator(Operator::Yank, span, MotionKind::Linewise, buf)
            }
            'r' => {
                self.begin_recording_with_count(key);
                self.pending = Pending::ReplaceChar;
                Outcome::Handled { mutated: false, yanked: false }
            }
            '~' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                let lb = line_bounds(buf.text(), self.cursor);
                let mut end = self.cursor;
                for _ in 0..n {
                    if end >= lb.end {
                        break;
                    }
                    end = next_char(buf.text(), end);
                }
                let out = self.apply_operator(
                    Operator::ToggleCase,
                    Span::new(self.cursor, end),
                    MotionKind::Charwise,
                    buf,
                );
                self.cursor = clamp_normal_cursor(buf.text(), end);
                out
            }
            'J' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                let mut mutated = false;
                for _ in 0..n {
                    let lb = line_bounds(buf.text(), self.cursor);
                    if lb.end >= buf.text().len() {
                        break;
                    }
                    if !mutated {
                        self.push_undo_raw(buf.text());
                        mutated = true;
                    }
                    let next_lb = line_bounds(buf.text(), lb.end + 1);
                    let next_nb = first_non_blank(buf.text(), next_lb.start);
                    let sep = if lb.start == lb.end { "" } else { " " };
                    buf.replace_range(lb.end, next_nb, sep);
                    self.cursor = clamp_normal_cursor(buf.text(), lb.end);
                }
                if mutated {
                    self.finish_recording();
                } else {
                    self.cancel_recording();
                }
                Outcome::Handled { mutated, yanked: false }
            }

            // --- Visual modes ---
            'v' => {
                self.count_buf = 0;
                self.mode = Mode::Visual;
                self.visual_anchor = Some(self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'V' => {
                self.count_buf = 0;
                self.mode = Mode::VisualLine;
                self.visual_anchor = Some(self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }

            // --- Paste ---
            'p' | 'P' => {
                self.begin_recording_with_count(key);
                let n = self.take_count();
                let out = self.paste_register(buf, ch == 'p', n);
                self.finish_recording();
                out
            }

            // --- Undo & Dot Repeat ---
            'u' => {
                let n = self.take_count();
                let mut did = false;
                for _ in 0..n {
                    if self.undo(buf) {
                        did = true;
                    }
                }
                Outcome::Handled { mutated: did, yanked: false }
            }
            '.' => {
                self.count_buf = 0;
                self.replay_dot(buf, objects)
            }

            _ => {
                self.count_buf = 0;
                Outcome::Handled { mutated: false, yanked: false }
            }
        }
    }

    fn begin_recording_with_count(&mut self, op_key: KeyEvent) {
        if self.replaying_dot {
            return;
        }
        self.recording_keys.clear();
        if self.count_buf > 0 {
            let mut n = self.count_buf;
            let mut digits = [0u8; 10];
            let mut len = 0usize;
            while n > 0 && len < digits.len() {
                digits[len] = (n % 10) as u8;
                n /= 10;
                len += 1;
            }
            for i in (0..len).rev() {
                self.recording_keys.push(KeyEvent::char((b'0' + digits[i]) as char));
            }
        }
        self.recording_keys.push(op_key);
        self.is_recording = true;
    }

    fn replay_dot<B: TextBuffer, O: TextObjectResolver>(
        &mut self,
        buf: &mut B,
        objects: &O,
    ) -> Outcome {
        let Some(rec) = self.last_dot.as_ref() else {
            return Outcome::Handled { mutated: false, yanked: false };
        };
        let mut keys = OwnVec::with_capacity_in(rec.keys.len(), self.alloc);
        for k in rec.keys.iter() {
            keys.push(*k);
        }
        self.replaying_dot = true;
        let mut any_mut = false;
        let mut any_yank = false;
        for k in keys.iter() {
            if let Outcome::Handled { mutated, yanked } = self.feed(*k, buf, objects) {
                any_mut |= mutated;
                any_yank |= yanked;
            }
        }
        self.replaying_dot = false;
        Outcome::Handled { mutated: any_mut, yanked: any_yank }
    }

    // =========================================================================
    // Visual / VisualLine Mode
    // =========================================================================

    fn feed_visual<B: TextBuffer, O: TextObjectResolver>(
        &mut self,
        key: KeyEvent,
        buf: &mut B,
        objects: &O,
    ) -> Outcome {
        if key.code == KeyCode::Escape {
            self.mode = Mode::Normal;
            self.visual_anchor = None;
            self.pending = Pending::None;
            self.count_buf = 0;
            self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
            return Outcome::Handled { mutated: false, yanked: false };
        }

        // Complete pending text object in Visual mode (`viw`, `va"`, `vi|`).
        if let Pending::Obj { scope, .. } = self.pending {
            self.pending = Pending::None;
            let count = self.take_count();
            if let KeyCode::Char(obj_key) = key.code
                && let Some((span, kind)) =
                    objects.resolve(buf.text(), self.cursor, scope, obj_key, count)
                && !span.is_empty()
            {
                if kind == MotionKind::Linewise {
                    self.mode = Mode::VisualLine;
                }
                self.visual_anchor = Some(span.start);
                self.cursor = prev_char(buf.text(), span.end);
            }
            return Outcome::Handled { mutated: false, yanked: false };
        }

        if let Pending::Find { kind, .. } = self.pending {
            self.pending = Pending::None;
            let count = self.take_count();
            if let KeyCode::Char(target) = key.code {
                self.last_find = Some((kind, target));
                if let Some(dest) = find_char_on_line(buf.text(), self.cursor, kind, target, count)
                {
                    self.cursor = dest;
                }
            }
            return Outcome::Handled { mutated: false, yanked: false };
        }

        if let Pending::PrefixG { .. } = self.pending {
            self.pending = Pending::None;
            if let KeyCode::Char('g') = key.code {
                let count = self.take_count();
                self.cursor = if count <= 1 {
                    first_non_blank(buf.text(), 0)
                } else {
                    goto_line(buf.text(), count)
                };
            }
            return Outcome::Handled { mutated: false, yanked: false };
        }

        let KeyCode::Char(ch) = key.code else {
            match key.code {
                KeyCode::Left => self.move_horiz(buf.text(), -1),
                KeyCode::Right => self.move_horiz(buf.text(), 1),
                KeyCode::Up => self.move_vert(buf.text(), -1),
                KeyCode::Down => self.move_vert(buf.text(), 1),
                _ => {}
            }
            return Outcome::Handled { mutated: false, yanked: false };
        };

        if ch.is_ascii_digit() && (ch != '0' || self.count_buf > 0) {
            self.push_count_digit((ch as u8 - b'0') as usize);
            return Outcome::Handled { mutated: false, yanked: false };
        }

        let sel_kind = if self.mode == Mode::VisualLine {
            MotionKind::Linewise
        } else {
            MotionKind::Charwise
        };

        match ch {
            'v' if self.mode == Mode::Visual => {
                self.set_mode(buf.text(), Mode::Normal);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'V' if self.mode == Mode::VisualLine => {
                self.set_mode(buf.text(), Mode::Normal);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'v' => {
                self.mode = Mode::Visual;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'V' => {
                self.mode = Mode::VisualLine;
                Outcome::Handled { mutated: false, yanked: false }
            }
            'o' | 'O' => {
                if let Some(anc) = self.visual_anchor.replace(self.cursor) {
                    self.cursor = anc;
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            'i' | 'a' => {
                self.pending = Pending::Obj {
                    op: None,
                    scope: if ch == 'i' { ObjectScope::Inner } else { ObjectScope::Around },
                };
                Outcome::Handled { mutated: false, yanked: false }
            }
            'd' | 'x' => {
                if let Some(span) = self.visual_span_for_op(buf.text()) {
                    self.visual_anchor = None;
                    self.mode = Mode::Normal;
                    return self.apply_operator(Operator::Delete, span, sel_kind, buf);
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            'c' | 's' => {
                if let Some(span) = self.visual_span_for_op(buf.text()) {
                    self.visual_anchor = None;
                    self.mode = Mode::Normal;
                    return self.apply_operator(Operator::Change, span, sel_kind, buf);
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            'y' => {
                if let Some(span) = self.visual_span_for_op(buf.text()) {
                    self.visual_anchor = None;
                    self.mode = Mode::Normal;
                    return self.apply_operator(Operator::Yank, span, sel_kind, buf);
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            '~' | 'u' | 'U' => {
                let op = match ch {
                    '~' => Operator::ToggleCase,
                    'u' => Operator::Lowercase,
                    _ => Operator::Uppercase,
                };
                if let Some(span) = self.visual_span_for_op(buf.text()) {
                    self.visual_anchor = None;
                    self.mode = Mode::Normal;
                    return self.apply_operator(op, span, sel_kind, buf);
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            '>' | '<' => {
                let op = if ch == '>' { Operator::Indent } else { Operator::Dedent };
                if let Some(span) = self.visual_span_for_op(buf.text()) {
                    self.visual_anchor = None;
                    self.mode = Mode::Normal;
                    return self.apply_operator(op, span, MotionKind::Linewise, buf);
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            'p' => {
                if let Some(span) = self.visual_span_for_op(buf.text()) {
                    self.push_undo_raw(buf.text());
                    let paste = OwnString::from_str_in(self.register.text.as_str(), self.alloc);
                    let replaced = OwnString::from_str_in(&buf.text()[span.start..span.end], self.alloc);
                    buf.replace_range(span.start, span.end, paste.as_str());
                    self.register = Register { text: replaced, kind: sel_kind };
                    self.visual_anchor = None;
                    self.mode = Mode::Normal;
                    self.cursor = clamp_normal_cursor(
                        buf.text(),
                        if paste.is_empty() {
                            span.start
                        } else {
                            prev_char(buf.text(), span.start + paste.len())
                        },
                    );
                    return Outcome::Handled { mutated: true, yanked: true };
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            // Motions extend selection cleanly.
            'h' => {
                let n = self.take_count();
                self.move_horiz(buf.text(), -(n as isize));
                Outcome::Handled { mutated: false, yanked: false }
            }
            'l' | ' ' => {
                let n = self.take_count();
                self.move_horiz(buf.text(), n as isize);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'j' => {
                let n = self.take_count();
                self.move_vert(buf.text(), n as isize);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'k' => {
                let n = self.take_count();
                self.move_vert(buf.text(), -(n as isize));
                Outcome::Handled { mutated: false, yanked: false }
            }
            '0' => {
                self.count_buf = 0;
                self.cursor = line_bounds(buf.text(), self.cursor).start;
                Outcome::Handled { mutated: false, yanked: false }
            }
            '^' => {
                self.count_buf = 0;
                self.cursor = first_non_blank(buf.text(), self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }
            '$' => {
                self.count_buf = 0;
                self.cursor = normal_line_last_char(buf.text(), self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'w' | 'W' => {
                let n = self.take_count();
                for _ in 0..n {
                    self.cursor = motion_word_forward(buf.text(), self.cursor, ch == 'W');
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'b' | 'B' => {
                let n = self.take_count();
                for _ in 0..n {
                    self.cursor = motion_word_backward(buf.text(), self.cursor, ch == 'B');
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }
            'e' | 'E' => {
                let n = self.take_count();
                for _ in 0..n {
                    self.cursor = motion_word_end(buf.text(), self.cursor, ch == 'E');
                }
                self.cursor = clamp_normal_cursor(buf.text(), self.cursor);
                Outcome::Handled { mutated: false, yanked: false }
            }
            '%' => {
                if let Some(dest) = motion_match_bracket(buf.text(), self.cursor) {
                    self.cursor = dest;
                }
                Outcome::Handled { mutated: false, yanked: false }
            }
            'G' => {
                self.count_buf = 0;
                self.cursor = clamp_normal_cursor(buf.text(), buf.text().len());
                Outcome::Handled { mutated: false, yanked: false }
            }
            'g' => {
                self.pending = Pending::PrefixG { op: None };
                Outcome::Handled { mutated: false, yanked: false }
            }
            'f' | 'F' | 't' | 'T' => {
                self.pending = Pending::Find { op: None, kind: find_kind_for(ch).unwrap() };
                Outcome::Handled { mutated: false, yanked: false }
            }
            _ => Outcome::Handled { mutated: false, yanked: false },
        }
    }

    fn visual_span_for_op(&self, text: &str) -> Option<Span> {
        let sp = self.visual_selection(text)?;
        if self.mode == Mode::VisualLine {
            let mut end = sp.end;
            if end < text.len() && text.as_bytes()[end] == b'\n' {
                end += 1;
            }
            Some(Span::new(sp.start, end))
        } else {
            Some(sp)
        }
    }

    // =========================================================================
    // Motion & Operator Execution
    // =========================================================================

    fn move_horiz(&mut self, text: &str, delta: isize) {
        let lb = line_bounds(text, self.cursor);
        let mut c = self.cursor;
        if delta < 0 {
            for _ in 0..delta.unsigned_abs() {
                if c <= lb.start {
                    break;
                }
                c = prev_char(text, c);
            }
        } else {
            let last = normal_line_last_char(text, self.cursor);
            for _ in 0..(delta as usize) {
                if c >= last {
                    break;
                }
                c = next_char(text, c);
            }
        }
        self.cursor = c;
        self.desired_col = None;
    }

    fn move_vert(&mut self, text: &str, delta: isize) {
        let (_, cur_col) = line_col(text, self.cursor);
        let target = *self.desired_col.get_or_insert(cur_col);
        self.cursor = move_vertical(text, self.cursor, delta, target, true);
    }

    fn resolve_operator_motion(
        &self,
        text: &str,
        op: Operator,
        ch: char,
        count: usize,
    ) -> Option<(Span, MotionKind)> {
        let start = self.cursor;
        match ch {
            'h' => {
                let lb = line_bounds(text, start);
                let mut d = start;
                for _ in 0..count {
                    if d <= lb.start {
                        break;
                    }
                    d = prev_char(text, d);
                }
                Some((Span::new(d, start), MotionKind::Charwise))
            }
            'l' | ' ' => {
                let lb = line_bounds(text, start);
                let mut d = start;
                for _ in 0..count {
                    if d >= lb.end {
                        break;
                    }
                    d = next_char(text, d);
                }
                Some((Span::new(start, d), MotionKind::Charwise))
            }
            '0' => {
                let lb = line_bounds(text, start);
                Some((Span::new(lb.start, start), MotionKind::Charwise))
            }
            '^' => {
                let nb = first_non_blank(text, start);
                Some((Span::new(nb, start), MotionKind::Charwise))
            }
            '$' => {
                let lb = line_bounds(text, start);
                Some((Span::new(start, lb.end), MotionKind::Charwise))
            }
            'w' | 'W' => {
                let big = ch == 'W';
                // Vim `cw` / `cW` acts like `ce` / `cE` when starting on a non-whitespace char.
                if op == Operator::Change
                    && char_at(text, start).is_some_and(|c| !c.is_whitespace())
                {
                    let mut d = start;
                    for _ in 0..count {
                        d = motion_word_end(text, d, big);
                    }
                    let end = if d < text.len() { next_char(text, d) } else { d };
                    return Some((Span::new(start, end), MotionKind::Charwise));
                }
                let mut d = start;
                for _ in 0..count {
                    d = motion_word_forward(text, d, big);
                }
                Some((Span::new(start, d), MotionKind::Charwise))
            }
            'b' | 'B' => {
                let big = ch == 'B';
                let mut d = start;
                for _ in 0..count {
                    d = motion_word_backward(text, d, big);
                }
                Some((Span::new(d, start), MotionKind::Charwise))
            }
            'e' | 'E' => {
                let big = ch == 'E';
                let mut d = start;
                for _ in 0..count {
                    d = motion_word_end(text, d, big);
                }
                let end = if d < text.len() { next_char(text, d) } else { d };
                Some((Span::new(start, end), MotionKind::Charwise))
            }
            '%' => {
                let dest = motion_match_bracket(text, start)?;
                if dest >= start {
                    Some((Span::new(start, next_char(text, dest)), MotionKind::Charwise))
                } else {
                    Some((Span::new(dest, next_char(text, start)), MotionKind::Charwise))
                }
            }
            'j' => {
                let (_, col) = line_col(text, start);
                let dest = move_vertical(text, start, count as isize, col, true);
                Some((linewise_span(text, start, dest), MotionKind::Linewise))
            }
            'k' => {
                let (_, col) = line_col(text, start);
                let dest = move_vertical(text, start, -(count as isize), col, true);
                Some((linewise_span(text, start, dest), MotionKind::Linewise))
            }
            'G' => {
                let dest = text.len();
                Some((linewise_span(text, start, dest), MotionKind::Linewise))
            }
            '{' | '}' => {
                let mut d = start;
                for _ in 0..count {
                    d = motion_paragraph(text, d, ch == '}');
                }
                Some((Span::new(start, d), MotionKind::Charwise))
            }
            ';' | ',' => {
                let (kind, target) = self.last_find?;
                let eff = if ch == ',' { kind.invert() } else { kind };
                let dest = find_char_on_line(text, start, eff, target, count)?;
                if dest >= start {
                    Some((Span::new(start, next_char(text, dest)), MotionKind::Charwise))
                } else {
                    Some((Span::new(dest, start), MotionKind::Charwise))
                }
            }
            _ => None,
        }
    }

    fn apply_find_operator<B: TextBuffer>(
        &mut self,
        op: Operator,
        kind: FindKind,
        target: char,
        count: usize,
        buf: &mut B,
    ) -> Outcome {
        let Some(dest) = find_char_on_line(buf.text(), self.cursor, kind, target, count) else {
            self.cancel_recording();
            return Outcome::Handled { mutated: false, yanked: false };
        };
        let span = if dest >= self.cursor {
            Span::new(self.cursor, next_char(buf.text(), dest))
        } else {
            Span::new(dest, self.cursor)
        };
        self.apply_operator(op, span, MotionKind::Charwise, buf)
    }

    fn delete_chars_under_cursor<B: TextBuffer>(
        &mut self,
        buf: &mut B,
        count: usize,
        backward: bool,
    ) -> Outcome {
        let text = buf.text();
        let lb = line_bounds(text, self.cursor);
        let span = if backward {
            let mut s = self.cursor;
            for _ in 0..count {
                if s <= lb.start {
                    break;
                }
                s = prev_char(text, s);
            }
            Span::new(s, self.cursor)
        } else {
            let mut e = self.cursor;
            for _ in 0..count {
                if e >= lb.end {
                    break;
                }
                e = next_char(text, e);
            }
            Span::new(self.cursor, e)
        };
        self.apply_operator(Operator::Delete, span, MotionKind::Charwise, buf)
    }

    fn apply_operator<B: TextBuffer>(
        &mut self,
        op: Operator,
        span: Span,
        kind: MotionKind,
        buf: &mut B,
    ) -> Outcome {
        let s = clamp_boundary(buf.text(), span.start);
        let e = clamp_boundary(buf.text(), span.end).max(s);
        let span = Span::new(s, e);

        match op {
            Operator::Yank => {
                self.register = Register {
                    text: OwnString::from_str_in(&buf.text()[span.start..span.end], self.alloc),
                    kind,
                };
                self.cursor = clamp_normal_cursor(buf.text(), span.start);
                self.cancel_recording();
                Outcome::Handled { mutated: false, yanked: true }
            }
            Operator::Delete => {
                if span.is_empty() {
                    self.cancel_recording();
                    return Outcome::Handled { mutated: false, yanked: false };
                }
                self.push_undo_raw(buf.text());
                let removed = OwnString::from_str_in(&buf.text()[span.start..span.end], self.alloc);
                self.register = Register { text: removed, kind };
                // For linewise deletion of the final line, trim the preceding newline if any.
                let del_start = if kind == MotionKind::Linewise
                    && span.end == buf.text().len()
                    && span.start > 0
                    && buf.text().as_bytes()[span.start - 1] == b'\n'
                {
                    span.start - 1
                } else {
                    span.start
                };
                buf.replace_range(del_start, span.end, "");
                self.cursor = if kind == MotionKind::Linewise {
                    first_non_blank(buf.text(), clamp_boundary(buf.text(), del_start))
                } else {
                    clamp_normal_cursor(buf.text(), del_start)
                };
                self.desired_col = None;
                self.finish_recording();
                Outcome::Handled { mutated: true, yanked: true }
            }
            Operator::Change => {
                self.push_undo_raw(buf.text());
                let (chg_s, chg_e) = if kind == MotionKind::Linewise {
                    // `cc` / `S` clears line content while keeping its newline.
                    let end = if span.end > span.start
                        && buf.text().as_bytes()[span.end - 1] == b'\n'
                    {
                        span.end - 1
                    } else {
                        span.end
                    };
                    (span.start, end)
                } else {
                    (span.start, span.end)
                };
                let removed = OwnString::from_str_in(&buf.text()[chg_s..chg_e], self.alloc);
                self.register = Register { text: removed, kind };
                buf.replace_range(chg_s, chg_e, "");
                self.cursor = clamp_boundary(buf.text(), chg_s);
                self.mode = Mode::Insert;
                self.insert_group = InsertEditGroup::None;
                self.desired_col = None;
                // Keep recording open until `Esc` leaves Insert mode so `.` repeats the full change!
                Outcome::Handled { mutated: true, yanked: true }
            }
            Operator::Lowercase | Operator::Uppercase | Operator::ToggleCase => {
                if span.is_empty() {
                    self.cancel_recording();
                    return Outcome::Handled { mutated: false, yanked: false };
                }
                self.push_undo_raw(buf.text());
                let slice = &buf.text()[span.start..span.end];
                let mut out = OwnString::with_capacity_in(slice.len(), self.alloc);
                for ch in slice.chars() {
                    match op {
                        Operator::Lowercase => {
                            for l in ch.to_lowercase() {
                                out.push(l);
                            }
                        }
                        Operator::Uppercase => {
                            for u in ch.to_uppercase() {
                                out.push(u);
                            }
                        }
                        Operator::ToggleCase => {
                            if ch.is_uppercase() {
                                for l in ch.to_lowercase() {
                                    out.push(l);
                                }
                            } else if ch.is_lowercase() {
                                for u in ch.to_uppercase() {
                                    out.push(u);
                                }
                            } else {
                                out.push(ch);
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                buf.replace_range(span.start, span.end, out.as_str());
                self.cursor = clamp_normal_cursor(buf.text(), span.start);
                self.finish_recording();
                Outcome::Handled { mutated: true, yanked: false }
            }
            Operator::Indent | Operator::Dedent => {
                self.push_undo_raw(buf.text());
                let ls = line_bounds(buf.text(), span.start).start;
                let le = line_bounds(buf.text(), span.end.saturating_sub(1).max(ls)).end;
                let slice = &buf.text()[ls..le];
                let mut out = OwnString::with_capacity_in(slice.len() + 16, self.alloc);
                for (i, line) in slice.split('\n').enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    if op == Operator::Indent {
                        if !line.is_empty() {
                            out.push_str("  ");
                        }
                        out.push_str(line);
                    } else if let Some(rest) = line.strip_prefix("  ") {
                        out.push_str(rest);
                    } else if let Some(rest) = line.strip_prefix('\t') {
                        out.push_str(rest);
                    } else if let Some(rest) = line.strip_prefix(' ') {
                        out.push_str(rest);
                    } else {
                        out.push_str(line);
                    }
                }
                buf.replace_range(ls, le, out.as_str());
                self.cursor = first_non_blank(buf.text(), ls);
                self.finish_recording();
                Outcome::Handled { mutated: true, yanked: false }
            }
        }
    }

    fn paste_register<B: TextBuffer>(&mut self, buf: &mut B, after: bool, count: usize) -> Outcome {
        if self.register.text.is_empty() {
            return Outcome::Handled { mutated: false, yanked: false };
        }
        self.push_undo_raw(buf.text());
        let reg_kind = self.register.kind;
        let single = self.register.text.as_str();
        let mut payload = OwnString::with_capacity_in(single.len() * count + 2, self.alloc);

        match reg_kind {
            MotionKind::Charwise => {
                for _ in 0..count {
                    payload.push_str(single);
                }
                let lb = line_bounds(buf.text(), self.cursor);
                let pos = if after && self.cursor < lb.end {
                    next_char(buf.text(), self.cursor)
                } else {
                    self.cursor
                };
                buf.replace_range(pos, pos, payload.as_str());
                self.cursor = clamp_normal_cursor(
                    buf.text(),
                    prev_char(buf.text(), pos + payload.len()),
                );
            }
            MotionKind::Linewise => {
                let body = single.strip_suffix('\n').unwrap_or(single);
                let lb = line_bounds(buf.text(), self.cursor);
                if after {
                    for _ in 0..count {
                        payload.push('\n');
                        payload.push_str(body);
                    }
                    buf.replace_range(lb.end, lb.end, payload.as_str());
                    self.cursor = first_non_blank(buf.text(), lb.end + 1);
                } else {
                    for _ in 0..count {
                        payload.push_str(body);
                        payload.push('\n');
                    }
                    buf.replace_range(lb.start, lb.start, payload.as_str());
                    self.cursor = first_non_blank(buf.text(), lb.start);
                }
            }
        }
        Outcome::Handled { mutated: true, yanked: false }
    }
}

// =============================================================================
// Internal span & search helpers
// =============================================================================

fn find_kind_for(ch: char) -> Option<FindKind> {
    match ch {
        'f' => Some(FindKind::Forward),
        'F' => Some(FindKind::Backward),
        't' => Some(FindKind::TillForward),
        'T' => Some(FindKind::TillBackward),
        _ => None,
    }
}

fn find_char_on_line(
    text: &str,
    cursor: usize,
    kind: FindKind,
    target: char,
    count: usize,
) -> Option<usize> {
    let lb = line_bounds(text, cursor);
    let mut rem = count.max(1);
    match kind {
        FindKind::Forward | FindKind::TillForward => {
            let mut p = next_char(text, cursor);
            let mut found = None;
            while p < lb.end {
                if char_at(text, p) == Some(target) {
                    rem -= 1;
                    if rem == 0 {
                        found = Some(p);
                        break;
                    }
                }
                p = next_char(text, p);
            }
            let pos = found?;
            if kind == FindKind::TillForward {
                let prev = prev_char(text, pos);
                if prev >= cursor { Some(prev) } else { None }
            } else {
                Some(pos)
            }
        }
        FindKind::Backward | FindKind::TillBackward => {
            let mut p = cursor;
            let mut found = None;
            while p > lb.start {
                p = prev_char(text, p);
                if char_at(text, p) == Some(target) {
                    rem -= 1;
                    if rem == 0 {
                        found = Some(p);
                        break;
                    }
                }
            }
            let pos = found?;
            if kind == FindKind::TillBackward {
                let next = next_char(text, pos);
                if next <= cursor { Some(next) } else { None }
            } else {
                Some(pos)
            }
        }
    }
}

fn goto_line(text: &str, one_based: usize) -> usize {
    let target = one_based.saturating_sub(1);
    let mut start = 0usize;
    for (li, part) in text.split('\n').enumerate() {
        if li == target {
            return first_non_blank(text, start);
        }
        start += part.len() + 1;
    }
    first_non_blank(text, line_bounds(text, text.len()).start)
}

fn linewise_span(text: &str, a: usize, b: usize) -> Span {
    let lo = a.min(b);
    let hi = a.max(b);
    let s = line_bounds(text, lo).start;
    let hi_lb = line_bounds(text, hi);
    let e = if hi_lb.end < text.len() && text.as_bytes()[hi_lb.end] == b'\n' {
        hi_lb.end + 1
    } else {
        hi_lb.end
    };
    Span::new(s, e)
}

fn multi_line_span(text: &str, cursor: usize, count: usize) -> Span {
    let first = line_bounds(text, cursor);
    let mut end = first.end;
    for _ in 1..count.max(1) {
        if end >= text.len() {
            break;
        }
        let next = line_bounds(text, end + 1);
        end = next.end;
    }
    if end < text.len() && text.as_bytes()[end] == b'\n' {
        end += 1;
    } else if first.start == 0 && end == text.len() {
        return Span::new(0, text.len());
    }
    let _ = line_bounds_with_newline; // keep helper used in single-line cases
    Span::new(first.start, end)
}
