//! The identity property of the edit fuzzer's model, as lane identity states it
//! (docs/v2/plan/design/code-identity.md):
//!
//! 1. an edit of comments, formatting or item order keeps every function's compile identity
//!    (`instance_identity`, in a debug and in a release unit, and in a debug unit that moves
//!    allocations to the stack) and every root's `code_id`;
//! 2. an edit of behaviour changes the `code_id` of every root that reaches what it edited, and
//!    of no other root;
//! 3. the same program in a fresh database, checked out at another path, with its functions
//!    asked in the other order, has the same identities.

use std::collections::BTreeMap;

use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty::{
    ParamEnvAndCrate,
    mir::{MirOptLevel, MirOptions},
    next_solver::{GenericArgs, ParamEnv},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_identity::{
    CodeId, CompileIdentity, Compiled, Instance, Subject, code_id, instance_identity,
};
use hir_ty_mir::mir::dispatch::Body;

use super::{Edit, LIBRARIES, Model, TestDB, Width, edits, run_cases};

impl Edit {
    /// An edit of comments, formatting or item order: no code changes.
    fn is_cosmetic(self) -> bool {
        matches!(self, Edit::EditComment(_) | Edit::Reformat(_) | Edit::Reorder(..))
    }
}

/// What one root of the model can run, leaving out comments, layout and item order: the parts
/// of the model its code reaches.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Reached {
    fields: Option<Vec<Width>>,
    consts: Option<[u32; 2]>,
    inlined: Option<u32>,
    cfg_on: Option<bool>,
    dropping: Option<bool>,
}

impl Model {
    /// What `root` (one of [`ROOTS`]) reaches: `entry` all of it but `B`; `mix` its constant;
    /// `mode` the `cfg` that picks it; `read_s` the static, made of both consts; `boxed`
    /// whether `B` has an `impl Drop`; `wide` nothing the model edits.
    fn reached(&self, root: &str) -> Reached {
        let all = root == "entry";
        Reached {
            // alloc: a test's model, a few widths
            fields: all.then(|| self.fields.clone()),
            consts: (all || root == "read_s").then_some(self.consts),
            inlined: (all || root == "mix").then_some(self.inlined),
            cfg_on: (all || root == "mode").then_some(self.cfg_on),
            dropping: (root == "boxed").then_some(self.dropping),
        }
    }

    /// Its source as a crate `edited` whose root file is at `path`.
    fn at(&self, path: &str) -> String {
        format!("{LIBRARIES}//- {path} crate:edited deps:alloc\n{}", self.body())
    }
}

/// A debug unit of the fixture's profile: level 1, nothing inlined.
const DEBUG: Compiled = Compiled::every(MirOptions {
    level: MirOptLevel::Debug,
    ub_checks: false,
    lifetime_markers: true,
    inline: false,
    heap_to_stack: false,
    world: None,
});

/// A debug unit of a program whose allocations are unobserved: `HeapToStack` moves `boxed`'s
/// box while `B` has no `impl Drop`.
const MOVED: Compiled = Compiled::every(MirOptions {
    level: MirOptLevel::Debug,
    ub_checks: false,
    lifetime_markers: true,
    inline: false,
    heap_to_stack: true,
    world: None,
});

/// A release unit: level 2 with the inliner on.
const RELEASE: Compiled = Compiled::every(MirOptions {
    level: MirOptLevel::Release,
    ub_checks: false,
    lifetime_markers: true,
    inline: true,
    heap_to_stack: false,
    world: None,
});

/// The model's functions with no generic parameter, each a root, by name.
const ROOTS: [&str; 6] = ["entry", "mix", "mode", "wide", "read_s", "boxed"];

/// Of each of [`ROOTS`]: its compile identity in a debug, a moving and a release unit, and its
/// `CodeId`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ids {
    debug: CompileIdentity,
    moved: CompileIdentity,
    release: CompileIdentity,
    code: CodeId,
}

/// Which way round [`identities`] asks for [`ROOTS`].
#[derive(Clone, Copy)]
enum Order {
    TopDown,
    BottomUp,
}

/// [`Ids`] of each root of `text`, in a database of its own, as another process would have
/// them, asked in `order`.
fn identities(text: &str, order: Order) -> BTreeMap<&'static str, Ids> {
    let db = fixture::db(text).into_db();
    blaze_query::db::attach(&db, || {
        let env: StoredParamEnvAndCrate =
            ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: fixture::test_crate(&db) }
                .store();
        let instance = |name: &str| Instance {
            subject: Subject::Body(Body::Fn(function(&db, name))),
            args: GenericArgs::new_from_slice(&[]).store(),
        };
        let compiled =
            |name: &str, how: Compiled| match instance_identity(&db, &instance(name), &env, how) {
                Ok(it) => *it,
                Err(why) => panic!("`{name}` has no compile identity: {why}\n{text}"),
            };
        let mut roots = ROOTS;
        if let Order::BottomUp = order {
            roots.reverse();
        }
        roots
            .into_iter()
            .map(|name| {
                let code = code_id(&db, &instance(name), &env)
                    .unwrap_or_else(|why| panic!("`{name}` has no CodeId: {why}\n{text}"));
                let ids = Ids {
                    debug: compiled(name, DEBUG),
                    moved: compiled(name, MOVED),
                    release: compiled(name, RELEASE),
                    code,
                };
                (name, ids)
            })
            .collect()
    })
}

/// The one function named `name` in the fixture's crate.
#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    // alloc: a test's lookup, a few functions
    let found: Vec<FunctionId> = crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        // alloc: a test's lookup, one module's declarations
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .filter_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

/// The property, after each of `edits`. Every finding of a step is reported at once.
fn identities_keep_or_change(edits: &[Edit]) {
    let mut model = Model::new();
    let mut was = identities(&model.at("/a/checkout/src/lib.rs"), Order::TopDown);
    for (n, edit) in edits.iter().enumerate() {
        let before = model.clone();
        model.apply(*edit);
        let now = identities(&model.at("/a/checkout/src/lib.rs"), Order::TopDown);
        // alloc: a test's findings
        let mut found = Vec::new();
        let elsewhere = identities(&model.at("/b/elsewhere/src/lib.rs"), Order::BottomUp);
        if elsewhere != now {
            found.push(format!(
                "checked out elsewhere and asked bottom-up: {elsewhere:?}, against {now:?}"
            ));
        }
        for root in ROOTS {
            let (then, here) = (was[root], now[root]);
            let changed = before.reached(root) != model.reached(root);
            if edit.is_cosmetic() && then != here {
                found.push(format!(
                    "`{root}`: {edit:?} changed no code and changed {then:?} to {here:?}"
                ));
            } else if changed && then.code == here.code {
                found.push(format!(
                    "`{root}`: {edit:?} changed what it runs and kept its CodeId {:?}",
                    here.code
                ));
            } else if !edit.is_cosmetic() && !changed && then.code != here.code {
                found.push(format!(
                    "`{root}`: {edit:?} changed nothing it reaches and changed its CodeId from {:?} to {:?}",
                    then.code, here.code
                ));
            }
        }
        if !found.is_empty() {
            panic!("after edits {:?}:\n- {}\n{}", &edits[..=n], found.join("\n- "), model.source());
        }
        was = now;
    }
}

/// Each edit kind once: the identity property's smoke run.
#[test]
fn each_edit_kind_keeps_or_changes_the_identities() {
    identities_keep_or_change(&[
        Edit::EditComment(1),
        Edit::Reformat(1),
        Edit::Reorder(0, 4),
        Edit::ChangeConst(0, 7),
        Edit::ChangeInlined(4),
        Edit::FlipCfg,
        Edit::AddField(Width::U8),
        Edit::ToggleDrop,
        Edit::Reorder(7, 2),
        Edit::ToggleDrop,
    ]);
}

#[test]
fn cosmetic_edits_keep_every_identity_and_behaviour_edits_change_the_code_id() {
    run_cases(12, |tc| identities_keep_or_change(&edits(&tc)));
}
