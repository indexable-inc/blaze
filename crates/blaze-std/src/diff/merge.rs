//! Three-way text merge (`diff3`): merges `ours` and `theirs` against a common `base`, resolving
//! non-overlapping changes automatically and surfacing overlapping divergent edits as typed
//! [`MergeRegion::Conflict`] regions with standard Git or `diff3` conflict markers.
//!
//! Zero recursion, zero integer `as` casts, zero `.clone()`, zero panicking slice indexes.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

use super::tokenize::{split_lines, strip_lf};
use super::{Algorithm, DiffError, DiffOpts, Op, ops};

/// Conflict marker rendering style.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictStyle {
    /// Standard 2-pane Git conflict markers (`<<<<<<< ours` / `=======` / `>>>>>>> theirs`).
    Standard,
    /// `diff3` 3-pane markers including the common base (`||||||| base`).
    Diff3,
}

/// Options for [`merge3`].
#[derive(Clone, Copy, Debug)]
pub struct MergeOpts<'a, A: Allocator> {
    pub algorithm: Algorithm,
    pub budget: usize,
    pub style: ConflictStyle,
    pub base_label: &'a str,
    pub ours_label: &'a str,
    pub theirs_label: &'a str,
    pub alloc: A,
}

/// One region of a three-way merge output.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MergeRegion<'a, A: Allocator> {
    /// Cleanly merged lines (from `base`, `ours`, or `theirs`).
    Clean(Vec<&'a str, A>),
    /// Divergent edits to the same base span that require resolution.
    Conflict {
        base: Vec<&'a str, A>,
        ours: Vec<&'a str, A>,
        theirs: Vec<&'a str, A>,
    },
}

/// Structured result of [`merge3`].
#[derive(Debug)]
pub struct MergeResult<'a, A: Allocator> {
    pub regions: Vec<MergeRegion<'a, A>, A>,
    pub conflict_count: usize,
}

impl<A: Allocator> MergeResult<'_, A> {
    /// True when the merge completed with zero conflicts.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflict_count == 0
    }

    /// True when at least one conflicting region exists.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        self.conflict_count > 0
    }

    /// Render the merged result (with conflict markers for any conflict regions) into `out`.
    pub fn render_into<W: fmt::Write>(
        &self,
        style: ConflictStyle,
        base_label: &str,
        ours_label: &str,
        theirs_label: &str,
        out: &mut W,
    ) -> fmt::Result {
        for region in self.regions.as_slice() {
            match region {
                MergeRegion::Clean(lines) => {
                    for &line in lines.as_slice() {
                        out.write_str(line)?;
                    }
                }
                MergeRegion::Conflict { base, ours, theirs } => {
                    writeln!(out, "<<<<<<< {ours_label}")?;
                    write_conflict_lines(ours.as_slice(), out)?;
                    if style == ConflictStyle::Diff3 {
                        writeln!(out, "||||||| {base_label}")?;
                        write_conflict_lines(base.as_slice(), out)?;
                    }
                    out.write_str("=======\n")?;
                    write_conflict_lines(theirs.as_slice(), out)?;
                    writeln!(out, ">>>>>>> {theirs_label}")?;
                }
            }
        }
        Ok(())
    }

    /// Render the merged text into a [`String`] in `alloc`.
    pub fn render_in<B: Allocator>(
        &self,
        style: ConflictStyle,
        base_label: &str,
        ours_label: &str,
        theirs_label: &str,
        alloc: B,
    ) -> Result<String<B>, AllocFailure> {
        let mut out = String::new_in(alloc);
        self.render_into(style, base_label, ours_label, theirs_label, &mut out).map_err(|_| {
            AllocFailure::CapacityOverflow { what: "bstd::diff::merge3", len: out.len(), additional: 1 }
        })?;
        Ok(out)
    }
}

fn write_conflict_lines<W: fmt::Write>(lines: &[&str], out: &mut W) -> fmt::Result {
    for &raw in lines {
        let (body, has_nl) = strip_lf(raw);
        out.write_str(body)?;
        if has_nl || !body.is_empty() {
            out.write_char('\n')?;
        }
    }
    Ok(())
}

/// One change span against `base` (in 0-based base line indices `[base_start..base_end)`).
#[derive(Clone, Copy, Debug)]
struct SideSpan {
    base_start: usize,
    base_end: usize,
    side_start: usize,
    side_end: usize,
}

/// Extract non-equal change spans mapping base ranges to side ranges.
fn extract_spans<A: Allocator>(
    ops_list: &[Op],
    alloc: A,
) -> Result<Vec<SideSpan, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    let mut base_pos = 0usize;
    let mut side_pos = 0usize;

    let mut i = 0usize;
    while let Some(&op) = ops_list.get(i) {
        match op {
            Op::Equal { old_start, new_start, len } => {
                base_pos = old_start + len;
                side_pos = new_start + len;
                i += 1;
            }
            Op::Delete { .. } | Op::Insert { .. } => {
                let bs = base_pos;
                let ss = side_pos;
                while let Some(&cur) = ops_list.get(i) {
                    match cur {
                        Op::Delete { old_start, len } => {
                            base_pos = old_start + len;
                            i += 1;
                        }
                        Op::Insert { new_start, len } => {
                            side_pos = new_start + len;
                            i += 1;
                        }
                        Op::Equal { .. } => break,
                    }
                }
                out.try_push(SideSpan {
                    base_start: bs,
                    base_end: base_pos,
                    side_start: ss,
                    side_end: side_pos,
                })
                .map_err(|r| DiffError::Alloc(r.why))?;
            }
        }
    }
    Ok(out)
}

fn push_clean<'a, A: Allocator + Copy>(
    regions: &mut Vec<MergeRegion<'a, A>, A>,
    lines: &[&'a str],
    alloc: A,
) -> Result<(), DiffError> {
    if lines.is_empty() {
        return Ok(());
    }
    if let Some(MergeRegion::Clean(prev)) = regions.last_mut() {
        for &l in lines {
            prev.try_push(l).map_err(|r| DiffError::Alloc(r.why))?;
        }
        return Ok(());
    }
    let mut buf = Vec::new_in(alloc);
    for &l in lines {
        buf.try_push(l).map_err(|r| DiffError::Alloc(r.why))?;
    }
    regions.try_push(MergeRegion::Clean(buf)).map_err(|r| DiffError::Alloc(r.why))
}

fn collect_slice<'a, A: Allocator>(
    lines: &[&'a str],
    start: usize,
    end: usize,
    alloc: A,
) -> Result<Vec<&'a str, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    if let Some(slice) = lines.get(start..end) {
        for &l in slice {
            out.try_push(l).map_err(|r| DiffError::Alloc(r.why))?;
        }
    }
    Ok(out)
}

/// Reconstruct the side's lines corresponding to `base[win_start..win_end]` given the spans of
/// that side overlapping `[win_start..win_end]`.
fn materialize_window<'a, A: Allocator>(
    base_lines: &[&'a str],
    side_lines: &[&'a str],
    spans: &[SideSpan],
    win_start: usize,
    win_end: usize,
    alloc: A,
) -> Result<Vec<&'a str, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    let mut bpos = win_start;
    for sp in spans {
        if let Some(unchanged) = base_lines.get(bpos..sp.base_start) {
            for &l in unchanged {
                out.try_push(l).map_err(|r| DiffError::Alloc(r.why))?;
            }
        }
        if let Some(changed) = side_lines.get(sp.side_start..sp.side_end) {
            for &l in changed {
                out.try_push(l).map_err(|r| DiffError::Alloc(r.why))?;
            }
        }
        bpos = sp.base_end;
    }
    if let Some(tail) = base_lines.get(bpos..win_end) {
        for &l in tail {
            out.try_push(l).map_err(|r| DiffError::Alloc(r.why))?;
        }
    }
    Ok(out)
}

/// Perform a three-way line merge of `ours` and `theirs` against common ancestor `base`.
///
/// - Regions changed by only one side take that side cleanly.
/// - Regions where both sides made identical edits converge cleanly (no false conflict).
/// - Overlapping divergent edits produce [`MergeRegion::Conflict`].
pub fn merge3<'a, A: Allocator + Copy>(
    base: &'a str,
    ours: &'a str,
    theirs: &'a str,
    opts: MergeOpts<'_, A>,
) -> Result<MergeResult<'a, A>, DiffError> {
    let alloc = opts.alloc;
    let base_lines = split_lines(base, alloc)?;
    let ours_lines = split_lines(ours, alloc)?;
    let theirs_lines = split_lines(theirs, alloc)?;

    let diff_opts = DiffOpts { algorithm: opts.algorithm, budget: opts.budget, alloc };
    let ours_ops = ops(base_lines.as_slice(), ours_lines.as_slice(), diff_opts)?;
    let theirs_ops = ops(base_lines.as_slice(), theirs_lines.as_slice(), diff_opts)?;

    let ours_spans = extract_spans(ours_ops.as_slice(), alloc)?;
    let theirs_spans = extract_spans(theirs_ops.as_slice(), alloc)?;

    let mut regions: Vec<MergeRegion<'a, A>, A> = Vec::new_in(alloc);
    let mut conflict_count = 0usize;
    let mut base_cursor = 0usize;
    let mut oi = 0usize;
    let mut ti = 0usize;

    while oi < ours_spans.len() || ti < theirs_spans.len() {
        let next_o = ours_spans.get(oi).copied();
        let next_t = theirs_spans.get(ti).copied();

        let (win_start, mut win_end, o_start, mut o_end, t_start, mut t_end) =
            match (next_o, next_t) {
                (Some(o), Some(t)) => {
                    if o.base_start <= t.base_start {
                        (o.base_start, o.base_end, oi, oi + 1, ti, ti)
                    } else {
                        (t.base_start, t.base_end, oi, oi, ti, ti + 1)
                    }
                }
                (Some(o), None) => (o.base_start, o.base_end, oi, oi + 1, ti, ti),
                (None, Some(t)) => (t.base_start, t.base_end, oi, oi, ti, ti + 1),
                (None, None) => break,
            };

        // Expand the active window to absorb any overlapping or touching spans on either side.
        loop {
            let mut grew = false;
            if let Some(o) = ours_spans.get(o_end).copied()
                && (o.base_start < win_end
                    || (o.base_start == win_end && (o.base_start == o.base_end || win_start == win_end)))
            {
                if o.base_end > win_end {
                    win_end = o.base_end;
                }
                o_end += 1;
                grew = true;
            }
            if let Some(t) = theirs_spans.get(t_end).copied()
                && (t.base_start < win_end
                    || (t.base_start == win_end && (t.base_start == t.base_end || win_start == win_end)))
            {
                if t.base_end > win_end {
                    win_end = t.base_end;
                }
                t_end += 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }

        // Emit untouched base lines before this window.
        if let Some(unchanged) = base_lines.as_slice().get(base_cursor..win_start) {
            push_clean(&mut regions, unchanged, alloc)?;
        }

        let o_slice = ours_spans.as_slice().get(o_start..o_end).unwrap_or(&[]);
        let t_slice = theirs_spans.as_slice().get(t_start..t_end).unwrap_or(&[]);

        if o_slice.is_empty() {
            let mat = materialize_window(
                base_lines.as_slice(),
                theirs_lines.as_slice(),
                t_slice,
                win_start,
                win_end,
                alloc,
            )?;
            push_clean(&mut regions, mat.as_slice(), alloc)?;
        } else if t_slice.is_empty() {
            let mat = materialize_window(
                base_lines.as_slice(),
                ours_lines.as_slice(),
                o_slice,
                win_start,
                win_end,
                alloc,
            )?;
            push_clean(&mut regions, mat.as_slice(), alloc)?;
        } else {
            let ours_mat = materialize_window(
                base_lines.as_slice(),
                ours_lines.as_slice(),
                o_slice,
                win_start,
                win_end,
                alloc,
            )?;
            let theirs_mat = materialize_window(
                base_lines.as_slice(),
                theirs_lines.as_slice(),
                t_slice,
                win_start,
                win_end,
                alloc,
            )?;
            if ours_mat.as_slice() == theirs_mat.as_slice() {
                // False conflict: both sides made the exact same edit.
                push_clean(&mut regions, ours_mat.as_slice(), alloc)?;
            } else {
                let base_mat = collect_slice(base_lines.as_slice(), win_start, win_end, alloc)?;
                regions
                    .try_push(MergeRegion::Conflict {
                        base: base_mat,
                        ours: ours_mat,
                        theirs: theirs_mat,
                    })
                    .map_err(|r| DiffError::Alloc(r.why))?;
                conflict_count += 1;
            }
        }

        base_cursor = win_end;
        oi = o_end;
        ti = t_end;
    }

    if let Some(tail) = base_lines.as_slice().get(base_cursor..) {
        push_clean(&mut regions, tail, alloc)?;
    }

    Ok(MergeResult { regions, conflict_count })
}
