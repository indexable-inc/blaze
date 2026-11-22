//! The inference variables: a union-find table per kind (type, int, float, const) and the
//! universes of region variables, under one undo log.
//!
//! References: rustc_type_ir/src/infer_ctxt.rs (`InferCtxtLike`: the operations), and rustc's
//! `InferCtxtUndoLogs` with `ena`'s snapshot tables (one log for every table, so a snapshot is
//! one position and a rollback undoes exactly the writes made since it, in O(those writes)).
//!
//! A table is four columns: parent, rank, value (`NONE` while unbound) and universe. Values are
//! row ids of the arena: a type for type, int and float variables (an int variable's value is
//! an integer type), a const for const variables. Writes are logged only while a snapshot is
//! open; with none open, `find` halves paths (writes nobody can roll back to before).

use blaze_own::{Alloc, Vec};

use crate::ids::{Universe, VarKind, ix, row, small};

/// An unbound variable's value.
pub(crate) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Table {
    Ty = 0,
    Int = 1,
    Float = 2,
    Const = 3,
}

impl Table {
    const ALL: [Table; 4] = [Table::Ty, Table::Int, Table::Float, Table::Const];

    const fn code(self) -> u8 {
        match self {
            Table::Ty => 0,
            Table::Int => 1,
            Table::Float => 2,
            Table::Const => 3,
        }
    }

    const fn at(self) -> usize {
        crate::ids::ix8(self.code())
    }
}

const OP_NEW: u8 = 0;
const OP_PARENT: u8 = 1;
const OP_RANK: u8 = 2;
const OP_VALUE: u8 = 3;
const OP_UNIVERSE: u8 = 4;
const OP_NEW_REGION: u8 = 5;

/// One write, with what it overwrote.
#[derive(Clone, Copy)]
struct Undo {
    table: u8,
    op: u8,
    at: u32,
    was: u32,
}

struct Uf<'a> {
    parent: Vec<u32, Alloc<'a>>,
    rank: Vec<u8, Alloc<'a>>,
    value: Vec<u32, Alloc<'a>>,
    universe: Vec<u32, Alloc<'a>>,
}

impl<'a> Uf<'a> {
    fn new_in(alloc: Alloc<'a>) -> Uf<'a> {
        Uf { parent: Vec::new_in(alloc), rank: Vec::new_in(alloc), value: Vec::new_in(alloc), universe: Vec::new_in(alloc) }
    }

    fn truncate(&mut self, n: usize) {
        self.parent.truncate(n);
        self.rank.truncate(n);
        self.value.truncate(n);
        self.universe.truncate(n);
    }

    fn bytes(&self) -> usize {
        self.parent.capacity() * 4 + self.rank.capacity() + self.value.capacity() * 4 + self.universe.capacity() * 4
    }
}

/// A position to roll the variables back to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VarSnapshot {
    log: u32,
    bound: u32,
    universe: Universe,
    depth: u32,
}

/// A variable of a table, as the binding log names it: its kind and its index in that kind's
/// table (the `a` column of its `TY_VAR`, `INT_VAR`, `FLOAT_VAR` or const `VAR` row).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VarId {
    pub kind: VarKind,
    pub index: u32,
}

/// A bug the snapshot test plants: the int table's writes are not logged.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    Clean,
    ForgetIntUndo,
    /// The binding log misses the int table's bindings.
    ForgetIntBinding,
}

/// The variable tables (see the module).
pub struct Vars<'a> {
    t: [Uf<'a>; 4],
    re_universe: Vec<u32, Alloc<'a>>,
    log: Vec<Undo, Alloc<'a>>,
    /// The binding log (see [`Vars::bindings`]).
    bound: Vec<VarId, Alloc<'a>>,
    open: u32,
    universe: Universe,
    #[cfg(test)]
    pub(crate) plant: Plant,
}

impl<'a> Vars<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Vars<'a> {
        Vars {
            t: [Uf::new_in(alloc), Uf::new_in(alloc), Uf::new_in(alloc), Uf::new_in(alloc)],
            re_universe: Vec::new_in(alloc),
            log: Vec::new_in(alloc),
            bound: Vec::new_in(alloc),
            open: 0,
            universe: Universe::ROOT,
            #[cfg(test)]
            plant: Plant::Clean,
        }
    }

    #[inline(always)]
    fn note(&mut self, table: Table, op: u8, at: u32, was: u32) {
        if self.open == 0 {
            return;
        }
        #[cfg(test)]
        if self.plant == Plant::ForgetIntUndo && table == Table::Int {
            return;
        }
        self.log.push(Undo { table: table.code(), op, at, was });
    }

    /// The current universe (`InferCtxtLike::universe`).
    pub fn universe(&self) -> Universe {
        self.universe
    }

    /// Enters a new universe (`InferCtxtLike::create_next_universe`); rolled back with the
    /// snapshot it was made in.
    pub fn create_next_universe(&mut self) -> Universe {
        self.universe = Universe(self.universe.0 + 1);
        self.universe
    }

    /// A new variable of `table` in `u`.
    pub(crate) fn new_var(&mut self, table: Table, u: Universe) -> u32 {
        let t = &mut self.t[table.at()];
        let Some(id) = row(t.parent.len()) else {
            panic!("blaze-ty: a task's variable table is full (rule 18)")
        };
        t.parent.push(id);
        t.rank.push(0);
        t.value.push(NONE);
        t.universe.push(u.0);
        self.note(table, OP_NEW, id, 0);
        id
    }

    /// A new region variable in `u`.
    pub(crate) fn new_region_var(&mut self, u: Universe) -> u32 {
        let Some(id) = row(self.re_universe.len()) else {
            panic!("blaze-ty: a task's region variable table is full (rule 18)")
        };
        self.re_universe.push(u.0);
        if self.open > 0 {
            self.log.push(Undo { table: 0, op: OP_NEW_REGION, at: id, was: 0 });
        }
        id
    }

    /// How many variables `table` has.
    pub(crate) fn count(&self, table: Table) -> usize {
        self.t[table.at()].parent.len()
    }

    pub fn region_var_count(&self) -> usize {
        self.re_universe.len()
    }

    /// How many variables of `kind` exist (region variables included), made by the context or
    /// by relating and folding (generalization, instantiation).
    pub fn var_count(&self, kind: VarKind) -> usize {
        match Table::of(kind) {
            Some(t) => self.count(t),
            None => self.re_universe.len(),
        }
    }

    /// The binding log: every variable that stopped being the unbound root of its class, in
    /// the order it happened. A bind logs the class's root; a union logs the root that went
    /// under the other, and the new root too when the class took a value from the joined
    /// one. A rollback truncates the log to its length at the snapshot; a commit keeps it.
    /// The solver's fulfillment wakes the obligations stalled on a logged variable (a stalled
    /// obligation waits on roots: what `probe` gives for an unbound variable).
    ///
    /// Without a snapshot open each variable is logged at most once (a class never becomes
    /// unbound or a root again), so the log holds at most one entry per variable plus what
    /// rolled-back snapshots made.
    pub fn bindings(&self) -> &[VarId] {
        &self.bound
    }

    #[inline(always)]
    fn log_bound(&mut self, table: Table, v: u32) {
        #[cfg(test)]
        if self.plant == Plant::ForgetIntBinding && table == Table::Int {
            return;
        }
        let kind = match table {
            Table::Ty => VarKind::Ty,
            Table::Int => VarKind::Int,
            Table::Float => VarKind::Float,
            Table::Const => VarKind::Const,
        };
        self.bound.push(VarId { kind, index: v });
    }

    /// How many undo entries are logged: what a write-free read must leave unchanged.
    #[cfg(test)]
    pub(crate) fn undo_len(&self) -> usize {
        self.log.len()
    }

    /// The root of `v`'s class, without writing (any snapshot state).
    #[inline]
    pub(crate) fn root(&self, table: Table, mut v: u32) -> u32 {
        let p = &self.t[table.at()].parent;
        loop {
            let up = p[ix(v)];
            if up == v {
                return v;
            }
            v = up;
        }
    }

    /// The root of `v`'s class; halves the path when no snapshot is open.
    #[inline]
    pub(crate) fn find(&mut self, table: Table, v: u32) -> u32 {
        if self.open > 0 {
            return self.root(table, v);
        }
        let p = &mut self.t[table.at()].parent;
        let mut v = v;
        loop {
            let up = p[ix(v)];
            if up == v {
                return v;
            }
            let grand = p[ix(up)];
            p[ix(v)] = grand;
            v = grand;
        }
    }

    /// The value of `v`'s class, if bound.
    #[inline]
    pub(crate) fn value(&self, table: Table, v: u32) -> Option<u32> {
        let r = self.root(table, v);
        let x = self.t[table.at()].value[ix(r)];
        if x == NONE { None } else { Some(x) }
    }

    /// The universe of `v`'s class.
    #[inline]
    pub(crate) fn universe_of(&self, table: Table, v: u32) -> Universe {
        let r = self.root(table, v);
        Universe(self.t[table.at()].universe[ix(r)])
    }

    pub fn region_universe(&self, v: u32) -> Universe {
        Universe(self.re_universe[ix(v)])
    }

    /// Joins two classes (`equate_*_vids_raw`): by rank, keeping the lower universe and
    /// whichever value one of them had. Gives the new root.
    pub(crate) fn union(&mut self, table: Table, a: u32, b: u32) -> u32 {
        let ra = self.find(table, a);
        let rb = self.find(table, b);
        if ra == rb {
            return ra;
        }
        let ti = table.at();
        let (rank_a, rank_b) = (self.t[ti].rank[ix(ra)], self.t[ti].rank[ix(rb)]);
        let (root, child) = if rank_a >= rank_b { (ra, rb) } else { (rb, ra) };
        self.t[ti].parent[ix(child)] = root;
        self.note(table, OP_PARENT, child, child);
        // An unbound root going under another stops being one; a bound one resolves to the
        // same value as before.
        if self.t[ti].value[ix(child)] == NONE {
            self.log_bound(table, child);
        }
        if rank_a == rank_b {
            let was = self.t[ti].rank[ix(root)];
            self.t[ti].rank[ix(root)] = was.saturating_add(1);
            self.note(table, OP_RANK, root, u32::from(was));
        }
        let (u_root, u_child) = (self.t[ti].universe[ix(root)], self.t[ti].universe[ix(child)]);
        if u_child < u_root {
            self.t[ti].universe[ix(root)] = u_child;
            self.note(table, OP_UNIVERSE, root, u_root);
        }
        let (v_root, v_child) = (self.t[ti].value[ix(root)], self.t[ti].value[ix(child)]);
        if v_root == NONE && v_child != NONE {
            self.t[ti].value[ix(root)] = v_child;
            self.note(table, OP_VALUE, root, NONE);
            self.log_bound(table, root);
        }
        root
    }

    /// Binds `v`'s class to `value` (`instantiate_*_var_raw`). The class must be unbound.
    pub(crate) fn bind(&mut self, table: Table, v: u32, value: u32) {
        let r = self.find(table, v);
        let ti = table.at();
        let was = self.t[ti].value[ix(r)];
        debug_assert!(was == NONE, "binding a bound variable");
        self.t[ti].value[ix(r)] = value;
        self.note(table, OP_VALUE, r, was);
        self.log_bound(table, r);
    }

    // ---- snapshots -----------------------------------------------------------------------

    /// Opens a snapshot (`InferCtxtLike::start_snapshot`).
    pub fn snapshot(&mut self) -> VarSnapshot {
        self.open += 1;
        let Some(log) = row(self.log.len()) else { panic!("blaze-ty: the undo log is full (rule 18)") };
        let Some(bound) = row(self.bound.len()) else { panic!("blaze-ty: the binding log is full (rule 18)") };
        VarSnapshot { log, bound, universe: self.universe, depth: self.open }
    }

    /// Undoes every write since `s` and closes it (`rollback_to`), with every snapshot opened
    /// inside it: a solver unwinding a stop rolls back to its root's snapshot in one step.
    pub fn rollback_to(&mut self, s: VarSnapshot) {
        assert!(s.depth >= 1 && s.depth <= self.open, "blaze-ty: rollback to a snapshot already closed");
        while self.log.len() > ix(s.log) {
            let Some(u) = self.log.pop() else { break };
            if u.op == OP_NEW_REGION {
                self.re_universe.truncate(ix(u.at));
                continue;
            }
            let t = &mut self.t[usize::from(u.table)];
            let at = ix(u.at);
            match u.op {
                OP_NEW => t.truncate(at),
                OP_PARENT => t.parent[at] = u.was,
                OP_RANK => t.rank[at] = small(u.was),
                OP_VALUE => t.value[at] = u.was,
                _ => t.universe[at] = u.was,
            }
        }
        self.bound.truncate(ix(s.bound));
        self.universe = s.universe;
        self.open = s.depth - 1;
    }

    /// Keeps every write since `s` and closes it.
    pub fn commit(&mut self, s: VarSnapshot) {
        debug_assert!(s.depth == self.open, "snapshots close innermost first");
        self.open -= 1;
        if self.open == 0 {
            self.log.clear();
        }
    }

    /// Whether a snapshot is open.
    pub fn in_snapshot(&self) -> bool {
        self.open > 0
    }

    pub(crate) fn bytes(&self) -> usize {
        self.t.iter().map(Uf::bytes).sum::<usize>() + self.re_universe.capacity() * 4
            + self.log.capacity() * 12
            + self.bound.capacity() * core::mem::size_of::<VarId>()
    }

    /// Which kinds of variable exist, for a check that every table was rolled back.
    pub(crate) fn counts(&self) -> [usize; 5] {
        let mut c = [0; 5];
        for t in Table::ALL {
            c[t.at()] = self.count(t);
        }
        c[4] = self.re_universe.len();
        c
    }
}

impl Table {
    /// The table of a kind of variable (regions have none).
    pub(crate) const fn of(k: VarKind) -> Option<Table> {
        match k {
            VarKind::Ty => Some(Table::Ty),
            VarKind::Int => Some(Table::Int),
            VarKind::Float => Some(Table::Float),
            VarKind::Const => Some(Table::Const),
            VarKind::Region => None,
        }
    }
}
