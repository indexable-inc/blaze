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

/// A crate whose `root` calls `width` functions, each the head of a chain `depth` calls deep
/// that ends in a pair of functions calling each other: wide enough that the callees'
/// summaries are asked ahead, with a cycle below them.
fn fan(width: usize, depth: usize) -> String {
    let mut text = String::from("\n//- /lib.rs crate:kernel deps:std\n");
    text.push_str("fn ping(n: u32) -> u32 {\n    if n == 0 { 0 } else { pong(n - 1) }\n}\n");
    text.push_str("fn pong(n: u32) -> u32 {\n    if n == 0 { 1 } else { ping(n - 1) }\n}\n");
    let mut calls = String::new();
    for chain in 0..width {
        for at in 0..depth {
            let next = match at + 1 == depth {
                true => "ping(n) ^ pong(n)".to_owned(),
                false => format!("c{chain}_{}(n) ^ c{chain}_{}(n ^ 1)", at + 1, at + 1),
            };
            text.push_str(&format!("fn c{chain}_{at}(n: u32) -> u32 {{\n    {next}\n}}\n"));
        }
        calls.push_str(&format!("    n ^= c{chain}_0(n);\n"));
    }
    text.push_str(&format!("pub fn root(mut n: u32) -> u32 {{\n{calls}    n\n}}\n"));
    text
}

/// What the summaries of `root` and of each chain's head say, with no id of the database in
/// it: each one's kinds and how many instances and sites it has.
fn said(how: fn(&mut dyn FnMut())) -> Vec<String> {
    const WIDTH: usize = 6;
    let db: Db = db_with(&fan(WIDTH, 4));
    let krate = crate_named(&db, "kernel");
    let mut said = Vec::new();
    blaze_query::db::attach(&db, || {
        let names = std::iter::once("root".to_owned()).chain((0..WIDTH).map(|it| format!("c{it}_0")));
        for name in names {
            let func = fn_named(&db, krate, &name);
            let (env, args) = generic(&db, func);
            let instance = Instance { def: Def::Body(Body::Fn(func)), args };
            let mut ask = || {
                let made = summary(&db, env.clone(), instance.clone());
                said.push(format!("{name}: {:?}, {} callees", made.kinds, made.callees.len()));
            };
            how(&mut ask);
        }
    });
    said
}

/// The refinement: the walk that asks its callees' summaries ahead on the pool gives the
/// summaries of the walk that asks each in its turn on one stack, for a fan of chains over a
/// cycle; and gives them again on a second run, whatever its jobs' order was.
#[test]
fn summaries_asked_ahead_are_the_summaries_asked_in_turn() {
    let in_turn = said(|ask| plant::asking_in_turn(ask));
    assert!(in_turn[0].ends_with("6 callees"), "root calls the six heads: {}", in_turn[0]);
    for run in 0..4 {
        assert_eq!(said(|ask| ask()), in_turn, "run {run}");
    }
}

/// The planted bug: a summary made from the gather of the ask-ahead, which lists its callees
/// and asks none. The oracle of [`summaries_asked_ahead_are_the_summaries_asked_in_turn`]
/// compares what each summary says, and one made so has no callee.
#[test]
fn the_oracle_catches_a_summary_made_from_the_gather_ahead() {
    let in_turn = said(|ask| plant::asking_in_turn(ask));
    let planted = said(|ask| plant::gathering_dry(ask));
    assert_ne!(planted, in_turn);
}
