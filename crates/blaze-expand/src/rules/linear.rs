//! The linear matcher: an arm's locations run over the call's tokens left to right, with the
//! bindings written once, in the form the transcriber reads.
//!
//! The Reference, "Macros By Example", is the rule. "When matching, no lookahead is performed;
//! if the compiler cannot unambiguously determine how to parse the macro invocation one token
//! at a time, then it is an error": so a place in a matcher is decided by the token under it.
//! A repetition's start and end are the only places with two ways on (stay or leave), and each
//! way is looked at before a position is made for it ([`fit`]): the way whose next location is
//! a fixed token that is not the input's token is never taken. What is left is almost always
//! ONE position, and one position writes its bindings straight into [`Flat`]. Where two ways
//! both take the token (`$($x:ident),* $(,)?` at a `,`), the positions go on side by side, as
//! the rule's "one token at a time" has it, each with the few events it has made since they
//! parted ([`Event`]); the one that survives writes its events and goes on alone.
//!
//! A fragment is parsed once, where exactly one position waits on it, by the grammar's entry
//! for its kind (`matcher::match_meta_var`), and bound as a range of the input's tokens.
//! A repetition of one `tt` that runs to its group's close (`{ $($body:tt)* }`) is taken in
//! one loop over the group's trees.
//!
//! Bindings ([`Flat`]): per binder, its fragments in the order bound; per repetition, for each
//! time it was reached, how many iterations had been made before. "A metavariable must appear
//! in exactly the same number, kind, and nesting order of repetitions in the transcriber as it
//! did in the matcher": so a binder's fragment for the iterations `[i1, i2, ..]` is found by
//! one offset per level, with no tree.
//!
//! What this does not decide is left to the exact mode (`matcher`, the position-set matcher,
//! which also makes a failing call's diagnostic): an arm that binds one name twice (rustc
//! refuses the definition: "duplicate matcher binding"), and a step where a fixed token and a
//! separator of another length take the same token ([`Outcome::Exact`]).

use blaze_mem::frame::Frame;
use blaze_own::{AVec, Alloc, Vec};

use super::{
    Arm, Bound as Refused, DEPTH_BOUND, Def, Engine, Frag, Loc, NONE, Program, RepKind,
    input::{Level, View, same_puncts},
    matcher::{Fragment, Origin, Toks, expect_separator, expect_tt, match_meta_var},
};
use crate::tokens::{Delim, Raw, TokenKind};

/// What a node of a match's bindings is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NodeKind {
    /// A repetition that matched no time.
    Empty,
    Fragment(Fragment),
    /// What a failed match bound (the exact mode's bindings alone).
    Missing(Frag),
    /// A repetition's iterations, this many.
    Nested(u32),
}

/// A match's bindings as the transcriber reads them: the seam between the matchers
/// (`strategy-is-a-type-parameter`).
pub(crate) trait Values {
    type Node: Copy;
    /// The binder's value outside every repetition; `None` for a name no binder declares.
    fn root(&self, binder: u32) -> Option<Self::Node>;
    fn kind(&self, node: Self::Node) -> NodeKind;
    /// Iteration `idx` of a [`NodeKind::Nested`] node.
    fn child(&self, node: Self::Node, idx: u32) -> Self::Node;
}

/// An arm's nesting.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ArmNest {
    /// The arm's repetitions are `Program::reps[rep_lo..rep_lo + reps]`.
    pub(crate) rep_lo: u32,
    pub(crate) reps: u32,
    /// The arm binds one name twice.
    pub(crate) dup: bool,
}

/// The nesting of a definition's matchers, read from its locations once a definition.
pub(crate) struct Nests<'a> {
    /// For each binder (`Arm::names + binder`), the innermost repetition around it, or [`NONE`].
    var_rep: AVec<'a, u32>,
    /// For each repetition, the one around it, or [`NONE`].
    rep_parent: AVec<'a, u32>,
    /// For each repetition, how many are around it.
    rep_depth: AVec<'a, u32>,
    pub(crate) arms: AVec<'a, ArmNest>,
}

impl<'a> Nests<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Self {
        Nests {
            var_rep: Vec::new_in(alloc),
            rep_parent: Vec::new_in(alloc),
            rep_depth: Vec::new_in(alloc),
            arms: Vec::new_in(alloc),
        }
    }
}

/// Fills `program.nests` from its arms' locations. A definition with an error has none: no
/// call of it is matched.
pub(crate) fn nest(program: &mut Program<'_>) {
    const UNSEEN: u32 = NONE - 1;
    if program.error.is_some() {
        return;
    }
    let Program { arms, locs, reps, names, nests, .. } = program;
    nests.var_rep.resize(names.len(), UNSEEN);
    nests.rep_parent.resize(reps.len(), NONE);
    nests.rep_depth.resize(reps.len(), 0);
    for arm in arms.iter() {
        let (mut cur, mut depth) = (NONE, 0u32);
        let (mut lo, mut hi) = (NONE, 0u32);
        let mut dup = false;
        let mut at = arm.locs as usize;
        loop {
            match locs[at] {
                Loc::Eof => break,
                Loc::RepStart(rep) => {
                    nests.rep_parent[rep as usize] = cur;
                    nests.rep_depth[rep as usize] = depth;
                    cur = rep;
                    depth += 1;
                    lo = lo.min(rep);
                    hi = hi.max(rep + 1);
                }
                Loc::RepEnd(rep) => {
                    cur = nests.rep_parent[rep as usize];
                    depth -= 1;
                }
                Loc::Var { binder, .. } => {
                    let slot = &mut nests.var_rep[(arm.names + binder) as usize];
                    dup |= *slot != UNSEEN;
                    *slot = cur;
                }
                _ => {}
            }
            at += 1;
        }
        let rep_lo = if lo == NONE { 0 } else { lo };
        nests.arms.push(ArmNest { rep_lo, reps: hi.saturating_sub(rep_lo), dup });
    }
}

/// What a position did to the bindings.
#[derive(Clone, Copy, Debug)]
enum Event {
    Bind(u32, Fragment),
    /// The repetition was reached (and may make no iteration).
    Enter(u32),
    /// An iteration of the repetition began.
    Iter(u32),
}

/// The bindings of one match, flat.
pub(crate) struct Flat<'p, 'f> {
    nests: &'p Nests<'p>,
    frame: &'f Frame,
    /// The arm's `Arm::names`, binder count and first repetition.
    names: u32,
    binders: u32,
    rep_lo: u32,
    /// Per binder, its fragments in the order bound: one per iteration of its innermost
    /// repetition, counted over the whole match.
    vals: Vec<Vec<Fragment, &'f Frame>, &'f Frame>,
    /// Per repetition, for each time it was reached (once per iteration of the repetition
    /// around it), how many iterations it had made before.
    offs: Vec<Vec<u32, &'f Frame>, &'f Frame>,
    /// Per repetition, its iterations so far.
    total: Vec<u32, &'f Frame>,
}

/// A node of [`Flat`]: `binder` under `level` repetitions, at the iteration `ord` of the
/// innermost of them (counted over the whole match).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct FlatNode {
    binder: u32,
    level: u32,
    ord: u32,
}

impl<'p, 'f> Flat<'p, 'f> {
    fn begin(&mut self, arm: &Arm, nest: ArmNest) {
        (self.names, self.binders, self.rep_lo) = (arm.names, arm.binders, nest.rep_lo);
        let (binders, reps) = (arm.binders as usize, nest.reps as usize);
        while self.vals.len() < binders {
            self.vals.push(Vec::new_in(self.frame));
        }
        while self.offs.len() < reps {
            self.offs.push(Vec::new_in(self.frame));
        }
        for vals in self.vals[..binders].iter_mut() {
            vals.clear();
        }
        for offs in self.offs[..reps].iter_mut() {
            offs.clear();
        }
        self.total.clear();
        self.total.resize(reps, 0);
    }

    #[inline]
    fn apply(&mut self, event: Event) {
        match event {
            Event::Bind(binder, fragment) => self.vals[binder as usize].push(fragment),
            Event::Enter(rep) => {
                let rep = (rep - self.rep_lo) as usize;
                let total = self.total[rep];
                self.offs[rep].push(total);
            }
            Event::Iter(rep) => self.total[(rep - self.rep_lo) as usize] += 1,
        }
    }

    /// The repetition `level` repetitions deep around `binder`, or `None` when the binder is
    /// under only `level` of them.
    fn rep_at(&self, binder: u32, level: u32) -> Option<u32> {
        let mut rep = self.nests.var_rep[(self.names + binder) as usize];
        if rep == NONE {
            return None;
        }
        let depth = self.nests.rep_depth[rep as usize] + 1;
        if level >= depth {
            return None;
        }
        for _ in 0..depth - 1 - level {
            rep = self.nests.rep_parent[rep as usize];
        }
        Some(rep)
    }
}

impl Values for Flat<'_, '_> {
    type Node = FlatNode;

    fn root(&self, binder: u32) -> Option<FlatNode> {
        (binder < self.binders).then_some(FlatNode { binder, level: 0, ord: 0 })
    }

    fn kind(&self, node: FlatNode) -> NodeKind {
        match self.rep_at(node.binder, node.level) {
            None => NodeKind::Fragment(self.vals[node.binder as usize][node.ord as usize]),
            Some(rep) => {
                let rep = (rep - self.rep_lo) as usize;
                let offs = &self.offs[rep];
                let start = offs[node.ord as usize];
                let end = offs.get(node.ord as usize + 1).copied().unwrap_or(self.total[rep]);
                match end - start {
                    0 => NodeKind::Empty,
                    len => NodeKind::Nested(len),
                }
            }
        }
    }

    fn child(&self, node: FlatNode, idx: u32) -> FlatNode {
        let rep = self.rep_at(node.binder, node.level).expect("a nested node is of a repetition");
        let start = self.offs[(rep - self.rep_lo) as usize][node.ord as usize];
        FlatNode { binder: node.binder, level: node.level + 1, ord: start + idx }
    }
}

/// Whether two bindings are the same values: every binder's node, and under it every
/// iteration's. The oracle's comparison of the two matchers.
pub(crate) fn same<A: Values, B: Values>(a: &A, b: &B, binders: u32, frame: &Frame) -> bool {
    let mut work: Vec<(A::Node, B::Node), &Frame> = Vec::new_in(frame);
    for binder in 0..binders {
        match (a.root(binder), b.root(binder)) {
            (Some(x), Some(y)) => work.push((x, y)),
            (None, None) => {}
            _ => return false,
        }
    }
    while let Some((x, y)) = work.pop() {
        let (kx, ky) = (a.kind(x), b.kind(y));
        if kx != ky {
            return false;
        }
        if let NodeKind::Nested(len) = kx {
            for idx in 0..len {
                work.push((a.child(x, idx), b.child(y, idx)));
            }
        }
    }
    true
}

/// A position: a place in the arm's matcher, and the events it has made since it parted from
/// the other positions (`Linear::events[_]`, newest first).
#[derive(Clone, Copy, Debug)]
struct Thread {
    dot: u32,
    pend: u32,
    /// Where the input goes on once the token this position waits on is taken, when that is
    /// not the next token: glued punctuation, a separator.
    after: u32,
    sep: bool,
}

/// How a location takes the token under it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    /// Not a fixed token: a metavariable, a repetition's start or end.
    Later,
    No,
    /// Takes the token; the input goes on at this index, or after the token ([`NONE`]).
    Token(u32),
    /// A group of the pattern over a group of the input: the next location takes its open.
    Inside,
    /// The matcher's end at the input's end.
    End,
}

/// The token under the positions of one step.
#[derive(Clone, Copy)]
struct Under {
    src: Level,
    /// The group's end, where a `Close` is taken.
    closed: bool,
    /// The input's end.
    at_end: bool,
}

/// How `loc` takes the token under it, by the token alone.
fn fit(view: &View<'_, '_>, def: Def<'_>, loc: Loc, under: Under, glued: &mut Option<u8>) -> Fit {
    let src = under.src;
    let token = (!src.is_empty()).then_some(src.at);
    let taken = |yes: bool| if yes { Fit::Token(NONE) } else { Fit::No };
    match loc {
        Loc::Var { .. } | Loc::RepStart(_) | Loc::RepEnd(_) => Fit::Later,
        Loc::Eof => match under.at_end {
            true => Fit::End,
            false => Fit::No,
        },
        Loc::Subtree(delim) => match token {
            Some(at) if view.kind(at) == TokenKind::Open(delim) => Fit::Inside,
            _ => Fit::No,
        },
        Loc::Open(_) => taken(token.is_some_and(|at| matches!(view.kind(at), TokenKind::Open(_)))),
        Loc::Close(_) => taken(under.closed),
        Loc::Literal(lhs) => taken(match token.map(|at| view.kind(at)) {
            Some(TokenKind::Literal { kind, .. }) => {
                matches!(def.kind(lhs), TokenKind::Literal { kind: k, .. } if k == kind)
                    && view.bytes(src.at) == def.bytes(lhs)
            }
            _ => false,
        }),
        Loc::Ident(lhs) => taken(match (token.map(|at| view.kind(at)), def.kind(lhs)) {
            (Some(TokenKind::Ident { raw, .. }), TokenKind::Ident { raw: want, .. }) => {
                raw == want && view.bytes(src.at) == def.bytes(lhs)
            }
            _ => false,
        }),
        Loc::Crate(_) => taken(match token.map(|at| view.kind(at)) {
            Some(TokenKind::Ident { raw, .. }) => raw == Raw::No && view.bytes(src.at) == b"$crate",
            _ => false,
        }),
        Loc::Punct { at: lhs, len: lhs_len } => {
            let len = *glued.get_or_insert_with(|| match token {
                Some(at) => view.glued(at, src.end),
                None => 0,
            });
            match len > 0 && same_puncts(view, src.at, len, def, lhs, lhs_len) {
                true => Fit::Token(src.at + u32::from(len)),
                false => Fit::No,
            }
        }
    }
}

/// What the linear matcher says of an arm.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Outcome {
    /// The arm matches: its bindings are in [`Linear::flat`].
    Matched,
    Failed,
    /// Not decided here: the exact mode matches the arm (the module's last paragraph).
    Exact,
}

/// The linear matcher's lists, kept across the arms of a call.
pub(crate) struct Linear<'p, 'f> {
    work: Vec<Thread, &'f Frame>,
    next: Vec<Thread, &'f Frame>,
    eof: Vec<Thread, &'f Frame>,
    /// The positions waiting on a fragment: the fragment, and the input after it.
    bb: Vec<(Thread, Fragment, u32), &'f Frame>,
    /// The events of positions not alone: the event, and the one before it or [`NONE`].
    events: Vec<(Event, u32), &'f Frame>,
    order: Vec<u32, &'f Frame>,
    /// The input after each group the positions are inside.
    stack: Vec<Level, &'f Frame>,
    pub(crate) flat: Flat<'p, 'f>,
}

impl<'p, 'f> Linear<'p, 'f> {
    pub(crate) fn new_in(program: &'p Program<'p>, frame: &'f Frame) -> Self {
        Linear {
            work: Vec::new_in(frame),
            next: Vec::new_in(frame),
            eof: Vec::new_in(frame),
            bb: Vec::new_in(frame),
            events: Vec::new_in(frame),
            order: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            flat: Flat {
                nests: &program.nests,
                frame,
                names: 0,
                binders: 0,
                rep_lo: 0,
                vals: Vec::new_in(frame),
                offs: Vec::new_in(frame),
                total: Vec::new_in(frame),
            },
        }
    }

    /// Writes the events ending at `pend`, oldest first: the position that made them is the
    /// only one left, so every other event is of a position that ended.
    fn flush(&mut self, mut pend: u32) {
        while pend != NONE {
            self.order.push(pend);
            pend = self.events[pend as usize].1;
        }
        while let Some(at) = self.order.pop() {
            self.flat.apply(self.events[at as usize].0);
        }
        self.events.clear();
    }

    /// An event of `thread`: written now when it is `alone`, else kept with the position.
    #[inline]
    fn emit(&mut self, thread: &mut Thread, event: Event, alone: bool) {
        match alone {
            true => self.flat.apply(event),
            false => {
                self.events.push((event, thread.pend));
                thread.pend = crate::tokens::index(self.events.len() - 1);
            }
        }
    }
}

/// Matches arm `arm_idx` against the call's tokens into `m.flat`.
pub(crate) fn match_<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    arm_idx: usize,
    view: &mut View<'_, 'f>,
    m: &mut Linear<'_, 'f>,
) -> Result<Outcome, Refused> {
    let program = engine.program;
    let def = engine.def;
    let arm = program.arms[arm_idx];
    let nest = program.nests.arms[arm_idx];
    if nest.dup {
        return Ok(Outcome::Exact);
    }
    m.flat.begin(&arm, nest);
    m.work.clear();
    m.next.clear();
    m.events.clear();
    m.stack.clear();
    let mut src = view.top();
    m.work.push(Thread { dot: arm.locs, pend: NONE, after: NONE, sep: false });
    'step: loop {
        m.bb.clear();
        m.eof.clear();
        let under = Under {
            src,
            closed: src.is_empty() && !m.stack.is_empty(),
            at_end: src.is_empty() && m.stack.is_empty(),
        };
        let mut glued: Option<u8> = None;
        while let Some(mut t) = m.work.pop() {
            engine.budget.step()?;
            // The only position there is: what it has made is the match's.
            let solo = m.work.is_empty() && m.next.is_empty() && m.bb.is_empty() && m.eof.is_empty();
            if solo && t.pend != NONE {
                m.flush(t.pend);
                t.pend = NONE;
            }
            let loc = program.locs[t.dot as usize];
            match loc {
                Loc::RepStart(r) => {
                    let rep = program.reps[r as usize];
                    m.emit(&mut t, Event::Enter(r), solo);
                    let after = program.locs[rep.after as usize];
                    let first = program.locs[rep.body as usize];
                    // One `tt` a time up to the group's close: every tree of the level, in
                    // one loop. With trees left, the way out (a close, the end) does not
                    // take the token, so the one position is the one in the body.
                    if solo
                        && rep.sep == NONE
                        && rep.kind != RepKind::ZeroOrOne
                        && matches!(after, Loc::Close(_) | Loc::Eof)
                        && let Loc::Var { binder, kind: Some(Frag::Tt), .. } = first
                        && program.locs[rep.body as usize + 1] == Loc::RepEnd(r)
                        && !src.is_empty()
                    {
                        while !src.is_empty() {
                            engine.budget.step()?;
                            let start = src.at;
                            if !expect_tt(view, &mut src) {
                                return Ok(Outcome::Failed);
                            }
                            let toks = Toks { at: start, end: src.at };
                            let whole = view.kind(start) == TokenKind::Open(Delim::Invisible)
                                && view.close_of(start) + 1 == src.at;
                            let origin = if whole { Origin::Ast } else { Origin::Raw };
                            m.flat.apply(Event::Iter(r));
                            m.flat.apply(Event::Bind(binder, Fragment::Tokens { toks, origin }));
                        }
                        // The level is at its end, and nothing of it is taken yet: the one
                        // position goes on after the repetition, with the end under it.
                        m.work.push(Thread { dot: rep.after, pend: NONE, after: NONE, sep: false });
                        continue 'step;
                    }
                    let skip = matches!(rep.kind, RepKind::ZeroOrMore | RepKind::ZeroOrOne)
                        && fit(view, def, after, under, &mut glued) != Fit::No;
                    let body = fit(view, def, first, under, &mut glued) != Fit::No;
                    if skip {
                        m.work.push(Thread { dot: rep.after, ..t });
                    }
                    if body {
                        let mut inner = Thread { dot: rep.body, sep: false, ..t };
                        m.emit(&mut inner, Event::Iter(r), solo && !skip);
                        m.work.push(inner);
                    }
                }
                Loc::RepEnd(r) => {
                    let rep = program.reps[r as usize];
                    let mut left = false;
                    if !t.sep && fit(view, def, program.locs[rep.after as usize], under, &mut glued) != Fit::No {
                        left = true;
                        m.work.push(Thread { dot: rep.after, sep: false, ..t });
                    }
                    if rep.sep != NONE && !t.sep {
                        let sep = program.seps[rep.sep as usize];
                        if let Some(after) = expect_separator(view, def, src, sep) {
                            t.after = after;
                            t.sep = true;
                            m.next.push(t);
                        }
                    } else if rep.kind != RepKind::ZeroOrOne
                        && fit(view, def, program.locs[rep.body as usize], under, &mut glued) != Fit::No
                    {
                        t.dot = rep.body;
                        t.sep = false;
                        m.emit(&mut t, Event::Iter(r), solo && !left);
                        m.work.push(t);
                    }
                }
                Loc::Var { binder, kind, .. } => {
                    // A specifier no edition has matches no input.
                    let Some(kind) = kind else { continue };
                    let mut fork = src;
                    let (value, error) = match_meta_var(engine, view, kind, &mut fork);
                    if error.is_some() {
                        continue;
                    }
                    match value.is_empty() {
                        true => {
                            m.emit(&mut t, Event::Bind(binder, Fragment::Empty), solo);
                            t.dot += 1;
                            m.work.push(t);
                        }
                        false => m.bb.push((t, value, fork.at)),
                    }
                }
                _ => match fit(view, def, loc, under, &mut glued) {
                    Fit::No | Fit::Later => {}
                    Fit::End => m.eof.push(t),
                    Fit::Inside => {
                        t.dot += 1;
                        m.work.push(t);
                    }
                    Fit::Token(after) => {
                        t.dot += 1;
                        t.after = after;
                        m.next.push(t);
                    }
                },
            }
        }
        if under.at_end {
            // "It transcribes the first successful match": a match is one position at the
            // matcher's end when the input ends.
            return Ok(match m.eof[..] {
                [t] => {
                    m.flush(t.pend);
                    Outcome::Matched
                }
                _ => Outcome::Failed,
            });
        }
        // No position takes the token; or a fragment and a token, or two fragments, could
        // be taken: "if the compiler cannot unambiguously determine how to parse the macro
        // invocation one token at a time, then it is an error".
        if (m.bb.is_empty() && m.next.is_empty())
            || !(m.bb.is_empty() || m.next.is_empty())
            || m.bb.len() > 1
        {
            return Ok(Outcome::Failed);
        }
        if !m.next.is_empty() {
            let first = m.next[0].after;
            if first != NONE {
                src.at = first;
            } else {
                if m.next.iter().any(|t| t.after != NONE) {
                    return Ok(Outcome::Exact);
                }
                match (!src.is_empty()).then_some(src.at) {
                    Some(at) => match view.kind(at) {
                        TokenKind::Open(_) => {
                            m.stack.push(Level { at: view.close_of(at) + 1, end: src.end });
                            if m.stack.len() > DEPTH_BOUND {
                                return Err(Refused(crate::error::RulesLimit::Depth));
                            }
                            src = view.inside(at);
                        }
                        _ => src.at += 1,
                    },
                    None => {
                        if let Some(level) = m.stack.pop() {
                            src = level;
                        }
                    }
                }
            }
            for t in m.next.iter_mut() {
                t.after = NONE;
            }
            core::mem::swap(&mut m.work, &mut m.next);
        } else {
            let (mut t, value, after) = m.bb.pop().expect("one position waits on a fragment");
            let Loc::Var { binder, .. } = program.locs[t.dot as usize] else {
                unreachable!("a position waiting on a fragment is at a metavariable")
            };
            m.flush(t.pend);
            t.pend = NONE;
            m.flat.apply(Event::Bind(binder, value));
            src.at = after;
            t.dot += 1;
            m.work.push(t);
        }
    }
}
