//! The fix offered for a one-segment value name that does not resolve (E0425): ranked by what is
//! left wrong after it (typeck.md, "Fixes ranked by what is left (2026-11-16)").
//!
//! The checker writes the FACTS (this file's [`Suggests`], a side table of `Checked`): for each
//! hole, the bindings in scope there whose type fits the hole's expected type, each with whether
//! the body uses it anywhere and how many scopes out it is declared. The CHOICE ([`pick`]) also
//! reads the names' spelling, which is text: the checker holds no text (a `Name` is a hash), so
//! the choice is a pure function of the record and the item's own text, which is already an
//! input of the body's key (every binding's spelling is its `Name`). A fix is data
//! ([`Fix`]: the hole's item-relative range and the item-relative range of the bytes that
//! replace it), never a formatted string; the words are made where the finding is drawn
//! (blaze-report `frame::words::HELPS`).
//!
//! Cost: nothing runs for a body with no unresolved one-segment value name: the walk only notes a
//! hole when a diagnostic was pushed at a path (`Machine::named_holes`), and [`collect`] returns
//! at once when there is none (`Suggests::steps == 0`). With holes, one pass over the body's
//! expressions builds the parent column and the use marks, then each hole walks its ancestors
//! and the patterns of the scopes it passes, shadowing checked in a hash set: O(exprs + pats)
//! per hole, the floor (`steps` counts every step; suite/suggest.rs: 200 locals, 404 steps).
//!
//! Not in this leg (typeck.md says how each extends): module items (needs a world question
//! listing a scope's value names), paths of more than one segment, type names, methods, fields.
//! No recursion: every walk is an explicit stack.

use blaze_own::{Alloc, Vec};

use crate::body::{BinOp, BodyView, Expr, Pat, Stmt};
use crate::diag::{Diag, DiagCode};
use crate::error::CheckError;
use crate::ids::{ExprId, PatId, Range};
use crate::portable::{PNode, PTy, PTys};

/// What a candidate is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Taken {
    /// A local binding or a parameter: its binding pattern.
    Local(PatId),
}

/// One name in scope at a hole whose type fits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cand {
    pub taken: Taken,
    /// The declared name, item-relative: a fix inserts the item's own bytes here.
    pub name: Range,
    /// Its type, in the record's type table.
    pub ty: PTy,
    /// The body reads it nowhere (the hole is no use: it did not resolve).
    pub unused: bool,
    /// Scopes between the hole and the declaration: 0 is the innermost.
    pub depth: u32,
}

/// One unresolved one-segment value name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Hole {
    /// The name as written, item-relative.
    pub at: Range,
    /// The type the context expects there, when one is known.
    pub expected: Option<PTy>,
    /// Its candidates: `Suggests::cands[first..first + len]`, innermost and latest first.
    pub first: u32,
    pub len: u32,
}

/// The side table: every hole of the body and their candidates.
#[derive(Debug, PartialEq, Eq)]
pub struct Suggests<'a> {
    pub holes: Vec<Hole, Alloc<'a>>,
    pub cands: Vec<Cand, Alloc<'a>>,
    /// The steps [`collect`] took: 0 for a body with no hole.
    pub steps: u32,
}

impl<'a> Suggests<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Suggests<'a> {
        Suggests { holes: Vec::new_in(alloc), cands: Vec::new_in(alloc), steps: 0 }
    }

    /// `hole`'s candidates.
    pub fn cands_of(&self, hole: &Hole) -> &[Cand] {
        let start = hole.first as usize;
        self.cands.get(start..start + hole.len as usize).unwrap_or(&[])
    }
}

/// A fix: the bytes at `at` replaced by the item's own bytes at `from` (both item-relative).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fix {
    pub at: Range,
    pub from: Range,
}

/// What the help says about a hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Nothing fits: no help.
    Nothing,
    /// The one candidate of the right type that nothing uses: "`b` is in scope, is a `u32`, and
    /// is never used".
    Unused(Cand),
    /// The one candidate spelled almost the same: "`count` is in scope and is spelled almost
    /// the same".
    Spelled(Cand),
    /// The best two are equal on every count: both are named, no fix is offered (a wrong fix is
    /// worse than none). Declared order.
    Tied(Cand, Cand),
}

impl Pick {
    /// The fix, for a pick of one.
    pub fn fix(self, hole: &Hole) -> Option<Fix> {
        match self {
            Pick::Unused(c) | Pick::Spelled(c) => Some(Fix { at: hole.at, from: c.name }),
            Pick::Nothing | Pick::Tied(..) => None,
        }
    }
}

/// Which rules [`pick`] applies: all of them, except in the control that shows rule (2)'s
/// effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rules {
    All,
    WithoutUnused,
}

/// The item's text, as `pick` reads names from it.
pub trait NameText {
    /// The bytes at `r`, item-relative; `None` when `r` is not in the text.
    fn bytes(&self, r: Range) -> Option<&[u8]>;
}

impl NameText for [u8] {
    fn bytes(&self, r: Range) -> Option<&[u8]> {
        self.get(r.start as usize..r.end as usize)
    }
}

/// Why `pick` could not decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickError {
    /// A record's range is not in the text it was given: the record and the text disagree.
    NotInText(Range),
}

/// The rank of a candidate: lower is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    /// 0: of the right type and used nowhere; 1 otherwise.
    used: u8,
    distance: u32,
    depth: u32,
}

/// The one candidate to offer for `hole`, or none (typeck.md, "Fixes ranked by what is left").
/// (1) type: done by the checker (only fitting candidates are recorded); (2) a candidate nothing
/// uses ranks first (a name with a leading `_` is unused on purpose and never counts); (3)
/// spelling: edit distance with transposition, at most `max(len, 3) / 3` for a candidate that (2)
/// does not rank; (4) nearness of declaration. Two best equal on every count: [`Pick::Tied`].
pub fn pick(s: &Suggests<'_>, hole: &Hole, text: &dyn NameText, rules: Rules, alloc: Alloc<'_>) -> Result<Pick, PickError> {
    let written = text.bytes(hole.at).ok_or(PickError::NotInText(hole.at))?;
    let limit = (written.len().max(3) / 3) as u32;
    let mut best: Option<(Rank, Cand)> = None;
    let mut second: Option<(Rank, Cand)> = None;
    let mut row: Vec<u32, Alloc<'_>> = Vec::new_in(alloc);
    for &c in s.cands_of(hole) {
        let pattern = text.bytes(c.name).ok_or(PickError::NotInText(c.name))?;
        // A binding pattern's range covers `ref`, `mut` and an `@ sub`: the name is its first
        // identifier that is not one of those keywords.
        let (from, name) = binding_name(pattern);
        let c = Cand { name: Range { start: c.name.start + from, end: c.name.start + from + name.len() as u32 }, ..c };
        let on_purpose = name.first() == Some(&b'_');
        let unused = rules == Rules::All && c.unused && !on_purpose;
        let distance = distance(written, name, &mut row);
        if !unused && distance > limit {
            continue;
        }
        let rank = Rank { used: u8::from(!unused), distance, depth: c.depth };
        // Candidates come innermost and latest first; a later one wins only when strictly
        // better, so the order of the record breaks nothing but true ties.
        match best {
            Some((b, _)) if rank >= b => {
                if second.is_none_or(|(s2, _)| rank < s2) {
                    second = Some((rank, c));
                }
            }
            _ => {
                second = best;
                best = Some((rank, c));
            }
        }
    }
    Ok(match (best, second) {
        (None, _) => Pick::Nothing,
        (Some((b, x)), Some((s2, y))) if b == s2 => {
            if x.name.start <= y.name.start { Pick::Tied(x, y) } else { Pick::Tied(y, x) }
        }
        (Some((b, c)), _) if b.used == 0 => Pick::Unused(c),
        (Some((_, c)), _) => Pick::Spelled(c),
    })
}

/// The binding's own name in a binding pattern's text, and where it starts.
fn binding_name(pattern: &[u8]) -> (u32, &[u8]) {
    let ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80;
    let mut i = 0;
    loop {
        while i < pattern.len() && !ident(pattern[i]) {
            i += 1;
        }
        let start = i;
        while i < pattern.len() && ident(pattern[i]) {
            i += 1;
        }
        let word = &pattern[start..i];
        if word != b"ref" && word != b"mut" || i >= pattern.len() {
            return (start as u32, word);
        }
    }
}

/// Edit distance with adjacent transposition (optimal string alignment), over bytes; `row`
/// is scratch of three rows.
fn distance(a: &[u8], b: &[u8], row: &mut Vec<u32, Alloc<'_>>) -> u32 {
    let w = b.len() + 1;
    row.clear();
    row.resize(3 * w, 0);
    for (j, cell) in row[..w].iter_mut().enumerate() {
        *cell = j as u32;
    }
    for i in 1..=a.len() {
        let cur = i % 3;
        let prev = (i - 1) % 3;
        let before = (i + 1) % 3;
        row[cur * w] = i as u32;
        for j in 1..w {
            let cost = u32::from(a[i - 1] != b[j - 1]);
            let mut d = (row[prev * w + j] + 1).min(row[cur * w + j - 1] + 1).min(row[prev * w + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(row[before * w + j - 2] + 1);
            }
            row[cur * w + j] = d;
        }
    }
    row[(a.len() % 3) * w + b.len()]
}

/// What `collect` reads of the finished check.
pub(crate) struct Facts<'r, 'a> {
    pub body: &'r BodyView<'r>,
    pub types: PTys<'r>,
    pub expr_ty: &'r [PTy],
    pub local_ty: &'r [PTy],
    pub diags: &'r [Diag],
    /// Each noted hole and the type its context expected (exported; `None`: no expectation).
    pub holes: &'r [(ExprId, Option<PTy>)],
    pub alloc: Alloc<'a>,
}

const NONE: u32 = u32::MAX;

/// The names bound so far in a hole's walk (a `Name`'s hash).
type Seen<'x> = blaze_hash::HashSet<u64, Alloc<'x>>;

/// Whether `t` says nothing to filter by (an error, a hole, a variable left).
fn unknown(types: PTys<'_>, t: PTy) -> Result<bool, CheckError> {
    Ok(matches!(types.node(t)?, PNode::Error | PNode::Infer))
}

/// The operator rule: the other operand's scalar type, for an operand of a builtin arithmetic,
/// bitwise or comparison operator (the Reference, "Arithmetic and Logical Binary Operators": both
/// sides of a builtin operator on scalars are one type).
fn operand_type(f: &Facts<'_, '_>, parent: &[u32], e: ExprId) -> Result<Option<PTy>, CheckError> {
    let Some(&p) = parent.get(e.ix()) else { return Ok(None) };
    if p == NONE {
        return Ok(None);
    }
    let Expr::Binary { op, lhs, rhs } = f.body.expr(ExprId(p))? else { return Ok(None) };
    if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::And | BinOp::Or) {
        return Ok(None);
    }
    let other = if lhs == e { rhs } else { lhs };
    let Some(&t) = f.expr_ty.get(other.ix()) else { return Ok(None) };
    Ok(match f.types.node(t)? {
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) => Some(t),
        _ => None,
    })
}

/// Whether a candidate of type `t` fits `expected`: equal, or either side unknown.
fn fits(types: PTys<'_>, t: PTy, expected: Option<PTy>) -> Result<bool, CheckError> {
    Ok(match expected {
        None => true,
        Some(x) => x == t || unknown(types, x)? || unknown(types, t)?,
    })
}

/// The side table for a finished check: nothing at all when no hole was noted.
pub(crate) fn collect<'a>(f: &Facts<'_, 'a>) -> Result<Suggests<'a>, CheckError> {
    let mut out = Suggests::new_in(f.alloc);
    // Only holes whose E0425 survived (a probe rolled back may have noted one).
    let live = |at: Range| f.diags.iter().any(|d| d.code == DiagCode::UnresolvedPath && d.at == at);
    if !f.holes.iter().any(|&(e, _)| live(f.body.expr_range(e))) {
        return Ok(out);
    }
    let n = f.body.exprs.len();
    let mut steps: u32 = 0;
    // The parent column and the use marks: one pass.
    let mut parent: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(n, f.alloc);
    parent.resize(n, NONE);
    let mut used: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(f.body.pats.len(), f.alloc);
    used.resize(f.body.pats.len(), false);
    let mut kids: Vec<ExprId, Alloc<'_>> = Vec::new_in(f.alloc);
    for (i, &e) in f.body.exprs.iter().enumerate() {
        steps += 1;
        if let Expr::Local(p) = e
            && let Some(u) = used.get_mut(p.ix())
        {
            *u = true;
        }
        kids.clear();
        f.body.children(e, &mut kids)?;
        for k in &kids {
            if let Some(slot) = parent.get_mut(k.ix()) {
                *slot = i as u32;
            }
        }
    }
    // Names already bound nearer the hole: membership only, never iterated.
    let mut seen: Seen<'_> = Seen::new_in(f.alloc);
    let mut pats: Vec<PatId, Alloc<'_>> = Vec::new_in(f.alloc);
    let mut lets: Vec<ExprId, Alloc<'_>> = Vec::new_in(f.alloc);
    for (hi, &(hole, expected)) in f.holes.iter().enumerate() {
        let at = f.body.expr_range(hole);
        // Once per place: a hole noted twice (a re-walk) is one.
        if !live(at) || f.holes[..hi].iter().any(|&(h, _)| h == hole) {
            continue;
        }
        let expected = match expected {
            Some(t) if !unknown(f.types, t)? => Some(t),
            _ => operand_type(f, &parent, hole)?,
        };
        let first = out.cands.len() as u32;
        seen.clear();
        let mut depth = 0u32;
        let mut child = hole;
        let mut locals_end = false;
        // Ancestors, innermost first: each scope's bindings visible at `child`.
        loop {
            steps += 1;
            let p = parent[child.ix()];
            if p == NONE {
                break;
            }
            let pe = ExprId(p);
            pats.clear();
            match f.body.expr(pe)? {
                Expr::Block { stmts, .. } | Expr::ConstBlock { stmts, .. } => {
                    let list = f.body.stmt_list(stmts)?;
                    let mut k = list.len();
                    for (j, &s) in list.iter().enumerate() {
                        let holds = match f.body.stmt(s)? {
                            Stmt::Let { init, else_, .. } => init == Some(child) || else_ == Some(child),
                            Stmt::Expr { expr, .. } => expr == child,
                            Stmt::Item => false,
                        };
                        if holds {
                            k = j;
                            break;
                        }
                    }
                    // Latest first.
                    for &s in list[..k].iter().rev() {
                        if let Stmt::Let { pat, .. } = f.body.stmt(s)? {
                            pats.push(pat);
                        }
                    }
                    locals_end = matches!(f.body.expr(pe)?, Expr::ConstBlock { .. });
                }
                Expr::Match { arms, .. } => {
                    for arm in f.body.arm_list(arms)? {
                        if arm.body == child || arm.guard == Some(child) {
                            pats.push(arm.pat);
                        }
                    }
                }
                Expr::Closure { params, body, .. } if body == child => {
                    pats.extend_from_slice(f.body.pat_list(params)?);
                }
                Expr::If { cond, then, .. } if then == child => lets.push(cond),
                Expr::Binary { op: BinOp::And, lhs, rhs } if rhs == child => lets.push(lhs),
                _ => {}
            }
            // `let` operands of a condition (a let chain): their patterns.
            while let Some(c) = lets.pop() {
                steps += 1;
                match f.body.expr(c)? {
                    Expr::Let { pat, .. } => pats.push(pat),
                    Expr::Binary { op: BinOp::And, lhs, rhs } => lets.extend([rhs, lhs]),
                    Expr::Paren(x) => lets.push(x),
                    _ => {}
                }
            }
            if !pats.is_empty() {
                add_bindings(f, &pats, depth, expected, &used, &mut seen, &mut out, &mut steps)?;
                depth += 1;
            }
            if locals_end {
                break;
            }
            child = pe;
        }
        if !locals_end {
            pats.clear();
            pats.extend_from_slice(f.body.params);
            add_bindings(f, &pats, depth, expected, &used, &mut seen, &mut out, &mut steps)?;
        }
        let len = out.cands.len() as u32 - first;
        out.holes.push(Hole { at, expected, first, len });
    }
    out.steps = steps;
    Ok(out)
}

/// The bindings of `roots` (in order; within a pattern, as written) not shadowed by a name
/// already seen, as candidates at `depth` when their type fits.
#[expect(clippy::too_many_arguments, reason = "one call site per scope kind; the walk's state")]
fn add_bindings<'a>(
    f: &Facts<'_, 'a>,
    roots: &[PatId],
    depth: u32,
    expected: Option<PTy>,
    used: &[bool],
    seen: &mut Seen<'_>,
    out: &mut Suggests<'a>,
    steps: &mut u32,
) -> Result<(), CheckError> {
    let mut stack: Vec<PatId, Alloc<'_>> = Vec::new_in(f.alloc);
    for &root in roots {
        stack.push(root);
        while let Some(p) = stack.pop() {
            *steps += 1;
            match f.body.pat(p)? {
                Pat::Bind { name, sub, .. } => {
                    stack.extend(sub);
                    if !seen.insert(name.0) {
                        continue;
                    }
                    let ty = f.local_ty.get(p.ix()).copied().ok_or(CheckError::Malformed(crate::error::Malformed::Pat(p)))?;
                    if fits(f.types, ty, expected)? {
                        let unused = !used.get(p.ix()).copied().unwrap_or(false);
                        out.cands.push(Cand { taken: Taken::Local(p), name: f.body.pat_range(p), ty, unused, depth });
                    }
                }
                Pat::Tuple { elems, .. } | Pat::Slice { elems, .. } | Pat::TupleStruct { elems, .. } => {
                    // Reversed, so the stack pops them as written.
                    stack.extend(f.body.pat_list(elems)?.iter().rev());
                }
                Pat::Ref { inner, .. } => stack.push(inner),
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    stack.extend(f.body.field_pat_list(fields)?.iter().rev().map(|fp| fp.pat))
                }
                // Every alternative binds the same names: the first says them.
                Pat::Or { alts } => stack.extend(f.body.pat_list(alts)?.first()),
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
            }
        }
    }
    Ok(())
}
