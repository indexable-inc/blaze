//! Whose a refusal of making one instance is: that test's ([`Scope::One`], its NOT RUN, rule 20)
//! or the suite's ([`Scope::Suite`], the command stops naming it, rule 19). THE ONE HOME of that
//! sorting for the ladder's errors (rule 30): each type answers [`scope`](RunError::scope) by an
//! exhaustive match with no `_` arm, so a new variant is sorted when it is added (rule 73), never
//! by a default. blaze-cli's `ParentError::scope` (run_parent.rs) asks these; test_drive.rs asks
//! it.
//!
//! Before 2026-11-21 the sorting was four functions in blaze-cli test_drive.rs
//! (`stops_suite`, `record_stops_suite`, `front_stops_suite`, the walk's arms of
//! `refused_instance`), three of them ending in `_ => false`: every class no list named was the
//! test's by that arm. The classification is kept exactly; the variants those arms held are now
//! named below, each with [`Scope::One`].
//!
//! The rule: a refusal is the suite's when no test's code can cause it or when it is a fault of
//! what every test shares: the store (a code record that would not be stored, sealed or
//! entered), a bound of the run itself (its instance cap, the program's queue cap), the run's
//! setup and start (its crates, edition, manifest, names, roots, the start's shape and process),
//! the program's crate set, and the engine's own check state (the tables did not fork, a
//! record's reads differ from the list it was checked against). Anything else is the instance's.
//! A refusal that carries another (`NotBuilt`, a row's refusal, a check failure asked) is the
//! scope of what it carries.

use crate::body_ids::WalkError;
use crate::code_records::CodeRecordError;
use crate::front2::check::CheckFail;
use crate::front2::program::Front2Refusal;
use crate::front2::program::lazy::LazyRefusal;
use crate::run::RunError;

/// Whose a refusal is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// The test's whose instance it was making: that test is NOT RUN, the suite goes on.
    One,
    /// The suite's: the command stops, naming it.
    Suite,
}

impl WalkError<RunError> {
    /// Whose this step of the record walk's refusal is (this module's rule).
    pub fn scope(&self) -> Scope {
        match self {
            // The store refused a read or a write, a stored entry does not read, one symbol is
            // tabled under two fns (an entry's declaration), or the walk's own bound on its
            // instances (rule 18).
            WalkError::Store(_) | WalkError::Entry { .. } | WalkError::TwoTabled { .. } | WalkError::Cap { .. } => Scope::Suite,
            // The relay's own protocol (D194: a row stepped while it is being made, a made entry
            // handed back to a row not begun for it): a fault of blaze's driver, which no test's
            // code can cause; as one test's NOT RUN it would hide the driver's bug.
            WalkError::Making { .. } | WalkError::NotBegun { .. } => Scope::Suite,
            WalkError::Make { why, .. } | WalkError::Verify { why, .. } => why.scope(),
            // A maker that asks no read list again over a store holding entries with one: the
            // driver's wiring, which no test's code can cause.
            WalkError::Unverified { .. } => Scope::Suite,
            // A bound the test's code passes (an instance name past its cap, the walk's names
            // past their arena, an item instantiating itself past `#![recursion_limit]`), a fn or
            // crate fact the command lacks, a name that does not list.
            WalkError::NoBody { .. }
            | WalkError::NoCrateFact { .. }
            | WalkError::Name { .. }
            | WalkError::NameTooLarge { .. }
            | WalkError::ArenaFull { .. }
            | WalkError::RecursionLimit { .. } => Scope::One,
        }
    }
}

impl RunError {
    /// Whose this failure of making an instance is (this module's rule).
    pub fn scope(&self) -> Scope {
        match self {
            RunError::NotBuilt { refusal, .. } => refusal.scope(),
            RunError::CodeRecord { error, .. } => error.scope(),
            RunError::Front2(front) => front.scope(),
            // A bound of the run itself, and the run's setup and start.
            RunError::TooManyInstances
            | RunError::OverflowDisagrees { .. }
            | RunError::Crates(_)
            | RunError::Edition { .. }
            | RunError::Names { .. }
            | RunError::Manifest { .. }
            | RunError::Roots(_)
            | RunError::NoMain
            | RunError::MainShape { .. }
            | RunError::NoHost { .. }
            | RunError::NoArgs { .. }
            | RunError::Args(_)
            | RunError::OwnProcess { .. } => Scope::Suite,
            // The instance's: its body, its callees, its types, data and code (each was `_`).
            RunError::Lower { .. }
            | RunError::Check { .. }
            | RunError::Need { .. }
            | RunError::Types { .. }
            | RunError::Build { .. }
            | RunError::ItemRefused { .. }
            | RunError::NoAdt { .. }
            | RunError::Emit { .. }
            | RunError::Callee { .. }
            | RunError::LeftOut { .. }
            | RunError::ConstAt { .. }
            | RunError::Projection { .. }
            | RunError::Resolve { .. }
            | RunError::NoBuiltinCall { .. }
            | RunError::Vtable { .. }
            | RunError::Place(_)
            | RunError::OneBody { .. }
            | RunError::StaticRecord { .. }
            | RunError::Lang(_)
            | RunError::Const { .. }
            | RunError::Statics { .. }
            | RunError::Tls { .. }
            | RunError::TlsCap { .. }
            | RunError::UnwindRaised { .. }
            | RunError::StaticFn { .. }
            | RunError::Trapped { .. } => Scope::One,
        }
    }
}

impl CodeRecordError {
    /// Whose this code record's fault is: the store's (it would not be stored, sealed or
    /// entered) is the suite's; an emit, a symbol, a routine, a shim or a datum that did not make
    /// is the instance's.
    pub fn scope(&self) -> Scope {
        match self {
            CodeRecordError::Store(_) | CodeRecordError::Seal(_) | CodeRecordError::Entry(_) => Scope::Suite,
            // Each was `matches!`'s `false`.
            CodeRecordError::Emit(_)
            | CodeRecordError::Symbol(_)
            | CodeRecordError::NoTabledFn { .. }
            | CodeRecordError::NoReads { .. }
            | CodeRecordError::Getter(_)
            | CodeRecordError::Ender(_)
            | CodeRecordError::Shim(_)
            | CodeRecordError::Routine(_)
            | CodeRecordError::Reads(_)
            | CodeRecordError::NotEmitting(_)
            | CodeRecordError::VtableDatum(_) => Scope::One,
        }
    }
}

impl Front2Refusal {
    /// Whose this front-end refusal is: the program's queue cap, its start (the start rule, the
    /// crate list) and its crate set (`Front`: a program file no `mod` reaches, two values under
    /// one key (rule 21), a lang item defined twice, roots that name no crate set) are the
    /// suite's; a check failure is its own scope; every other is the instance's.
    pub fn scope(&self) -> Scope {
        match self {
            Front2Refusal::Lazy(lazy) => lazy.scope(),
            Front2Refusal::Asked { error, .. } | Front2Refusal::Check(error) => error.scope(),
            Front2Refusal::Front(_)
            | Front2Refusal::Capped { .. }
            | Front2Refusal::Unlisted { .. }
            | Front2Refusal::NoMain
            | Front2Refusal::MainTakenBy { .. }
            | Front2Refusal::MainNotWritten { .. }
            | Front2Refusal::NoLangStart { .. }
            | Front2Refusal::Start { .. }
            | Front2Refusal::NoHostedStart
            | Front2Refusal::StartFact { .. }
            | Front2Refusal::TwoEntries { .. } => Scope::Suite,
            // Each was `_`.
            Front2Refusal::Lower(_)
            | Front2Refusal::Spell(_)
            | Front2Refusal::Unchecked { .. }
            | Front2Refusal::NoBody { .. }
            | Front2Refusal::Const { .. }
            | Front2Refusal::Static { .. }
            | Front2Refusal::NoHead { .. }
            | Front2Refusal::Attrs { .. }
            | Front2Refusal::Unplaced { .. }
            | Front2Refusal::Abi { .. }
            | Front2Refusal::Literal { .. }
            | Front2Refusal::Intrinsic { .. }
            | Front2Refusal::TargetIntrinsic { .. }
            | Front2Refusal::AdtRow { .. }
            | Front2Refusal::ObjectTrait { .. }
            | Front2Refusal::Lang { .. }
            | Front2Refusal::OwnLeftOut { .. }
            | Front2Refusal::NoRowPath(_) => Scope::One,
        }
    }
}

impl LazyRefusal {
    /// Whose this lazy program's refusal is: a row's is what it carries, a check's is its
    /// failure's; every other (`NotFound`, `Unanswered` and `RecordsVoid` among them) is the
    /// instance's.
    pub fn scope(&self) -> Scope {
        match self {
            LazyRefusal::Row { refusal, .. } => refusal.scope(),
            LazyRefusal::Check { error, .. } => error.scope(),
            // Each was `_`.
            LazyRefusal::NotFound { .. }
            | LazyRefusal::AskedTwice { .. }
            | LazyRefusal::Lower { .. }
            | LazyRefusal::Spell { .. }
            | LazyRefusal::LocalNotBuilt { .. }
            | LazyRefusal::Diagnostic { .. }
            | LazyRefusal::Head { .. }
            | LazyRefusal::Adt { .. }
            | LazyRefusal::Const { .. }
            | LazyRefusal::NotLazyYet { .. }
            | LazyRefusal::NeedsSolvedNames { .. }
            | LazyRefusal::Unanswered { .. }
            | LazyRefusal::Table { .. }
            | LazyRefusal::RecordsVoid { .. }
            | LazyRefusal::NoStatic { .. }
            | LazyRefusal::FmtArgument { .. }
            | LazyRefusal::Typed { .. } => Scope::One,
        }
    }
}

impl CheckFail {
    /// Whose this check failure is: the engine's own check state (the tables did not fork, a
    /// record's reads differ from the list it was checked against) is the suite's; every other
    /// is the body's, so the instance's.
    pub fn scope(&self) -> Scope {
        match self {
            CheckFail::Tables { .. } | CheckFail::ReadsDiffer { .. } => Scope::Suite,
            // Each was `matches!`'s `false`.
            CheckFail::Gap { .. }
            | CheckFail::NothingToFill { .. }
            | CheckFail::Head { .. }
            | CheckFail::Buckets { .. }
            | CheckFail::ImplQuestion { .. }
            | CheckFail::Check { .. }
            | CheckFail::Diags { .. }
            | CheckFail::Rounds { .. }
            | CheckFail::Const { .. }
            | CheckFail::Resolve { .. }
            | CheckFail::InlineFor { .. } => Scope::One,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use blaze_ids::ContentId;
    use blaze_typeck::ItemKey;

    use super::Scope;
    use crate::code_records::{CodeKey, CodeRecordError, StoreError};
    use crate::front2::program::Front2Refusal;
    use crate::front2::program::lazy::LazyRefusal;
    use crate::run::RunError;

    /// A class the old `_ => false` arms held is the instance's by name now (`TlsCap`,
    /// `NoTabledFn`, `NoBody`); TWINS: each carried (by `NotBuilt`, by a row's refusal, by a code
    /// record) keeps its scope; CONTROLS: the same carriers around a suite's class
    /// (`NoHostedStart`, a store's `TwoValues`) stop the suite.
    /// Prints nothing; passes as `scope::tests::a_refusal_is_the_scope_of_what_it_carries ... ok`.
    #[test]
    fn a_refusal_is_the_scope_of_what_it_carries() {
        let item = ItemKey(3);
        let one = || Front2Refusal::NoBody { function: "lib::callee".into() };
        let suite = || Front2Refusal::NoHostedStart;
        let row = |refusal| Front2Refusal::Lazy(LazyRefusal::Row { function: "lib::callee".into(), refusal: Box::new(refusal) });
        let carried = |why| RunError::NotBuilt { refusal: Box::new(why), not_built: 1, reached: 2 };
        assert_eq!(RunError::TlsCap { thread_local: item, cap: 4 }.scope(), Scope::One, "a thread local past its cap is the instance's");
        assert_eq!(one().scope(), Scope::One);
        assert_eq!(row(one()).scope(), Scope::One, "twin: through a row's refusal");
        assert_eq!(carried(RunError::Front2(row(one()))).scope(), Scope::One, "twin: carried by `NotBuilt`");
        assert_eq!(suite().scope(), Scope::Suite, "control: the hosted start is the suite's");
        assert_eq!(row(suite()).scope(), Scope::Suite, "control: through a row's refusal");
        assert_eq!(carried(RunError::Front2(row(suite()))).scope(), Scope::Suite, "control: carried by `NotBuilt`");
        let record = |error| RunError::CodeRecord { function: crate::run::Named { path: "lib::callee".into(), at: crate::front::At { file: 0, place: 80 } }, error };
        assert_eq!(record(CodeRecordError::NoTabledFn { tabled: item }).scope(), Scope::One, "a record with no tabled fn is the instance's");
        let store = CodeRecordError::Store(StoreError::TwoValues { key: CodeKey(ContentId([7; 32])) });
        assert_eq!(carried(record(store)).scope(), Scope::Suite, "control: a carried store fault stops the suite");
    }

    /// The ban of the sorting's old homes (rule 30): no crate's `src/` defines the four
    /// functions scope.rs replaced again (spelled in pieces here, so this file does not match).
    /// Prints `no_second_home_of_the_scope: N files scanned, 0 definitions`.
    /// Control: the scan finds a planted definition.
    #[test]
    fn no_second_home_of_the_scope() {
        use std::string::{String, ToString};
        use std::vec::Vec;
        let banned: Vec<String> = ["stops", "record_stops", "front_stops"]
            .iter()
            .map(|head| std::format!("fn {head}_{}(", "suite"))
            .chain([std::format!("fn refused_{}(", "instance"), std::format!("fn refused_{}(", "at")])
            .collect();
        let hits = |file: &str, text: &str, out: &mut Vec<(String, usize)>| {
            for (n, line) in text.lines().enumerate() {
                if banned.iter().any(|name| line.contains(name.as_str())) {
                    out.push((file.to_string(), n + 1));
                }
            }
        };
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut work = Vec::new();
        for entry in std::fs::read_dir(&crates).expect("the crates folder is readable") {
            let src = entry.expect("a directory entry").path().join("src");
            if src.is_dir() {
                work.push(src);
            }
        }
        let (mut files, mut found) = (0usize, Vec::new());
        while let Some(dir) = work.pop() {
            for entry in std::fs::read_dir(&dir).expect("a source folder is readable") {
                let path = entry.expect("a directory entry").path();
                if path.is_dir() {
                    work.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    files += 1;
                    hits(&path.display().to_string(), &text, &mut found);
                }
            }
        }
        assert!(files > 1000, "the scan saw the workspace's sources: {files} files");
        assert!(found.is_empty(), "a refusal's scope is asked of its type (blaze-ladder scope.rs), never sorted again: {found:?}");
        let mut planted = Vec::new();
        hits("planted.rs", &std::format!("fn f() {{}}\n{}why: &E) -> bool {{ true }}\n", banned[2]), &mut planted);
        assert_eq!(planted, [("planted.rs".to_string(), 2)], "control: the scan finds a planted definition");
        std::println!("no_second_home_of_the_scope: {files} files scanned, 0 definitions");
    }
}
