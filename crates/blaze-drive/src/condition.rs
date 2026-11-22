//! Conditions (drive.md, "One check covers every configuration"; bottom.md 11): `blaze check`
//! answers for every target, feature set and switch in one run.
//!
//! A [`Condition`] is a partial assignment of cfg variables: a target is one variable whose
//! value fixes every `target_*` atom; each package's `feature = "x"` is a boolean variable; `test`,
//! `debug_assertions` and each `--cfg` flag are booleans. Atoms of one variable are one choice,
//! so a variable has one value or none. A task runs under a condition; reading a variable the
//! condition does not fix answers `Need(Cfg(variable))`, and the machine re-enters it once per
//! value the request's [`Space`] still allows, each narrower ([`Space::fork`]). Its record is
//! stored with the condition it *read*, and a lookup takes a stored record whose condition the
//! asker's [`implies`](Condition::implies): the record that read nothing serves everyone.
//!
//! An item's conditions are capped at [`MOST_CONDITIONS`]: past it the answer is a typed
//! finding naming the item and the variables ([`ConditionError::OverCap`]), never a sample.
//!
//! Memory: a condition is [`MOST_ATOMS`] pairs inline, 66 bytes, `Copy`; no allocation.

use core::fmt;

/// A cfg variable, numbered by the request (targets first, then features per package, then
/// switches): an interned id within one request.
pub type Var = u16;
/// A variable's value: a target's index in the request's list, or 0/1 for a boolean.
pub type Value = u16;

/// The most variables one condition fixes.
pub const MOST_ATOMS: usize = 16;
/// The most conditions one item is checked under (bottom.md 11, bound 2).
pub const MOST_CONDITIONS: u32 = 64;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConditionError {
    /// More than [`MOST_ATOMS`] variables read by one item.
    TooManyAtoms { var: Var },
    /// A variable the space does not have.
    UnknownVar { var: Var },
    /// An item forked past [`MOST_CONDITIONS`]; `vars` are the variables it read.
    OverCap { conditions: u32, vars: Condition },
}

impl fmt::Display for ConditionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConditionError::TooManyAtoms { var } => {
                write!(f, "an item reads more than {MOST_ATOMS} cfg variables (at variable {var})")
            }
            ConditionError::UnknownVar { var } => write!(f, "cfg variable {var} is not in the request's space"),
            ConditionError::OverCap { conditions, vars } => {
                write!(f, "an item forks into {conditions} configurations, past {MOST_CONDITIONS} (bottom.md 11): it reads")?;
                for var in vars.vars() {
                    write!(f, " {var}")?;
                }
                Ok(())
            }
        }
    }
}

impl core::error::Error for ConditionError {}

/// A partial assignment, sorted by variable; unused pairs are zero so equal conditions are equal
/// bytes (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Condition {
    len: u8,
    atoms: [(Var, Value); MOST_ATOMS],
}

impl Default for Condition {
    fn default() -> Condition {
        Condition::ANY
    }
}

impl Condition {
    /// Fixes nothing: every configuration the request allows.
    pub const ANY: Condition = Condition { len: 0, atoms: [(0, 0); MOST_ATOMS] };

    pub fn atoms(&self) -> &[(Var, Value)] {
        &self.atoms[..self.len as usize]
    }

    /// The variables fixed, in order (`OverCap`'s list).
    pub fn vars(&self) -> impl Iterator<Item = Var> + '_ {
        self.atoms().iter().map(|(var, _)| *var)
    }

    pub fn get(&self, var: Var) -> Option<Value> {
        let atoms = self.atoms();
        atoms.binary_search_by_key(&var, |(v, _)| *v).ok().map(|at| atoms[at].1)
    }

    /// This condition with `var` fixed at `value`; `None` if it fixes `var` otherwise.
    pub fn with(&self, var: Var, value: Value) -> Result<Option<Condition>, ConditionError> {
        let atoms = self.atoms();
        match atoms.binary_search_by_key(&var, |(v, _)| *v) {
            Ok(at) => Ok((atoms[at].1 == value).then_some(*self)),
            Err(at) => {
                if self.len as usize == MOST_ATOMS {
                    return Err(ConditionError::TooManyAtoms { var });
                }
                let mut next = *self;
                next.atoms.copy_within(at..self.len as usize, at + 1);
                next.atoms[at] = (var, value);
                next.len += 1;
                Ok(Some(next))
            }
        }
    }

    /// Every configuration this admits, `other` admits: this fixes each atom `other` does.
    pub fn implies(&self, other: &Condition) -> bool {
        other.atoms().iter().all(|(var, value)| self.get(*var) == Some(*value))
    }

    /// Both at once; `None` if they disagree on a variable.
    pub fn and(&self, other: &Condition) -> Result<Option<Condition>, ConditionError> {
        let mut out = *self;
        for (var, value) in other.atoms() {
            match out.with(*var, *value)? {
                Some(next) => out = next,
                None => return Ok(None),
            }
        }
        Ok(Some(out))
    }
}

/// The configurations a request allows: how many values each variable has, and the feature
/// dependencies as boolean implications (`a = 1` needs `b = 1`), transitively closed by the
/// caller ([`Space::closed`] checks) so a pairwise test is exact.
#[derive(Clone, Copy, Debug)]
pub struct Space<'a> {
    pub values: &'a [u16],
    pub implications: &'a [(Var, Var)],
}

/// The rule a finding of [`ConditionError::OverCap`] carries (rule 81: a finding has no level;
/// `Config::is_error` says it fails a `run`).
pub const OVER_CAP_RULE: u16 = 0xC0F6;

impl Space<'static> {
    /// No variables: one configuration, every job under [`Condition::ANY`].
    pub const NONE: Space<'static> = Space { values: &[], implications: &[] };
}

impl Space<'_> {
    /// How many configurations a job under `condition` that forks on `var` spans at most: the
    /// product of the value counts of every variable it then fixes. A function of the path
    /// alone, so the cap ([`MOST_CONDITIONS`]) refuses the same node in every order the jobs
    /// settle in (rule 25), never the one that happened to fork last.
    pub fn span(&self, condition: &Condition, var: Var) -> u32 {
        let count = |v: Var| u32::from(self.values.get(v as usize).copied().unwrap_or(1)).max(1);
        let fixed = condition.vars().filter(|v| *v != var).fold(1u32, |n, v| n.saturating_mul(count(v)));
        fixed.saturating_mul(count(var))
    }

    /// Whether some configuration the space allows satisfies `condition`. Exact when the
    /// implications are closed: setting each unfixed boolean to 1 only where an implication from
    /// a fixed 1 forces it satisfies every other implication.
    pub fn allows(&self, condition: &Condition) -> bool {
        let atoms = condition.atoms();
        let mut ones = [0 as Var; MOST_ATOMS];
        let mut zeros = [0 as Var; MOST_ATOMS];
        let mut n_ones = 0usize;
        let mut n_zeros = 0usize;
        let mut max_var = 0 as Var;
        for &(var, value) in atoms {
            if !self.values.get(var as usize).is_some_and(|n| value < *n) {
                return false;
            }
            if value == 1 {
                ones[n_ones] = var;
                n_ones += 1;
                max_var = max_var.max(var);
            } else if value == 0 {
                zeros[n_zeros] = var;
                n_zeros += 1;
                max_var = max_var.max(var);
            }
        }
        if n_ones == 0 || n_zeros == 0 || self.implications.is_empty() {
            return true;
        }
        // Exact O(1) stack bitset when every fixed 0/1 variable fits under 256.
        if max_var < 256 {
            let mut one_bits = [0u64; 4];
            let mut zero_bits = [0u64; 4];
            for &v in &ones[..n_ones] {
                let u = usize::from(v);
                one_bits[u >> 6] |= 1u64 << (u & 63);
            }
            for &v in &zeros[..n_zeros] {
                let u = usize::from(v);
                zero_bits[u >> 6] |= 1u64 << (u & 63);
            }
            for &(a, b) in self.implications {
                if a < 256 && b < 256 {
                    let au = usize::from(a);
                    let bu = usize::from(b);
                    if (one_bits[au >> 6] & (1u64 << (au & 63))) != 0
                        && (zero_bits[bu >> 6] & (1u64 << (bu & 63))) != 0
                    {
                        return false;
                    }
                }
            }
            return true;
        }
        // Fallback for large variable indices: 64-bit bloom mask + binary search on sorted slices.
        let mut one_mask = 0u64;
        let mut zero_mask = 0u64;
        for &v in &ones[..n_ones] {
            one_mask |= 1u64 << (v & 63);
        }
        for &v in &zeros[..n_zeros] {
            zero_mask |= 1u64 << (v & 63);
        }
        let ones = &ones[..n_ones];
        let zeros = &zeros[..n_zeros];
        for &(a, b) in self.implications {
            if ((one_mask >> (a & 63)) & (zero_mask >> (b & 63)) & 1) != 0
                && ones.binary_search(&a).is_ok()
                && zeros.binary_search(&b).is_ok()
            {
                return false;
            }
        }
        true
    }

    /// Whether the implications are closed: `a -> b` and `b -> c` give `a -> c`.
    pub fn closed(&self) -> bool {
        let imps = self.implications;
        if imps.len() <= 1 {
            return true;
        }
        // Fast path when `implications` is sorted (the standard canonical order): group by `a`,
        // binary-search `b`'s outgoing slice, and merge-check that every `c != a` in `b`'s slice
        // is present in `a`'s slice.
        if imps.windows(2).all(|w| w[0] <= w[1]) {
            let mut a_start = 0usize;
            while a_start < imps.len() {
                let a = imps[a_start].0;
                let mut a_end = a_start + 1;
                while a_end < imps.len() && imps[a_end].0 == a {
                    a_end += 1;
                }
                let a_edges = &imps[a_start..a_end];
                for &(_, b) in a_edges {
                    let b_start = imps.partition_point(|&(x, _)| x < b);
                    let mut b_idx = b_start;
                    let mut a_idx = 0usize;
                    while b_idx < imps.len() && imps[b_idx].0 == b {
                        let c = imps[b_idx].1;
                        b_idx += 1;
                        if c == a {
                            continue;
                        }
                        while a_idx < a_edges.len() && a_edges[a_idx].1 < c {
                            a_idx += 1;
                        }
                        if a_idx == a_edges.len() || a_edges[a_idx].1 != c {
                            return false;
                        }
                    }
                }
                a_start = a_end;
            }
            return true;
        }
        // Unsorted fallback: preserve exact semantics for arbitrary caller slices.
        imps.iter().all(|(a, b)| {
            imps.iter().filter(|(b2, _)| b2 == b).all(|(_, c)| a == c || imps.contains(&(*a, *c)))
        })
    }

    /// The narrower conditions a task re-entered on `Need(Cfg(var))` runs under: one per value
    /// of `var` the space still allows under `condition`, in value order.
    pub fn fork(&self, condition: &Condition, var: Var) -> Result<impl Iterator<Item = Condition> + '_, ConditionError> {
        let count = *self.values.get(var as usize).ok_or(ConditionError::UnknownVar { var })?;
        let condition = *condition;
        if condition.len as usize == MOST_ATOMS && condition.get(var).is_none() {
            return Err(ConditionError::TooManyAtoms { var });
        }
        Ok((0..count).filter_map(move |value| condition.with(var, value).ok().flatten()).filter(move |next| self.allows(next)))
    }
}

#[cfg(test)]
mod model {
    //! The model (rule 64): items read cfg variables (which ones may depend on the values read)
    //! and use earlier items' results. Checked once under the whole space, forking on each
    //! unfixed read and storing each record under what it read, the findings must equal the
    //! union of checking every full configuration separately.

    use std::collections::{BTreeMap, BTreeSet};
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

    use super::{Condition, ConditionError, MOST_CONDITIONS, Space, Value, Var};

    /// The bug this test plants to show its oracle catches it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Plant {
        None,
        /// A record is stored under the condition its job began at, not the one it read: an
        /// asker that does not imply what it read is served it.
        RanUnderCondition,
    }

    #[derive(Clone, Copy, Debug)]
    enum Step {
        Atom(Var),
        Use(usize),
        /// Skip the next step when the result so far is odd: which variables are read depends
        /// on the values read.
        SkipIfOdd,
    }

    struct World {
        values: Vec<u16>,
        implications: Vec<(Var, Var)>,
        items: Vec<Vec<Step>>,
    }

    fn mix(h: u64, x: u64) -> u64 {
        (h ^ (x + 1)).wrapping_mul(0x0100_0000_01b3)
    }

    fn draw(tape: &Tape) -> World {
        let targets = 1 + tape.index(3, "targets") as u16;
        let features = tape.index(4, "features");
        let mut values = Vec::from([targets]);
        values.extend((0..features).map(|_| 2));
        values.push(2); // `test`
        let vars = values.len();
        let mut implications = Vec::new();
        for a in 1..=features {
            for b in a + 1..=features {
                if tape.one_in(3, "feature dependency") {
                    implications.push((a as Var, b as Var));
                }
            }
        }
        // Close them: a -> b -> c gives a -> c.
        loop {
            let more: Vec<(Var, Var)> = implications
                .iter()
                .flat_map(|(a, b)| implications.iter().filter(move |(b2, _)| b2 == b).map(move |(_, c)| (*a, *c)))
                .filter(|pair| !implications.contains(pair))
                .collect();
            if more.is_empty() {
                break;
            }
            implications.extend(more);
            implications.sort();
            implications.dedup();
        }
        let n = 1 + tape.index(6, "items");
        let items = (0..n)
            .map(|i| {
                (0..tape.index(5, "steps"))
                    .map(|_| match tape.index(if i == 0 { 2 } else { 3 }, "step") {
                        0 => Step::Atom(tape.index(vars, "variable") as Var),
                        1 => Step::SkipIfOdd,
                        _ => Step::Use(tape.index(i, "used item")),
                    })
                    .collect()
            })
            .collect();
        World { values, implications, items }
    }

    fn space(world: &World) -> Space<'_> {
        Space { values: &world.values, implications: &world.implications }
    }

    /// The finding of a result: a third of results are findings.
    fn finding(h: u64) -> Option<u64> {
        (h % 3 == 0).then_some(h)
    }

    /// One full configuration, directly.
    fn eval(world: &World, item: usize, config: &[Value]) -> u64 {
        let mut h = item as u64;
        let mut skip = false;
        for step in &world.items[item] {
            if core::mem::take(&mut skip) {
                continue;
            }
            match *step {
                Step::Atom(var) => h = mix(h, u64::from(config[var as usize])),
                Step::Use(j) => h = mix(h, eval(world, j, config)),
                Step::SkipIfOdd => skip = h % 2 == 1,
            }
        }
        h
    }

    enum Ran {
        Done { read: Condition, h: u64 },
        Need(Var),
    }

    type Records = BTreeMap<usize, Vec<(Condition, u64)>>;

    /// One entry of `item` under `under`, using the stored records of earlier items.
    fn run(world: &World, records: &Records, item: usize, under: &Condition) -> Result<Ran, Broke> {
        let harness = |e: ConditionError| Broke::Harness(format!("{e}"));
        let mut h = item as u64;
        let mut read = Condition::ANY;
        let mut skip = false;
        for step in &world.items[item] {
            if core::mem::take(&mut skip) {
                continue;
            }
            match *step {
                Step::Atom(var) => match under.get(var) {
                    Some(value) => {
                        read = read.with(var, value).map_err(harness)?.ok_or(Broke::Harness(String::from("read disagrees")))?;
                        h = mix(h, u64::from(value));
                    }
                    None => return Ok(Ran::Need(var)),
                },
                Step::Use(j) => {
                    let held = records.get(&j).map_or(&[][..], |r| r.as_slice());
                    match held.iter().find(|(cond, _)| under.implies(cond)) {
                        Some((cond, hj)) => {
                            read = read.and(cond).map_err(harness)?.ok_or(Broke::Harness(String::from("use disagrees")))?;
                            h = mix(h, *hj);
                        }
                        // No record the asker's condition implies: the asker forks on a
                        // variable a record of `j` fixes and it does not.
                        None => {
                            let var = held
                                .iter()
                                .flat_map(|(cond, _)| cond.atoms().iter().copied())
                                .find(|(var, _)| under.get(*var).is_none())
                                .map(|(var, _)| var)
                                .ok_or(Broke::Oracle(format!("item {j} has no record for {under:?}")))?;
                            return Ok(Ran::Need(var));
                        }
                    }
                }
                Step::SkipIfOdd => skip = h % 2 == 1,
            }
        }
        Ok(Ran::Done { read, h })
    }

    /// The whole space at once: each item from `ANY`, forked on each unfixed read.
    fn whole(world: &World, plant: Plant) -> Result<Records, Broke> {
        let space = space(world);
        let mut records = Records::new();
        for item in 0..world.items.len() {
            let mut queue = Vec::from([Condition::ANY]);
            let mut conditions = 1;
            while let Some(under) = queue.pop() {
                match run(world, &records, item, &under)? {
                    Ran::Done { read, h } => {
                        // The planted bug: stored under the condition the job began at, not
                        // what it read.
                        let stored = if plant == Plant::RanUnderCondition { Condition::ANY } else { read };
                        records.entry(item).or_default().push((stored, h));
                    }
                    Ran::Need(var) => {
                        let forks: Vec<Condition> =
                            space.fork(&under, var).map_err(|e| Broke::Harness(format!("{e}")))?.collect();
                        conditions = (conditions + forks.len() as u32).saturating_sub(1);
                        if conditions > MOST_CONDITIONS {
                            return Err(Broke::Harness(format!("{}", ConditionError::OverCap { conditions, vars: under })));
                        }
                        queue.extend(forks);
                    }
                }
            }
        }
        Ok(records)
    }

    fn configs(world: &World) -> Vec<Vec<Value>> {
        let space = space(world);
        let mut all: Vec<Vec<Value>> = Vec::from([Vec::new()]);
        for count in &world.values {
            all = all.into_iter().flat_map(|c| (0..*count).map(move |v| [c.as_slice(), &[v]].concat())).collect();
        }
        all.into_iter()
            .filter(|c| {
                let mut cond = Condition::ANY;
                for (var, value) in c.iter().enumerate() {
                    cond = cond.with(var as Var, *value).ok().flatten().unwrap_or(cond);
                }
                space.allows(&cond)
            })
            .collect()
    }

    fn whole_equals_each(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let world = draw(tape);
        let records = whole(&world, plant)?;
        let mut each = BTreeSet::new();
        let mut once = BTreeSet::new();
        for (at, config) in configs(&world).iter().enumerate() {
            let mut full = Condition::ANY;
            for (var, value) in config.iter().enumerate() {
                full = full.with(var as Var, *value).ok().flatten().ok_or(Broke::Harness(String::from("config")))?;
            }
            for item in 0..world.items.len() {
                if let Some(f) = finding(eval(&world, item, config)) {
                    each.insert((at, item, f));
                }
                for (cond, h) in records.get(&item).map_or(&[][..], |r| r.as_slice()) {
                    if let (true, Some(f)) = (full.implies(cond), finding(*h)) {
                        once.insert((at, item, f));
                    }
                }
            }
        }
        match each == once {
            true => Ok(()),
            false => Err(Broke::Oracle(format!(
                "checking the whole space found {} findings, each configuration {}: they differ",
                once.len(),
                each.len()
            ))),
        }
    }

    #[test]
    fn the_whole_space_equals_each_configuration() {
        check(SimConfig::cases(500), |tape| whole_equals_each(tape, Plant::None));
    }

    #[test]
    fn control_a_record_stored_under_its_start_condition_is_caught() {
        control(SimConfig::cases(4000), Plant::None, Plant::RanUnderCondition, "they differ", whole_equals_each);
    }

    #[test]
    fn a_condition_is_sorted_and_implies_its_parts() {
        let a = Condition::ANY.with(3, 1).unwrap().unwrap().with(1, 0).unwrap().unwrap();
        assert_eq!(a.atoms(), &[(1, 0), (3, 1)]);
        assert!(a.implies(&Condition::ANY.with(3, 1).unwrap().unwrap()));
        assert!(!Condition::ANY.implies(&a));
        assert_eq!(a.with(1, 1).unwrap(), None);
    }

    #[test]
    fn fast_closed_and_allows_match_naive_for_small_and_large_vars() {
        // Chain 0 -> 1 -> 2, closed vs unclosed, sorted and unsorted.
        let values = [2u16; 320];
        let closed_sorted = [(0, 1), (0, 2), (1, 2), (260, 270), (260, 280), (270, 280)];
        let s = Space { values: &values, implications: &closed_sorted };
        assert!(s.closed());
        let ok = Condition::ANY.with(0, 1).unwrap().unwrap().with(2, 1).unwrap().unwrap();
        assert!(s.allows(&ok));
        let bad_small = Condition::ANY.with(0, 1).unwrap().unwrap().with(2, 0).unwrap().unwrap();
        assert!(!s.allows(&bad_small));
        let bad_large = Condition::ANY.with(260, 1).unwrap().unwrap().with(280, 0).unwrap().unwrap();
        assert!(!s.allows(&bad_large));
        let ok_large = Condition::ANY.with(260, 1).unwrap().unwrap().with(280, 1).unwrap().unwrap();
        assert!(s.allows(&ok_large));

        let unclosed_sorted = [(0, 1), (1, 2)];
        assert!(!Space { values: &values, implications: &unclosed_sorted }.closed());

        let unclosed_unsorted = [(1, 2), (0, 1)];
        assert!(!Space { values: &values, implications: &unclosed_unsorted }.closed());

        let closed_unsorted = [(1, 2), (0, 2), (0, 1)];
        assert!(Space { values: &values, implications: &closed_unsorted }.closed());
    }
}
