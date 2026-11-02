//! A simulation of a parsed pattern on all of its paths at once, in the
//! manner of Pike's VM: the pattern is compiled to a small program whose
//! alternatives and repeats are splits, and the simulation advances a
//! list of threads through the text one character at a time, keeping
//! them in the order CPython's backtracking would try them. The first
//! thread to reach the end of the program in that order gives the same
//! match end as [`Matcher`](super::matcher::Matcher) does, in time
//! proportional to the text rather than to the paths through it.
//!
//! The program can express a pattern whose unrolled size stays within
//! [`PROGRAM_LIMIT`] and that has no backreference, conditional group,
//! atomic group, possessive repeat or unbounded repeat of something
//! that can match empty; anything else is [`Unsupported`] and stays
//! with the backtracking matcher. CPython stops such a repeat after an
//! iteration that consumed nothing, which tells apart paths that reach
//! the same instruction with different progress through the repeat,
//! while the simulation merges them, and that can change which match
//! end is preferred. A program compiled for recognition alone
//! ([`Program::compile_recognizer`]) admits these repeats: merging
//! changes the preferred end but not whether a match exists.
//!
//! The simulation on its own costs more per character than backtracking
//! does on the common run of a pattern, so a program whose instructions
//! never look at the text around a position (no anchors or lookarounds)
//! remembers the thread lists it passes through as the states of a
//! deterministic automaton, built lazily one transition at a time (see
//! [`Dfa`]): a character seen before in a state seen before costs a
//! table lookup.
//!
//! Because the threads alive at any position are the whole state of a
//! run, a run that reaches the end of its text with threads still
//! waiting for input can hand them back ([`Resume`]) and be carried on
//! later over a longer text that begins the same way, instead of
//! starting over.

use super::categories::class_ranges;
use super::constants::{
    AtCode, ChCode, MAXREPEAT, SRE_FLAG_DOTALL, SRE_FLAG_IGNORECASE, SRE_FLAG_UNICODE,
    unicode_version,
};
use super::matcher::{Touch, at_matches, combine_flags, literal_eq, set_matches};
use super::parser::{OpCode, ParsedPattern, SetItem, SubPattern};
use super::width::get_width;
use crate::native::{HashMap, HashSet};
use crate::sys::sync::Mutex;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// The most instructions a program may have, counting the repeats it
/// unrolls.
const PROGRAM_LIMIT: usize = 4096;

/// The most thread lists a program's [`Dfa`] remembers before it forgets
/// them all and starts over.
const DFA_STATES: usize = 1024;

/// A transition not yet taken in a [`DfaState`].
const UNKNOWN: u32 = u32::MAX;

/// Marks a stack entry in [`Program::follow`] that only adds its
/// instruction to the thread list, so a lazy loop's own thread can come
/// after everything reachable from its exit.
const EMIT: usize = 1 << (usize::BITS - 1);

/// The pattern uses a construct the simulation cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported;

/// A single-character test.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Unit {
    Literal(u32),
    NotLiteral(u32),
    Any,
    /// A set, with which ASCII characters it holds (under the flags of
    /// the instruction it belongs to) as a bitmap (see
    /// [`Compiler::unit_of`]).
    In(Vec<SetItem>, [u64; 2]),
}

impl Unit {
    /// Add to `bounds` the codepoints at which the test's answer
    /// changes, when it follows from codepoint ranges: not under
    /// IGNORECASE, whose folding crosses ranges.
    fn bounds(&self, flags: u32, bounds: &mut Vec<u32>) -> bool {
        if flags & SRE_FLAG_IGNORECASE != 0 {
            return false;
        }
        let mut range = |lo: u32, hi: u32| bounds.extend([lo, hi.saturating_add(1)]);
        match self {
            Unit::Literal(cp) | Unit::NotLiteral(cp) => range(*cp, *cp),
            Unit::Any => range(u32::from('\n'), u32::from('\n')),
            Unit::In(items, _) => {
                for item in items {
                    match item {
                        SetItem::Negate => {}
                        SetItem::Literal(cp) => range(*cp, *cp),
                        SetItem::Range(lo, hi) => range(*lo, *hi),
                        SetItem::Category(_) if flags & SRE_FLAG_UNICODE == 0 => {}
                        SetItem::Category(category) => {
                            let class = match category {
                                ChCode::Digit | ChCode::NotDigit => ChCode::Digit,
                                ChCode::Space | ChCode::NotSpace => ChCode::Space,
                                ChCode::Word | ChCode::NotWord => ChCode::Word,
                            };
                            for &(lo, hi) in
                                class_ranges(class, unicode_version(flags)).unwrap_or(&[])
                            {
                                range(lo, hi);
                            }
                        }
                    }
                }
            }
        }
        true
    }

    fn matches(&self, c: char, flags: u32) -> bool {
        match self {
            Unit::Literal(cp) => literal_eq(*cp, c, flags),
            Unit::NotLiteral(cp) => !literal_eq(*cp, c, flags),
            Unit::Any => c != '\n' || flags & SRE_FLAG_DOTALL != 0,
            Unit::In(items, ascii) => match u32::from(c) {
                cp if cp < 128 => ascii[(cp >> 6) as usize] >> (cp & 63) & 1 != 0,
                _ => set_matches(items, c, flags),
            },
        }
    }
}

fn ascii_table(items: &[SetItem], flags: u32) -> [u64; 2] {
    let mut table = [0; 2];
    for cp in 0..128u32 {
        if set_matches(items, char::from_u32(cp).unwrap_or('\0'), flags) {
            table[(cp >> 6) as usize] |= 1 << (cp & 63);
        }
    }
    table
}

#[derive(Clone)]
enum Inst {
    Unit(Unit, u32),
    /// An unbounded repeat of one unit: consume a character and stay,
    /// or continue after; `greedy` says which comes first.
    Loop(Unit, u32, bool),
    At(AtCode, u32),
    /// A lookaround: whether the program at `entry` matches at this
    /// position (ahead), or ends exactly here from `behind` characters
    /// back.
    Look {
        negate: bool,
        behind: Option<usize>,
        entry: usize,
    },
    /// Continue at both targets, the first in preference.
    Split(usize, usize),
    Jmp(usize),
    Fail,
    Match,
}

/// A compiled pattern.
pub struct Program {
    insts: Vec<Inst>,
    /// Thread lists from earlier runs, kept to save allocating them again.
    scratch: Mutex<Vec<Scratch>>,
    /// The transitions taken so far between the thread lists of a program
    /// whose threads never look at the text around them, so that the
    /// lists need not be followed again, shared with the program's
    /// clones; `None` for a program with an anchor or lookaround, whose
    /// lists depend on where they are.
    dfa: Option<Arc<Mutex<Dfa>>>,
    /// How the characters beyond ASCII fall into classes that every
    /// character test in the program treats alike (see
    /// [`Program::class`]).
    classes: Classes,
}

/// How a program tells the characters beyond ASCII apart for its
/// [`Dfa`].
#[derive(Clone)]
enum Classes {
    /// By where they fall among these sorted codepoints, at each of
    /// which some test's answer changes.
    Ranges(Vec<u32>),
    /// By which of these tests (instructions) they pass, for tests
    /// whose answers do not follow from codepoint ranges.
    Tests(Vec<usize>),
    /// Each by itself: too many tests to count.
    ByChar,
}

impl Classes {
    /// The classes of `insts`, from one instruction of each distinct
    /// character test: the copies an unrolled repeat emits tell the
    /// characters apart no further.
    fn of(insts: &[Inst]) -> Classes {
        let mut seen: HashSet<(&Unit, u32)> = HashSet::default();
        let units: Vec<usize> = (0..insts.len())
            .filter(|&pc| match &insts[pc] {
                Inst::Unit(unit, flags) | Inst::Loop(unit, flags, _) => seen.insert((unit, *flags)),
                _ => false,
            })
            .collect();
        let mut bounds = Vec::new();
        for &pc in &units {
            let (Inst::Unit(unit, flags) | Inst::Loop(unit, flags, _)) = &insts[pc] else {
                unreachable!("units are character tests")
            };
            if !unit.bounds(*flags, &mut bounds) {
                return if units.len() > 63 {
                    Classes::ByChar
                } else {
                    Classes::Tests(units)
                };
            }
        }
        bounds.retain(|&bound| bound >= 128);
        bounds.sort_unstable();
        bounds.dedup();
        Classes::Ranges(bounds)
    }
}

impl Clone for Program {
    fn clone(&self) -> Program {
        Program {
            insts: self.insts.clone(),
            scratch: Mutex::new(Vec::new()),
            dfa: self.dfa.clone(),
            classes: self.classes.clone(),
        }
    }
}

/// The thread lists a program's runs have passed through, each with the
/// list one character leads to from it where that has been worked out.
#[derive(Default)]
struct Dfa {
    states: Vec<DfaState>,
    /// Each state's threads, by state.
    ids: HashMap<Arc<[usize]>, u32>,
    /// The state a run starts in, once worked out.
    start: Option<u32>,
}

/// One thread list, in order of preference and cut after the first
/// thread that has matched, since the ones after it never run.
struct DfaState {
    pcs: Arc<[usize]>,
    /// How many of the threads wait for a character: all of them, or
    /// all but the matched one at the end.
    waiting: usize,
    /// The state each ASCII character leads to, or [`UNKNOWN`].
    ascii: [u32; 128],
    /// The state each class of other character seen so far leads to
    /// (see [`Program::class`]).
    other: Vec<(u64, u32)>,
}

impl DfaState {
    fn next(&self, class: u64) -> u32 {
        match class {
            cp if cp < 128 => self.ascii[cp as usize],
            _ => self
                .other
                .iter()
                .find(|(known, _)| *known == class)
                .map_or(UNKNOWN, |(_, next)| *next),
        }
    }

    fn set_next(&mut self, class: u64, next: u32) {
        match class {
            cp if cp < 128 => self.ascii[cp as usize] = next,
            _ => self.other.push((class, next)),
        }
    }
}

impl Dfa {
    /// The state whose threads are `pcs`, made when new; the caller
    /// keeps the number of states within [`DFA_STATES`].
    fn intern(&mut self, insts: &[Inst], pcs: &[usize]) -> u32 {
        let waiting = pcs
            .iter()
            .position(|&pc| matches!(insts[pc], Inst::Match))
            .unwrap_or(pcs.len());
        let pcs = &pcs[..(waiting + 1).min(pcs.len())];
        if let Some(&id) = self.ids.get(pcs) {
            return id;
        }
        let id = self.states.len() as u32;
        let pcs: Arc<[usize]> = Arc::from(pcs);
        self.states.push(DfaState {
            pcs: Arc::clone(&pcs),
            waiting,
            ascii: [UNKNOWN; 128],
            other: Vec::new(),
        });
        self.ids.insert(pcs, id);
        id
    }

    fn clear(&mut self) {
        self.states.clear();
        self.ids.clear();
        self.start = None;
    }

    /// The state `resume`'s threads are in: the one it names when that
    /// is still theirs (usually the very threads it was made from, so
    /// their identity settles it before their contents are compared),
    /// else found or made afresh.
    fn resumed(&mut self, insts: &[Inst], resume: &Resume) -> u32 {
        match self.states.get(resume.state as usize) {
            Some(state) if Arc::ptr_eq(&state.pcs, &resume.pcs) || state.pcs == resume.pcs => {
                resume.state
            }
            _ => self.intern(insts, &resume.pcs),
        }
    }

    /// How to carry on from `state`, where a run that started at `start`
    /// of a text and had found a match ending at `found`, if any, was
    /// when the text ended.
    fn resume_at(&self, state: u32, found: Option<usize>, start: usize) -> Resume {
        Resume {
            pcs: Arc::clone(&self.states[state as usize].pcs),
            found: found.map(|found| found - start),
            state,
        }
    }
}

/// Whether `p` repeats something that itself contains a repeat, the
/// shape on which backtracking can take time quadratic in the text (each
/// way of splitting a run between the inner and outer repeats is tried
/// in turn when what follows fails).
pub fn has_nested_repeats(p: &SubPattern) -> bool {
    p.data.iter().any(|op| match op {
        OpCode::MaxRepeat { item, .. }
        | OpCode::MinRepeat { item, .. }
        | OpCode::PossessiveRepeat { item, .. } => has_repeat(item),
        OpCode::Branch(alternatives) => alternatives.iter().any(has_nested_repeats),
        OpCode::Subpattern { p, .. } | OpCode::AtomicGroup(p) => has_nested_repeats(p),
        _ => false,
    })
}

fn has_repeat(p: &SubPattern) -> bool {
    p.data.iter().any(|op| match op {
        OpCode::MaxRepeat { .. } | OpCode::MinRepeat { .. } | OpCode::PossessiveRepeat { .. } => {
            true
        }
        OpCode::Branch(alternatives) => alternatives.iter().any(has_repeat),
        OpCode::Subpattern { p, .. } | OpCode::AtomicGroup(p) => has_repeat(p),
        _ => false,
    })
}

struct Compiler<'p> {
    insts: Vec<Inst>,
    /// How many more sequences may be compiled: the unrolling of a
    /// counted repeat of an empty sequence emits nothing, so the
    /// instruction limit alone would not bound it.
    remaining: usize,
    recognition_only: bool,
    /// Lookaround bodies still to compile, with the instruction that
    /// refers to each.
    pending: Vec<(usize, &'p SubPattern, u32)>,
    /// The ASCII bitmap of each set already emitted, by the address of
    /// its items and the flags in force, so that the copies an unrolled
    /// repeat emits share one test of the 128 characters.
    tables: HashMap<(usize, u32), [u64; 2]>,
}

impl<'p> Compiler<'p> {
    fn emit(&mut self, inst: Inst) -> Result<usize, Unsupported> {
        if self.insts.len() >= PROGRAM_LIMIT {
            return Err(Unsupported);
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    /// The single-character test `op` is, under `flags`, or `None` for
    /// any other op.
    fn unit_of(&mut self, op: &OpCode, flags: u32) -> Option<Unit> {
        Some(match op {
            OpCode::Literal(cp) => Unit::Literal(*cp),
            OpCode::NotLiteral(cp) => Unit::NotLiteral(*cp),
            OpCode::Any => Unit::Any,
            OpCode::In(items) => {
                let ascii = *self
                    .tables
                    .entry((items.as_ptr() as usize, flags))
                    .or_insert_with(|| ascii_table(items, flags));
                Unit::In(items.clone(), ascii)
            }
            _ => return None,
        })
    }

    /// CPython's `_simple`: a one-op pattern that is a unit, or a
    /// non-capturing group around one, with the flags in force inside it.
    fn single_unit(&mut self, p: &SubPattern, flags: u32) -> Option<(Unit, u32)> {
        match p.data.as_slice() {
            [
                OpCode::Subpattern {
                    group: None,
                    add_flags,
                    del_flags,
                    p,
                },
            ] => self.single_unit(p, combine_flags(flags, *add_flags, *del_flags)),
            [op] => self.unit_of(op, flags).map(|unit| (unit, flags)),
            _ => None,
        }
    }

    fn seq(&mut self, p: &'p SubPattern, flags: u32) -> Result<(), Unsupported> {
        if self.remaining == 0 {
            return Err(Unsupported);
        }
        self.remaining -= 1;
        for op in &p.data {
            if let Some(unit) = self.unit_of(op, flags) {
                self.emit(Inst::Unit(unit, flags))?;
                continue;
            }
            match op {
                OpCode::At(at) => self.emit(Inst::At(*at, flags))?,
                OpCode::Failure => self.emit(Inst::Fail)?,
                OpCode::Branch(alternatives) => {
                    self.branch(alternatives, flags)?;
                    continue;
                }
                OpCode::Subpattern {
                    add_flags,
                    del_flags,
                    p,
                    ..
                } => {
                    self.seq(p, combine_flags(flags, *add_flags, *del_flags))?;
                    continue;
                }
                OpCode::Assert { direction, p } => self.look(false, *direction, p, flags)?,
                OpCode::AssertNot { direction, p } => self.look(true, *direction, p, flags)?,
                OpCode::MaxRepeat { min, max, item } => {
                    self.repeat(item, *min, *max, true, flags)?;
                    continue;
                }
                OpCode::MinRepeat { min, max, item } => {
                    self.repeat(item, *min, *max, false, flags)?;
                    continue;
                }
                OpCode::GroupRef(_)
                | OpCode::GroupRefExists { .. }
                | OpCode::AtomicGroup(_)
                | OpCode::PossessiveRepeat { .. } => return Err(Unsupported),
                OpCode::Literal(_) | OpCode::NotLiteral(_) | OpCode::Any | OpCode::In(_) => {
                    unreachable!("units are compiled above")
                }
            };
        }
        Ok(())
    }

    fn branch(&mut self, alternatives: &'p [SubPattern], flags: u32) -> Result<(), Unsupported> {
        let mut jumps = Vec::new();
        for (i, alternative) in alternatives.iter().enumerate() {
            if i + 1 == alternatives.len() {
                self.seq(alternative, flags)?;
                break;
            }
            let split = self.emit(Inst::Split(0, 0))?;
            self.seq(alternative, flags)?;
            jumps.push(self.emit(Inst::Jmp(0))?);
            self.insts[split] = Inst::Split(split + 1, self.insts.len());
        }
        let end = self.insts.len();
        for jump in jumps {
            self.insts[jump] = Inst::Jmp(end);
        }
        Ok(())
    }

    fn look(
        &mut self,
        negate: bool,
        direction: i32,
        p: &'p SubPattern,
        flags: u32,
    ) -> Result<usize, Unsupported> {
        let behind = (direction < 0).then(|| usize::try_from(get_width(p).0).unwrap_or(usize::MAX));
        let index = self.emit(Inst::Look {
            negate,
            behind,
            entry: 0,
        })?;
        self.pending.push((index, p, flags));
        Ok(index)
    }

    fn repeat(
        &mut self,
        item: &'p SubPattern,
        min: u32,
        max: u32,
        greedy: bool,
        flags: u32,
    ) -> Result<(), Unsupported> {
        if !self.recognition_only && max == MAXREPEAT && get_width(item).0 == 0 {
            return Err(Unsupported);
        }
        for _ in 0..min {
            self.seq(item, flags)?;
        }
        if max == MAXREPEAT {
            if let Some((unit, unit_flags)) = self.single_unit(item, flags) {
                self.emit(Inst::Loop(unit, unit_flags, greedy))?;
                return Ok(());
            }
            let head = self.emit(Inst::Split(0, 0))?;
            self.seq(item, flags)?;
            self.emit(Inst::Jmp(head))?;
            self.insts[head] = split_inst(head + 1, self.insts.len(), greedy);
            return Ok(());
        }
        let mut splits = Vec::new();
        for _ in min..max {
            splits.push(self.emit(Inst::Split(0, 0))?);
            self.seq(item, flags)?;
        }
        let exit = self.insts.len();
        for split in splits {
            self.insts[split] = split_inst(split + 1, exit, greedy);
        }
        Ok(())
    }
}

fn split_inst(body: usize, exit: usize, greedy: bool) -> Inst {
    if greedy {
        Inst::Split(body, exit)
    } else {
        Inst::Split(exit, body)
    }
}

/// The threads still waiting for input at the end of a text, in order of
/// preference, with the match found by then (as an offset from where the
/// run started): enough to carry on when more text follows, see
/// [`Program::resume`].
#[derive(Debug, Clone)]
pub struct Resume {
    /// The threads, cut like a [`DfaState`]'s after the first that
    /// matched, if any did.
    pcs: Arc<[usize]>,
    found: Option<usize>,
    /// The [`Dfa`] state the threads were in, to be checked before
    /// use: the automaton may have forgotten it since.
    state: u32,
}

impl PartialEq for Resume {
    fn eq(&self, other: &Resume) -> bool {
        (Arc::ptr_eq(&self.pcs, &other.pcs) || self.pcs == other.pcs) && self.found == other.found
    }
}

impl Eq for Resume {}

/// The threads alive at one position, in order of preference, with a
/// generation stamp per instruction recording which list last took it.
struct Threads {
    pcs: Vec<usize>,
    marks: Vec<usize>,
}

struct Scratch {
    current: Threads,
    next: Threads,
    stack: Vec<usize>,
    generation: usize,
}

impl Scratch {
    fn new(size: usize) -> Scratch {
        let threads = || Threads {
            pcs: Vec::with_capacity(size),
            marks: alloc::vec![0; size],
        };
        Scratch {
            current: threads(),
            next: threads(),
            stack: Vec::with_capacity(size),
            generation: 0,
        }
    }
}

impl Program {
    /// Compile `parsed`, or report that it uses something the program
    /// cannot express.
    pub fn compile(parsed: &ParsedPattern) -> Result<Program, Unsupported> {
        Self::compile_with(parsed, false)
    }

    /// Compile `parsed` for checking whether a match exists: unlike
    /// [`compile`](Self::compile) this admits an unbounded repeat of
    /// something that can match empty, at the price that the end
    /// [`scan`](Self::scan) finds need not be the one CPython prefers. A
    /// caller reads only whether a match was found, giving an exact end
    /// to check the whole of a string.
    pub fn compile_recognizer(parsed: &ParsedPattern) -> Result<Program, Unsupported> {
        Self::compile_with(parsed, true)
    }

    fn compile_with(
        parsed: &ParsedPattern,
        recognition_only: bool,
    ) -> Result<Program, Unsupported> {
        let mut compiler = Compiler {
            insts: Vec::new(),
            remaining: PROGRAM_LIMIT,
            recognition_only,
            pending: Vec::new(),
            tables: HashMap::default(),
        };
        compiler.seq(&parsed.pattern, parsed.flags)?;
        compiler.emit(Inst::Match)?;
        while let Some((index, p, flags)) = compiler.pending.pop() {
            let entry = compiler.insts.len();
            compiler.seq(p, flags)?;
            compiler.emit(Inst::Match)?;
            if let Inst::Look { entry: target, .. } = &mut compiler.insts[index] {
                *target = entry;
            }
        }
        let positional = compiler
            .insts
            .iter()
            .any(|inst| matches!(inst, Inst::At(..) | Inst::Look { .. }));
        let classes = Classes::of(&compiler.insts);
        Ok(Program {
            insts: compiler.insts,
            scratch: Mutex::new(Vec::new()),
            dfa: (!positional).then(|| Arc::new(Mutex::new(Dfa::default()))),
            classes,
        })
    }

    /// Whether the program remembers the states its runs pass through
    /// (see [`Dfa`]), which it cannot when its threads look at the text
    /// around them.
    pub fn remembers(&self) -> bool {
        self.dfa.is_some()
    }

    /// Which characters `c` behaves like in every one of the program's
    /// character tests, so that its transitions can stand for them all:
    /// an ASCII character is its own class, with the tests looked up by
    /// it; any other is told apart as [`Classes`] says.
    #[inline]
    fn class(&self, c: char) -> u64 {
        let cp = u32::from(c);
        if cp < 128 {
            return u64::from(cp);
        }
        self.class_beyond_ascii(c)
    }

    /// [`class`](Self::class) for a character beyond ASCII, kept out of
    /// the stepping loops so that an ASCII character costs them nothing
    /// but the test.
    #[inline(never)]
    fn class_beyond_ascii(&self, c: char) -> u64 {
        let cp = u32::from(c);
        match &self.classes {
            Classes::Ranges(bounds) => 128 + bounds.partition_point(|&bound| bound <= cp) as u64,
            Classes::ByChar => u64::from(cp),
            Classes::Tests(units) => {
                let mut class = 1 << 63;
                for (i, &pc) in units.iter().enumerate() {
                    let (Inst::Unit(unit, flags) | Inst::Loop(unit, flags, _)) = &self.insts[pc]
                    else {
                        unreachable!("units are character tests")
                    };
                    if unit.matches(c, *flags) {
                        class |= 1 << i;
                    }
                }
                class
            }
        }
    }

    fn take_scratch(&self) -> Scratch {
        self.scratch
            .lock()
            .pop()
            .unwrap_or_else(|| Scratch::new(self.insts.len()))
    }

    fn return_scratch(&self, scratch: Scratch) {
        self.scratch.lock().push(scratch);
    }

    /// The end of the match at `start` that CPython's
    /// `pattern.match(text, start)` finds, or with `exact_end` the one
    /// among the matches ending there that CPython's backtracking finds
    /// first (`pattern.fullmatch(text)` for the text's end), with how the
    /// match depended on the edges of the text (never [`Touch::Tail`]:
    /// the threads alive at the end are carried whole instead) and, when
    /// threads were still waiting for input there and nothing else
    /// looked at an edge, those threads, for [`resume`](Self::resume). A
    /// `start` past the end of the text is clamped to it, as CPython
    /// does.
    pub fn scan(
        &self,
        text: &[char],
        start: usize,
        exact_end: Option<usize>,
    ) -> (Option<usize>, Touch, Option<Resume>) {
        let start = start.min(text.len());
        if let (Some(dfa), None) = (&self.dfa, exact_end) {
            let mut dfa = dfa.lock();
            let state = match dfa.start {
                Some(state) => state,
                None => {
                    let mut scratch = self.take_scratch();
                    scratch.generation += 1;
                    scratch.current.pcs.clear();
                    let mut touch = Touch::Never;
                    let generation = scratch.generation;
                    self.follow(
                        &mut scratch.current,
                        &mut scratch.stack,
                        generation,
                        0,
                        text,
                        start,
                        &mut touch,
                    );
                    let state = dfa.intern(&self.insts, &scratch.current.pcs);
                    self.return_scratch(scratch);
                    dfa.start = Some(state);
                    state
                }
            };
            let (found, touch, state) = self.dfa_steps(&mut dfa, state, text, start, None);
            let resume = (touch == Touch::End).then(|| dfa.resume_at(state, found, start));
            return (found, touch, resume);
        }
        let mut scratch = self.take_scratch();
        let found = self.run_from(0, text, start, exact_end, &mut scratch);
        self.return_scratch(scratch);
        found
    }

    /// Carry on a run that [`scan`](Self::scan) started at `start` of a
    /// text of `consumed` characters, on `text`, which continues that
    /// text after `start + consumed`: the match and touch a run over all
    /// of `text` from `start` would report, without the threads the end
    /// of `text` leaves waiting (nothing carries on from here). Its one
    /// caller, the lexer simulation's carry, is better off with it inlined
    /// whatever the compiler would decide on its own.
    #[inline(always)]
    pub fn resume(
        &self,
        resume: &Resume,
        text: &[char],
        start: usize,
        consumed: usize,
    ) -> (Option<usize>, Touch) {
        let found = resume.found.map(|found| start + found);
        if let Some(dfa) = &self.dfa {
            let mut dfa = dfa.lock();
            if dfa.states.len() >= DFA_STATES {
                dfa.clear();
            }
            let state = dfa.resumed(&self.insts, resume);
            let (end, touch, _) = self.dfa_steps(&mut dfa, state, text, start + consumed, found);
            return (end, touch);
        }
        let mut scratch = self.take_scratch();
        scratch.current.pcs.clear();
        scratch.current.pcs.extend_from_slice(&resume.pcs);
        let (end, touch, _) = self.steps(
            text,
            start,
            start + consumed,
            None,
            found,
            Touch::Never,
            &mut scratch,
        );
        self.return_scratch(scratch);
        (end, touch)
    }

    fn run_from(
        &self,
        entry: usize,
        text: &[char],
        start: usize,
        exact_end: Option<usize>,
        scratch: &mut Scratch,
    ) -> (Option<usize>, Touch, Option<Resume>) {
        let mut touch = Touch::Never;
        scratch.generation += 1;
        scratch.current.pcs.clear();
        let generation = scratch.generation;
        self.follow(
            &mut scratch.current,
            &mut scratch.stack,
            generation,
            entry,
            text,
            start,
            &mut touch,
        );
        self.steps(text, start, start, exact_end, None, touch, scratch)
    }

    /// Whether the threads a run left waiting would match through `c`:
    /// on a text that continues with `c`, the match ends at or after it.
    /// Only known for a program with a [`Dfa`]; for one whose threads
    /// look at the text around them, `false`.
    pub fn swallows(&self, resume: &Resume, c: char) -> bool {
        let Some(dfa) = &self.dfa else {
            return false;
        };
        let mut dfa = dfa.lock();
        if dfa.states.len() >= DFA_STATES {
            dfa.clear();
        }
        let state = dfa.resumed(&self.insts, resume);
        let class = self.class(c);
        let next = match dfa.states[state as usize].next(class) {
            UNKNOWN => self.dfa_transition(&mut dfa, state, c, class, &[c], 0),
            next => next,
        };
        let next = &dfa.states[next as usize];
        next.waiting < next.pcs.len()
    }

    /// [`steps`](Self::steps) for a program with a [`Dfa`], from `state`
    /// at `pos`: each character leads to the state remembered for it, or
    /// to the state its threads lead to, remembered then. The state
    /// returned is the one the run was in when the text ended, for
    /// [`Dfa::resume_at`] (with [`Touch::Never`], the run died first).
    fn dfa_steps(
        &self,
        dfa: &mut Dfa,
        mut state: u32,
        text: &[char],
        mut pos: usize,
        mut found: Option<usize>,
    ) -> (Option<usize>, Touch, u32) {
        loop {
            let states = &dfa.states;
            let c = loop {
                let current = &states[state as usize];
                if current.waiting < current.pcs.len() {
                    found = Some(pos);
                }
                if current.waiting == 0 {
                    return (found, Touch::Never, state);
                }
                let Some(&c) = text.get(pos) else {
                    return (found, Touch::End, state);
                };
                let next = match u32::from(c) {
                    cp if cp < 128 => current.ascii[cp as usize],
                    _ => current.next(self.class_beyond_ascii(c)),
                };
                if next == UNKNOWN {
                    break c;
                }
                state = next;
                pos += 1;
            };
            state = self.dfa_transition(dfa, state, c, self.class(c), text, pos);
            pos += 1;
        }
    }

    /// Work out and remember where `c`, of `class`, leads from `state`
    /// at `pos`, forgetting every state first when there are too many.
    fn dfa_transition(
        &self,
        dfa: &mut Dfa,
        mut state: u32,
        c: char,
        class: u64,
        text: &[char],
        pos: usize,
    ) -> u32 {
        let mut scratch = self.take_scratch();
        scratch.generation += 1;
        scratch.next.pcs.clear();
        let mut touch = Touch::Never;
        let current = &dfa.states[state as usize];
        for &pc in &current.pcs[..current.waiting] {
            let target = match &self.insts[pc] {
                Inst::Unit(unit, flags) => unit.matches(c, *flags).then_some(pc + 1),
                Inst::Loop(unit, flags, _) => unit.matches(c, *flags).then_some(pc),
                _ => unreachable!("only characters wait for input"),
            };
            if let Some(target) = target {
                let generation = scratch.generation;
                self.follow(
                    &mut scratch.next,
                    &mut scratch.stack,
                    generation,
                    target,
                    text,
                    pos + 1,
                    &mut touch,
                );
            }
        }
        if dfa.states.len() >= DFA_STATES {
            let pcs = Arc::clone(&dfa.states[state as usize].pcs);
            dfa.clear();
            state = dfa.intern(&self.insts, &pcs);
        }
        let next = dfa.intern(&self.insts, &scratch.next.pcs);
        self.return_scratch(scratch);
        dfa.states[state as usize].set_next(class, next);
        next
    }

    /// Advance the threads in `scratch.current` from `pos` to the end of
    /// the text or until none is left.
    fn steps(
        &self,
        text: &[char],
        start: usize,
        mut pos: usize,
        exact_end: Option<usize>,
        mut found: Option<usize>,
        mut touch: Touch,
        scratch: &mut Scratch,
    ) -> (Option<usize>, Touch, Option<Resume>) {
        let Scratch {
            current,
            next,
            stack,
            generation,
        } = scratch;
        while !current.pcs.is_empty() {
            *generation += 1;
            next.pcs.clear();
            let c = text.get(pos).copied();
            let mut waiting = 0;
            for i in 0..current.pcs.len() {
                let pc = current.pcs[i];
                let target = match &self.insts[pc] {
                    Inst::Match => {
                        if exact_end.is_none_or(|end| end == pos) {
                            found = Some(pos);
                            break;
                        }
                        continue;
                    }
                    Inst::Unit(unit, flags) => {
                        c.is_some_and(|c| unit.matches(c, *flags)).then_some(pc + 1)
                    }
                    Inst::Loop(unit, flags, _) => {
                        c.is_some_and(|c| unit.matches(c, *flags)).then_some(pc)
                    }
                    Inst::At(..)
                    | Inst::Look { .. }
                    | Inst::Split(..)
                    | Inst::Jmp(_)
                    | Inst::Fail => {
                        unreachable!("only characters and matches wait for input")
                    }
                };
                waiting = i + 1;
                if let Some(target) = target {
                    self.follow(next, stack, *generation, target, text, pos + 1, &mut touch);
                }
            }
            if c.is_none() {
                if waiting == 0 {
                    return (found, touch, None);
                }
                let resume = (touch == Touch::Never && exact_end.is_none()).then(|| Resume {
                    pcs: Arc::from(&current.pcs[..waiting]),
                    found: found.map(|found| found - start),
                    state: UNKNOWN,
                });
                return (found, Touch::End, resume);
            }
            core::mem::swap(current, next);
            pos += 1;
        }
        (found, touch, None)
    }

    /// Add to `list` the threads reachable from `pc` at `pos` without
    /// consuming input, in order of preference.
    fn follow(
        &self,
        list: &mut Threads,
        stack: &mut Vec<usize>,
        generation: usize,
        pc: usize,
        text: &[char],
        pos: usize,
        touch: &mut Touch,
    ) {
        stack.clear();
        stack.push(pc);
        while let Some(pc) = stack.pop() {
            if pc & EMIT != 0 {
                list.pcs.push(pc & !EMIT);
                continue;
            }
            if list.marks[pc] == generation {
                continue;
            }
            list.marks[pc] = generation;
            match &self.insts[pc] {
                Inst::Jmp(target) => stack.push(*target),
                Inst::Split(first, second) => {
                    stack.push(*second);
                    stack.push(*first);
                }
                Inst::At(at, flags) => {
                    if pos == 0
                        || (!matches!(at, AtCode::Beginning | AtCode::BeginningString)
                            && pos + 1 >= text.len())
                    {
                        *touch = Touch::End;
                    }
                    if at_matches(*at, text, pos, *flags) {
                        stack.push(pc + 1);
                    }
                }
                Inst::Look {
                    negate,
                    behind,
                    entry,
                } => {
                    let (matched, inner) = self.look(*behind, *entry, text, pos);
                    if behind.is_some_and(|w| w > pos) || inner != Touch::Never {
                        *touch = Touch::End;
                    }
                    if matched != *negate {
                        stack.push(pc + 1);
                    }
                }
                Inst::Fail => {}
                Inst::Loop(_, _, greedy) => {
                    if *greedy {
                        list.pcs.push(pc);
                        stack.push(pc + 1);
                    } else {
                        stack.push(pc | EMIT);
                        stack.push(pc + 1);
                    }
                }
                Inst::Unit(..) | Inst::Match => list.pcs.push(pc),
            }
        }
    }

    /// Whether the lookaround body at `entry` matches at `pos`: ahead of
    /// it, or ending exactly at it from its fixed width `behind` back.
    fn look(
        &self,
        behind: Option<usize>,
        entry: usize,
        text: &[char],
        pos: usize,
    ) -> (bool, Touch) {
        let mut scratch = self.take_scratch();
        let mut touch = Touch::Never;
        let mut run = |start: usize, exact_end: Option<usize>| {
            let (found, inner, _) = self.run_from(entry, text, start, exact_end, &mut scratch);
            if inner != Touch::Never {
                touch = Touch::End;
            }
            found.is_some()
        };
        let found = match behind {
            None => run(pos, None),
            Some(w) => w <= pos && run(pos - w, Some(pos)),
        };
        self.return_scratch(scratch);
        (found, touch)
    }
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/re_vm_tests.rs"]
mod tests;
