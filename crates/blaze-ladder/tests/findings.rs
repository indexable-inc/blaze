//! One source of truth for refusals (rule 30): what `blaze run`'s findings path prints
//! (blaze-cli main.rs's `program_refusals`, which calls `blaze_ladder::run::refusals`) is what
//! the runner stops at. The two disagreed once: the findings path used an older lowering and
//! printed `?` as unsupported in a program that ran. Its own binary: the rung runner in
//! tests/ladder.rs points fd 1 at a pipe.

#![feature(allocator_api)]

mod rungs;

use blaze_build::OverflowChecks;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{build, refusals};
use rungs::{RUNNING, ladder, sources};

/// A rung that runs yields no refusal finding.
#[test]
fn a_running_rung_has_no_findings() {
    for rung in RUNNING {
        let dir = ladder().join(rung);
        let files = sources(&dir);
        let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
        let found = refusals(&views, "src/main.rs", rungs::x::overflow_of(&dir), blaze_own::Alloc::global());
        assert!(found.is_empty(), "rung {rung} runs, yet the findings path refuses it: {found:?}");
    }
}

/// The control: a program the runner refuses, at the front end and at a later stage, yields
/// exactly the refusals the runner's own error carries.
#[test]
fn a_refused_program_has_the_runners_refusal() {
    let programs = [
        // The front end refuses it: no file for the module.
        "mod gone;\nfn main() {}",
        // A later stage refuses it: `main`'s shape.
        "fn main() -> u8 { 1 }",
        // A later stage refuses it: a type error in a body.
        "fn main() { let x: u8 = (); }",
    ];
    for text in programs {
        let views = [SourceFile { path: "src/main.rs", text }];
        let found = refusals(&views, "src/main.rs", OverflowChecks::On, blaze_own::Alloc::global());
        assert!(!found.is_empty(), "`{text}` is refused");
        let Err(error) = build(&views, "src/main.rs", OverflowChecks::On, blaze_own::Alloc::global()) else { panic!("`{text}` built") };
        assert_eq!(error.refusals().as_slice(), found.as_slice(), "`{text}`");
    }
}
