//! Unified diff hunk construction, formatting, strict bounded parsing, and forward/reverse
//! patch application (zero panicking indexes, zero casts, zero clones).

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

use super::tokenize::{split_lines, strip_lf};
use super::{ApplyPatchError, DiffError, DiffPlant, Op, ParsePatchError, ParsePatchLimits, ParseWhy};

/// A single line inside a unified diff [`Hunk`], without its trailing `\n`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HunkLine<'a> {
    /// Unchanged context line (` `).
    Context(&'a str),
    /// Removed line from the old text (`-`).
    Delete(&'a str),
    /// Added line in the new text (`+`).
    Insert(&'a str),
}

/// One unified diff hunk (`@@ -old_start,old_count +new_start,new_count @@`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk<'a, A: Allocator> {
    /// 1-based start line in the old text (or 0-based anchor when `old_count == 0`).
    pub old_start: usize,
    /// Number of old-side lines covered by this hunk (`Context` + `Delete`).
    pub old_count: usize,
    /// 1-based start line in the new text (or 0-based anchor when `new_count == 0`).
    pub new_start: usize,
    /// Number of new-side lines covered by this hunk (`Context` + `Insert`).
    pub new_count: usize,
    /// Optional section heading appearing after the closing `@@`.
    pub header: Option<&'a str>,
    /// Ordered lines of the hunk.
    pub lines: Vec<HunkLine<'a>, A>,
    /// True when the last old-side line in this hunk had no trailing `\n`.
    pub old_no_newline_at_eof: bool,
    /// True when the last new-side line in this hunk had no trailing `\n`.
    pub new_no_newline_at_eof: bool,
}

/// A parsed unified patch for one file (headers + hunks).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Patch<'a, A: Allocator> {
    /// Old file name from `--- <name>` (if present).
    pub old_name: Option<&'a str>,
    /// New file name from `+++ <name>` (if present).
    pub new_name: Option<&'a str>,
    /// Hunks in ascending line order.
    pub hunks: Vec<Hunk<'a, A>, A>,
}

/// Summary statistics of a diff or patch.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DiffStats {
    pub equal: usize,
    pub inserted: usize,
    pub deleted: usize,
}

impl<A: Allocator> Patch<'_, A> {
    /// True when the patch carries no hunks (identical inputs).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Count context, inserted, and deleted lines across all hunks.
    #[must_use]
    pub fn stats(&self) -> DiffStats {
        let mut s = DiffStats::default();
        for hunk in self.hunks.as_slice() {
            for line in hunk.lines.as_slice() {
                match line {
                    HunkLine::Context(_) => s.equal += 1,
                    HunkLine::Delete(_) => s.deleted += 1,
                    HunkLine::Insert(_) => s.inserted += 1,
                }
            }
        }
        s
    }

    /// Render this patch into unified diff text in `out`.
    pub fn write_into<W: fmt::Write>(&self, out: &mut W) -> fmt::Result {
        write_patch_hunks(self.old_name, self.new_name, self.hunks.as_slice(), out, DiffPlant::None)
    }
}

/// Build unified diff hunks from line slices and a coalesced edit script `ops`.
pub(crate) fn build_hunks<'a, A: Allocator + Copy>(
    old_lines: &[&'a str],
    new_lines: &[&'a str],
    ops: &[Op],
    context: usize,
    alloc: A,
    plant: DiffPlant,
) -> Result<Vec<Hunk<'a, A>, A>, DiffError> {
    let mut hunks = Vec::new_in(alloc);

    let mut change_indices = Vec::new_in(alloc);
    for (idx, op) in ops.iter().enumerate() {
        if !matches!(op, Op::Equal { .. }) {
            change_indices.try_push(idx).map_err(|r| DiffError::Alloc(r.why))?;
        }
    }
    if change_indices.is_empty() {
        return Ok(hunks);
    }

    let mut group_start = 0usize;
    while group_start < change_indices.len() {
        let mut group_end = group_start;
        while group_end + 1 < change_indices.len() {
            let cur_op_idx = change_indices.get(group_end).copied().unwrap_or(0);
            let next_op_idx = change_indices.get(group_end + 1).copied().unwrap_or(0);
            let mut gap = 0usize;
            if let Some(between) = ops.get((cur_op_idx + 1)..next_op_idx) {
                for op in between {
                    if let Op::Equal { len, .. } = *op {
                        gap += len;
                    }
                }
            }
            if gap <= context.saturating_mul(2) {
                group_end += 1;
            } else {
                break;
            }
        }

        let first_change_op = change_indices.get(group_start).copied().unwrap_or(0);
        let last_change_op = change_indices.get(group_end).copied().unwrap_or(0);

        let (lead_old_start, lead_new_start, lead_len) = if first_change_op > 0
            && let Some(&Op::Equal { old_start, new_start, len }) = ops.get(first_change_op - 1)
        {
            let take = len.min(context);
            (old_start + len - take, new_start + len - take, take)
        } else {
            let (os, ns) = op_start_coords(ops, first_change_op);
            (os, ns, 0)
        };

        let (trail_old_start, trail_len) = if let Some(&Op::Equal { old_start, len, .. }) =
            ops.get(last_change_op + 1)
        {
            (old_start, len.min(context))
        } else {
            (0, 0)
        };

        let mut lines = Vec::new_in(alloc);
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut last_old_had_lf = true;
        let mut last_new_had_lf = true;

        if let Some(lead_slice) = old_lines.get(lead_old_start..(lead_old_start + lead_len)) {
            for &raw in lead_slice {
                let (body, had_lf) = strip_lf(raw);
                lines.try_push(HunkLine::Context(body)).map_err(|r| DiffError::Alloc(r.why))?;
                old_count += 1;
                new_count += 1;
                last_old_had_lf = had_lf;
                last_new_had_lf = had_lf;
            }
        }

        if let Some(body_ops) = ops.get(first_change_op..=last_change_op) {
            for op in body_ops {
                match *op {
                    Op::Equal { old_start, len, .. } => {
                        if let Some(slice) = old_lines.get(old_start..(old_start + len)) {
                            for &raw in slice {
                                let (body, had_lf) = strip_lf(raw);
                                lines
                                    .try_push(HunkLine::Context(body))
                                    .map_err(|r| DiffError::Alloc(r.why))?;
                                old_count += 1;
                                new_count += 1;
                                last_old_had_lf = had_lf;
                                last_new_had_lf = had_lf;
                            }
                        }
                    }
                    Op::Delete { old_start, len } => {
                        if let Some(slice) = old_lines.get(old_start..(old_start + len)) {
                            for &raw in slice {
                                let (body, had_lf) = strip_lf(raw);
                                lines
                                    .try_push(HunkLine::Delete(body))
                                    .map_err(|r| DiffError::Alloc(r.why))?;
                                old_count += 1;
                                last_old_had_lf = had_lf;
                            }
                        }
                    }
                    Op::Insert { new_start, len } => {
                        if let Some(slice) = new_lines.get(new_start..(new_start + len)) {
                            for &raw in slice {
                                let (body, had_lf) = strip_lf(raw);
                                lines
                                    .try_push(HunkLine::Insert(body))
                                    .map_err(|r| DiffError::Alloc(r.why))?;
                                new_count += 1;
                                last_new_had_lf = had_lf;
                            }
                        }
                    }
                }
            }
        }

        if let Some(trail_slice) = old_lines.get(trail_old_start..(trail_old_start + trail_len)) {
            for &raw in trail_slice {
                let (body, had_lf) = strip_lf(raw);
                lines.try_push(HunkLine::Context(body)).map_err(|r| DiffError::Alloc(r.why))?;
                old_count += 1;
                new_count += 1;
                last_old_had_lf = had_lf;
                last_new_had_lf = had_lf;
            }
        }

        let old_start = if old_count == 0 { lead_old_start } else { lead_old_start + 1 };
        let new_start = if new_count == 0 { lead_new_start } else { lead_new_start + 1 };
        let old_no_newline_at_eof =
            plant != DiffPlant::SkipNoNewlineMarker && old_count > 0 && !last_old_had_lf;
        let new_no_newline_at_eof =
            plant != DiffPlant::SkipNoNewlineMarker && new_count > 0 && !last_new_had_lf;

        hunks
            .try_push(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                header: None,
                lines,
                old_no_newline_at_eof,
                new_no_newline_at_eof,
            })
            .map_err(|r| DiffError::Alloc(r.why))?;

        group_start = group_end + 1;
    }

    Ok(hunks)
}

fn op_start_coords(ops: &[Op], op_idx: usize) -> (usize, usize) {
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    if let Some(prefix) = ops.get(..op_idx) {
        for op in prefix {
            match *op {
                Op::Equal { old_start, new_start, len } => {
                    old_pos = old_start + len;
                    new_pos = new_start + len;
                }
                Op::Delete { old_start, len } => {
                    old_pos = old_start + len;
                }
                Op::Insert { new_start, len } => {
                    new_pos = new_start + len;
                }
            }
        }
    }
    (old_pos, new_pos)
}

/// Format hunks (and optional file headers) as standard unified diff text.
pub(crate) fn write_patch_hunks<A: Allocator, W: fmt::Write>(
    old_name: Option<&str>,
    new_name: Option<&str>,
    hunks: &[Hunk<'_, A>],
    out: &mut W,
    plant: DiffPlant,
) -> fmt::Result {
    if hunks.is_empty() {
        return Ok(());
    }
    if let (Some(old), Some(new)) = (old_name, new_name) {
        writeln!(out, "--- {old}")?;
        writeln!(out, "+++ {new}")?;
    }
    for hunk in hunks {
        let old_count = if plant == DiffPlant::OffByOneHunkCount {
            hunk.old_count + 1
        } else {
            hunk.old_count
        };
        write!(
            out,
            "@@ -{},{} +{},{} @@",
            hunk.old_start, old_count, hunk.new_start, hunk.new_count
        )?;
        if let Some(hdr) = hunk.header {
            write!(out, " {hdr}")?;
        }
        out.write_char('\n')?;

        let last_old_idx = hunk
            .lines
            .as_slice()
            .iter()
            .rposition(|l| matches!(l, HunkLine::Context(_) | HunkLine::Delete(_)));
        let last_new_idx = hunk
            .lines
            .as_slice()
            .iter()
            .rposition(|l| matches!(l, HunkLine::Context(_) | HunkLine::Insert(_)));

        for (idx, line) in hunk.lines.as_slice().iter().enumerate() {
            match *line {
                HunkLine::Context(body) => {
                    out.write_char(' ')?;
                    out.write_str(body)?;
                    out.write_char('\n')?;
                    if Some(idx) == last_old_idx
                        && Some(idx) == last_new_idx
                        && (hunk.old_no_newline_at_eof || hunk.new_no_newline_at_eof)
                    {
                        out.write_str("\\ No newline at end of file\n")?;
                    }
                }
                HunkLine::Delete(body) => {
                    out.write_char('-')?;
                    out.write_str(body)?;
                    out.write_char('\n')?;
                    if Some(idx) == last_old_idx && hunk.old_no_newline_at_eof {
                        out.write_str("\\ No newline at end of file\n")?;
                    }
                }
                HunkLine::Insert(body) => {
                    out.write_char('+')?;
                    out.write_str(body)?;
                    out.write_char('\n')?;
                    if Some(idx) == last_new_idx && hunk.new_no_newline_at_eof {
                        out.write_str("\\ No newline at end of file\n")?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Format a unified diff string with ANSI colors into `out`.
pub(crate) fn format_patch_ansi_into<W: fmt::Write>(patch_text: &str, out: &mut W) -> fmt::Result {
    use crate::ansi::{Color, Style};

    const HEADER: Style = Style::new().bold();
    const HUNK: Style = Style::new().fg(Color::Cyan);
    const DEL: Style = Style::new().fg(Color::Red);
    const ADD: Style = Style::new().fg(Color::Green);
    const META: Style = Style::new().dim();

    for raw in patch_text.split_inclusive('\n') {
        let (line, has_nl) = strip_lf(raw);
        let style = if line.starts_with("@@ ") {
            Some(HUNK)
        } else if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff ") {
            Some(HEADER)
        } else if line.starts_with('-') {
            Some(DEL)
        } else if line.starts_with('+') {
            Some(ADD)
        } else if line.starts_with('\\') {
            Some(META)
        } else {
            None
        };

        if let Some(st) = style {
            write!(out, "{}", st.paint(line))?;
        } else {
            out.write_str(line)?;
        }
        if has_nl {
            out.write_char('\n')?;
        }
    }
    Ok(())
}

/// Format a unified diff string with ANSI colors AND inline word-level emphasis on paired `-`/`+`
/// line blocks (highlighting the exact changed words in bold red/green, like `delta` / GitHub).
pub(crate) fn format_patch_ansi_inline_into<A: Allocator + Copy, W: fmt::Write>(
    patch_text: &str,
    alloc: A,
    out: &mut W,
) -> fmt::Result {
    use crate::ansi::{Color, Style};

    const HEADER: Style = Style::new().bold();
    const HUNK: Style = Style::new().fg(Color::Cyan);
    const DEL: Style = Style::new().fg(Color::Red);
    const DEL_EMPH: Style = Style::new().fg(Color::BrightRed).bold().underline();
    const ADD: Style = Style::new().fg(Color::Green);
    const ADD_EMPH: Style = Style::new().fg(Color::BrightGreen).bold().underline();
    const META: Style = Style::new().dim();

    let Ok(raw_lines) = split_lines(patch_text, alloc) else {
        return format_patch_ansi_into(patch_text, out);
    };

    let mut i = 0usize;
    while let Some(&raw) = raw_lines.get(i) {
        let (line, has_nl) = strip_lf(raw);
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff ") {
            write!(out, "{}", HEADER.paint(line))?;
            if has_nl {
                out.write_char('\n')?;
            }
            i += 1;
            continue;
        }
        if line.starts_with("@@ ") {
            write!(out, "{}", HUNK.paint(line))?;
            if has_nl {
                out.write_char('\n')?;
            }
            i += 1;
            continue;
        }
        if line.starts_with('\\') {
            write!(out, "{}", META.paint(line))?;
            if has_nl {
                out.write_char('\n')?;
            }
            i += 1;
            continue;
        }
        if line.starts_with('-') {
            // Collect consecutive `-` lines followed immediately by consecutive `+` lines.
            let del_start = i;
            while let Some(&r) = raw_lines.get(i) {
                let (l, _) = strip_lf(r);
                if l.starts_with('-') && !l.starts_with("--- ") {
                    i += 1;
                } else {
                    break;
                }
            }
            let del_end = i;
            let add_start = i;
            while let Some(&r) = raw_lines.get(i) {
                let (l, _) = strip_lf(r);
                if l.starts_with('+') && !l.starts_with("+++ ") {
                    i += 1;
                } else {
                    break;
                }
            }
            let add_end = i;
            let del_count = del_end - del_start;
            let add_count = add_end - add_start;
            let paired = del_count.min(add_count);

            // Render deleted lines, word-diffing paired indices against their corresponding `+` line.
            for idx in 0..del_count {
                let Some(&dr) = raw_lines.get(del_start + idx) else { continue };
                let (dl, d_nl) = strip_lf(dr);
                let d_body = dl.strip_prefix('-').unwrap_or(dl);
                write!(out, "{}", DEL.paint('-'))?;
                if idx < paired
                    && let Some(&ar) = raw_lines.get(add_start + idx)
                {
                    let (al, _) = strip_lf(ar);
                    let a_body = al.strip_prefix('+').unwrap_or(al);
                    if let Ok(word_changes) = super::words(
                        d_body,
                        a_body,
                        super::DiffOpts {
                            algorithm: super::Algorithm::Myers,
                            budget: super::DEFAULT_BUDGET,
                            alloc,
                        },
                    ) {
                        for ch in word_changes.as_slice() {
                            match *ch {
                                super::TextChange::Equal(s) => write!(out, "{}", DEL.paint(s))?,
                                super::TextChange::Delete(s) => {
                                    write!(out, "{}", DEL_EMPH.paint(s))?
                                }
                                super::TextChange::Insert(_) => {}
                            }
                        }
                    } else {
                        write!(out, "{}", DEL.paint(d_body))?;
                    }
                } else {
                    write!(out, "{}", DEL.paint(d_body))?;
                }
                if d_nl {
                    out.write_char('\n')?;
                }
            }

            // Render added lines, word-diffing paired indices against their corresponding `-` line.
            for idx in 0..add_count {
                let Some(&ar) = raw_lines.get(add_start + idx) else { continue };
                let (al, a_nl) = strip_lf(ar);
                let a_body = al.strip_prefix('+').unwrap_or(al);
                write!(out, "{}", ADD.paint('+'))?;
                if idx < paired
                    && let Some(&dr) = raw_lines.get(del_start + idx)
                {
                    let (dl, _) = strip_lf(dr);
                    let d_body = dl.strip_prefix('-').unwrap_or(dl);
                    if let Ok(word_changes) = super::words(
                        d_body,
                        a_body,
                        super::DiffOpts {
                            algorithm: super::Algorithm::Myers,
                            budget: super::DEFAULT_BUDGET,
                            alloc,
                        },
                    ) {
                        for ch in word_changes.as_slice() {
                            match *ch {
                                super::TextChange::Equal(s) => write!(out, "{}", ADD.paint(s))?,
                                super::TextChange::Insert(s) => {
                                    write!(out, "{}", ADD_EMPH.paint(s))?
                                }
                                super::TextChange::Delete(_) => {}
                            }
                        }
                    } else {
                        write!(out, "{}", ADD.paint(a_body))?;
                    }
                } else {
                    write!(out, "{}", ADD.paint(a_body))?;
                }
                if a_nl {
                    out.write_char('\n')?;
                }
            }
            continue;
        }
        if line.starts_with('+') {
            write!(out, "{}", ADD.paint(line))?;
            if has_nl {
                out.write_char('\n')?;
            }
            i += 1;
            continue;
        }
        out.write_str(line)?;
        if has_nl {
            out.write_char('\n')?;
        }
        i += 1;
    }
    Ok(())
}

/// Parse one unified diff patch from `text`.
pub(crate) fn parse_single_patch<'a, A: Allocator + Copy>(
    text: &'a str,
    limits: ParsePatchLimits,
    alloc: A,
) -> Result<Patch<'a, A>, ParsePatchError> {
    let mut patches = parse_multi_patch(text, limits, alloc)?;
    match patches.len() {
        0 => Ok(Patch { old_name: None, new_name: None, hunks: Vec::new_in(alloc) }),
        1 => match patches.pop() {
            Some(patch) => Ok(patch),
            None => Ok(Patch { old_name: None, new_name: None, hunks: Vec::new_in(alloc) }),
        },
        _ => Err(ParsePatchError { line: 1, why: ParseWhy::MultipleFiles }),
    }
}

/// Parse one or more file patches from unified diff `text` under strict `limits`.
pub(crate) fn parse_multi_patch<'a, A: Allocator + Copy>(
    text: &'a str,
    limits: ParsePatchLimits,
    alloc: A,
) -> Result<Vec<Patch<'a, A>, A>, ParsePatchError> {
    if text.len() > limits.most_bytes {
        return Err(ParsePatchError {
            line: 1,
            why: ParseWhy::InputTooLarge { bytes: text.len(), limit: limits.most_bytes },
        });
    }

    let mut patches: Vec<Patch<'a, A>, A> = Vec::new_in(alloc);
    if text.is_empty() {
        return Ok(patches);
    }

    let raw_lines = split_lines(text, alloc)
        .map_err(|_| ParsePatchError { line: 1, why: ParseWhy::OutOfMemory })?;
    let total_lines = raw_lines.len();
    let mut i = 0usize;
    let mut total_hunks = 0usize;
    let mut total_hunk_lines = 0usize;
    let mut current: Option<Patch<'a, A>> = None;

    while let Some(&raw_line) = raw_lines.get(i) {
        let line_no = i + 1;
        let (line, _) = strip_lf(raw_line);

        if line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
        {
            if let Some(patch) = current.take()
                && (!patch.hunks.is_empty() || patch.old_name.is_some())
            {
                patches
                    .try_push(patch)
                    .map_err(|_| ParsePatchError { line: line_no, why: ParseWhy::OutOfMemory })?;
            }
            i += 1;
            continue;
        }

        if let Some(old_rest) = line.strip_prefix("--- ") {
            if let Some(patch) = current.take()
                && (!patch.hunks.is_empty() || patch.old_name.is_some())
            {
                patches
                    .try_push(patch)
                    .map_err(|_| ParsePatchError { line: line_no, why: ParseWhy::OutOfMemory })?;
            }
            let Some(&next_raw) = raw_lines.get(i + 1) else {
                return Err(ParsePatchError { line: line_no, why: ParseWhy::MissingNewHeader });
            };
            let (next_line, _) = strip_lf(next_raw);
            let Some(new_rest) = next_line.strip_prefix("+++ ") else {
                return Err(ParsePatchError { line: line_no + 1, why: ParseWhy::MissingNewHeader });
            };
            let old_name = old_rest.split('\t').next().unwrap_or(old_rest);
            let new_name = new_rest.split('\t').next().unwrap_or(new_rest);
            current = Some(Patch {
                old_name: Some(old_name),
                new_name: Some(new_name),
                hunks: Vec::new_in(alloc),
            });
            i += 2;
            continue;
        }

        if line.starts_with("@@ ") {
            total_hunks += 1;
            if total_hunks > limits.most_hunks {
                return Err(ParsePatchError {
                    line: line_no,
                    why: ParseWhy::TooManyHunks { limit: limits.most_hunks },
                });
            }

            let (old_start, old_count, new_start, new_count, header) =
                parse_hunk_header(line, line_no)?;
            i += 1;

            let mut hunk_lines = Vec::new_in(alloc);
            let mut seen_old = 0usize;
            let mut seen_new = 0usize;
            let mut old_no_newline = false;
            let mut new_no_newline = false;
            let mut last_kind: Option<u8> = None;

            while let Some(&raw_body) = raw_lines.get(i) {
                let cur_line_no = i + 1;
                let (body_line, _) = strip_lf(raw_body);

                if body_line.starts_with('\\') {
                    match last_kind {
                        Some(b'-') => old_no_newline = true,
                        Some(b'+') => new_no_newline = true,
                        Some(b' ') => {
                            old_no_newline = true;
                            new_no_newline = true;
                        }
                        _ => {
                            return Err(ParsePatchError {
                                line: cur_line_no,
                                why: ParseWhy::UnexpectedNoNewlineMarker,
                            });
                        }
                    }
                    i += 1;
                    continue;
                }

                if seen_old == old_count && seen_new == new_count {
                    break;
                }

                total_hunk_lines += 1;
                if total_hunk_lines > limits.most_lines {
                    return Err(ParsePatchError {
                        line: cur_line_no,
                        why: ParseWhy::TooManyLines { limit: limits.most_lines },
                    });
                }

                if body_line.is_empty() {
                    hunk_lines
                        .try_push(HunkLine::Context(""))
                        .map_err(|_| ParsePatchError { line: cur_line_no, why: ParseWhy::OutOfMemory })?;
                    seen_old += 1;
                    seen_new += 1;
                    last_kind = Some(b' ');
                    i += 1;
                    continue;
                }

                if let Some(rest) = body_line.strip_prefix(' ') {
                    hunk_lines
                        .try_push(HunkLine::Context(rest))
                        .map_err(|_| ParsePatchError { line: cur_line_no, why: ParseWhy::OutOfMemory })?;
                    seen_old += 1;
                    seen_new += 1;
                    last_kind = Some(b' ');
                } else if let Some(rest) = body_line.strip_prefix('-') {
                    hunk_lines
                        .try_push(HunkLine::Delete(rest))
                        .map_err(|_| ParsePatchError { line: cur_line_no, why: ParseWhy::OutOfMemory })?;
                    seen_old += 1;
                    last_kind = Some(b'-');
                } else if let Some(rest) = body_line.strip_prefix('+') {
                    hunk_lines
                        .try_push(HunkLine::Insert(rest))
                        .map_err(|_| ParsePatchError { line: cur_line_no, why: ParseWhy::OutOfMemory })?;
                    seen_new += 1;
                    last_kind = Some(b'+');
                } else {
                    let first = body_line.as_bytes().first().copied().unwrap_or(0);
                    return Err(ParsePatchError {
                        line: cur_line_no,
                        why: ParseWhy::InvalidLinePrefix { byte: first },
                    });
                }

                if seen_old > old_count || seen_new > new_count {
                    return Err(ParsePatchError {
                        line: cur_line_no,
                        why: ParseWhy::HunkLineCountMismatch {
                            expected_old: old_count,
                            actual_old: seen_old,
                            expected_new: new_count,
                            actual_new: seen_new,
                        },
                    });
                }
                i += 1;
            }

            if seen_old != old_count || seen_new != new_count {
                return Err(ParsePatchError {
                    line: line_no,
                    why: ParseWhy::HunkLineCountMismatch {
                        expected_old: old_count,
                        actual_old: seen_old,
                        expected_new: new_count,
                        actual_new: seen_new,
                    },
                });
            }

            let patch = current.get_or_insert_with(|| Patch {
                old_name: None,
                new_name: None,
                hunks: Vec::new_in(alloc),
            });
            patch
                .hunks
                .try_push(Hunk {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                    header,
                    lines: hunk_lines,
                    old_no_newline_at_eof: old_no_newline,
                    new_no_newline_at_eof: new_no_newline,
                })
                .map_err(|_| ParsePatchError { line: line_no, why: ParseWhy::OutOfMemory })?;
            continue;
        }

        if line.is_empty() {
            i += 1;
            continue;
        }

        return Err(ParsePatchError { line: line_no, why: ParseWhy::UnexpectedLine });
    }

    if let Some(patch) = current {
        patches
            .try_push(patch)
            .map_err(|_| ParsePatchError { line: total_lines, why: ParseWhy::OutOfMemory })?;
    }

    Ok(patches)
}

fn parse_hunk_header(
    line: &str,
    line_no: usize,
) -> Result<(usize, usize, usize, usize, Option<&str>), ParsePatchError> {
    let rest = line
        .strip_prefix("@@ ")
        .ok_or(ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
    let (ranges, after) = rest
        .split_once(" @@")
        .ok_or(ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
    let header = after.strip_prefix(' ').filter(|s| !s.is_empty());

    let mut parts = ranges.split(' ');
    let old_part = parts
        .next()
        .and_then(|s| s.strip_prefix('-'))
        .ok_or(ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
    let new_part = parts
        .next()
        .and_then(|s| s.strip_prefix('+'))
        .ok_or(ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
    if parts.next().is_some() {
        return Err(ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader });
    }

    let (old_start, old_count) = parse_range(old_part, line_no)?;
    let (new_start, new_count) = parse_range(new_part, line_no)?;
    Ok((old_start, old_count, new_start, new_count, header))
}

fn parse_range(s: &str, line_no: usize) -> Result<(usize, usize), ParsePatchError> {
    match s.split_once(',') {
        Some((start, count)) => {
            let start = start
                .parse::<usize>()
                .map_err(|_| ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
            let count = count
                .parse::<usize>()
                .map_err(|_| ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
            Ok((start, count))
        }
        None => {
            let start = s
                .parse::<usize>()
                .map_err(|_| ParsePatchError { line: line_no, why: ParseWhy::InvalidHunkHeader })?;
            Ok((start, 1))
        }
    }
}

/// Apply `hunks` forward (`reverse = false`) or backward (`reverse = true`) to `base`.
pub(crate) fn apply_hunks<A: Allocator + Copy, B: Allocator>(
    base: &str,
    hunks: &[Hunk<'_, B>],
    max_offset: usize,
    reverse: bool,
    alloc: A,
) -> Result<String<A>, ApplyPatchError> {
    if hunks.is_empty() {
        return String::try_from_str_in(base, alloc).map_err(ApplyPatchError::Alloc);
    }

    let base_lines = split_lines(base, alloc).map_err(|e| match e {
        DiffError::Alloc(a) => ApplyPatchError::Alloc(a),
        DiffError::BudgetExceeded { .. } => ApplyPatchError::Alloc(AllocFailure::CapacityOverflow {
            what: "bstd::diff::apply_hunks",
            len: base.len(),
            additional: 0,
        }),
    })?;

    let mut out = String::try_with_capacity_in(base.len(), alloc).map_err(ApplyPatchError::Alloc)?;
    let mut cursor = 0usize;

    for (hunk_idx, hunk) in hunks.iter().enumerate() {
        let (src_start, src_count, src_no_nl, dst_no_nl) = if !reverse {
            (hunk.old_start, hunk.old_count, hunk.old_no_newline_at_eof, hunk.new_no_newline_at_eof)
        } else {
            (hunk.new_start, hunk.new_count, hunk.new_no_newline_at_eof, hunk.old_no_newline_at_eof)
        };

        let ideal_pos = if src_count == 0 {
            src_start
        } else {
            src_start.checked_sub(1).ok_or(ApplyPatchError::InvalidHunkStart {
                hunk: hunk_idx,
                start: src_start,
            })?
        };

        let matched_pos = find_hunk_position(
            base_lines.as_slice(),
            cursor,
            ideal_pos,
            max_offset,
            hunk,
            src_count,
            src_no_nl,
            reverse,
            hunk_idx,
        )?;

        if let Some(prefix_lines) = base_lines.as_slice().get(cursor..matched_pos) {
            for &line in prefix_lines {
                out.try_push_str(line).map_err(ApplyPatchError::Alloc)?;
            }
        }

        let last_dst_idx = hunk.lines.as_slice().iter().rposition(|l| is_dst_line(*l, reverse));
        for (line_idx, line) in hunk.lines.as_slice().iter().enumerate() {
            let body = match (*line, reverse) {
                (HunkLine::Context(s), _) => Some(s),
                (HunkLine::Insert(s), false) | (HunkLine::Delete(s), true) => Some(s),
                (HunkLine::Delete(_), false) | (HunkLine::Insert(_), true) => None,
            };
            if let Some(body) = body {
                out.try_push_str(body).map_err(ApplyPatchError::Alloc)?;
                let omit_nl = dst_no_nl && Some(line_idx) == last_dst_idx;
                if !omit_nl {
                    out.try_push('\n').map_err(ApplyPatchError::Alloc)?;
                }
            }
        }

        cursor = matched_pos + src_count;
    }

    if let Some(tail_lines) = base_lines.as_slice().get(cursor..) {
        for &line in tail_lines {
            out.try_push_str(line).map_err(ApplyPatchError::Alloc)?;
        }
    }

    Ok(out)
}

#[inline]
fn is_src_line(line: HunkLine<'_>, reverse: bool) -> bool {
    match (line, reverse) {
        (HunkLine::Context(_), _) => true,
        (HunkLine::Delete(_), false) | (HunkLine::Insert(_), true) => true,
        (HunkLine::Insert(_), false) | (HunkLine::Delete(_), true) => false,
    }
}

#[inline]
fn is_dst_line(line: HunkLine<'_>, reverse: bool) -> bool {
    match (line, reverse) {
        (HunkLine::Context(_), _) => true,
        (HunkLine::Insert(_), false) | (HunkLine::Delete(_), true) => true,
        (HunkLine::Delete(_), false) | (HunkLine::Insert(_), true) => false,
    }
}

fn find_hunk_position<B: Allocator>(
    base_lines: &[&str],
    cursor: usize,
    ideal_pos: usize,
    max_offset: usize,
    hunk: &Hunk<'_, B>,
    src_count: usize,
    src_no_nl: bool,
    reverse: bool,
    hunk_idx: usize,
) -> Result<usize, ApplyPatchError> {
    if ideal_pos >= cursor
        && ideal_pos + src_count <= base_lines.len()
        && hunk_matches_at(base_lines, ideal_pos, hunk, src_no_nl, reverse)
    {
        return Ok(ideal_pos);
    }

    for delta in 1..=max_offset {
        if let Some(pos) = ideal_pos.checked_add(delta)
            && pos >= cursor
            && pos + src_count <= base_lines.len()
            && hunk_matches_at(base_lines, pos, hunk, src_no_nl, reverse)
        {
            return Ok(pos);
        }
        if let Some(pos) = ideal_pos.checked_sub(delta)
            && pos >= cursor
            && pos + src_count <= base_lines.len()
            && hunk_matches_at(base_lines, pos, hunk, src_no_nl, reverse)
        {
            return Ok(pos);
        }
    }

    Err(ApplyPatchError::HunkMismatch {
        hunk: hunk_idx,
        expected_line: if src_count == 0 { ideal_pos } else { ideal_pos + 1 },
    })
}

fn hunk_matches_at<B: Allocator>(
    base_lines: &[&str],
    start: usize,
    hunk: &Hunk<'_, B>,
    src_no_nl: bool,
    reverse: bool,
) -> bool {
    let last_src_idx = hunk.lines.as_slice().iter().rposition(|l| is_src_line(*l, reverse));
    let mut pos = start;
    for (line_idx, line) in hunk.lines.as_slice().iter().enumerate() {
        let expected_body = match (*line, reverse) {
            (HunkLine::Context(s), _) => Some(s),
            (HunkLine::Delete(s), false) | (HunkLine::Insert(s), true) => Some(s),
            (HunkLine::Insert(_), false) | (HunkLine::Delete(_), true) => None,
        };
        if let Some(expected_body) = expected_body {
            let Some(&actual_raw) = base_lines.get(pos) else {
                return false;
            };
            let (actual_body, actual_had_lf) = strip_lf(actual_raw);
            let expected_had_lf = !(src_no_nl && Some(line_idx) == last_src_idx);
            if actual_body != expected_body || actual_had_lf != expected_had_lf {
                return false;
            }
            pos += 1;
        }
    }
    true
}

impl From<AllocFailure> for ApplyPatchError {
    fn from(value: AllocFailure) -> Self {
        ApplyPatchError::Alloc(value)
    }
}
