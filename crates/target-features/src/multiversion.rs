//! `#[blaze::multiversion]`'s levels (docs/v2/plan/design/simd.md, "`#[blaze::multiversion]`"):
//! which clones a function gets, with which target features, and which one runs on a CPU.
//!
//! A level is written as a CPU of the target's table ([`crate::cpus`]: `x86-64-v3`) or as rustc's
//! feature names, comma-separated (`avx512f,avx512vl`). A [`Plan`] is the levels of one function
//! in one unit: each level's set is the unit's with the level's, closed by what each implies; a
//! level that adds nothing to the unit is dropped, so a unit already compiled for the machine it
//! runs on has no clone. [`Plan::select`] picks the clone to run from the features at hand: those
//! the CPU reports, when the dispatch cell is filled, or those of a calling clone, when one
//! multiversioned function calls another directly.
//!
//! The invariant every user of a plan relies on: a clone is entered only where every feature of
//! its set is present. `select` returns only a clone whose added features are all at hand, and
//! the unit's own are the program's precondition, as they are of every function of the unit.
//!
//! Pure data and pure functions: the front end names the levels (the attribute), the code
//! generator the unit's features, std's detection the CPU's.

use std::fmt;

use blaze_ids::{ContentId, Hasher};

use crate::{TargetArch, TargetFeature, TargetFeatureSet, cpus};

/// The most levels one function names: a bound on the clones of one function, well under the
/// code generator's feature sets per process.
pub const MOST_LEVELS: usize = 8;

/// What [`Plan::key`] hashes in.
pub const KEY_DOMAIN: &str = "blaze v2 multiversion plan 1";

/// The levels of `#[blaze::multiversion]` with no list, least preferred first. x86 has its
/// three microarchitecture levels past the baseline. Every other target has none: AArch64's
/// baseline has `neon`, and `core::simd` has no scalable vectors for SVE to widen.
pub fn default_levels(arch: TargetArch) -> &'static [&'static str] {
    match arch {
        TargetArch::X86 => &["x86-64-v2", "x86-64-v3", "x86-64-v4"],
        _ => &[],
    }
}

/// One clone of a multiversioned function: the level it was named by and what it is compiled
/// with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Level {
    /// The level as the attribute (or [`default_levels`]) wrote it.
    pub name: Box<str>,
    /// Every feature the clone's code may use: the unit's and the level's, with what each
    /// implies. Part of the clone's code identity.
    pub features: TargetFeatureSet,
    /// `features` without the unit's: what the CPU must be found to have for the clone to run.
    /// Never empty.
    pub added: TargetFeatureSet,
}

/// Why a function's levels are no plan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// Level `at` (from 0) is empty, or names an empty feature (`"avx2,,fma"`).
    Empty { at: usize },
    /// `name`, in level `level`, is no CPU of the target and no target feature of its arch.
    Unknown { level: String, name: String, triple: String },
    /// Two levels come to one set of features: their clones would be one code under two names.
    Same { first: String, second: String },
    /// More than [`MOST_LEVELS`] levels.
    TooMany { levels: usize },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Empty { at } => write!(
                f,
                "level {at} of `#[blaze::multiversion]` is empty or names an empty feature: a \
                 level is a CPU (`\"x86-64-v3\"`) or target features (`\"avx2,fma\"`)"
            ),
            Refusal::Unknown { level, name, triple } if level == name => write!(
                f,
                "level `{level}` of `#[blaze::multiversion]` is no CPU of target `{triple}` \
                 (target_features::cpus) and no target feature of its architecture"
            ),
            Refusal::Unknown { level, name, triple } => write!(
                f,
                "`{name}` in level `{level}` of `#[blaze::multiversion]` is no target feature of \
                 target `{triple}`'s architecture"
            ),
            Refusal::Same { first, second } => write!(
                f,
                "levels `{first}` and `{second}` of `#[blaze::multiversion]` enable the same \
                 target features in this unit: each level must add something the others lack"
            ),
            Refusal::TooMany { levels } => write!(
                f,
                "`#[blaze::multiversion]` names {levels} levels; a function has at most \
                 {MOST_LEVELS}"
            ),
        }
    }
}

/// What [`Plan::select`] picked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selected {
    /// The function as the unit compiles it: no clone's features are all at hand.
    Baseline,
    /// The clone at this index of [`Plan::clones`].
    Clone(usize),
}

/// The clones of one multiversioned function in one unit, in the order its levels were declared:
/// least preferred first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Plan {
    arch: TargetArch,
    /// The unit's features with what they imply: the baseline's set.
    unit: TargetFeatureSet,
    clones: Vec<Level>,
}

impl Plan {
    /// The plan of `levels` (the attribute's list; [`default_levels`] of `arch` when it is
    /// empty) for a unit of target `triple` compiled with `unit`.
    pub fn new(
        triple: &str,
        arch: TargetArch,
        unit: &TargetFeatureSet,
        levels: &[&str],
    ) -> Result<Plan, Refusal> {
        let levels = if levels.is_empty() { default_levels(arch) } else { levels };
        if levels.len() > MOST_LEVELS {
            return Err(Refusal::TooMany { levels: levels.len() });
        }
        let unit = unit.implied_in(arch);
        // alloc: per multiversioned function and unit, at most `MOST_LEVELS` clones
        let mut clones: Vec<Level> = Vec::with_capacity(levels.len());
        for (at, &name) in levels.iter().enumerate() {
            let named = level_features(triple, arch, at, name)?;
            let features = unit.union(&named).implied_in(arch);
            let added: TargetFeatureSet =
                features.iter().filter(|it| !unit.contains(*it)).collect();
            if added.is_empty() {
                // The unit already has the level: its clone would be the baseline again.
                continue;
            }
            if let Some(first) = clones.iter().find(|it| it.features == features) {
                // alloc: an error, naming both levels
                return Err(Refusal::Same {
                    first: first.name.to_string(),
                    second: name.to_owned(),
                });
            }
            // alloc: the level's name, kept for errors, the key and debug names
            clones.push(Level { name: name.into(), features, added });
        }
        Ok(Plan { arch, unit, clones })
    }

    /// The clones, least preferred first. Empty when the unit has every level already.
    pub fn clones(&self) -> &[Level] {
        &self.clones
    }

    /// The baseline's features: the unit's, with what they imply.
    pub fn unit(&self) -> &TargetFeatureSet {
        &self.unit
    }

    /// The clone to run where the features `has` are present, beside the unit's own: the last
    /// declared one whose added features are all in `has`, else the baseline.
    pub fn select(&self, has: &TargetFeatureSet) -> Selected {
        match self.clones.iter().rposition(|it| has.is_superset(&it.added)) {
            Some(at) => Selected::Clone(at),
            None => Selected::Baseline,
        }
    }

    /// The key of the function's dispatch table: the arch, the baseline's features, and each
    /// clone's level and features in order. Two plans that select differently differ in it.
    pub fn key(&self) -> ContentId {
        let mut hasher = Hasher::new(KEY_DOMAIN);
        hasher.update(&[self.arch as u8]);
        hash_set(&mut hasher, &self.unit);
        hasher.update(&(self.clones.len() as u32).to_le_bytes());
        for clone in &self.clones {
            hasher.part(clone.name.as_bytes());
            hash_set(&mut hasher, &clone.features);
        }
        hasher.finish()
    }
}

/// `set` by its features' names, in the order of their bytes, counted.
fn hash_set(hasher: &mut Hasher, set: &TargetFeatureSet) {
    hasher.update(&(set.iter().count() as u32).to_le_bytes());
    for name in set.names() {
        hasher.part(name.as_bytes());
    }
}

/// The features level `name` (the `at`th) names on target `triple`: a CPU's, as rustc gives
/// them, or each feature of a comma-separated list, which must be of `arch`'s table.
fn level_features(
    triple: &str,
    arch: TargetArch,
    at: usize,
    name: &str,
) -> Result<TargetFeatureSet, Refusal> {
    if name.is_empty() {
        return Err(Refusal::Empty { at });
    }
    // alloc: an error, naming the level, the name and the target
    let unknown = |feature: &str| Refusal::Unknown {
        level: name.to_owned(),
        name: feature.to_owned(),
        triple: triple.to_owned(),
    };
    let mut set = TargetFeatureSet::EMPTY;
    if let Some(cpu) = cpus::of(triple).and_then(|it| it.features(name)) {
        for &feature in cpu {
            set.insert(TargetFeature::from_name(feature).ok_or_else(|| unknown(feature))?);
        }
        return Ok(set);
    }
    for feature in name.split(',') {
        if feature.is_empty() {
            return Err(Refusal::Empty { at });
        }
        let of_arch = arch.rustc_table().any(|(it, _)| it == feature);
        let found = TargetFeature::from_name(feature).filter(|_| of_arch);
        set.insert(found.ok_or_else(|| unknown(feature))?);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    const LINUX: &str = "x86_64-unknown-linux-gnu";

    /// A small lattice of x86 features: `avx2` implies `avx`, `fma` implies `avx`, `avx512f`
    /// implies `avx2`, `fma` and `f16c`, and `bmi2`, `popcnt` and `lzcnt` imply nothing here.
    const UNIVERSE: [&str; 7] = ["avx", "avx2", "fma", "avx512f", "bmi2", "popcnt", "lzcnt"];

    fn feature(name: &str) -> TargetFeature {
        TargetFeature::from_name(name).unwrap_or_else(|| panic!("{name} is no target feature"))
    }

    /// The features of [`UNIVERSE`] whose bit is set in `mask`, as written (not closed).
    fn subset(mask: u8) -> TargetFeatureSet {
        let names = UNIVERSE.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0);
        names.map(|(_, name)| feature(name)).collect()
    }

    /// `mask`'s features as a level: a comma-separated list.
    fn level(mask: u8) -> String {
        let names = UNIVERSE.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0);
        names.map(|(_, name)| *name).collect::<Vec<_>>().join(",")
    }

    /// x86-64's baseline, which every unit of the tests has.
    fn baseline() -> TargetFeatureSet {
        ["fxsr", "sse", "sse2", "x87"].into_iter().map(feature).collect()
    }

    fn plan(unit: &TargetFeatureSet, levels: &[&str]) -> Result<Plan, Refusal> {
        Plan::new(LINUX, TargetArch::X86, unit, levels)
    }

    /// The oracle, from the plan's public data alone: `selected` is a clone every added feature
    /// of which `has` holds, and no later clone is one; the baseline only when no clone is.
    fn check(plan: &Plan, has: &TargetFeatureSet, selected: Selected) -> Result<(), String> {
        let supported = |level: &Level| level.added.iter().all(|it| has.contains(it));
        let best = plan.clones().iter().rposition(supported);
        match selected {
            Selected::Clone(at) => {
                let Some(clone) = plan.clones().get(at) else {
                    return Err(format!("clone {at} of {} was selected", plan.clones().len()));
                };
                if let Some(lacks) = clone.added.iter().find(|it| !has.contains(*it)) {
                    return Err(format!(
                        "clone `{}` was selected on a CPU without `{}`",
                        clone.name,
                        lacks.name()
                    ));
                }
                match best {
                    Some(best) if best > at => Err(format!(
                        "clone `{}` was selected though `{}`, declared after it, is supported",
                        clone.name,
                        plan.clones()[best].name
                    )),
                    _ => Ok(()),
                }
            }
            Selected::Baseline => match best {
                Some(best) => Err(format!(
                    "the baseline was selected though clone `{}` is supported",
                    plan.clones()[best].name
                )),
                None => Ok(()),
            },
        }
    }

    /// A planted bug: the most preferred clone, whatever the CPU has.
    fn select_without_asking(plan: &Plan, _has: &TargetFeatureSet) -> Selected {
        match plan.clones().len() {
            0 => Selected::Baseline,
            clones => Selected::Clone(clones - 1),
        }
    }

    /// A planted bug: never a clone.
    fn select_the_baseline(_plan: &Plan, _has: &TargetFeatureSet) -> Selected {
        Selected::Baseline
    }

    /// A drawn case: a unit past the baseline, up to four levels, and what the CPU has (closed
    /// by what it implies, as a CPU's features are). `None` when the levels are no plan.
    fn drawn(tc: &hegel::TestCase) -> Option<(Plan, TargetFeatureSet)> {
        let unit = baseline().union(&subset(tc.draw(gs::integers::<u8>().max_value(127))));
        let masks: Vec<u8> = tc.draw(gs::vecs(gs::integers::<u8>().max_value(126)).max_size(4));
        // A level names at least one feature: masks from 1.
        let levels: Vec<String> = masks.iter().map(|&mask| level(mask + 1)).collect();
        let levels: Vec<&str> = levels.iter().map(String::as_str).collect();
        // No levels would ask for the defaults: a drawn plan names its own.
        if levels.is_empty() {
            return None;
        }
        let has = subset(tc.draw(gs::integers::<u8>().max_value(127))).implied_in(TargetArch::X86);
        match plan(&unit, &levels) {
            Ok(plan) => Some((plan, has)),
            Err(Refusal::Same { .. }) => None,
            Err(other) => panic!("levels {levels:?}: {other}"),
        }
    }

    /// The invariant: the clone selected is supported by the CPU (every feature it adds to the
    /// unit is present), and no supported clone is declared after it; the baseline is selected
    /// only when no clone is supported.
    #[hegel::test]
    fn the_selected_clone_is_supported_and_the_best(tc: hegel::TestCase) {
        let Some((plan, has)) = drawn(&tc) else { return };
        if let Err(why) = check(&plan, &has, plan.select(&has)) {
            panic!("{plan:?} on {has:?}: {why}");
        }
    }

    #[test]
    fn the_oracle_catches_a_clone_the_cpu_lacks() {
        let plan = plan(&baseline(), &["avx2", "avx512f"]).expect("two levels");
        let has = subset(0b11).implied_in(TargetArch::X86);
        assert_eq!(check(&plan, &has, plan.select(&has)), Ok(()), "the selection as it is passes");
        let why = check(&plan, &has, select_without_asking(&plan, &has))
            .expect_err("the oracle missed a clone selected on a CPU that lacks its features");
        assert!(why.contains("clone `avx512f` was selected on a CPU without `avx512f`"), "{why}");
    }

    #[test]
    fn the_oracle_catches_the_baseline_chosen_over_a_supported_clone() {
        let plan = plan(&baseline(), &["avx2", "avx512f"]).expect("two levels");
        let has = subset(0b11).implied_in(TargetArch::X86);
        assert_eq!(check(&plan, &has, plan.select(&has)), Ok(()), "the selection as it is passes");
        let why = check(&plan, &has, select_the_baseline(&plan, &has))
            .expect_err("the oracle missed the baseline selected over a supported clone");
        assert!(why.contains("the baseline was selected though clone `avx2`"), "{why}");
        // And a clone that is supported but not the best.
        let has = subset(0b1111).implied_in(TargetArch::X86);
        let why = check(&plan, &has, Selected::Clone(0)).expect_err("a worse clone was missed");
        assert!(why.contains("though `avx512f`, declared after it, is supported"), "{why}");
    }

    /// Every subset of the lattice as the CPU, against plans of every shape the levels take:
    /// nested, unordered, overlapping, a level the unit has.
    #[test]
    fn every_cpu_of_the_lattice_gets_its_best_clone() {
        let plans: &[(&[&str], &[&str])] = &[
            (&[], &["avx2", "avx512f"]),
            (&[], &["avx512f", "avx2"]),
            (&[], &["bmi2,popcnt", "avx2,fma", "avx2,fma,bmi2,lzcnt"]),
            (&["avx2"], &["avx", "avx2", "avx512f"]),
            (&["avx512f", "bmi2", "popcnt", "lzcnt"], &["avx2,fma", "avx512f"]),
        ];
        for (unit, levels) in plans {
            let unit = baseline().union(&unit.iter().map(|it| feature(it)).collect());
            let plan = plan(&unit, levels).expect("the levels are a plan");
            for mask in 0..=127u8 {
                for has in [subset(mask), subset(mask).implied_in(TargetArch::X86)] {
                    if let Err(why) = check(&plan, &has, plan.select(&has)) {
                        panic!("{levels:?} on {has:?}: {why}");
                    }
                }
            }
            // No feature at hand is the baseline; every feature is the last clone, if any.
            assert_eq!(plan.select(&TargetFeatureSet::EMPTY), Selected::Baseline);
            let all = subset(127).implied_in(TargetArch::X86);
            let last = plan.clones().len().checked_sub(1);
            assert_eq!(plan.select(&all), last.map_or(Selected::Baseline, Selected::Clone));
        }
    }

    /// The invariant: no two clones of a plan have one feature set, and none has the unit's, so
    /// the baseline and the clones are compiled with pairwise different sets and (their feature
    /// names being part of an instance's shape) never share a code identity.
    #[hegel::test]
    fn clones_never_share_a_feature_set(tc: hegel::TestCase) {
        let Some((plan, _)) = drawn(&tc) else { return };
        if let Err(why) = distinct(plan.unit(), plan.clones()) {
            panic!("{plan:?}: {why}");
        }
    }

    /// The oracle of [`clones_never_share_a_feature_set`].
    fn distinct(unit: &TargetFeatureSet, clones: &[Level]) -> Result<(), String> {
        for (at, clone) in clones.iter().enumerate() {
            if clone.features == *unit || clone.added.is_empty() {
                return Err(format!("clone `{}` is compiled as the baseline is", clone.name));
            }
            if !clone.features.is_superset(unit) {
                return Err(format!("clone `{}` lacks a feature of its unit", clone.name));
            }
            if let Some(same) = clones[..at].iter().find(|it| it.features == clone.features) {
                return Err(format!("clones `{}` and `{}` have one set", same.name, clone.name));
            }
        }
        Ok(())
    }

    #[test]
    fn the_oracle_catches_two_clones_of_one_set() {
        let plan = plan(&baseline(), &["avx2", "avx512f"]).expect("two levels");
        assert_eq!(distinct(plan.unit(), plan.clones()), Ok(()), "the plan as it is passes");
        // The planted bug: a plan that kept a level twice, and one that kept the unit's own.
        let twice = [plan.clones()[0].clone(), plan.clones()[1].clone(), plan.clones()[0].clone()];
        let why = distinct(plan.unit(), &twice).expect_err("the oracle missed a set kept twice");
        assert!(why.contains("clones `avx2` and `avx2` have one set"), "{why}");
        let unit = plan.clones()[1].features;
        let why = distinct(&unit, plan.clones()).expect_err("the oracle missed the unit's own set");
        assert!(why.contains("clone `avx512f` is compiled as the baseline is"), "{why}");
    }

    #[test]
    fn a_level_is_the_units_features_with_its_own_and_what_they_imply() {
        let plan = plan(&baseline(), &["avx2", "avx512f"]).expect("two levels");
        let [v3, v4] = plan.clones() else { panic!("{plan:?}") };
        for name in ["sse2", "avx", "avx2"] {
            assert!(v3.features.contains(feature(name)), "avx2 has {name}");
        }
        assert!(!v3.features.contains(feature("avx512f")) && !v3.added.contains(feature("sse2")));
        for name in ["avx2", "fma", "f16c", "avx512f"] {
            assert!(v4.added.contains(feature(name)), "avx512f adds {name}");
        }
        // A unit that has a level already keeps no clone of it: a dev build for this machine.
        let unit = baseline().union(&subset(0b10));
        let kept = super::Plan::new(LINUX, TargetArch::X86, &unit, &["avx", "avx2", "avx512f"]);
        let kept = kept.expect("three levels");
        assert_eq!(kept.clones().iter().map(|it| &*it.name).collect::<Vec<_>>(), ["avx512f"]);
        let all = baseline().union(&subset(127));
        assert!(plan_of(&all, &["avx2", "avx512f"]).clones().is_empty());
    }

    fn plan_of(unit: &TargetFeatureSet, levels: &[&str]) -> Plan {
        plan(unit, levels).unwrap_or_else(|why| panic!("{levels:?}: {why}"))
    }

    #[test]
    fn the_defaults_are_the_targets_levels() {
        // x86: the three levels, as the CPU table has them, each past the one before.
        let x86 = plan_of(&baseline(), &[]);
        let names: Vec<&str> = x86.clones().iter().map(|it| &*it.name).collect();
        assert_eq!(names, ["x86-64-v2", "x86-64-v3", "x86-64-v4"]);
        let [v2, v3, v4] = x86.clones() else { panic!("{x86:?}") };
        assert!(v2.features.contains(feature("sse4.2")) && !v2.features.contains(feature("avx")));
        assert!(v3.features.contains(feature("avx2")) && v3.features.is_superset(&v2.features));
        assert!(v4.features.contains(feature("avx512f")) && v4.features.is_superset(&v3.features));
        // A machine of level 3 runs clone 1; one of the baseline, the baseline.
        assert_eq!(x86.select(&v3.features), Selected::Clone(1));
        assert_eq!(x86.select(&baseline()), Selected::Baseline);
        // AArch64 and a target with no table: no level, so no clone.
        let neon: TargetFeatureSet = [feature("neon")].into_iter().collect();
        let arm = Plan::new("aarch64-apple-darwin", TargetArch::AArch64, &neon, &[]);
        assert!(arm.expect("no levels").clones().is_empty());
        let other = Plan::new("avr-none", TargetArch::Avr, &TargetFeatureSet::EMPTY, &[]);
        assert!(other.expect("no levels").clones().is_empty());
    }

    #[test]
    fn levels_that_are_no_plan_are_refused_by_name() {
        let refused = |levels: &[&str]| plan(&baseline(), levels).expect_err("refused");
        assert_eq!(refused(&[""]), Refusal::Empty { at: 0 });
        assert_eq!(refused(&["avx2", "avx2,,fma"]), Refusal::Empty { at: 1 });
        let unknown = refused(&["avx2", "avx9000"]);
        assert!(unknown.to_string().contains("level `avx9000`"), "{unknown}");
        assert!(unknown.to_string().contains(LINUX), "{unknown}");
        // A feature of another arch's table is none of this one's.
        let neon = refused(&["avx2,neon"]);
        assert!(neon.to_string().contains("`neon` in level `avx2,neon`"), "{neon}");
        // `avx2` and `avx,avx2` are one set; so are a CPU and its features spelled out.
        let same = refused(&["avx2", "avx,avx2"]);
        assert_eq!(same, Refusal::Same { first: "avx2".into(), second: "avx,avx2".into() });
        assert!(same.to_string().contains("`avx2` and `avx,avx2`"), "{same}");
        let many = ["avx"; MOST_LEVELS + 1];
        assert_eq!(refused(&many), Refusal::TooMany { levels: MOST_LEVELS + 1 });
    }

    /// The dispatch table's key covers what selection reads: the levels, their order, their
    /// sets and the unit's; and is the same for the same plan (deterministic-output).
    #[test]
    fn the_key_covers_the_levels_their_order_and_the_unit() {
        let key = |unit: &TargetFeatureSet, levels: &[&str]| plan_of(unit, levels).key();
        let base = baseline();
        assert_eq!(key(&base, &["avx2", "avx512f"]), key(&base, &["avx2", "avx512f"]));
        let keys = [
            key(&base, &["avx2", "avx512f"]),
            key(&base, &["avx512f", "avx2"]),
            key(&base, &["avx2"]),
            key(&base, &["avx2,bmi2", "avx512f"]),
            key(&base.union(&subset(1)), &["avx2", "avx512f"]),
            key(&base, &["x86-64-v3"]),
        ];
        for (at, one) in keys.iter().enumerate() {
            assert!(keys[..at].iter().all(|it| it != one), "key {at} is another's");
        }
    }
}
