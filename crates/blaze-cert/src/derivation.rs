//! The derivation's format (certificates.md, "The rows"): a DAG of steps `(goal, by)`, one row
//! each, in columns (structure of arrays), little-endian, written once by a [`Builder`] and read
//! in place by a [`View`].
//!
//! Rows are in post-order: a step's nested steps are rows BEFORE it, so a sub-derivation two
//! steps share is one row both name, and the root is the last row. A coinductive back-edge is
//! the one reference to a LATER row (an ancestor, written after its descendants). Goals are the
//! solver's canonical words (`blaze_solve::canon`): definitions by their portable keys, terms by
//! structure, inference variables numbered by first occurrence across the whole certificate.
//!
//! [`View::read`] checks every limit before a row is trusted: the length, the caps, every end
//! offset, every tag and payload, every nested reference (to an earlier row) and every
//! back-edge (to a later row). A violation is a [`FormatError`] naming the row; nothing in
//! this module panics on any input.

use blaze_int::{Int, idx, len32};
use blaze_own::{Alloc, Vec};
use blaze_solve::canon::Canon;
use blaze_solve::term::{Def, Pred, Ty};
use blaze_solve::tys::Tys;
use blaze_solve::SolveError;

/// The format's first bytes.
pub const MAGIC: [u8; 4] = *b"bcrt";
/// The format's version; a reader refuses any other.
pub const VERSION: u32 = 1;
/// The header: magic, version, then the counts of rows, goal words, argument words, nested
/// references and impl keys, each a `u32`.
pub const HEADER: usize = 28;
/// The fixed bytes of a row: its goal's end, its arguments' end, its nested list's end, its
/// payload (`u32` each) and its tag (`u8`).
pub const ROW_BYTES: usize = 17;

// The caps (rule 18): a certificate past any of them is refused, never truncated.
pub const MAX_ROWS: u32 = 1 << 20;
pub const MAX_GOAL_WORDS: u32 = 1 << 24;
pub const MAX_ARG_WORDS: u32 = 1 << 24;
pub const MAX_NESTED: u32 = 1 << 22;
pub const MAX_DEFS: u32 = 1 << 20;

/// A back-edge not closed yet (the builder's mark; never in a finished certificate).
const UNSET: u32 = u32::MAX;

const TAG_IMPL: u8 = 0;
const TAG_ENV: u8 = 1;
const TAG_BUILTIN: u8 = 2;
const TAG_AUTO: u8 = 3;
const TAG_OBJECT: u8 = 4;
const TAG_ALIAS_BOUND: u8 = 5;
const TAG_NORMALIZE: u8 = 6;
const TAG_COINDUCTIVE: u8 = 7;

/// A row of one derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepId(pub u32);

/// The builtin rules a step may name, one per rule of the solver's builtin trait candidates
/// (builtin.rs `assemble`) that the checker proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    /// `consider_builtin_sizedness_candidates`, `Sized`.
    Sized,
    /// The same, `MetaSized`.
    MetaSized,
    /// `consider_builtin_copy_clone_candidate`.
    CopyClone,
    /// `consider_builtin_tuple_candidate`.
    Tuple,
    /// `consider_builtin_fn_ptr_trait_candidate`.
    FnPtr,
    /// `PointeeSized`: every type.
    PointeeSized,
    /// `Pointee`, `DiscriminantKind`, `Destruct`: every type, no nested goal.
    AnyType,
    /// `consider_error_guaranteed_candidate`: an error self type implements every trait.
    Error,
}

impl Builtin {
    pub const ALL: [Builtin; 8] = [
        Builtin::Sized,
        Builtin::MetaSized,
        Builtin::CopyClone,
        Builtin::Tuple,
        Builtin::FnPtr,
        Builtin::PointeeSized,
        Builtin::AnyType,
        Builtin::Error,
    ];

    pub fn code(self) -> u32 {
        match self {
            Builtin::Sized => 0,
            Builtin::MetaSized => 1,
            Builtin::CopyClone => 2,
            Builtin::Tuple => 3,
            Builtin::FnPtr => 4,
            Builtin::PointeeSized => 5,
            Builtin::AnyType => 6,
            Builtin::Error => 7,
        }
    }

    pub fn from_code(c: u32) -> Option<Builtin> {
        Builtin::ALL.iter().copied().find(|b| b.code() == c)
    }
}

/// How a step is proven, as a builder is given it.
#[derive(Clone, Copy, Debug)]
pub enum By<'x> {
    /// An impl: its key and its generic arguments (arena terms), every one; nested: the steps of
    /// the impl's where-clauses in declaration order.
    Impl { impl_: Def, args: &'x [Ty] },
    /// The environment's clause at this index (of the elaborated env the checker is handed).
    Env { clause: u32 },
    Builtin(Builtin),
    /// The structural impl of an auto trait (`consider_auto_trait_candidate`); nested: one step
    /// per constituent type, in the rule's order.
    Auto,
    /// The bound at this index of the `dyn` self type's list; nested: the trait's supertraits
    /// and its non-generic associated types' bounds, as the solver adds them.
    Object { bound: u32 },
    /// The item bound at this index of the self type's rigid alias (its associated type's
    /// bounds, or an opaque type's).
    AliasBound { bound: u32 },
    /// A projection `<.. as Trait>::A == U` by the impl that defines `A`: its key and arguments;
    /// nested: the impl's where-clauses.
    Normalize { impl_: Def, args: &'x [Ty] },
}

/// A view of a run of little-endian `u32` words in a certificate's bytes.
#[derive(Clone, Copy, Debug)]
pub struct Words<'d> {
    bytes: &'d [u8],
}

impl<'d> Words<'d> {
    pub fn len(&self) -> usize {
        self.bytes.len() / 4
    }

    /// Word `i`; `None` past the end.
    pub fn get(&self, i: usize) -> Option<u32> {
        let o = i.checked_mul(4)?;
        le32(self.bytes, o)
    }

    /// Appends every word to `out` (the decoder takes `&[u32]`; the bytes need not be aligned).
    pub fn copy_into(&self, out: &mut Vec<u32, Alloc<'_>>) {
        for c in self.bytes.chunks_exact(4) {
            out.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
    }
}

/// A step as read.
#[derive(Clone, Copy, Debug)]
pub enum Step<'d> {
    Impl { impl_: Def, args: Words<'d> },
    Env { clause: u32 },
    Builtin(Builtin),
    Auto,
    Object { bound: u32 },
    AliasBound { bound: u32 },
    Normalize { impl_: Def, args: Words<'d> },
    /// Closes a cycle with the later row `target`, whose goal this row's goal must equal.
    Coinductive { target: StepId },
}

/// A table of the format, for a cap's refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Rows,
    GoalWords,
    ArgWords,
    Nested,
    Defs,
}

/// A column of end offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Goal,
    Args,
    Nested,
}

/// Why bytes are not a derivation. Each names the row (or table) it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// Fewer bytes than the header.
    Short { have: usize },
    Magic,
    Version(u32),
    TooMany { table: Table, n: u32, cap: u32 },
    /// The bytes are not exactly the header and the columns the counts give.
    Length { want: u64, have: u64 },
    /// No row: a derivation has at least its root.
    Empty,
    /// An end offset that goes back, past its table's count, or (for a goal) leaves it empty;
    /// `row` is the row count when the last end is not the table's count.
    Ends { column: Column, row: u32 },
    Tag { row: u32, tag: u8 },
    /// A nested reference to this row or a later one (rows are in post-order).
    NestedForward { row: u32, child: u32 },
    /// A back-edge to this row, an earlier one or past the last.
    BackEdge { row: u32, target: u32 },
    /// An impl key's index past the keys.
    DefIndex { row: u32, index: u32 },
    BuiltinCode { row: u32, code: u32 },
    /// Arguments on a step that takes none, or nested steps on a back-edge.
    Payload { row: u32 },
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "blaze-cert: not a derivation: {self:?}")
    }
}

impl core::error::Error for FormatError {}

/// Why a builder refused a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildError {
    TooMany { table: Table, cap: u32 },
    NestedForward { row: u32, child: u32 },
    BackEdge { row: u32, target: u32 },
    /// A back-edge never closed (`Builder::close_cycle`).
    Unclosed { row: u32 },
    Empty,
    /// The goal or an argument could not be written in canonical form.
    Canon(SolveError),
}

impl core::fmt::Display for BuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "blaze-cert: a derivation cannot be built: {self:?}")
    }
}

impl core::error::Error for BuildError {}

fn le32(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Writes a derivation step by step (post-order) and then its bytes.
pub struct Builder<'a> {
    /// Kept across rows: a variable's canonical number is its first occurrence in the whole
    /// certificate, so `?0` in two rows is one variable.
    canon: Canon<'a>,
    scratch: Vec<u32, Alloc<'a>>,
    goal_end: Vec<u32, Alloc<'a>>,
    arg_end: Vec<u32, Alloc<'a>>,
    nested_end: Vec<u32, Alloc<'a>>,
    payload: Vec<u32, Alloc<'a>>,
    tag: Vec<u8, Alloc<'a>>,
    goal_words: Vec<u32, Alloc<'a>>,
    arg_words: Vec<u32, Alloc<'a>>,
    nested: Vec<u32, Alloc<'a>>,
    defs: Vec<Def, Alloc<'a>>,
    /// The first error: every later call returns it.
    spent: Option<BuildError>,
}

fn grow(n: usize, add: usize, table: Table, cap: u32) -> Result<(), BuildError> {
    let total = n.checked_add(add).ok_or(BuildError::TooMany { table, cap })?;
    if total > idx(cap) {
        return Err(BuildError::TooMany { table, cap });
    }
    Ok(())
}

fn len_u32(n: usize, table: Table, cap: u32) -> Result<u32, BuildError> {
    len32(n).map_err(|_| BuildError::TooMany { table, cap })
}

impl<'a> Builder<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Builder<'a> {
        Builder {
            canon: Canon::new_in(alloc),
            scratch: Vec::new_in(alloc),
            goal_end: Vec::new_in(alloc),
            arg_end: Vec::new_in(alloc),
            nested_end: Vec::new_in(alloc),
            payload: Vec::new_in(alloc),
            tag: Vec::new_in(alloc),
            goal_words: Vec::new_in(alloc),
            arg_words: Vec::new_in(alloc),
            nested: Vec::new_in(alloc),
            defs: Vec::new_in(alloc),
            spent: None,
        }
    }

    /// The rows written so far.
    pub fn rows(&self) -> usize {
        self.tag.len()
    }

    fn next_row(&self) -> Result<u32, BuildError> {
        grow(self.rows(), 1, Table::Rows, MAX_ROWS)?;
        len_u32(self.rows(), Table::Rows, MAX_ROWS)
    }

    /// Appends the step proving `goal` by `by`, its nested steps `nested` (rows already
    /// written, in the order the rule's clauses come). Every check runs before any column
    /// grows; after an error the builder is spent and refuses every later call with it (its
    /// variable numbering may have moved, so its bytes would not be a fresh build's).
    pub fn push<T: Tys>(&mut self, tys: &T, goal: Pred, by: By<'_>, nested: &[StepId]) -> Result<StepId, BuildError> {
        let r = self.push_row(tys, goal, by, nested);
        self.spend(r)
    }

    /// Appends a back-edge for `goal`: the step it closes a cycle with is an ancestor, written
    /// later and named by [`Builder::close_cycle`].
    pub fn back_edge<T: Tys>(&mut self, tys: &T, goal: Pred) -> Result<StepId, BuildError> {
        let r = self.push_row(tys, goal, By::Auto, &[]).map(|row| {
            // The row is the last one: mark it a back-edge with no target yet.
            let last = self.tag.len() - 1;
            self.tag[last] = TAG_COINDUCTIVE;
            self.payload[last] = UNSET;
            row
        });
        self.spend(r)
    }

    fn spend<X>(&mut self, r: Result<X, BuildError>) -> Result<X, BuildError> {
        if let Some(e) = self.spent {
            return Err(e);
        }
        if let Err(e) = &r {
            self.spent = Some(*e);
        }
        r
    }

    fn push_row<T: Tys>(&mut self, tys: &T, goal: Pred, by: By<'_>, nested: &[StepId]) -> Result<StepId, BuildError> {
        if let Some(e) = self.spent {
            return Err(e);
        }
        let row = self.next_row()?;
        for &c in nested {
            if c.0 >= row {
                return Err(BuildError::NestedForward { row, child: c.0 });
            }
        }
        let (tag, args, def): (u8, &[Ty], Option<Def>) = match by {
            By::Impl { impl_, args } => (TAG_IMPL, args, Some(impl_)),
            By::Normalize { impl_, args } => (TAG_NORMALIZE, args, Some(impl_)),
            By::Env { .. } => (TAG_ENV, &[], None),
            By::Builtin(_) => (TAG_BUILTIN, &[], None),
            By::Auto => (TAG_AUTO, &[], None),
            By::Object { .. } => (TAG_OBJECT, &[], None),
            By::AliasBound { .. } => (TAG_ALIAS_BOUND, &[], None),
        };
        // The goal's words, then the arguments', in one scratch: `split` marks where.
        self.scratch.clear();
        self.canon.pred(tys, &mut self.scratch, goal).map_err(BuildError::Canon)?;
        let split = self.scratch.len();
        for &t in args {
            self.canon.term(tys, &mut self.scratch, t).map_err(BuildError::Canon)?;
        }
        let n_args = self.scratch.len() - split;
        grow(self.goal_words.len(), split, Table::GoalWords, MAX_GOAL_WORDS)?;
        grow(self.arg_words.len(), n_args, Table::ArgWords, MAX_ARG_WORDS)?;
        grow(self.nested.len(), nested.len(), Table::Nested, MAX_NESTED)?;
        if def.is_some() {
            grow(self.defs.len(), 1, Table::Defs, MAX_DEFS)?;
        }
        let payload = match by {
            By::Impl { .. } | By::Normalize { .. } => len_u32(self.defs.len(), Table::Defs, MAX_DEFS)?,
            By::Env { clause } => clause,
            By::Builtin(b) => b.code(),
            By::Auto => 0,
            By::Object { bound } => bound,
            By::AliasBound { bound } => bound,
        };
        // Every check passed (the caps bound each length below `u32::MAX`): commit.
        if let Some(d) = def {
            self.defs.push(d);
        }
        self.goal_words.extend_from_slice(&self.scratch[..split]);
        self.arg_words.extend_from_slice(&self.scratch[split..]);
        for &c in nested {
            self.nested.push(c.0);
        }
        self.goal_end.push(len_u32(self.goal_words.len(), Table::GoalWords, MAX_GOAL_WORDS)?);
        self.arg_end.push(len_u32(self.arg_words.len(), Table::ArgWords, MAX_ARG_WORDS)?);
        self.nested_end.push(len_u32(self.nested.len(), Table::Nested, MAX_NESTED)?);
        self.payload.push(payload);
        self.tag.push(tag);
        Ok(StepId(row))
    }

    /// Names the ancestor `target` the back-edge `back` closes a cycle with.
    pub fn close_cycle(&mut self, back: StepId, target: StepId) -> Result<(), BuildError> {
        let b = idx(back.0);
        let open = self.tag.get(b) == Some(&TAG_COINDUCTIVE) && self.payload.get(b) == Some(&UNSET);
        let r = if !open || target.0 <= back.0 || idx(target.0) >= self.rows() {
            Err(BuildError::BackEdge { row: back.0, target: target.0 })
        } else {
            self.payload[b] = target.0;
            Ok(())
        };
        self.spend(r)
    }

    /// Writes the certificate's bytes to `out` (cleared first): the same steps give the same
    /// bytes on every machine (rule 25).
    pub fn finish(&self, out: &mut Vec<u8, Alloc<'_>>) -> Result<(), BuildError> {
        if let Some(e) = self.spent {
            return Err(e);
        }
        if self.rows() == 0 {
            return Err(BuildError::Empty);
        }
        for (i, (&t, &p)) in self.tag.iter().zip(self.payload.iter()).enumerate() {
            if t == TAG_COINDUCTIVE && p == UNSET {
                return Err(BuildError::Unclosed { row: len_u32(i, Table::Rows, MAX_ROWS)? });
            }
        }
        out.clear();
        out.extend_from_slice(&MAGIC);
        let counts = [
            VERSION,
            len_u32(self.rows(), Table::Rows, MAX_ROWS)?,
            len_u32(self.goal_words.len(), Table::GoalWords, MAX_GOAL_WORDS)?,
            len_u32(self.arg_words.len(), Table::ArgWords, MAX_ARG_WORDS)?,
            len_u32(self.nested.len(), Table::Nested, MAX_NESTED)?,
            len_u32(self.defs.len(), Table::Defs, MAX_DEFS)?,
        ];
        for c in counts {
            out.extend_from_slice(&c.to_le_bytes());
        }
        for col in [&self.goal_end, &self.arg_end, &self.nested_end, &self.payload] {
            for &w in col.iter() {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.tag);
        for col in [&self.goal_words, &self.arg_words, &self.nested] {
            for &w in col.iter() {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        for d in self.defs.iter() {
            out.extend_from_slice(&d.0.to_le_bytes());
        }
        Ok(())
    }
}

/// A derivation read in place: the bytes, the counts and where each column starts. Only
/// [`View::read`] makes one, after every check, so its accessors index in bounds.
#[derive(Clone, Copy, Debug)]
pub struct View<'d> {
    bytes: &'d [u8],
    rows: u32,
    goal_end: usize,
    arg_end: usize,
    nested_end: usize,
    payload: usize,
    tag: usize,
    goal_words: usize,
    arg_words: usize,
    nested: usize,
    defs: usize,
}

impl<'d> View<'d> {
    /// Reads and checks `bytes` (the module doc): a [`FormatError`] for any violation.
    pub fn read(bytes: &'d [u8]) -> Result<View<'d>, FormatError> {
        if bytes.len() < HEADER {
            return Err(FormatError::Short { have: bytes.len() });
        }
        if bytes.get(0..4) != Some(&MAGIC[..]) {
            return Err(FormatError::Magic);
        }
        let h = |i: usize| le32(bytes, 4 + 4 * i).ok_or(FormatError::Short { have: bytes.len() });
        let version = h(0)?;
        if version != VERSION {
            return Err(FormatError::Version(version));
        }
        let (rows, goals, args, nested, defs) = (h(1)?, h(2)?, h(3)?, h(4)?, h(5)?);
        for (table, n, cap) in [
            (Table::Rows, rows, MAX_ROWS),
            (Table::GoalWords, goals, MAX_GOAL_WORDS),
            (Table::ArgWords, args, MAX_ARG_WORDS),
            (Table::Nested, nested, MAX_NESTED),
            (Table::Defs, defs, MAX_DEFS),
        ] {
            if n > cap {
                return Err(FormatError::TooMany { table, n, cap });
            }
        }
        // Under the caps every sum below is far from `u64::MAX`, and the offsets after it from
        // `usize::MAX` on a 32- or 64-bit target (the length matched).
        let want = HEADER.widen_to::<u64>()
            + u64::from(rows) * ROW_BYTES.widen_to::<u64>()
            + 4 * (u64::from(goals) + u64::from(args) + u64::from(nested))
            + 16 * u64::from(defs);
        let have = bytes.len().widen_to::<u64>();
        if want != have {
            return Err(FormatError::Length { want, have });
        }
        if rows == 0 {
            return Err(FormatError::Empty);
        }
        let r = idx(rows);
        let goal_end = HEADER;
        let arg_end = goal_end + 4 * r;
        let nested_end = arg_end + 4 * r;
        let payload = nested_end + 4 * r;
        let tag = payload + 4 * r;
        let goal_words = tag + r;
        let arg_words = goal_words + 4 * idx(goals);
        let nested_at = arg_words + 4 * idx(args);
        let defs_at = nested_at + 4 * idx(nested);
        let v = View { bytes, rows, goal_end, arg_end, nested_end, payload, tag, goal_words, arg_words, nested: nested_at, defs: defs_at };
        v.check_rows(goals, args, nested, defs)?;
        Ok(v)
    }

    /// Element `i` of the `u32` column at `at`. In bounds for every `i` below the column's
    /// count: `read` checked the length the counts give, so an index past it is a bug here.
    fn col(&self, at: usize, i: u32) -> u32 {
        let o = at + 4 * idx(i);
        let b = self.bytes;
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    fn check_rows(&self, goals: u32, args: u32, nested: u32, defs: u32) -> Result<(), FormatError> {
        let (mut g0, mut a0, mut n0) = (0u32, 0u32, 0u32);
        for row in 0..self.rows {
            let (g, a, n) = (self.col(self.goal_end, row), self.col(self.arg_end, row), self.col(self.nested_end, row));
            if g <= g0 || g > goals {
                return Err(FormatError::Ends { column: Column::Goal, row });
            }
            if a < a0 || a > args {
                return Err(FormatError::Ends { column: Column::Args, row });
            }
            if n < n0 || n > nested {
                return Err(FormatError::Ends { column: Column::Nested, row });
            }
            let tag = self.bytes[self.tag + idx(row)];
            let p = self.col(self.payload, row);
            let takes_args = matches!(tag, TAG_IMPL | TAG_NORMALIZE);
            match tag {
                TAG_IMPL | TAG_NORMALIZE => {
                    if p >= defs {
                        return Err(FormatError::DefIndex { row, index: p });
                    }
                }
                TAG_BUILTIN => {
                    if Builtin::from_code(p).is_none() {
                        return Err(FormatError::BuiltinCode { row, code: p });
                    }
                }
                TAG_COINDUCTIVE => {
                    if p <= row || p >= self.rows {
                        return Err(FormatError::BackEdge { row, target: p });
                    }
                    if n != n0 {
                        return Err(FormatError::Payload { row });
                    }
                }
                TAG_ENV | TAG_AUTO | TAG_OBJECT | TAG_ALIAS_BOUND => {}
                _ => return Err(FormatError::Tag { row, tag }),
            }
            if !takes_args && a != a0 {
                return Err(FormatError::Payload { row });
            }
            for k in n0..n {
                let child = self.col(self.nested, k);
                if child >= row {
                    return Err(FormatError::NestedForward { row, child });
                }
            }
            (g0, a0, n0) = (g, a, n);
        }
        if g0 != goals {
            return Err(FormatError::Ends { column: Column::Goal, row: self.rows });
        }
        if a0 != args {
            return Err(FormatError::Ends { column: Column::Args, row: self.rows });
        }
        if n0 != nested {
            return Err(FormatError::Ends { column: Column::Nested, row: self.rows });
        }
        Ok(())
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// The root: the last row.
    pub fn root(&self) -> StepId {
        StepId(self.rows - 1)
    }

    /// `[start, end)` of row `i` in an end-offset column.
    fn range(&self, at: usize, i: u32) -> (u32, u32) {
        let end = self.col(at, i);
        let start = if i == 0 { 0 } else { self.col(at, i - 1) };
        (start, end)
    }

    fn words(&self, base: usize, (s, e): (u32, u32)) -> Words<'d> {
        // In bounds and ordered: `read` checked every end offset.
        let (s, e) = (base + 4 * idx(s), base + 4 * idx(e));
        Words { bytes: &self.bytes[s..e] }
    }

    /// Row `i`'s goal words (`i < rows`).
    pub fn goal(&self, i: StepId) -> Words<'d> {
        self.words(self.goal_words, self.range(self.goal_end, i.0))
    }

    /// Row `i`'s nested steps, in order.
    pub fn nested(&self, i: StepId) -> impl Iterator<Item = StepId> + '_ {
        let (s, e) = self.range(self.nested_end, i.0);
        (s..e).map(move |k| StepId(self.col(self.nested, k)))
    }

    /// How many nested steps row `i` has.
    pub fn nested_len(&self, i: StepId) -> u32 {
        let (s, e) = self.range(self.nested_end, i.0);
        e - s
    }

    /// Row `i`'s step.
    pub fn step(&self, i: StepId) -> Step<'d> {
        let p = self.col(self.payload, i.0);
        let args = self.words(self.arg_words, self.range(self.arg_end, i.0));
        let def = || {
            // In bounds: `read` checked the index against the keys' count.
            let o = self.defs + 16 * idx(p);
            let mut a = [0u8; 16];
            a.copy_from_slice(&self.bytes[o..o + 16]);
            Def(u128::from_le_bytes(a))
        };
        match self.bytes[self.tag + idx(i.0)] {
            TAG_IMPL => Step::Impl { impl_: def(), args },
            TAG_NORMALIZE => Step::Normalize { impl_: def(), args },
            TAG_ENV => Step::Env { clause: p },
            TAG_BUILTIN => match Builtin::from_code(p) {
                Some(b) => Step::Builtin(b),
                None => unreachable!("View::read checked row {}'s builtin code {p}", i.0),
            },
            TAG_AUTO => Step::Auto,
            TAG_OBJECT => Step::Object { bound: p },
            TAG_ALIAS_BOUND => Step::AliasBound { bound: p },
            _ => Step::Coinductive { target: StepId(p) },
        }
    }
}
