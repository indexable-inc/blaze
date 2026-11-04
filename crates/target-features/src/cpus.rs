//! The CPUs of `-C target-cpu` of each target and the `target_feature`s rustc gives each: the
//! one home of what blaze knows of CPUs. blaze-units gives `cfg(target_feature)` by it, the code
//! generator the features its code may use and the name LLVM tunes for.
//!
//! A target's CPUs are those `rustc --print target-cpus` lists and the pinned LLVM's aliases of
//! them (`apple-m1` of `apple-a14`, which rustc's list leaves out and `-C target-cpu` takes),
//! each with the features `rustc --print cfg -C target-cpu=<cpu>` prints. `table.rs` is what
//! `tools/cpus.py` writes from harness/answers.py's record of the pinned nightly and
//! `llvm_cpus.json`.

mod table;

use std::fmt;

/// One target's CPUs.
pub struct Cpus {
    /// Each CPU's name and the index of its features in `sets`, sorted by name.
    names: &'static [(&'static str, u16)],
    /// The distinct sets of features, each sorted.
    sets: &'static [&'static [&'static str]],
}

impl Cpus {
    /// The `target_feature`s of `cpu`, sorted; `None` for a name of no CPU of the target.
    pub fn features(&self, cpu: &str) -> Option<&'static [&'static str]> {
        let at = self.names.binary_search_by(|(it, _)| (*it).cmp(cpu)).ok()?;
        self.sets.get(usize::from(self.names[at].1)).copied()
    }

    /// Each CPU's name and the index of its features in [`Self::sets`], sorted by name.
    pub fn names(&self) -> &'static [(&'static str, u16)] {
        self.names
    }

    /// The distinct sets of features the CPUs have, each sorted.
    pub fn sets(&self) -> &'static [&'static [&'static str]] {
        self.sets
    }
}

/// The CPUs of the target `triple`; `None` for a target blaze has no data of.
pub fn of(triple: &str) -> Option<&'static Cpus> {
    let at = table::TARGETS.binary_search_by(|(it, _)| (*it).cmp(triple)).ok()?;
    Some(&table::TARGETS[at].1)
}

/// A `-C target-cpu` that names no CPU blaze knows of the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownCpu {
    pub triple: String,
    pub cpu: String,
    /// Whether blaze has data of the target at all.
    pub known_target: bool,
}

impl fmt::Display for UnknownCpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { triple, cpu, known_target } = self;
        if *known_target {
            write!(
                f,
                "`-C target-cpu={cpu}` is no CPU blaze knows of target `{triple}`: blaze knows \
                 those `rustc --print target-cpus` lists and the pinned LLVM's aliases of them \
                 (target_features::cpus, written by tools/cpus.py)"
            )
        } else {
            write!(
                f,
                "`-C target-cpu={cpu}`: blaze has no CPUs of target `{triple}` (tools/cpus.py)"
            )
        }
    }
}

/// The `target_feature`s CPU `cpu` of target `triple` has, sorted, as rustc gives them with
/// `-C target-cpu=<cpu>` and no `-C target-feature`; an error naming both for a CPU blaze does
/// not know.
pub fn features(triple: &str, cpu: &str) -> Result<&'static [&'static str], UnknownCpu> {
    // alloc: an error, naming the target and the CPU
    let unknown =
        |known_target| UnknownCpu { triple: triple.to_owned(), cpu: cpu.to_owned(), known_target };
    let cpus = of(triple).ok_or_else(|| unknown(false))?;
    cpus.features(cpu).ok_or_else(|| unknown(true))
}

#[cfg(test)]
mod tests {
    /// Apple's M1 to M3 are LLVM aliases (`apple-m1` of `apple-a14`) that rustc's list of CPUs
    /// leaves out: each has its CPU's features, and every table is sorted for its searches.
    #[test]
    fn an_llvm_alias_is_the_cpu_it_names() {
        let darwin = "aarch64-apple-darwin";
        let aliases =
            [("apple-m1", "apple-a14"), ("apple-m2", "apple-a15"), ("apple-m3", "apple-a16")];
        for (alias, cpu) in aliases {
            assert_eq!(super::features(darwin, alias), super::features(darwin, cpu), "{alias}");
        }
        assert!(super::features(darwin, "apple-m4").unwrap().contains(&"sme"));
        let unknown = super::features(darwin, "apple-m99").unwrap_err().to_string();
        assert!(unknown.contains("apple-m99") && unknown.contains(darwin), "{unknown}");
        assert!(super::table::TARGETS.is_sorted_by(|a, b| a.0 < b.0));
        for (triple, cpus) in super::table::TARGETS {
            assert!(cpus.names.is_sorted_by(|a, b| a.0 < b.0), "{triple}");
            assert!(cpus.sets.iter().all(|it| it.is_sorted_by(|a, b| a < b)), "{triple}");
        }
    }
}
