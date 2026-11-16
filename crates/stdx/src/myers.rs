//! Myers' O((N+M)D) difference algorithm in linear space ("An O(ND) Difference Algorithm and
//! Its Variations", 1986, section 4b: the middle snake, divide and conquer), and a unified-diff
//! renderer over it for text compared line by line.
//!
//! The edit script is minimal (as few deleted and inserted elements as any script) and
//! canonical: no two neighbouring ops share a tag, and within a run of changes the deletes come
//! before the inserts, as `diff -u` prints them.

use std::fmt::Write as _;
use std::ops::{Index, IndexMut, Range};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Equal,
    Delete,
    Insert,
}

/// One step of an edit script: `old[old]` and `new[new]` are equal (`Equal`, ranges of one
/// length), `old[old]` goes (`Delete`, `new` empty at the place), or `new[new]` comes in
/// (`Insert`, `old` empty at the place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    pub tag: Tag,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// The minimal edit script that turns `old` into `new`.
pub fn diff<T: PartialEq>(old: &[T], new: &[T]) -> Vec<Op> {
    let max_d = (old.len() + new.len()).div_ceil(2) + 1;
    let mut forward = V::new(max_d);
    let mut backward = V::new(max_d);
    // alloc: the script the caller asked for.
    let mut ops = Vec::new();
    conquer(old, 0..old.len(), new, 0..new.len(), &mut forward, &mut backward, &mut ops);
    ops
}

/// The diagonal-indexed furthest-reaching x of each path, `k` from `-max_d` to `max_d - 1`.
struct V {
    offset: isize,
    v: Vec<usize>,
}

impl V {
    fn new(max_d: usize) -> Self {
        // alloc: the two frontiers of one diff, linear in its input.
        Self { offset: max_d as isize, v: vec![0; 2 * max_d] }
    }
}

impl Index<isize> for V {
    type Output = usize;
    fn index(&self, k: isize) -> &usize {
        &self.v[(k + self.offset) as usize]
    }
}

impl IndexMut<isize> for V {
    fn index_mut(&mut self, k: isize) -> &mut usize {
        &mut self.v[(k + self.offset) as usize]
    }
}

fn prefix<T: PartialEq>(old: &[T], new: &[T]) -> usize {
    old.iter().zip(new).take_while(|(a, b)| a == b).count()
}

fn suffix<T: PartialEq>(old: &[T], new: &[T]) -> usize {
    old.iter().rev().zip(new.iter().rev()).take_while(|(a, b)| a == b).count()
}

fn conquer<T: PartialEq>(
    old: &[T],
    mut o: Range<usize>,
    new: &[T],
    mut n: Range<usize>,
    forward: &mut V,
    backward: &mut V,
    ops: &mut Vec<Op>,
) {
    let head = prefix(&old[o.clone()], &new[n.clone()]);
    push(ops, Tag::Equal, o.start..o.start + head, n.start..n.start + head);
    o.start += head;
    n.start += head;
    let tail = suffix(&old[o.clone()], &new[n.clone()]);
    o.end -= tail;
    n.end -= tail;
    if o.is_empty() || n.is_empty() {
        push(ops, Tag::Delete, o.clone(), n.start..n.start);
        push(ops, Tag::Insert, o.end..o.end, n.clone());
    } else {
        let (x, y) = middle_snake(old, o.clone(), new, n.clone(), forward, backward);
        conquer(old, o.start..x, new, n.start..y, forward, backward, ops);
        conquer(old, x..o.end, new, y..n.end, forward, backward, ops);
    }
    push(ops, Tag::Equal, o.end..o.end + tail, n.end..n.end + tail);
}

/// A point on an optimal path through the middle of the edit graph of `old[o]` and `new[n]`,
/// both non-empty, so that each side is a smaller problem.
fn middle_snake<T: PartialEq>(
    old: &[T],
    o: Range<usize>,
    new: &[T],
    n: Range<usize>,
    forward: &mut V,
    backward: &mut V,
) -> (usize, usize) {
    let (rows, cols) = (o.len(), n.len());
    let delta = rows as isize - cols as isize;
    let odd = delta & 1 == 1;
    forward[1] = 0;
    backward[1] = 0;
    // Both searches meet by d = ceil((rows + cols) / 2), so the loop always returns.
    for d in 0..=(rows + cols).div_ceil(2) as isize {
        for k in (-d..=d).rev().step_by(2) {
            let mut x = if k == -d || (k != d && forward[k - 1] < forward[k + 1]) {
                forward[k + 1]
            } else {
                forward[k - 1] + 1
            };
            let y = (x as isize - k) as usize;
            let start = (x, y);
            if x < rows && y < cols {
                x += prefix(&old[o.start + x..o.end], &new[n.start + y..n.end]);
            }
            forward[k] = x;
            if odd && (k - delta).abs() < d && forward[k] + backward[-(k - delta)] >= rows {
                return (o.start + start.0, n.start + start.1);
            }
        }
        for k in (-d..=d).rev().step_by(2) {
            let mut x = if k == -d || (k != d && backward[k - 1] < backward[k + 1]) {
                backward[k + 1]
            } else {
                backward[k - 1] + 1
            };
            let mut y = (x as isize - k) as usize;
            if x < rows && y < cols {
                let run = suffix(&old[o.start..o.end - x], &new[n.start..n.end - y]);
                x += run;
                y += run;
            }
            backward[k] = x;
            if !odd && (k - delta).abs() <= d && backward[k] + forward[-(k - delta)] >= rows {
                return (o.end - x, n.end - y);
            }
        }
    }
    unreachable!("the forward and backward searches of Myers' algorithm always meet")
}

/// Append one op, keeping the script canonical: empty ops are dropped, neighbours of one tag
/// merged, and a delete that follows an insert moved in front of it.
fn push(ops: &mut Vec<Op>, tag: Tag, old: Range<usize>, new: Range<usize>) {
    if old.is_empty() && new.is_empty() {
        return;
    }
    if tag == Tag::Delete
        && let Some(last) = ops.last()
        && last.tag == Tag::Insert
    {
        let Some(insert) = ops.pop() else { return };
        push(ops, Tag::Delete, old.clone(), insert.new.start..insert.new.start);
        push(ops, Tag::Insert, old.end..old.end, insert.new);
        return;
    }
    if let Some(last) = ops.last_mut()
        && last.tag == tag
    {
        last.old.end = old.end;
        last.new.end = new.end;
        return;
    }
    ops.push(Op { tag, old, new });
}

/// Append the unified diff of `old` against `new`, line by line with `context` lines of context
/// around each change, as `diff -u` writes it: a `--- {old_label}` and `+++ {new_label}` header
/// before the first hunk, `@@ -l,s +l,s @@` hunk headers, and `\ No newline at end of file` after
/// a last line without one. Nothing is appended when the texts are equal.
pub fn unified(
    out: &mut String,
    old: &str,
    new: &str,
    old_label: &str,
    new_label: &str,
    context: usize,
) {
    // alloc: the lines of the two texts, for one diff.
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    // alloc: as above.
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = diff(&old_lines, &new_lines);
    let mut header = false;
    for hunk in hunks(&ops, context) {
        if !header {
            for (sign, label) in [("--- ", old_label), ("+++ ", new_label)] {
                out.push_str(sign);
                out.push_str(label);
                out.push('\n');
            }
            header = true;
        }
        let (Some(first), Some(last)) = (hunk.first(), hunk.last()) else { continue };
        let old_range = HunkRange(first.old.start..last.old.end);
        let new_range = HunkRange(first.new.start..last.new.end);
        writeln!(out, "@@ -{old_range} +{new_range} @@").expect("a String takes any write");
        for op in hunk {
            let (sign, lines) = match op.tag {
                Tag::Equal => (' ', &old_lines[op.old.clone()]),
                Tag::Delete => ('-', &old_lines[op.old.clone()]),
                Tag::Insert => ('+', &new_lines[op.new.clone()]),
            };
            for line in lines {
                out.push(sign);
                out.push_str(line);
                if !line.ends_with('\n') {
                    out.push_str("\n\\ No newline at end of file\n");
                }
            }
        }
    }
}

/// The script cut into hunks: each change with up to `context` equal elements either side, and
/// two changes in one hunk when at most `2 * context` equal elements part them.
fn hunks(ops: &[Op], context: usize) -> Vec<Vec<Op>> {
    // alloc: a copy of the script to trim, for one rendered diff.
    let mut ops = ops.to_vec();
    // Context before the first change and after the last: at most `context`.
    if let Some(first) = ops.first_mut()
        && first.tag == Tag::Equal
    {
        let cut = first.old.len().saturating_sub(context);
        first.old.start += cut;
        first.new.start += cut;
    }
    if let Some(last) = ops.last_mut()
        && last.tag == Tag::Equal
    {
        let cut = last.old.len().saturating_sub(context);
        last.old.end -= cut;
        last.new.end -= cut;
    }
    // alloc: the hunks of one rendered diff.
    let mut hunks = Vec::new();
    // alloc: as above.
    let mut current = Vec::new();
    for op in ops {
        if op.tag == Tag::Equal && op.old.len() > 2 * context {
            // A long equal run ends this hunk and starts the next. Only one between two changes
            // is this long: the first and the last were trimmed to `context` above.
            let (o, n) = (op.old, op.new);
            current.push(Op {
                tag: Tag::Equal,
                old: o.start..o.start + context,
                new: n.start..n.start + context,
            });
            hunks.push(std::mem::take(&mut current));
            current.push(Op {
                tag: Tag::Equal,
                old: o.end - context..o.end,
                new: n.end - context..n.end,
            });
            continue;
        }
        current.push(op);
    }
    if current.iter().any(|op| op.tag != Tag::Equal) {
        hunks.push(current);
    }
    hunks
}

/// A hunk header's range: `start,len` 1-based, `start` alone for one line, and the line before
/// for an empty range.
struct HunkRange(Range<usize>);

impl std::fmt::Display for HunkRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.0.len();
        match len {
            1 => write!(f, "{}", self.0.start + 1),
            0 => write!(f, "{},0", self.0.start),
            _ => write!(f, "{},{len}", self.0.start + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Config, Tape};

    use super::*;

    /// Short sequences over a three-letter alphabet, so that equal runs and changes both occur.
    fn sequence(tape: &Tape) -> Vec<u8> {
        tape.vec(0..=8, "elements", |tape| tape.int_in(0..=2u8, "element"))
    }

    /// The length of a longest common subsequence, by the textbook table.
    fn lcs(a: &[u8], b: &[u8]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                table[i][j] = if a[i] == b[j] {
                    table[i + 1][j + 1] + 1
                } else {
                    table[i + 1][j].max(table[i][j + 1])
                };
            }
        }
        table[0][0]
    }

    #[test]
    fn script_applies_and_is_minimal_and_canonical() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let old = sequence(tape);
            let new = sequence(tape);
            let ops = diff(&old, &new);
            let (mut at_old, mut at_new) = (0, 0);
            let mut rebuilt = Vec::new();
            let mut changed = 0;
            for op in &ops {
                assert_eq!((op.old.start, op.new.start), (at_old, at_new), "ops are contiguous");
                match op.tag {
                    Tag::Equal => {
                        assert_eq!(old[op.old.clone()], new[op.new.clone()]);
                        rebuilt.extend_from_slice(&old[op.old.clone()]);
                    }
                    Tag::Delete => assert!(op.new.is_empty() && !op.old.is_empty()),
                    Tag::Insert => {
                        assert!(op.old.is_empty() && !op.new.is_empty());
                        rebuilt.extend_from_slice(&new[op.new.clone()]);
                    }
                }
                if op.tag != Tag::Equal {
                    changed += op.old.len() + op.new.len();
                }
                (at_old, at_new) = (op.old.end, op.new.end);
            }
            assert_eq!((at_old, at_new), (old.len(), new.len()), "ops cover both sides");
            assert_eq!(rebuilt, new, "the script turns old into new");
            assert_eq!(changed, old.len() + new.len() - 2 * lcs(&old, &new), "minimal");
            for pair in ops.windows(2) {
                assert_ne!(pair[0].tag, pair[1].tag, "neighbours are merged");
                assert!(
                    !(pair[0].tag == Tag::Insert && pair[1].tag == Tag::Delete),
                    "deletes come first"
                );
            }
        });
    }

    #[test]
    fn unified_reads_as_diff_u() {
        let mut out = String::new();
        unified(&mut out, "a\nb\nc\n", "a\nx\nc\n", "a/f", "b/f", 3);
        assert_eq!(out, "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+x\n c\n");
        let mut out = String::new();
        unified(&mut out, "old", "new\n", "a/f", "b/f", 3);
        assert_eq!(out, "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n");
        let mut out = String::new();
        unified(&mut out, "same\n", "same\n", "a/f", "b/f", 3);
        assert_eq!(out, "");
        let line = |i: usize| match i {
            2 => "two\n".to_owned(),
            17 => "seventeen\n".to_owned(),
            _ => format!("{i}\n"),
        };
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        let new: String = (0..20).map(line).collect();
        let mut out = String::new();
        unified(&mut out, &old, &new, "a", "b", 3);
        assert_eq!(
            out,
            "--- a\n+++ b\n@@ -1,6 +1,6 @@\n 0\n 1\n-2\n+two\n 3\n 4\n 5\n\
             @@ -15,6 +15,6 @@\n 14\n 15\n 16\n-17\n+seventeen\n 18\n 19\n"
        );
    }
}
