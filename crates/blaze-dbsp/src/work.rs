/// Work counters a maintainer bumps; tests assert them (the cost tests of design §10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Work {
    /// Op deltas applied.
    pub ops: u64,
    /// Readers (imports, directives, glob edges, impls) woken.
    pub woken: u64,
    /// Aggregates rebuilt (a module's bindings, an impl's key).
    pub rebuilt: u64,
    /// Nodes in DRed over-delete regions.
    pub region: u64,
    /// Published slices set.
    pub slices: u64,
    /// Keys whose derived value changed.
    pub keys: u64,
    /// Rows of a maintained table moved, inserted or removed (a module's row in a crate's table).
    pub moved: u64,
}

impl std::ops::AddAssign for Work {
    fn add_assign(&mut self, other: Work) {
        self.ops += other.ops;
        self.woken += other.woken;
        self.rebuilt += other.rebuilt;
        self.region += other.region;
        self.slices += other.slices;
        self.keys += other.keys;
        self.moved += other.moved;
    }
}
