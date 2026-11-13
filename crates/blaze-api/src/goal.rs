//! The build goal (docs/v2/plan/design/tiers.md): what a build, run or test waits for before
//! it answers. The one home of the idea: the CLI's `--goal`, a request's [`Targets::goal`],
//! the daemon's choice of each function's first code and what a build says of itself
//! ([`Tiers`]) all read this type.
//!
//! [`Targets::goal`]: crate::Targets::goal

use std::fmt;

use serde::{Deserialize, Serialize};

/// What a request waits for.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Goal {
    /// Done when every function the request needs has tier 0 code (copy-and-patch stencils)
    /// and the program or test can start. The optimized code of the same functions is made
    /// after the answer.
    Edit,
    /// Done when every function the request needs has its optimized code (LLVM's). Never
    /// answered with tier 0 code: a function that cannot be optimized fails the build, named.
    Max,
}

impl Goal {
    pub const ALL: [Goal; 2] = [Goal::Edit, Goal::Max];

    /// The names `--goal` takes, in [`Goal::ALL`]'s order.
    pub const NAMES: &'static [&'static str] = &["edit", "max"];

    /// Its name on a command line and in what a build says.
    pub const fn name(self) -> &'static str {
        match self {
            Goal::Edit => "edit",
            Goal::Max => "max",
        }
    }

    /// The goal named `name`, as `--goal` takes it (any case); `None` for any other word.
    pub fn named(name: &str) -> Option<Goal> {
        Goal::ALL.into_iter().find(|it| it.name().eq_ignore_ascii_case(name))
    }

    /// The goal of a Cargo profile no `--goal` overrides: `max` for a release profile (named
    /// `release` or `bench`, or one whose `opt-level` is 2, 3, `s` or `z`, as one inheriting
    /// from `release` is), `edit` for every other (`dev`, `test`, and any at `opt-level` 0 or
    /// 1). A profile that asks for `codegen-backend = "stencil"` asks for tier 0 code by name,
    /// so its goal is `edit` whatever its level.
    pub fn of_profile(name: &str, opt_level: &str, codegen_backend: Option<&str>) -> Goal {
        if codegen_backend == Some("stencil") {
            return Goal::Edit;
        }
        let release = matches!(name, "release" | "bench");
        let optimized = !matches!(opt_level, "0" | "1");
        match release || optimized {
            true => Goal::Max,
            false => Goal::Edit,
        }
    }
}

impl fmt::Display for Goal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A build's goal and how much of the code it linked is optimized: what its last line and
/// the status line say ([`Tiers::status`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tiers {
    pub goal: Goal,
    /// The functions of the images the build linked, each image's counted once; 0 for a
    /// build that linked none (everything it placed came from the store as it was).
    pub functions: u64,
    /// Of `functions`, those whose linked code is the optimized tier's (LLVM at `-O2` or
    /// `-O3`).
    pub optimized: u64,
}

impl Tiers {
    /// A goal with no image linked yet.
    pub const fn of(goal: Goal) -> Tiers {
        Tiers { goal, optimized: 0, functions: 0 }
    }

    /// These counts with another image's.
    #[must_use]
    pub const fn with(self, optimized: u64, functions: u64) -> Tiers {
        Tiers {
            goal: self.goal,
            optimized: self.optimized.saturating_add(optimized),
            functions: self.functions.saturating_add(functions),
        }
    }

    /// The share of the linked functions that are optimized, in whole percent, rounded down:
    /// 100 only when every one is. `None` when no image was linked.
    pub fn percent(&self) -> Option<u64> {
        let of = u128::from(self.functions);
        (of > 0).then(|| (u128::from(self.optimized) * 100 / of) as u64)
    }

    /// Where the code stands: `tier 0 · 38% optimized` under `edit` (`tier 0` alone when no
    /// image was linked), `optimized` under `max`. Under `max`, code a package's own
    /// `opt-level` of 0 or 1 holds at tier 0 is said, never counted as optimized.
    pub fn status(&self) -> String {
        // alloc: one short line a build
        match (self.goal, self.percent()) {
            (Goal::Edit, None) => "tier 0".to_owned(),
            (Goal::Edit, Some(percent)) => format!("tier 0 · {percent}% optimized"),
            (Goal::Max, None | Some(100)) => "optimized".to_owned(),
            (Goal::Max, Some(percent)) => {
                format!("{percent}% optimized, the rest as its package's opt-level asks")
            }
        }
    }

    /// The goal and where the code stands, as a build's last line and the status line say
    /// them: `goal edit: tier 0 · 38% optimized`.
    pub fn line(&self) -> String {
        // alloc: one short line a build
        format!("goal {}: {}", self.goal, self.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goal_is_read_by_its_name_and_nothing_else() {
        assert_eq!(Goal::named("edit"), Some(Goal::Edit));
        assert_eq!(Goal::named("max"), Some(Goal::Max));
        assert_eq!(Goal::named("MAX"), Some(Goal::Max), "as clap's `ignore_case`");
        for other in ["", "fast", "balanced", "release", "edit ", "0"] {
            assert_eq!(Goal::named(other), None, "{other:?}");
        }
        for goal in Goal::ALL {
            assert_eq!(Goal::named(goal.name()), Some(goal));
            assert_eq!(goal.to_string(), goal.name());
        }
        let names: Vec<&str> = Goal::ALL.iter().map(|it| it.name()).collect();
        assert_eq!(names, Goal::NAMES);
    }

    #[test]
    fn dev_profiles_edit_and_release_profiles_max() {
        for (name, level) in [("dev", "0"), ("test", "0"), ("dev", "1"), ("mine", "0")] {
            assert_eq!(Goal::of_profile(name, level, None), Goal::Edit, "{name} at {level}");
        }
        let release = [
            ("release", "3"),
            ("bench", "3"),
            // By name: cargo's release profile with its level lowered is a release build still.
            ("release", "0"),
            // By level: a profile that inherits from `release`, or `dev` raised to it.
            ("production", "3"),
            ("dev", "2"),
            ("small", "s"),
            ("small", "z"),
        ];
        for (name, level) in release {
            assert_eq!(Goal::of_profile(name, level, None), Goal::Max, "{name} at {level}");
        }
        assert_eq!(Goal::of_profile("dev", "0", Some("llvm")), Goal::Edit);
        assert_eq!(Goal::of_profile("release", "3", Some("llvm")), Goal::Max);
        assert_eq!(
            Goal::of_profile("release", "3", Some("stencil")),
            Goal::Edit,
            "a profile that asks for stencils asks for tier 0"
        );
    }

    #[test]
    fn the_status_says_the_tier_and_how_much_is_optimized() {
        assert_eq!(Tiers::of(Goal::Edit).status(), "tier 0");
        assert_eq!(Tiers::of(Goal::Edit).with(38, 100).status(), "tier 0 · 38% optimized");
        assert_eq!(Tiers::of(Goal::Edit).with(0, 7).status(), "tier 0 · 0% optimized");
        assert_eq!(
            Tiers::of(Goal::Edit).with(999, 1000).status(),
            "tier 0 · 99% optimized",
            "rounded down: 100% is every function"
        );
        assert_eq!(Tiers::of(Goal::Edit).with(7, 7).status(), "tier 0 · 100% optimized");
        assert_eq!(Tiers::of(Goal::Max).status(), "optimized");
        assert_eq!(Tiers::of(Goal::Max).with(7, 7).status(), "optimized");
        assert_eq!(
            Tiers::of(Goal::Max).with(3, 4).status(),
            "75% optimized, the rest as its package's opt-level asks",
            "`max` never says tier 0 code is optimized"
        );
        assert_eq!(
            Tiers::of(Goal::Edit).with(38, 100).line(),
            "goal edit: tier 0 · 38% optimized"
        );
        assert_eq!(Tiers::of(Goal::Max).with(2, 2).line(), "goal max: optimized");
    }

    #[test]
    fn images_counts_add_up() {
        let tiers = Tiers::of(Goal::Edit).with(1, 4).with(2, 6);
        assert_eq!((tiers.optimized, tiers.functions), (3, 10));
        assert_eq!(tiers.percent(), Some(30));
        assert_eq!(Tiers::of(Goal::Max).percent(), None);
    }
}
