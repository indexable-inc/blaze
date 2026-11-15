//! Nested text edits compose. A capture of a `replace` is a metavariable of its binding bound to
//! a node in the replaced range: one strictly inside it, or the replaced node itself when the
//! template writes it. An edit whose node lies inside a capture is written first, and the
//! replace is written from the capture's text with that edit made, so `$r.expect(..)` over
//! `w.join().unwrap().unwrap()` writes the inner match's rewrite where `$r` lands. Actions are
//! written smallest node first, so nesting composes at any depth, and every edit keeps the
//! original file's offsets: an inner edit leaves the request, its text carried by the outer.
//!
//! A capture the template writes twice carries the rewrite twice; one it never writes drops
//! the edits inside it with its text, and a `delete` is a replace whose template writes no
//! capture. Still refused as an overlap naming both rules: an edit in the replaced range that
//! is inside no capture (it edits text the template replaces, straddles a capture's edge, or
//! sits in a `$$$run`, which binds as text and has no place), two edits inside one capture that
//! overlap, and two edits of one range or two insertions at one offset (`without_overlap`).

use std::alloc::{Allocator, Global};
use std::cmp::Ordering;
use std::ops::Range;

use astlog_engine::{Segment, TextKind, Value};
use blaze_intent::{Files, TextEdit};
use syntax::{TextRange, TextSize};

use super::{Made, Placed, Res, Written, file_text, locate, overlap, overlaps, same_edit, text_edit};
use crate::sema::{Node, Sema};
use crate::splice::Rewritten;

/// A written text edit, kept until every action that could hold it is written.
struct Done<'p> {
    made: Made<'p>,
    /// The range of the node it acts on.
    node: TextRange,
    /// Its replacement without the parentheses its old parent asked for: the text it reads
    /// as when it is a whole capture, whose new place decides them.
    bare: Range<usize>,
    /// The range of the replace whose capture took it in.
    under: Option<TextRange>,
}

/// Write the text actions `placed` into `out` as edits of the original files, each nested one
/// in the capture of the replace that holds it.
///
/// # Errors
///
/// An action's own failure to write, and an edit inside a replace and inside none of its
/// captures, or two that overlap inside one capture, naming both rules.
pub(super) fn compose<'a, 'p, F: Files, A: Allocator, B: Allocator>(
    facts: &Sema<'a, F>,
    mut placed: Vec<Placed<'a, '_, 'p>, A>,
    out: &mut Vec<Made<'p>, B>,
) -> Res<()> {
    // Smallest node first: a node inside another's range is shorter than it.
    placed.sort_by(|a, b| {
        (&a.path, a.range.len(), a.range.start()).cmp(&(&b.path, b.range.len(), b.range.start()))
    });
    let mut done = Vec::with_capacity_in(placed.len(), Global);
    // Reused per action: the edits it holds, its captures, and their rewritten text.
    let mut inner = Vec::new_in(Global);
    let mut captures = Vec::new_in(Global);
    let mut rewritten = Vec::new_in(Global);
    // Where the file being written starts in `done`: the actions are sorted by file, so an
    // action looks for the edits it holds among its own file's, not the whole request's.
    let mut file_from = 0;
    for action in placed {
        rewritten.clear();
        if done.last().is_some_and(|last: &Done<'_>| last.made.edit.path != action.path) {
            file_from = done.len();
        }
        let (node, by) = (action.range, action.by);
        // A delete is a replace whose template writes nothing; an insertion holds no text.
        if matches!(action.kind, TextKind::Replace | TextKind::Delete) {
            inner.clear();
            inner.extend((file_from..done.len()).filter(|&at| within(&done[at], &action)));
            inner.sort_by(|&a: &usize, &b: &usize| by_place(&done[a].made.edit, &done[b].made.edit));
            captures_of(facts, &action, &mut captures);
            let loose = inner.iter().find(|&&at| !captures.iter().any(|(_, it)| holds(*it, &done[at])));
            if let Some(&at) = loose {
                return Err(overlap(facts.files, by, &done[at].made));
            }
            for &(var, capture) in &captures {
                let source = file_text(facts.files, &action.path, capture)?;
                // Offsets in `source`: every edit held lies in the capture, sorted, apart.
                let local = |at: TextSize| usize::from(at - capture.start());
                let mut text = String::new();
                let mut from = capture.start();
                let mut last: Option<&Done<'_>> = None;
                for it in inner.iter().map(|&at| &done[at]).filter(|it| holds(capture, it)) {
                    let edit = &it.made.edit;
                    if let Some(last) = last {
                        if same_edit(&last.made.edit, edit) {
                            continue;
                        }
                        if overlaps(&last.made.edit, edit) {
                            return Err(overlap(facts.files, last.made.by, &it.made));
                        }
                    }
                    text.push_str(&source[local(from)..local(edit.range.start())]);
                    let whole = edit.range == capture;
                    text.push_str(if whole { &edit.text[it.bare.clone()] } else { &edit.text });
                    from = edit.range.end();
                    last = Some(it);
                }
                if last.is_some() {
                    text.push_str(&source[local(from)..]);
                    rewritten.push((var, text));
                }
            }
            for &at in &inner {
                done[at].under = Some(node);
            }
        }
        let (edit, bare) = text_edit(facts, action, Rewritten(&rewritten))?;
        done.push(Done { made: Made { edit, by }, node, bare, under: None });
    }
    out.extend(done.into_iter().filter(|it| it.under.is_none()).map(|it| it.made));
    Ok(())
}

/// Whether `it` acts on a node strictly inside the range `action` replaces, and no other
/// replace took it in: one of the same range, the same replace asked by another row, may.
fn within(it: &Done<'_>, action: &Placed<'_, '_, '_>) -> bool {
    it.made.edit.path == action.path
        && action.range.contains_range(it.node)
        && it.node != action.range
        && it.under.is_none_or(|under| under == action.range)
}

/// Whether the capture at `capture` holds `it`: its node and the text it edits.
fn holds(capture: TextRange, it: &Done<'_>) -> bool {
    capture.contains_range(it.node) && capture.contains_range(it.made.edit.range)
}

/// Edits in the order they are made: by offset, then end, then text, so the same edit twice
/// is adjacent.
fn by_place(a: &TextEdit, b: &TextEdit) -> Ordering {
    (a.range.start(), a.range.end(), &a.text).cmp(&(b.range.start(), b.range.end(), &b.text))
}

/// Into `into`, by name, the captures of `action`: each metavariable of its binding bound to a
/// node of its file inside the replaced range, the replaced node itself only when the template
/// writes it. A node a macro made has no text of the file, so it holds no edit and is none.
fn captures_of<'w, F: Files, A: Allocator>(
    facts: &Sema<'_, F>,
    action: &Placed<'_, 'w, '_>,
    into: &mut Vec<(&'w str, TextRange), A>,
) {
    into.clear();
    let Written { template, binding, .. } = action.written;
    for (var, value) in binding {
        let Value::Node(node @ Node::Syn { .. }) = value else { continue };
        let Some(Ok((path, range))) = facts.view_of(*node).map(|it| locate(facts, it)) else { continue };
        let written = || template.segments.iter().any(|it| matches!(it, Segment::Var(it) if it == var));
        if path == action.path && action.range.contains_range(range) && (range != action.range || written()) {
            into.push((var.as_str(), range));
        }
    }
    into.sort_unstable_by(|a, b| a.0.cmp(b.0));
}
