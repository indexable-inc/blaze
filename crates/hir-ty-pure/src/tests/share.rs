//! Summaries keyed by an instance's home (`hir_ty_mir::mir::home`): two build scripts share the summary of
//! what both reach in the toolchain and a common dependency, an instance whose arguments name a
//! script's own type, whose impl may change what it does, is that script's, and a crate that
//! answers differently from the script (its lang items, its features, a dependency of its own
//! it does not load and the script does) is no home. [`keys_agree`], run over every script fixture, removes the cause:
//! the summary through the home equals the one worked out in the script itself.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_ids::ContentId;
use hir_def::{FunctionId, ModuleDefId, lang_item::lang_items, nameres::crate_def_map};
use hir_expand::{fixture, name::Name};
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    method_resolution::loaded_crates,
    next_solver::{DbInterner, Features, GenericArgs, ParamEnv},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    dispatch::Body,
    home::{is_generic, stands_for},
};
use intern::Symbol;
use rustc_type_ir::inherent::GenericArgs as _;

use super::fixture::{TOOLCHAIN, crate_named, db_with, lang_crate};
use crate::{
    Purity, Rule,
    identity::{content, content_in},
    main_of, script_purity,
    walk::{Def, Instance, Summary, def_crate, home_env, name_of, summary, summary_in},
};

/// `shared` is a dependency of three scripts. `script_b` implements `shared::Show` for a type
/// of its own whose `show` reads `HOME`; `script_c` solves with a recursion limit of its own.
const CRATES: &str = r#"
//- /lib.rs crate:shared deps:std
pub trait Show {
    fn show(&self);
}
impl Show for u8 {
    fn show(&self) {
        println!("cargo:rustc-cfg=byte");
    }
}
pub fn each<T: Show>(it: T) {
    it.show();
}
pub fn helper() {
    println!("cargo:rerun-if-changed=build.rs");
}
//- /a.rs crate:script_a deps:std,shared
fn main() {
    shared::helper();
    shared::each(1u8);
}
//- /b.rs crate:script_b deps:std,shared
struct Local;
impl shared::Show for Local {
    fn show(&self) {
        let _ = std::env::var("HOME");
    }
}
fn main() {
    shared::helper();
    shared::each(1u8);
    shared::each(Local);
}
//- /c.rs crate:script_c deps:std,shared
#![recursion_limit = "256"]
fn main() {
    shared::helper();
    shared::each(1u8);
}
//- /spec.rs crate:spec deps:std
#![feature(min_specialization)]
pub fn nothing() {}
//- /s.rs crate:script_s deps:std,shared,spec
fn main() {
    spec::nothing();
    shared::helper();
}
"#;

fn db() -> Db {
    let mut db = db_with(CRATES);
    blaze_item_hash::identity::set_build(&mut db, ContentId([7; 32]));
    db
}

/// The home of `instance` asked from `asker` (`hir_ty_mir::mir::home::home`).
fn home_of(db: &Db, asker: Crate, instance: &Instance) -> Option<Crate> {
    hir_ty_mir::mir::home::home(
        db,
        asker,
        Some(def_crate(db, instance.def)),
        instance.args.as_ref(),
    )
}

fn env(krate: Crate) -> StoredParamEnvAndCrate {
    ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store()
}

/// Every instance with a body the walk from the `main` of `script` reaches, each once.
fn reached(db: &Db, script: Crate) -> Vec<Instance> {
    reached_from(db, script, main_instance(db, script))
}

fn main_instance(db: &Db, script: Crate) -> Instance {
    let main = main_of(db, script).expect("the script has a `main`");
    Instance { def: Def::Body(Body::Fn(main)), args: GenericArgs::new_from_slice(&[]).store() }
}

/// Every instance with a body the walk from `root`, asked from `asker`, reaches, each once.
fn reached_from(db: &Db, asker: Crate, root: Instance) -> Vec<Instance> {
    let (env, mut seen, mut stack) = (env(asker), vec![root.clone()], vec![root]);
    while let Some(instance) = stack.pop() {
        for callee in summary(db, env.clone(), instance).callees.iter() {
            if matches!(callee.def, Def::Body(_)) && !seen.contains(callee) {
                seen.push(callee.clone());
                stack.push(callee.clone());
            }
        }
    }
    seen
}

/// The test of the cause: for every instance the walk from `script`'s `main` reaches, the
/// summary and identity part through its home equal those worked out in `script` itself. The
/// script fixtures (`scripts.rs`) run it too.
#[track_caller]
pub(super) fn keys_agree(db: &Db, script: Crate) {
    keys_agree_from(db, script, main_instance(db, script));
}

/// The same from any root asked from `asker`: `fn_purity`'s (`fns.rs`), a library crate's.
#[track_caller]
pub(super) fn keys_agree_from(db: &Db, asker: Crate, root: Instance) {
    let env = env(asker);
    for instance in reached_from(db, asker, root) {
        let at =
            || format!("{} (home {:?})", name_of(db, &instance), home_of(db, asker, &instance));
        let (through, own) = (
            summary(db, env.clone(), instance.clone()),
            summary_in(db, env.clone(), instance.clone()),
        );
        assert_eq!(through, own, "the summary of {} through its home is not the asker's own", at());
        let (through, own) = (
            content(db, env.clone(), instance.clone()),
            content_in(db, env.clone(), instance.clone()),
        );
        assert_eq!(
            through,
            own,
            "the identity part of {} through its home is not the asker's own",
            at()
        );
    }
}

/// The instance named `name` (as an impure script's reason names it, by its whole path:
/// `` `shared::helper` ``) among `instances`, the
/// one whose arguments name `ty` when given.
#[track_caller]
fn named<'a>(db: &Db, instances: &'a [Instance], name: &str, ty: Option<&str>) -> &'a Instance {
    instances
        .iter()
        .find(|it| {
            name_of(db, it) == name
                && ty.is_none_or(|ty| format!("{:?}", it.args.as_ref()).contains(ty))
        })
        .unwrap_or_else(|| panic!("no instance {name} ({ty:?}) is reached"))
}

/// The free function `name` at the root of `krate`.
#[track_caller]
fn fn_named(db: &Db, krate: Crate, name: &str) -> FunctionId {
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    match data.name_in_scope(db, &Name::new_symbol_root(Symbol::intern(name))).take_values() {
        Some(ModuleDefId::FunctionId(func)) => func,
        _ => panic!("no function `{name}` at the root of the crate"),
    }
}

#[test]
fn two_scripts_share_the_summaries_of_what_both_reach() {
    let db = db();
    let (a, b) = (crate_named(&db, "script_a"), crate_named(&db, "script_b"));
    blaze_query::db::attach(&db, || {
        let (in_a, in_b) = (reached(&db, a), reached(&db, b));
        let mut shared = 0;
        for instance in in_a.iter().filter(|it| in_b.contains(it)) {
            let one = summary(&db, env(a), instance.clone());
            let other = summary(&db, env(b), instance.clone());
            assert!(
                std::ptr::eq(one, other),
                "{} is summarized once for each script",
                name_of(&db, instance)
            );
            let one = content(&db, env(a), instance.clone());
            let other = content(&db, env(b), instance.clone());
            assert!(
                std::ptr::eq(one, other),
                "{} has an identity part per script",
                name_of(&db, instance)
            );
            shared += 1;
        }
        // `main` is each script's own; `helper`, `each::<u8>`, `<u8 as Show>::show` and the
        // toolchain's printing under them are both scripts'.
        assert!(shared >= 4, "only {shared} instances are reached by both scripts");
        let helper = named(&db, &in_a, "`shared::helper`", None);
        assert_eq!(home_of(&db, a, helper), Some(crate_named(&db, "shared")));
        let toolchain = in_a.iter().filter(|it| {
            home_of(&db, a, it)
                .is_some_and(|home| matches!(home.data(&db).origin, CrateOrigin::Lang(_)))
        });
        assert!(toolchain.count() > 0, "no toolchain function is keyed by a toolchain crate");
        for script in [a, b] {
            keys_agree(&db, script);
            let purity = script_purity(&db, main_of(&db, script).unwrap());
            assert_eq!(
                matches!(purity, Purity::Pure(_)),
                script == a,
                "`script_a` is pure and `script_b` reads `HOME`: {purity:?}"
            );
        }
    });
}

#[test]
fn a_downstream_impl_keys_the_instance_by_the_crate_of_its_type() {
    let db = db();
    let (a, b) = (crate_named(&db, "script_a"), crate_named(&db, "script_b"));
    let shared = crate_named(&db, "shared");
    blaze_query::db::attach(&db, || {
        let in_b = reached(&db, b);
        let byte = named(&db, &in_b, "`shared::each`", Some("u8"));
        let local = named(&db, &in_b, "`shared::each`", Some("Local"));
        // `script_b`'s impl of `Show` for `Local` is what `each::<Local>` runs: it is keyed by
        // `script_b`, the crate that sees the impl, and reads `HOME` there.
        assert_eq!(home_of(&db, b, local), Some(b));
        assert!(
            summary(&db, env(b), local.clone()).impure.is_some(),
            "`each::<Local>` reads `HOME`"
        );
        // `each::<u8>` names no crate of a script: one summary, `shared`'s, pure.
        assert_eq!(home_of(&db, b, byte), Some(shared));
        let from_b = summary(&db, env(b), byte.clone());
        assert!(from_b.impure.is_none(), "`each::<u8>` only prints");
        assert!(std::ptr::eq(from_b, summary(&db, env(a), byte.clone())));
    });
}

#[test]
fn a_script_that_solves_differently_keeps_its_own_summaries_of_generics() {
    let db = db();
    let (a, c) = (crate_named(&db, "script_a"), crate_named(&db, "script_c"));
    let shared = crate_named(&db, "shared");
    blaze_query::db::attach(&db, || {
        let in_c = reached(&db, c);
        // Not generic, `helper` is `shared`'s whoever asks, as the code generator compiles it
        // (`hir_ty_mir::mir::home::env_of`): one summary, the one of the body that runs.
        let helper = named(&db, &in_c, "`shared::helper`", None);
        assert_eq!(home_of(&db, c, helper), Some(shared));
        assert_eq!(home_env(&db, &env(c), helper), env(shared));
        let helper_a = named(&db, &reached(&db, a), "`shared::helper`", None).clone();
        assert!(std::ptr::eq(summary(&db, env(c), helper.clone()), summary(&db, env(a), helper_a)));
        // A recursion limit of its own can change what the solver answers in a generic: no
        // sharing of one, with `shared` or with `std`, the second candidate, which
        // `stands_for` checks the same. `each::<u8>` names only `shared`, so with `script_a`
        // it is `shared`'s; with `script_c` it stays `script_c`'s, a summary of its own.
        let byte = named(&db, &in_c, "`shared::each`", Some("u8"));
        assert!(is_generic(byte.args.as_ref()));
        assert!(
            !stands_for(&db, shared, c)
                && !stands_for(&db, lang_crate(&db, LangCrateOrigin::Std), c)
        );
        assert_eq!(home_of(&db, c, byte), None);
        assert_eq!(home_env(&db, &env(c), byte), env(c));
        let byte_a = named(&db, &reached(&db, a), "`shared::each`", Some("u8")).clone();
        assert_eq!(home_of(&db, a, &byte_a), Some(shared));
        assert!(!std::ptr::eq(summary(&db, env(c), byte.clone()), summary(&db, env(a), byte_a)));
        // Every other instance it reaches in the toolchain is not generic (the walk stops at the
        // modelled `io::_print`) and so is its own crate's; a generic one would be `script_c`'s.
        for instance in in_c
            .iter()
            .filter(|it| matches!(def_crate(&db, it.def).data(&db).origin, CrateOrigin::Lang(_)))
        {
            let want = (!is_generic(instance.args.as_ref())).then(|| def_crate(&db, instance.def));
            assert_eq!(home_of(&db, c, instance), want, "the home of {}", name_of(&db, instance));
        }
        keys_agree(&db, c);
    });
}

#[test]
fn a_specializing_dependency_of_the_script_alone_leaves_sharing() {
    let db = db();
    let (a, s) = (crate_named(&db, "script_a"), crate_named(&db, "script_s"));
    let shared = crate_named(&db, "shared");
    let std = lang_crate(&db, LangCrateOrigin::Std);
    blaze_query::db::attach(&db, || {
        let in_s = reached(&db, s);
        let helper = named(&db, &in_s, "`shared::helper`", None);
        // `spec` may specialize, but `shared` and `std` do not depend on it, so by the orphan
        // rule it holds no impl their walks could match: both still stand for `script_s`.
        assert!(stands_for(&db, shared, s));
        assert!(stands_for(&db, std, s));
        assert_eq!(home_of(&db, s, helper), Some(shared));
        let in_a = reached(&db, a);
        let helper_a = named(&db, &in_a, "`shared::helper`", None);
        assert!(std::ptr::eq(
            summary(&db, env(s), helper.clone()),
            summary(&db, env(a), helper_a.clone())
        ));
        keys_agree(&db, s);
    });
}

/// `k` depends on `dep`, but names it only through `m`'s macro in a body, which
/// `loaded_crates` misses: `dep`'s impl of `t::Tr` for `DType` is out of `k`'s sight, while
/// the script, which names `dep`, sees it. The call goes through `t::call::<DType>`, whose impl
/// is chosen where the instance is walked (`k`'s own type check of `run` is the same either
/// way). `run` is generic, so it is worked out in its home, which must not be `k`; a
/// non-generic `run` is `k`'s, as rustc compiles it, and gets the miss itself until
/// `loaded_crates` sees macro-only dependencies (#1618). Filled in two ways: `min_specialization` for `t` and `dep` with a `default` blanket
/// impl that `dep`'s specializes, or no feature and no blanket impl, `dep`'s impl alone.
const MISSED: &str = r#"
//- /t.rs crate:t deps:std
{feature}
pub trait Tr {
    fn go(&self);
}
pub fn call<T: Tr>(it: T) {
    it.go();
}
{blanket}
//- /m.rs crate:m deps:std
#[macro_export]
macro_rules! val {
    () => {
        ::dep::DType
    };
}
//- /dep.rs crate:dep deps:std,t
{feature}
pub struct DType;
impl t::Tr for DType {
    fn go(&self) {
        {dep_go}
    }
}
//- /k.rs crate:k deps:std,t,m,dep
pub fn run<U>() {
    t::call(m::val!());
}
//- /build.rs crate:build_script deps:std,k,dep
fn main() {
    let _ = dep::DType;
    k::run::<u8>();
}
"#;

/// [`MISSED`] filled in.
fn missed(feature: &str, blanket: &str, dep_go: &str) -> String {
    MISSED.replace("{feature}", feature).replace("{blanket}", blanket).replace("{dep_go}", dep_go)
}

/// The script of `crates` (a [`MISSED`]), with the instances it reaches checked: `k` is no home
/// of `run`, and the summaries through the home are the script's own. What the script is, the
/// direction a walk in the script goes.
fn a_missed_dependency_of_the_home(crates: &str) -> (Purity, bool) {
    let mut db = db_with(crates);
    blaze_item_hash::identity::set_build(&mut db, ContentId([7; 32]));
    let (k, dep, script) =
        (crate_named(&db, "k"), crate_named(&db, "dep"), crate_named(&db, "build_script"));
    blaze_query::db::attach(&db, || {
        assert!(
            !loaded_crates(&db, k).contains(&dep),
            "the fixture's premise: `k` names `dep` only through a macro"
        );
        assert!(loaded_crates(&db, script).contains(&dep));
        // The effect first: through its home or in the script, one summary.
        keys_agree(&db, script);
        // The cause: `dep` is a dependency of `k` the script loads and `k` does not.
        assert!(!stands_for(&db, k, script));
        let in_script = reached(&db, script);
        let run = named(&db, &in_script, "`k::run`", None);
        assert_eq!(home_of(&db, script, run), None);
        let reads_env = summary(&db, env(script), main_instance(&db, script)).kinds.has(Rule::Env);
        (script_purity(&db, main_of(&db, script).unwrap()).clone(), reads_env) // clone: a test's answer
    })
}

#[test]
fn a_dependency_the_home_does_not_load_with_a_specializing_impl_keeps_the_scripts_summary() {
    // In `k` the blanket impl's empty `go` runs; in the script, `dep`'s, which reads `HOME`:
    // keyed by `k`, the script would be pure.
    let (purity, reads_env) = a_missed_dependency_of_the_home(&missed(
        "#![feature(min_specialization)]",
        "impl<T> Tr for T {\n    default fn go(&self) {}\n}",
        r#"let _ = std::env::var("HOME");"#,
    ));
    assert!(
        matches!(purity, Purity::Impure(_)) && reads_env,
        "`dep`'s `go` reads `HOME`: {purity:?}"
    );
}

#[test]
fn a_dependency_the_home_does_not_load_with_a_plain_impl_keeps_the_scripts_summary() {
    // In `k` the call has no impl, which the walk refuses; in the script it is `dep`'s, pure.
    // No crate may specialize, so this is the case a check of specializing crates alone misses.
    let (purity, _) = a_missed_dependency_of_the_home(&missed("", "", ""));
    assert!(matches!(purity, Purity::Pure(_)), "`dep`'s `go` does nothing: {purity:?}");
}

#[test]
fn a_dependency_of_a_dependency_the_home_does_not_load_keeps_the_scripts_summary() {
    // `k` → `x` → `dep`, and `x` names `dep` only through `m`'s macro: `dep` is in `k`'s
    // dependency closure though not among its direct dependencies, and `k` is no home either.
    // `go_x` is generic too: a non-generic one is `x`'s, which misses `dep` (#1618).
    let crates = missed("", "", "").replacen(
        "//- /k.rs crate:k deps:std,t,m,dep\npub fn run<U>() {\n    t::call(m::val!());\n}",
        "//- /x.rs crate:x deps:std,t,m,dep\npub fn go_x<U>() {\n    t::call(m::val!());\n}\n//- /k.rs crate:k deps:std,x\npub fn run<U>() {\n    x::go_x::<U>();\n}",
        1,
    );
    assert!(crates.contains("crate:x"), "the fixture's `k` is replaced");
    let (purity, _) = a_missed_dependency_of_the_home(&crates);
    assert!(matches!(purity, Purity::Pure(_)), "`dep`'s `go` does nothing: {purity:?}");
}

#[test]
fn an_open_instance_or_a_generic_environment_has_no_home_and_a_non_generic_one_its_own() {
    let db = db();
    let (a, shared) = (crate_named(&db, "script_a"), crate_named(&db, "shared"));
    blaze_query::db::attach(&db, || {
        let each = fn_named(&db, shared, "each");
        let interner = DbInterner::new_with(&db, shared);
        let params = GenericArgs::identity_for_item(interner, each.into()).store();
        let open = Instance { def: Def::Body(Body::Fn(each)), args: params };
        assert_eq!(home_of(&db, a, &open), None, "`each::<T>` holds a parameter");
        let generic =
            ParamEnvAndCrate { param_env: db.trait_environment(each.into()), krate: a }.store();
        let closed = Instance {
            def: Def::Body(Body::Fn(fn_named(&db, shared, "helper"))),
            args: GenericArgs::new_from_slice(&[]).store(),
        };
        let byte = named(&db, &reached(&db, a), "`shared::each`", Some("u8")).clone();
        assert_eq!(
            home_env(&db, &generic, &byte),
            generic,
            "a generic environment is kept for a generic"
        );
        assert_eq!(home_env(&db, &env(a), &byte), env(shared));
        // Not generic, `helper` is compiled once in `shared`, whatever the caller's clauses.
        assert_eq!(
            home_env(&db, &generic, &closed),
            env(shared),
            "a non-generic instance is its own crate's"
        );
        assert_eq!(home_env(&db, &env(a), &closed), env(shared));
    });
}

#[test]
fn solve_alike_compares_the_recursion_limit_and_the_features_the_solver_reads() {
    let db = db_with(
        r#"
//- /plain.rs crate:plain deps:std
pub fn f() {}
//- /featured.rs crate:featured deps:std
#![feature(never_type)]
pub fn f() {}
//- /featured_too.rs crate:featured_too deps:std
#![feature(never_type)]
pub fn f() {}
//- /other.rs crate:other deps:std
#![feature(never_type, coroutine_clone)]
pub fn f() {}
//- /limited.rs crate:limited deps:std
#![recursion_limit = "256"]
pub fn f() {}
"#,
    );
    let at = |name| crate_named(&db, name);
    let (core, std) =
        (lang_crate(&db, LangCrateOrigin::Core), lang_crate(&db, LangCrateOrigin::Std));
    let alike = |a, b| Features::solve_alike(&db, a, b);
    // `core` is `staged_api`: no feature bound holds there, as none does in a featureless crate.
    assert!(alike(at("plain"), core));
    assert!(alike(at("plain"), std));
    assert!(!alike(at("featured"), std), "a feature bound may hold in `featured` alone");
    assert!(alike(at("featured"), at("featured_too")));
    assert!(!alike(at("featured"), at("other")));
    assert!(!alike(at("plain"), at("limited")), "the recursion limits differ");
}

/// A `Box` for the stand-in `alloc`, the `owned_box` lang item (which takes `alloc` the
/// `lang_items` feature, and `staged_api` as the real one has).
const BOXED: &str = r#"
pub mod boxed {
    #[lang = "owned_box"]
    pub struct Box<T>(*mut T);

    impl<T> Drop for Box<T> {
        fn drop(&mut self) {
            unsafe { crate::alloc::dealloc(self.0 as *mut u8, 0, 1) }
        }
    }
}

pub mod vec {"#;

#[test]
fn a_no_std_crate_without_alloc_is_no_home_of_a_box() {
    // `h` is `#![no_std]` without `extern crate alloc`: its lang items have no `owned_box`,
    // though it loads `alloc` through `std`. Worked out in `h`, the drop glue of `Box<Noisy>`
    // would not reach `Noisy`'s `drop`, which calls a foreign function.
    let toolchain = TOOLCHAIN.replacen("pub mod vec {", BOXED, 1).replacen(
        "//- /alloc.rs crate:alloc\n",
        "//- /alloc.rs crate:alloc\n#![feature(lang_items, staged_api)]\n",
        1,
    );
    let crates = r#"
//- /h.rs crate:h deps:std
#![no_std]
unsafe extern "C" {
    fn close(fd: i32) -> i32;
}
pub struct Noisy;
impl Drop for Noisy {
    fn drop(&mut self) {
        unsafe {
            close(1);
        }
    }
}
pub fn consume<T>(_: T) {}
//- /build.rs crate:build_script deps:std,alloc,h
fn make() -> alloc::boxed::Box<h::Noisy> {
    loop {}
}
fn main() {
    h::consume(make());
}
"#;
    let mut db = fixture::db(&format!("{toolchain}{crates}")).into_db();
    blaze_item_hash::identity::set_build(&mut db, ContentId([7; 32]));
    let (h, script) = (crate_named(&db, "h"), crate_named(&db, "build_script"));
    let alloc = lang_crate(&db, LangCrateOrigin::Alloc);
    blaze_query::db::attach(&db, || {
        assert!(lang_items(&db, alloc).OwnedBox.is_some(), "the fixture's `Box` is `owned_box`");
        assert!(lang_items(&db, h).OwnedBox.is_none(), "`h` sees no `owned_box`");
        assert!(lang_items(&db, script).OwnedBox.is_some());
        // The effect first: through its home or in the script, one summary, and the script is
        // impure, as dropping the box drops `Noisy`.
        keys_agree(&db, script);
        let purity = script_purity(&db, main_of(&db, script).unwrap());
        assert!(matches!(purity, Purity::Impure(_)), "dropping the box drops `Noisy`: {purity:?}");
        // The cause: `h`'s lang items are not the script's, so `h` is no home.
        assert!(!stands_for(&db, h, script), "their lang items differ");
        let in_script = reached(&db, script);
        let consume = named(&db, &in_script, "`h::consume`", None);
        assert_ne!(home_of(&db, script, consume), Some(h));
        // A `core` home has no `owned_box` either: what names only `core` is worked out in
        // `std`, which loads it and sees the script's lang items.
        let core = lang_crate(&db, LangCrateOrigin::Core);
        assert!(!stands_for(&db, core, script));
        let std = lang_crate(&db, LangCrateOrigin::Std);
        assert!(stands_for(&db, std, script));
    });
}

/// What a summary says, with each instance and function by name: comparable across two
/// databases, whose interned ids differ.
fn said(db: &Db, it: &Summary) -> String {
    let names = |instances: &mut dyn Iterator<Item = &Instance>| {
        instances.map(|it| name_of(db, it)).collect::<Vec<_>>()
    };
    let sites: Vec<_> =
        it.sites.iter().map(|site| (name_of(db, &site.instance), site.span, site.given)).collect();
    format!(
        "{:?} {:?} {} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {}",
        it.effects,
        it.env,
        it.deferred.len(),
        it.ret,
        names(&mut it.callees.iter()),
        it.impure,
        it.kinds,
        it.sets,
        it.witnesses,
        sites,
        it.memory,
        it.trusted_calls.len(),
    )
}

#[test]
fn the_order_scripts_are_asked_in_changes_no_summary() {
    // `ping` and `pong` call each other, `ping` alone reads a variable, so `pong`'s impurity
    // goes through `ping`, and the two scripts enter the cycle at opposite ends. Shared,
    // whichever is asked first heads the cycle, and with `pong` at its head the first pass sees
    // `ping`'s initial value: every summary (its witnesses and the path an impurity goes
    // through too) still comes out the same (rule 25).
    const CYCLE: &str = r#"
//- /lib.rs crate:lib deps:std
pub fn ping(n: u32) {
    if n > 0 {
        pong(n - 1)
    } else {
        let _ = std::env::var("PING");
    }
}
pub fn pong(n: u32) {
    if n > 0 {
        ping(n - 1)
    }
}
//- /x.rs crate:script_x deps:std,lib
fn main() {
    lib::ping(3);
}
//- /y.rs crate:script_y deps:std,lib
fn main() {
    lib::pong(3);
}
"#;
    let walk = |first: &str, second: &str| {
        let mut db = db_with(CYCLE);
        blaze_item_hash::identity::set_build(&mut db, ContentId([7; 32]));
        blaze_query::db::attach(&db, || {
            let lib = crate_named(&db, "lib");
            let mut out = Vec::new();
            for script in [first, second] {
                let script = crate_named(&db, script);
                for it in reached(&db, script)
                    .iter()
                    .filter(|it| ["`lib::ping`", "`lib::pong`"].contains(&&*name_of(&db, it)))
                {
                    assert_eq!(home_of(&db, script, it), Some(lib), "the cycle is shared");
                }
                let purity = script_purity(&db, main_of(&db, script).unwrap()).clone();
                let mut summaries: Vec<_> = reached(&db, script)
                    .iter()
                    .map(|it| (name_of(&db, it), said(&db, summary(&db, env(script), it.clone()))))
                    .collect();
                summaries.sort();
                out.push((format!("{purity:?}"), summaries));
            }
            out
        })
    };
    let (xy, mut yx) = (walk("script_x", "script_y"), walk("script_y", "script_x"));
    yx.reverse();
    let names: Vec<&str> = xy[0].1.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"`lib::ping`") && names.contains(&"`lib::pong`"),
        "both scripts reach the cycle: {names:?}"
    );
    assert!(
        xy[1].0.contains(r#""`lib::pong`", "`lib::ping`""#),
        "`script_y`'s impurity goes `pong` → `ping`: {}",
        xy[1].0
    );
    assert_eq!(xy, yx);
}
