//! THE `ImplBucket` TASK'S READS AT CELL GRAIN (lane BH2, lane BH1's NOT DONE 1): a crate's impl
//! tables task (engine_heads.rs `tables_of`) read, under `Whole`, each impl header's head as
//! EVERY scope crate's names record whole, so an item added to any member crate entered the
//! tables of every crate whose scope holds it. Under `Cells` (engine_scope_reads.rs
//! `ScopeReads::TABLES_AT_CELLS`) a header's head read is the cell of the head task that covers
//! that head (`Owners::place`, as a body's), and a names read its cell. The task's own crate's
//! record stays whole under both (its impl rows have no cell).
//!
//! The fixture: two members, `b` depending on `a`; `b`'s one impl names `a`'s trait in its header
//! (`impl a::Measure for Meter {}`), so `b`'s tables read `a::Measure`'s head, and no fn body of
//! one member reads a head of the other. The edit: a private fn added to `a`, which moves `a`'s
//! names record and neither `a::Measure`'s head nor anything of `b`.

use super::engine_check_tests::{engine_opts, entries, opts, placed_ids, profile};
use super::*;
use crate::engine_scope_reads::ScopeReadsAs;

const A_LIB: &str = "pub trait Measure {}\n\npub fn one() -> u8 {\n    1\n}\n";

const B_LIB: &str = "pub struct Meter {\n    pub n: u8,\n}\n\nimpl a::Measure for Meter {}\n";

/// The two members, `a`'s lib as given.
fn two_members(io: &Io, a_lib: &str) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", a_lib);
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(io, "b/src/lib.rs", B_LIB);
}

/// Heads-stage tasks entered for a names record read whole that moved (`Because` groups).
fn heads_for_whole_records(checked: &engine_check::Checked) -> u32 {
    checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Heads && group.cause == "read moved" && group.cell == Some(crate::engine_scope_reads::CellKind::WholeRecord)).map(|(_, count)| *count).sum()
}

/// The profile's "entered because" section, with the tasks each group entered.
fn because_lines(profile: &str) -> std::string::String {
    profile.lines().skip_while(|line| !line.starts_with("entered because")).take_while(|line| line.starts_with("entered because") || line.starts_with("  ")).collect::<std::vec::Vec<_>>().join("\n")
}

/// A PRIVATE FN ADDED TO `a` enters `b`'s impl tables task under `Whole` (its header's head read
/// as `a`'s record whole) and not under `Cells` (that read is `a::Measure`'s head cell, which the
/// edit leaves equal): fewer Heads tasks entered for a record read whole under `Cells`. Both
/// sides answer as a fresh run (the oracle), and the cold run checks clean.
#[test]
fn a_private_fn_added_to_a_dependency_member_enters_no_other_crates_impl_tables() {
    let sides = [(ScopeReadsAs::Cells, false, "cells"), (ScopeReadsAs::Whole, false, "whole"), (ScopeReadsAs::Cells, true, "fresh")];
    let ios: std::vec::Vec<Io> = sides.iter().map(|_| Io::simulated(SimWorld::new(41, Limits::USUAL))).collect();
    for io in ios.iter() {
        two_members(io, A_LIB);
    }
    let run = |io: &Io, reads: ScopeReadsAs, fresh: bool| {
        let mut o = opts(io, 2, fresh);
        o.scope_reads = reads;
        let (ids, checked) = engine_opts(io, o);
        (placed_ids(&ids), checked, profile(io))
    };
    for ((reads, fresh, label), io) in sides.iter().zip(ios.iter()) {
        let (ids, checked, _) = run(io, *reads, *fresh);
        assert!(ids.is_empty() && checked.status.0 != Status::FAILED.0, "the first request ({label}) checks clean: {ids:?}: {}", entries(&checked));
    }
    let added = format!("{A_LIB}\nfn two() -> u8 {{\n    2\n}}\n");
    let mut answers = std::vec::Vec::new();
    for ((reads, fresh, label), io) in sides.iter().zip(ios.iter()) {
        write(io, "a/src/lib.rs", &added);
        let (ids, checked, profile) = run(io, *reads, *fresh);
        println!("a private fn added to `a` ({label}): {}\n{}", entries(&checked), because_lines(&profile));
        answers.push((ids, checked));
    }
    for ((_, _, label), (ids, checked)) in sides.iter().zip(answers.iter()) {
        assert_eq!((ids, checked.status.0), (&answers[2].0, answers[2].1.status.0), "the {label} side answers as a fresh run");
    }
    let (cells, whole) = (heads_for_whole_records(&answers[0].1), heads_for_whole_records(&answers[1].1));
    assert!(cells < whole, "Heads tasks entered for a record read whole: Cells {cells}, Whole {whole}: under Whole `b`'s impl tables read `a`'s record whole, under Cells `a::Measure`'s head cell");
}
