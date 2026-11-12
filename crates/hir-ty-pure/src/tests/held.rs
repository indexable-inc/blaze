//! What the walk keeps of the bodies it reads: nothing an instance. A summary reads its
//! instance's monomorphized body once and keeps the summary; the body is made for that run and
//! dropped with it (`dispatch::body_once`). Until 2026-11-12 it came from the memo that keeps
//! one body an instance until the build lets every MIR go, and a cold build of tempo held
//! 4,197,786 of them under one unit's codegen.

use base_db::Db;
use blaze_query::db::Database as _;
use hir_ty_mir::mir::dispatch::Body;

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
};
use crate::{
    function::generic,
    walk::{Def, Instance, plant, summary},
};

/// A crate whose `root` calls one generic function, `leaf`, with `instances` different type
/// arguments (`u8`, `(u8,)`, `((u8,),)`, ...): two bodies whatever `instances` is.
fn ladder(instances: usize) -> String {
    let mut ty = String::from("u8");
    let mut calls = String::new();
    for _ in 0..instances {
        calls.push_str(&format!("    n = leaf::<{ty}>(n);\n"));
        ty = format!("({ty},)");
    }
    format!(
        "\n//- /lib.rs crate:kernel deps:std\n\
         fn leaf<T>(x: u32) -> u32 {{\n    x\n}}\n\n\
         pub fn root(mut n: u32) -> u32 {{\n{calls}    n\n}}\n"
    )
}

/// How many MIR values the database holds once `root`'s summary, and so each instance's it
/// reaches, is made.
fn held_after_the_walk(instances: usize, keeps_bodies: bool) -> usize {
    let db: Db = db_with(&ladder(instances));
    let krate = crate_named(&db, "kernel");
    let root = fn_named(&db, krate, "root");
    blaze_query::db::attach(&db, || {
        let (env, args) = generic(&db, root);
        let instance = Instance { def: Def::Body(Body::Fn(root)), args };
        let walk = || _ = summary(&db, env, instance);
        match keeps_bodies {
            true => plant::keeping_bodies(walk),
            false => walk(),
        }
    });
    db.storage().values_of(blaze_mem::Tag::Mir)
}

/// The invariant: the MIR the purity walk leaves held is by body, never by instance: a walk
/// over 8 instances of one generic and a walk over 64 hold the same number of MIR values.
#[test]
fn the_walk_holds_no_body_an_instance() {
    let (few, many) = (held_after_the_walk(8, false), held_after_the_walk(64, false));
    assert_eq!(few, many, "MIR values held after a walk of 8 instances and of 64");
}

/// The planted bug: each summary reads its body from the memo that keeps it, as the walk did.
/// The oracle of [`the_walk_holds_no_body_an_instance`] compares the two counts, and here 56
/// more instances hold at least 56 more bodies.
#[test]
fn the_oracle_catches_a_walk_that_keeps_each_instances_body() {
    let (few, many) = (held_after_the_walk(8, true), held_after_the_walk(64, true));
    assert!(many >= few + 56, "the memoized walk held {few} MIR values for 8 instances and {many} for 64");
}
