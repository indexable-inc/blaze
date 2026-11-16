//! Integration tests, examples and benches as units (docs/v2/plan/design/test.md, "The other
//! targets"): each target the layout finds in `tests/`, `examples/` or `benches/` (or a
//! `[[test]]`, `[[example]]`, `[[bench]]` table writes) is a unit of its own kind, depending on
//! its package's plain library unit and on the package's dev-dependencies, made only when tests
//! are asked ([`crate::Ask::tests`]), as the library's test unit is.

use blaze_manifest::layout::ExtraKind;
use blaze_own::{AVec, Alloc, String, Vec};

use crate::{Text, UnitKind};

/// An integration test, example or bench of a package, as the rules read it.
#[derive(Clone, Debug)]
pub struct Extra<'a> {
    pub kind: ExtraKind,
    pub name: Text<'a>,
    /// The root file, relative to the package's directory.
    pub root: Text<'a>,
    pub harness: bool,
    pub test: bool,
    /// `required-features`, as written (sorted here).
    pub required_features: AVec<'a, Text<'a>>,
}

impl<'a> Extra<'a> {
    /// The layout's target; `root` is its root relative to the package's directory.
    pub fn of(target: &blaze_manifest::layout::ExtraTarget<'_>, root: Text<'a>, alloc: Alloc<'a>) -> Extra<'a> {
        let mut required_features = Vec::with_capacity_in(target.required_features.len(), alloc);
        for feature in target.required_features.iter() {
            required_features.push(String::from_str_in(feature, alloc));
        }
        // Sorted at the boundary: the unit's record holds them, and the written order is not
        // what they mean.
        required_features.sort_by(|a: &Text<'a>, b: &Text<'a>| a.as_str().cmp(b.as_str()));
        required_features.dedup();
        Extra {
            kind: target.kind,
            name: String::from_str_in(&target.name, alloc),
            root,
            harness: target.harness,
            test: target.test,
            required_features,
        }
    }

    /// The unit's kind.
    pub(crate) fn unit_kind(&self) -> UnitKind<'a> {
        let unit = ExtraUnit { name: self.name.clone(), harness: self.harness, test: self.test, required_features: self.required_features.clone() };
        match self.kind {
            ExtraKind::Test => UnitKind::IntegrationTest(unit),
            ExtraKind::Example => UnitKind::Example(unit),
            ExtraKind::Bench => UnitKind::Bench(unit),
        }
    }
}

/// What a unit of an integration test, example or bench carries beyond its kind: its target's
/// name (two of one package are two units), and as data for the runner, which decides later,
/// `harness`, `test` and `required-features`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExtraUnit<'a> {
    pub name: Text<'a>,
    pub harness: bool,
    pub test: bool,
    /// Sorted.
    pub required_features: AVec<'a, Text<'a>>,
}

/// Whether a unit of `kind` is compiled with `test` in its cfg: the library's and a binary's
/// test units, and an integration test or bench with a harness ("The `harness` field indicates
/// that the `--test` flag will be passed to `rustc`", the Cargo Book, "Cargo Targets"; `--test`
/// is what sets `cfg(test)`). An example is compiled as a program, as is a harness-less test.
pub(crate) fn has_test_cfg(kind: &UnitKind<'_>) -> bool {
    match kind {
        UnitKind::Test { .. } => true,
        UnitKind::IntegrationTest(unit) | UnitKind::Bench(unit) => unit.harness,
        UnitKind::Lib | UnitKind::ProcMacro | UnitKind::Bin(_) | UnitKind::BuildScript | UnitKind::Example(_) => false,
    }
}
