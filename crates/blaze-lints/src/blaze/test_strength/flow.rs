//! The core of [`super`]: which parts of which values a body looks at, over a small IR that
//! [`super::lower`] makes from MIR. Plain data in, plain data out: no database, no IO, no clock,
//! so it runs the same under a property test as under a lint (AGENTS.md rule 14, sans-IO).
//!
//! A *source* ([`Src`]) is a value the analysis follows: what a call of the code under test
//! returns or writes through a `&mut` argument, a parameter of a summarized body, or a mark on
//! the result of a comparison. A *fact* ([`Fact`]) on a local says that the local, at the
//! position `at`, holds the part `part` of a source. Facts only grow: the analysis is a
//! flow-insensitive fixpoint over every statement of every block, so a fact once true of a local
//! anywhere is taken as true everywhere in the body. That over-approximates what flows where, and
//! every over-approximation here makes more parts *observed*, never fewer: a finding of "not
//! observed" is only made where no path the analysis keeps could observe the part.
//!
//! An *observation* ([`Obs`]) is a part of a source a check depends on: the value switched on by
//! a branch one of whose targets fails the test (an `assert!`, the arm of a `match` that panics,
//! `?` out of a test), the condition of a MIR `Assert`, the variant `unwrap` checks, or a value
//! that escapes where the analysis cannot follow it. A block fails when no path from it returns:
//! a call that diverges, an infinite loop, an abort.
//!
//! The lattice: per source, the set of parts observed, ordered by inclusion. A part is a path of
//! [`Step`]s; a path ending in a terminal step says only that property of the part is observed
//! ([`Step::Discr`]: which variant, [`Step::Len`]: the length, [`Step::Probe`]: one membership
//! or prefix), and one ending in [`Step::Whole`] that all of it is. Every transfer function only
//! adds facts and observations, so the fixpoint exists and adding a check to a body never takes
//! an observation away (`prop.rs` holds the core to that).

use std::collections::BTreeSet;

/// The longest part and the deepest position a fact keeps: a longer part is cut there and read
/// whole, and a deeper position is cut there holding the part whole ([`widen`]).
pub(crate) const DEPTH: usize = 6;

/// The most facts one local holds; past it the body is followed no further ([`Outcome::saturated`]).
pub(crate) const FACTS: usize = 256;

/// The most observations one body makes before it is followed no further.
pub(crate) const OBSERVATIONS: usize = 4096;

/// The most rounds of the fixpoint; a body still changing after them is saturated.
pub(crate) const ROUNDS: usize = 48;

/// The most blocks a body may have to be followed.
pub(crate) const BLOCKS: usize = 2048;

/// The most block visits the control-dependence regions of one body may take.
pub(crate) const REGION_WORK: usize = 1 << 22;

/// The most places one write is carried to through borrows.
pub(crate) const ALIAS_HOPS: usize = 64;

/// One step of a path into a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Step {
    /// A field of a struct, tuple, closure or variant, by index.
    Field(u32),
    /// The payload of a variant of an enum, by index.
    Variant(u32),
    /// Any element of a sequence (indices are not told apart).
    Elem,
    /// Terminal: which variant the value is.
    Discr,
    /// Terminal: the length of a sequence.
    Len,
    /// Terminal: one membership or prefix test of a sequence or string.
    Probe,
    /// Terminal: all of the part, through something the analysis does not look inside.
    Whole,
}

impl Step {
    /// Whether the step ends a path: what is past it is not a part of the value.
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Step::Discr | Step::Len | Step::Probe | Step::Whole)
    }
}

/// A path into a value, outermost step first.
pub(crate) type Path = Vec<Step>;

/// An output of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Out {
    /// What it returns.
    Ret,
    /// What it writes through its argument of this index (`self` is 0).
    Arg(u32),
}

/// A value the analysis follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Src {
    /// An output of the call of the code under test at this site.
    Call { site: u32, out: Out },
    /// The parameter of this index, of a body being summarized.
    Param(u32),
    /// The result of the comparison of this mark: observed when the comparison is asserted.
    Mark(u32),
}

/// A local holds, at the position `at`, the part `part` of `src`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Fact {
    pub(crate) src: Src,
    pub(crate) part: Path,
    pub(crate) at: Path,
    /// The shadow call (`is_some`, `len`, `contains`, ...) the part came through, if any.
    pub(crate) how: Option<u32>,
}

/// A place: a local and a path into it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Place {
    pub(crate) local: u32,
    pub(crate) path: Path,
}

impl Place {
    pub(crate) fn local(local: u32) -> Place {
        Place { local, path: Vec::new() } 
    }

    /// The place `more` further in.
    pub(crate) fn join(&self, more: &[Step]) -> Place {
        let mut path = self.path.clone(); // clone: a short path
        path.extend_from_slice(more);
        Place { local: self.local, path }
    }
}

/// A statement of the IR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stmt {
    /// `to = from`: a move, copy, cast or reborrow.
    Copy { to: Place, from: Place },
    /// `to = &of`: a copy that also carries every later write through `to` to `of`.
    Borrow { to: Place, of: Place },
    /// `to = op(from..)`: arithmetic and other values made from all of their inputs. With a
    /// mark, the comparison whose result the mark follows (`==`, `!=`).
    Derive { to: Place, from: Vec<Place>, mark: Option<u32> },
    /// `to = step(from)`: the discriminant or the length of a place.
    Read { to: Place, from: Place, step: Step },
    /// `to = Aggregate { .. }`: each place moved in at its path.
    Aggregate { to: Place, fields: Vec<(Path, Place)> },
    /// The place goes where the analysis does not follow (a yield, a tail call): observed whole.
    Escape { from: Place },
}

/// What a call does, as the analysis knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Returns its first argument as it is (`clone`, `as_ref`, `deref`, `iter`).
    Identity,
    /// Returns the payload of the variant `variant` of its first argument, checking that it is
    /// that variant first when `checks` (`unwrap`, `expect`; `unwrap_or` does not check).
    Unwrap { variant: u32, checks: bool },
    /// Returns one property of its first argument (`step`), through the shadow call `site`:
    /// `is_some`, `len`, `contains`.
    Shadow { step: Step, site: u32 },
    /// Returns an element of its first argument, in `Some` (the variant `wrap`) when there is
    /// one (`next`, `get`, `index`).
    Element { wrap: Option<u32> },
    /// `Try::branch`: the `ok` variant's payload is `Continue`'s, the other `Break`'s (with a
    /// payload of its own when `err_field`).
    Branch { ok: u32, err_field: bool },
    /// `PartialEq::eq` of something whose every part it reads: the result is made from both
    /// arguments whole, and `mark` follows it.
    Compare { mark: u32 },
    /// A body the analysis has summarized ([`Body::summaries`]), and the mark that follows its
    /// result when it is a comparison.
    Summary { index: u32, mark: Option<u32> },
    /// A body the analysis does not look into: the result is made from every argument whole,
    /// and so are the arguments of `writes` (written through `&mut`); when `observes`, every
    /// argument counts as observed whole too (a closure it may run, a workspace function past
    /// the depth summaries go to).
    Opaque { observes: bool, writes: Vec<u32> },
    /// The call does nothing the analysis follows (`drop`).
    Ignore,
    /// The call fails the test (`?` out of a test that returns a `Result`): it does not return.
    Fail,
}

/// A call of the code under test: its site and the arguments it writes through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Subject {
    pub(crate) site: u32,
    pub(crate) mut_args: Vec<u32>,
}

/// A call terminator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) effect: Effect,
    pub(crate) args: Vec<Option<Place>>,
    pub(crate) dest: Place,
    /// Where it returns to; `None` for a call that diverges.
    pub(crate) target: Option<u32>,
    pub(crate) subject: Option<Subject>,
}

/// A terminator of the IR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Term {
    Goto(Vec<u32>),
    /// A branch on `on` (`None` for a constant).
    Switch { on: Option<Place>, targets: Vec<u32> },
    /// A MIR `Assert`: fails unless `cond` holds, else goes on to `target`.
    Assert { cond: Option<Place>, target: u32 },
    Call(Call),
    Return,
    /// Fails the test.
    Fail,
    /// Ends a path that neither returns nor fails: an unreachable arm, a cleanup block.
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Block {
    pub(crate) stmts: Vec<Stmt>,
    pub(crate) term: Term,
}

/// What a body does with its parameters, for its callers: [`Summary::of`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Summary {
    /// The body could not be followed: a call of it is [`Effect::Opaque`] that observes.
    pub(crate) opaque: bool,
    /// The parts of parameters it observes: (parameter, part).
    pub(crate) obs: Vec<(u32, Path)>,
    /// What it returns of its parameters: (parameter, part, where in the result).
    pub(crate) ret: Vec<(u32, Path, Path)>,
    /// What it writes through its `&mut` parameters: (written parameter, read parameter, part,
    /// where in the written one).
    pub(crate) writes: Vec<(u32, u32, Path, Path)>,
}

/// The most entries a summary keeps; past it the summary is opaque.
pub(crate) const SUMMARY: usize = 512;

impl Summary {
    pub(crate) fn opaque() -> Summary {
        Summary { opaque: true, ..Summary::default() }
    }

    /// The summary of a body run with its parameters as sources, `mut_params` the indices of
    /// those that are `&mut`.
    pub(crate) fn of(outcome: &Outcome, mut_params: &[u32]) -> Summary {
        if outcome.saturated {
            return Summary::opaque();
        }
        let mut summary = Summary::default();
        for obs in &outcome.obs {
            if let Src::Param(i) = obs.src {
                summary.obs.push((i, obs.part.clone())); // clone: a short path
            }
        }
        for fact in &outcome.ret {
            if let Src::Param(i) = fact.src {
                summary.ret.push((i, fact.part.clone(), fact.at.clone())); // clone: short paths
            }
        }
        for (written, fact) in &outcome.writes {
            let Src::Param(read) = fact.src else { continue };
            let seed = read == *written && fact.part.is_empty() && fact.at.is_empty();
            if mut_params.contains(written) && !seed {
                summary.writes.push((*written, read, fact.part.clone(), fact.at.clone())); // clone: short paths
            }
        }
        summary.obs.sort();
        summary.obs.dedup();
        summary.ret.sort();
        summary.ret.dedup();
        summary.writes.sort();
        summary.writes.dedup();
        if summary.obs.len() + summary.ret.len() + summary.writes.len() > SUMMARY {
            return Summary::opaque();
        }
        summary
    }
}

/// A body of the IR.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Body {
    pub(crate) locals: u32,
    pub(crate) blocks: Vec<Block>,
    /// The locals of the parameters, in order.
    pub(crate) params: Vec<u32>,
    /// Whether the parameters are sources ([`Src::Param`]): a body being summarized.
    pub(crate) seed_params: bool,
    pub(crate) ret: u32,
    /// Whether what it returns goes where the analysis does not follow (a closure of a test):
    /// observed whole.
    pub(crate) ret_escapes: bool,
    /// The summaries its [`Effect::Summary`] calls index.
    pub(crate) summaries: Vec<Summary>,
}

/// A part of a source a check depends on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Obs {
    pub(crate) src: Src,
    pub(crate) part: Path,
    /// The shadow call the part came through, if any: a weak check.
    pub(crate) how: Option<u32>,
}

/// An asserted comparison: the sources on each side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Compared {
    pub(crate) mark: u32,
    pub(crate) left: Vec<Src>,
    pub(crate) right: Vec<Src>,
}

/// What a body observes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// A bound was hit: what the body does is not known, and every source in it is to be taken
    /// as observed whole.
    pub(crate) saturated: bool,
    pub(crate) obs: BTreeSet<Obs>,
    /// The facts on the return slot at the fixpoint.
    pub(crate) ret: Vec<Fact>,
    /// The facts on each parameter's local at the fixpoint, by parameter index.
    pub(crate) writes: Vec<(u32, Fact)>,
    pub(crate) compares: Vec<Compared>,
}

impl Outcome {
    fn saturated() -> Outcome {
        Outcome { saturated: true, ..Outcome::default() }
    }
}

/// `part` with `more` after it: unchanged once it ends in a terminal step, and cut to
/// [`DEPTH`] (read whole there).
pub(crate) fn extend(part: &[Step], more: &[Step]) -> Path {
    let mut out: Path = part.to_vec(); 
    if out.last().is_some_and(|it| it.terminal()) {
        return out;
    }
    for &step in more {
        if out.len() >= DEPTH {
            out.push(Step::Whole);
            return out;
        }
        out.push(step);
        if step.terminal() {
            return out;
        }
    }
    out
}

/// How a read of a place meets a fact on its local.
enum Seen {
    /// The read is inside the fact's part: it reads this part of the source.
    Inner(Path),
    /// The fact sits inside what is read: the part, at this position in the value read.
    Outer(Path, Path),
}

/// How a read of the path `read` meets `fact`.
/// `fact` with its position cut to [`DEPTH`]: a part put deeper (a value nested in itself
/// round after round, as `acc = (acc, x)` in a loop) is held at the cut, whole, so any read at
/// or under the cut meets all of it. Positions stay bounded, so the fixpoint ends; and nothing
/// is observed by the cut itself, only by a check that reads there, so a body with no checks
/// still observes nothing.
fn widen(fact: Fact) -> Fact {
    if fact.at.len() <= DEPTH {
        return fact;
    }
    let mut at = fact.at;
    at.truncate(DEPTH);
    Fact { part: extend(&fact.part, &[Step::Whole]), at, ..fact }
}

fn align(fact: &Fact, read: &[Step]) -> Option<Seen> {
    if read.starts_with(&fact.at) {
        return Some(Seen::Inner(extend(&fact.part, &read[fact.at.len()..])));
    }
    if fact.at.starts_with(read) {
        // clone: a short path
        return Some(Seen::Outer(fact.part.clone(), fact.at[read.len()..].to_vec()));
    }
    None
}

/// For each block, whether some path from it returns (or stops): a block where none does fails.
fn returns(body: &Body) -> Vec<bool> {
    let n = body.blocks.len();
    let mut ok = vec![false; n]; 
    for _ in 0..=n {
        let mut grew = false;
        for (at, block) in body.blocks.iter().enumerate() {
            if ok[at] {
                continue;
            }
            let now = match &block.term {
                Term::Return | Term::Stop => true,
                Term::Fail => false,
                term => successors(term).iter().any(|&it| ok.get(it as usize).copied().unwrap_or(false)),
            };
            if now {
                ok[at] = true;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    ok
}

/// The blocks a terminator goes on to.
fn successors(term: &Term) -> Vec<u32> {
    match term {
        Term::Goto(targets) | Term::Switch { targets, .. } => targets.clone(), // clone: a few block ids
        Term::Assert { target, .. } => vec![*target], 
        Term::Call(call) if matches!(call.effect, Effect::Fail) => Vec::new(), 
        Term::Call(call) => call.target.into_iter().collect(),
        Term::Return | Term::Fail | Term::Stop => Vec::new(), 
    }
}

/// The locals a statement reads.
fn stmt_reads(stmt: &Stmt, out: &mut BTreeSet<u32>) {
    match stmt {
        Stmt::Copy { from, .. } | Stmt::Read { from, .. } | Stmt::Escape { from } => {
            out.insert(from.local);
        }
        Stmt::Borrow { of, .. } => {
            out.insert(of.local);
        }
        Stmt::Derive { from, .. } => out.extend(from.iter().map(|it| it.local)),
        Stmt::Aggregate { fields, .. } => out.extend(fields.iter().map(|(_, it)| it.local)),
    }
}

/// The local a statement writes.
fn stmt_writes(stmt: &Stmt) -> Option<u32> {
    match stmt {
        Stmt::Copy { to, .. }
        | Stmt::Borrow { to, .. }
        | Stmt::Derive { to, .. }
        | Stmt::Read { to, .. }
        | Stmt::Aggregate { to, .. } => Some(to.local),
        Stmt::Escape { .. } => None,
    }
}

fn term_reads(term: &Term, out: &mut BTreeSet<u32>) {
    match term {
        Term::Switch { on: Some(on), .. } => {
            out.insert(on.local);
        }
        Term::Assert { cond: Some(cond), .. } => {
            out.insert(cond.local);
        }
        Term::Call(call) => out.extend(call.args.iter().flatten().map(|it| it.local)),
        Term::Switch { on: None, .. }
        | Term::Assert { cond: None, .. }
        | Term::Goto(_)
        | Term::Return
        | Term::Fail
        | Term::Stop => {}
    }
}

/// Control dependence: for each branch on a place, the locals assigned in blocks only some of
/// its targets reach and read after the branch's arms join (the `true` and `false` of a
/// `matches!`). Those locals are made from the place switched on. `None` when the work would
/// pass [`REGION_WORK`].
fn regions(body: &Body) -> Option<Vec<(Place, Vec<u32>)>> {
    let n = body.blocks.len();
    
    let mut reads: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); n];
    let mut writes: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); n];
    for (at, block) in body.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            stmt_reads(stmt, &mut reads[at]);
            if let Some(it) = stmt_writes(stmt) {
                writes[at].insert(it);
            }
        }
        term_reads(&block.term, &mut reads[at]);
        if let Term::Call(call) = &block.term {
            writes[at].insert(call.dest.local);
        }
    }
    
    let succ: Vec<Vec<u32>> = body.blocks.iter().map(|it| successors(&it.term)).collect();
    let mut work = 0usize;
    
    let mut out = Vec::new();
    for block in &body.blocks {
        let Term::Switch { on: Some(on), targets } = &block.term else { continue };
        let mut distinct: Vec<u32> = targets.clone(); // clone: a few block ids
        distinct.sort_unstable();
        distinct.dedup();
        if distinct.len() < 2 {
            continue;
        }
        work = work.saturating_add(n.saturating_mul(distinct.len()));
        if work > REGION_WORK {
            return None;
        }
        // How many of the targets reach each block.
        let mut count = vec![0u32; n]; 
        for &target in &distinct {
            let mut seen = vec![false; n]; 
            let mut stack = vec![target]; 
            while let Some(at) = stack.pop() {
                let Some(flag) = seen.get_mut(at as usize) else { continue };
                if *flag {
                    continue;
                }
                *flag = true;
                count[at as usize] += 1;
                stack.extend(succ[at as usize].iter().copied());
            }
        }
        let inside = |at: usize| count[at] == 1;
        let mut assigned = BTreeSet::new();
        for at in (0..n).filter(|&it| inside(it)) {
            assigned.extend(writes[at].iter().copied());
        }
        let mut dependent: Vec<u32> = Vec::new(); 
        for local in assigned {
            if (0..n).any(|at| !inside(at) && count[at] > 0 && reads[at].contains(&local)) {
                dependent.push(local);
            }
        }
        if !dependent.is_empty() {
            out.push((on.clone(), dependent)); // clone: one place per branch
        }
    }
    Some(out)
}

/// The state of one run.
struct State<'b> {
    body: &'b Body,
    facts: Vec<BTreeSet<Fact>>,
    /// For each local, the places a borrow held in it points to.
    aliases: Vec<Vec<Place>>,
    obs: BTreeSet<Obs>,
    changed: bool,
    full: bool,
}

impl<'b> State<'b> {
    fn new(body: &'b Body) -> State<'b> {
        let n = body.locals as usize;
        
        let mut aliases: Vec<Vec<Place>> = vec![Vec::new(); n];
        for block in &body.blocks {
            for stmt in &block.stmts {
                if let Stmt::Borrow { to, of } = stmt
                    && to.path.is_empty()
                    && let Some(it) = aliases.get_mut(to.local as usize)
                    && !it.contains(of)
                {
                    it.push(of.clone()); // clone: one place per borrow
                }
            }
        }
        State {
            body,
            facts: vec![BTreeSet::new(); n],
            aliases,
            obs: BTreeSet::new(),
            changed: false,
            full: false,
        }
    }

    /// What a read of `place` sees: each fact of its local it meets.
    fn reads(&self, place: &Place) -> Vec<(Src, Seen, Option<u32>)> {
        let Some(facts) = self.facts.get(place.local as usize) else { return Vec::new() };
        facts
            .iter()
            .filter_map(|fact| Some((fact.src, align(fact, &place.path)?, fact.how)))
            .collect() 
    }

    fn observe(&mut self, src: Src, part: Path, how: Option<u32>) {
        if self.obs.len() >= OBSERVATIONS {
            self.full = true;
            return;
        }
        self.obs.insert(Obs { src, part, how });
    }

    /// Adds `fact` to `local`, and to every place a borrow in `local` points to.
    fn add(&mut self, local: u32, fact: Fact) {
        let mut stack = vec![(local, fact)]; 
        let mut hops = 0;
        while let Some((local, fact)) = stack.pop() {
            hops += 1;
            if hops > ALIAS_HOPS {
                self.full = true;
                return;
            }
            let Some(facts) = self.facts.get_mut(local as usize) else { continue };
            if facts.contains(&fact) {
                continue;
            }
            if facts.len() >= FACTS {
                self.full = true;
                return;
            }
            facts.insert(fact.clone()); // clone: the fact, kept and carried on
            self.changed = true;
            for of in self.aliases.get(local as usize).into_iter().flatten() {
                let mut at = of.path.clone(); // clone: a short path
                at.extend_from_slice(&fact.at);
                let carried = widen(Fact { at, ..fact.clone() }); // clone: the fact, carried through the borrow
                stack.push((of.local, carried));
            }
        }
    }

    /// Puts `part` of `src` at `rel` inside `to`; a position past [`DEPTH`] is widened
    /// ([`widen`]).
    fn put(&mut self, to: &Place, rel: &[Step], src: Src, part: Path, how: Option<u32>) {
        let mut at = to.path.clone(); // clone: a short path
        at.extend_from_slice(rel);
        self.add(to.local, widen(Fact { src, part, at, how }));
    }

    fn copy(&mut self, to: &Place, from: &Place) {
        for (src, seen, how) in self.reads(from) {
            match seen {
                Seen::Inner(part) => self.put(to, &[], src, part, how),
                Seen::Outer(part, rel) => self.put(to, &rel, src, part, how),
            }
        }
    }

    /// `to` is made from all of `from`.
    fn derive(&mut self, to: &Place, from: &Place) {
        for (src, seen, how) in self.reads(from) {
            let part = match seen {
                Seen::Inner(part) | Seen::Outer(part, _) => part,
            };
            self.put(to, &[], src, extend(&part, &[Step::Whole]), how);
        }
    }

    fn read_step(&mut self, to: &Place, from: &Place, step: Step) {
        for (src, seen, how) in self.reads(from) {
            if let Seen::Inner(part) = seen {
                self.put(to, &[], src, extend(&part, &[step]), how);
            }
        }
    }

    fn observe_whole(&mut self, place: &Place) {
        for (src, seen, how) in self.reads(place) {
            let part = match seen {
                Seen::Inner(part) | Seen::Outer(part, _) => part,
            };
            self.observe(src, extend(&part, &[Step::Whole]), how);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Copy { to, from } | Stmt::Borrow { to, of: from } => self.copy(to, from),
            Stmt::Derive { to, from, mark } => {
                for it in from {
                    self.derive(to, it);
                }
                if let Some(mark) = mark {
                    self.put(to, &[], Src::Mark(*mark), Vec::new(), None); 
                }
            }
            Stmt::Read { to, from, step } => self.read_step(to, from, *step),
            Stmt::Aggregate { to, fields } => {
                for (prefix, from) in fields {
                    self.copy(&to.join(prefix), from);
                }
            }
            Stmt::Escape { from } => self.observe_whole(from),
        }
    }

    fn call(&mut self, call: &Call) {
        let arg = |i: usize| call.args.get(i).and_then(Option::as_ref);
        let dest = &call.dest;
        match &call.effect {
            Effect::Identity => {
                if let Some(first) = arg(0) {
                    self.copy(dest, first);
                }
            }
            Effect::Unwrap { variant, checks } => {
                if let Some(first) = arg(0) {
                    let payload = [Step::Variant(*variant), Step::Field(0)];
                    for (src, seen, how) in self.reads(first) {
                        match seen {
                            Seen::Inner(part) => {
                                if *checks {
                                    self.observe(src, extend(&part, &[Step::Discr]), how);
                                }
                                self.put(dest, &[], src, extend(&part, &payload), how);
                            }
                            Seen::Outer(part, rel) if rel.starts_with(&payload) => {
                                self.put(dest, &rel[2..], src, part, how);
                            }
                            Seen::Outer(..) => {}
                        }
                    }
                }
                // `unwrap_or(default)`: the default may be what is returned.
                for other in call.args.iter().skip(1).flatten() {
                    self.copy(dest, other);
                }
            }
            Effect::Shadow { step, site } => {
                if let Some(first) = arg(0) {
                    for (src, seen, _) in self.reads(first) {
                        if let Seen::Inner(part) = seen {
                            self.put(dest, &[], src, extend(&part, &[*step]), Some(*site));
                        }
                    }
                }
                for other in call.args.iter().skip(1).flatten() {
                    self.derive(dest, other);
                }
            }
            Effect::Element { wrap } => {
                let pre: Vec<Step> = match wrap {
                    Some(variant) => vec![Step::Variant(*variant), Step::Field(0)], 
                    None => Vec::new(), 
                };
                if let Some(first) = arg(0) {
                    for (src, seen, how) in self.reads(first) {
                        match seen {
                            Seen::Inner(part) => {
                                self.put(dest, &pre, src, extend(&part, &[Step::Elem]), how);
                            }
                            Seen::Outer(part, rel) if rel.first() == Some(&Step::Elem) => {
                                let mut at = pre.clone(); // clone: a short path
                                at.extend_from_slice(&rel[1..]);
                                self.put(dest, &at, src, part, how);
                            }
                            Seen::Outer(..) => {}
                        }
                    }
                }
                for other in call.args.iter().skip(1).flatten() {
                    self.derive(dest, other);
                }
            }
            Effect::Branch { ok: good, err_field } => {
                let Some(first) = arg(0) else { return };
                let bad = 1 - (*good).min(1);
                let ok_payload = [Step::Variant(*good), Step::Field(0)];
                let err_payload: Vec<Step> = match err_field {
                    true => vec![Step::Variant(bad), Step::Field(0)], 
                    false => vec![Step::Variant(bad)], 
                };
                let cont = [Step::Variant(0), Step::Field(0)];
                let brk = [Step::Variant(1), Step::Field(0)];
                for (src, seen, how) in self.reads(first) {
                    match seen {
                        Seen::Inner(part) => {
                            self.put(dest, &[], src, extend(&part, &[Step::Discr]), how);
                            self.put(dest, &cont, src, extend(&part, &ok_payload), how);
                            self.put(dest, &brk, src, extend(&part, &err_payload), how);
                        }
                        Seen::Outer(part, rel) => {
                            if rel.starts_with(&ok_payload) {
                                let mut at = cont.to_vec(); 
                                at.extend_from_slice(&rel[2..]);
                                self.put(dest, &at, src, part, how);
                            } else if *err_field && rel.starts_with(&err_payload) {
                                let mut at = brk.to_vec(); 
                                at.extend_from_slice(&rel[2..]);
                                self.put(dest, &at, src, part, how);
                            }
                        }
                    }
                }
            }
            Effect::Compare { mark } => {
                for it in call.args.iter().take(2).flatten() {
                    self.derive(dest, it);
                }
                self.put(dest, &[], Src::Mark(*mark), Vec::new(), None); 
            }
            Effect::Summary { index, mark } => {
                let body = self.body;
                let Some(summary) = body.summaries.get(*index as usize) else {
                    self.opaque(call, true, &[]);
                    return;
                };
                if summary.opaque {
                    self.opaque(call, true, &[]);
                    return;
                }
                for (i, part) in &summary.obs {
                    if let Some(it) = arg(*i as usize) {
                        self.observe_whole_at(&it.join(part));
                    }
                }
                for (i, part, at) in &summary.ret {
                    if let Some(it) = arg(*i as usize) {
                        self.copy(&dest.join(at), &it.join(part));
                    }
                }
                for (written, read, part, at) in &summary.writes {
                    if let (Some(to), Some(from)) = (arg(*written as usize), arg(*read as usize)) {
                        self.copy(&to.join(at), &from.join(part));
                    }
                }
                if let Some(mark) = mark {
                    self.put(dest, &[], Src::Mark(*mark), Vec::new(), None); 
                }
            }
            Effect::Opaque { observes, writes } => self.opaque(call, *observes, writes),
            Effect::Ignore | Effect::Fail => {}
        }
    }

    /// What a summary observes at `place`: the part itself, which may end in a terminal step.
    fn observe_whole_at(&mut self, place: &Place) {
        for (src, seen, how) in self.reads(place) {
            match seen {
                Seen::Inner(part) => {
                    let part = match part.last().is_some_and(|it| it.terminal()) {
                        true => part,
                        false => extend(&part, &[Step::Whole]),
                    };
                    self.observe(src, part, how);
                }
                Seen::Outer(part, _) => self.observe(src, extend(&part, &[Step::Whole]), how),
            }
        }
    }

    fn opaque(&mut self, call: &Call, observes: bool, writes: &[u32]) {
        for it in call.args.iter().flatten() {
            self.derive(&call.dest, it);
            if observes {
                self.observe_whole(it);
            }
        }
        for &written in writes {
            let Some(Some(to)) = call.args.get(written as usize) else { continue };
            for (at, from) in call.args.iter().enumerate() {
                if at as u32 == written {
                    continue;
                }
                if let Some(from) = from {
                    self.derive(&Place::local(to.local), from);
                }
            }
        }
    }

    /// The outputs of a call of the code under test, as sources.
    fn seed(&mut self, call: &Call) {
        let Some(subject) = &call.subject else { return };
        let ret = Src::Call { site: subject.site, out: Out::Ret };
        self.put(&call.dest, &[], ret, Vec::new(), None); 
        for &i in &subject.mut_args {
            if let Some(Some(arg)) = call.args.get(i as usize) {
                let src = Src::Call { site: subject.site, out: Out::Arg(i) };
                self.put(arg, &[], src, Vec::new(), None); 
            }
        }
    }

    /// The sources a read of `place` meets, of calls of the code under test.
    fn sources(&self, place: &Place) -> Vec<Src> {
        let mut out: Vec<Src> = self
            .reads(place)
            .into_iter()
            .map(|(src, _, _)| src)
            .filter(|it| matches!(it, Src::Call { .. }))
            .collect(); 
        out.sort();
        out.dedup();
        out
    }
}

/// Runs the analysis over `body`.
pub(crate) fn run(body: &Body) -> Outcome {
    if body.blocks.len() > BLOCKS {
        return Outcome::saturated();
    }
    let ok = returns(body);
    let Some(regions) = regions(body) else { return Outcome::saturated() };
    let mut state = State::new(body);
    if body.seed_params {
        for (i, &local) in body.params.iter().enumerate() {
            let src = Src::Param(i as u32);
            
            state.add(local, Fact { src, part: Vec::new(), at: Vec::new(), how: None });
        }
    }
    let mut settled = false;
    for _ in 0..ROUNDS {
        state.changed = false;
        for block in &body.blocks {
            for stmt in &block.stmts {
                state.stmt(stmt);
            }
            match &block.term {
                Term::Switch { on: Some(on), targets } => {
                    if targets.iter().any(|&it| !ok.get(it as usize).copied().unwrap_or(true)) {
                        state.observe_whole(on);
                    }
                }
                Term::Assert { cond: Some(cond), .. } => state.observe_whole(cond),
                Term::Call(call) => {
                    state.seed(call);
                    state.call(call);
                }
                Term::Switch { on: None, .. }
                | Term::Assert { cond: None, .. }
                | Term::Goto(_)
                | Term::Return
                | Term::Fail
                | Term::Stop => {}
            }
        }
        for (on, locals) in &regions {
            for &local in locals {
                state.derive(&Place::local(local), on);
            }
        }
        if state.full {
            return Outcome::saturated();
        }
        if !state.changed {
            settled = true;
            break;
        }
    }
    if !settled {
        return Outcome::saturated();
    }
    if body.ret_escapes {
        state.observe_whole(&Place::local(body.ret));
    }
    if state.full {
        return Outcome::saturated();
    }
    // The comparisons whose result is observed: what an assertion compares.
    let mut compares = Vec::new(); 
    let asserted =
        |mark: u32| state.obs.iter().any(|it| it.src == Src::Mark(mark));
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let Stmt::Derive { from, mark: Some(mark), .. } = stmt
                && let [left, right, ..] = from.as_slice()
                && asserted(*mark)
            {
                compares.push(Compared {
                    mark: *mark,
                    left: state.sources(left),
                    right: state.sources(right),
                });
            }
        }
        if let Term::Call(call) = &block.term {
            let mark = match &call.effect {
                Effect::Compare { mark } | Effect::Summary { mark: Some(mark), .. } => *mark,
                Effect::Summary { mark: None, .. }
                | Effect::Identity
                | Effect::Unwrap { .. }
                | Effect::Shadow { .. }
                | Effect::Element { .. }
                | Effect::Branch { .. }
                | Effect::Opaque { .. }
                | Effect::Ignore
                | Effect::Fail => continue,
            };
            if let (Some(Some(left)), Some(Some(right))) = (call.args.first(), call.args.get(1))
                && asserted(mark)
            {
                compares.push(Compared {
                    mark,
                    left: state.sources(left),
                    right: state.sources(right),
                });
            }
        }
    }
    let ret = state.facts.get(body.ret as usize).map(|it| it.iter().cloned().collect()).unwrap_or_default(); // clone: the return slot's facts
    let mut writes = Vec::new(); 
    for (i, &local) in body.params.iter().enumerate() {
        for fact in state.facts.get(local as usize).into_iter().flatten() {
            writes.push((i as u32, fact.clone())); // clone: one fact
        }
    }
    Outcome { saturated: false, obs: state.obs, ret, writes, compares }
}
