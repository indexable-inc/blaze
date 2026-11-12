//! Revision tests: a small project and a sequence of source revisions, built one after
//! another on incremental state that is kept between them, with three things held after
//! each: the result is what a clean build of that revision gives, what was made again and
//! what was reused is what the test states, and nothing panicked on the way
//! (docs/harness.md, "Revision tests"). rustc's `tests/incremental` is the pattern.
//!
//! A test is declarative (`cases/<name>/test.toml` over `base/` and `rev2/`.. overlays;
//! [`spec`]) and runs through a driver: [`sim`], the deterministic simulation, in-process
//! and seeded, as far down the pipeline as blaze runs without IO of its own; and [`real`],
//! real processes and a real daemon, for what only a kernel shows and as the cross-check.
//! [`rustc`] reads rustc's own single-file form (`//@ revisions:` and `#[cfg(<revision>)]`).
//!
//! Every case builds std, which only a blaze with LLVM can: in the seed profile (no
//! `native`) this target is empty.
#![cfg(all(unix, feature = "native"))]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "a test harness: it starts processes, waits on them by polling (std has no timed wait on a child) and bounds its own parallelism with a process-wide count"
)]

mod check;
mod generate;
mod oracle;
mod real;
mod rustc;
mod spec;

use spec::Test;

/// Each case under the real driver: one `#[test]` a case, named for it.
macro_rules! real {
    ($($name:ident)*) => {
        mod real_driver {
            $(
                #[test]
                fn $name() {
                    let test = super::Test::read(&super::spec::cases(), stringify!($name));
                    assert!(
                        test.drivers.iter().any(|it| it == "real"),
                        "{}: listed under the real driver, which its `drivers` does not name",
                        test.name
                    );
                    super::real::run(&test);
                }
            )*
        }
        const REAL: &[&str] = &[$(stringify!($name)),*];
    };
}

real! {
    a_body_edit_reaches_a_running_program
    a_body_edit_remakes_one_function
    a_broken_revision_fails_and_recovers
    a_cfg_twin_chosen_by_a_build_script
    a_changed_const_remakes_its_two_readers
    a_changed_proc_macro_remakes_the_users_of_its_expansion
    a_changed_struct_layout_remakes_what_uses_it
    a_changed_trait_impl_remakes_what_resolves_to_it
    a_comment_edit_remakes_nothing
    a_dependency_edit_reaches_only_what_uses_it
    a_feature_flip_follows_the_cfg
    a_panic_under_the_launcher_names_its_frames
    a_renamed_function_keeps_its_code
    a_save_by_rename_is_read_whole
    a_save_is_built_before_it_is_asked_for
    a_second_project_reuses_the_standard_library
    a_signature_change_remakes_the_callers
    a_terminal_of_another_width_remakes_nothing
    a_touch_without_a_change
    an_added_and_a_removed_function
    an_edit_after_a_kill
    an_edit_after_a_restart
    an_impl_overriding_a_provided_method_changes_its_vtable
    an_unrelated_environment_variable
    build_then_run_then_build_are_no_ops
    the_first_edit_after_a_cold_build_with_a_build_script_dependency
    the_first_test_after_a_run
    the_same_sources_in_another_directory_under_another_home
}

/// Every directory of `cases/` runs under each driver it names: a case no list holds would
/// never run.
#[test]
fn every_case_is_listed() {
    let mut unlisted = Vec::new();
    for entry in std::fs::read_dir(spec::cases()).unwrap() {
        let entry = entry.unwrap();
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let test = Test::read(&spec::cases(), &name);
        for driver in &test.drivers {
            let listed = match driver.as_str() {
                "real" => REAL.contains(&name.as_str()),
                // The simulated driver's own list is held by its `every_sim_case_is_listed`.
                "sim" => true,
                other => panic!("{name}: no driver `{other}`"),
            };
            if !listed {
                unlisted.push(format!("{name} ({driver})"));
            }
        }
    }
    assert!(unlisted.is_empty(), "cases no `#[test]` runs: {unlisted:?}; add each to its driver's list in main.rs");
}

/// rustc's `tests/incremental` through the real driver (`rustc.rs`): the directory
/// `BLAZE_REVISIONS_RUSTC` names, its report written where `BLAZE_REVISIONS_RUSTC_REPORT`
/// says, only the tests whose path holds `BLAZE_REVISIONS_RUSTC_ONLY` when that is set.
/// Not part of the gate: the corpus is not in the tree, and it takes tens of minutes.
#[test]
#[ignore = "needs rustc's tests/incremental, fetched outside the tree: docs/harness.md, \"Revision tests\""]
fn rustc_incremental_corpus() {
    let dir = std::env::var("BLAZE_REVISIONS_RUSTC").expect("BLAZE_REVISIONS_RUSTC names rustc's tests/incremental");
    let report = std::env::var("BLAZE_REVISIONS_RUSTC_REPORT").expect("BLAZE_REVISIONS_RUSTC_REPORT names the report to write");
    let only = std::env::var("BLAZE_REVISIONS_RUSTC_ONLY").ok();
    rustc::corpus(std::path::Path::new(&dir), std::path::Path::new(&report), only.as_deref());
}
