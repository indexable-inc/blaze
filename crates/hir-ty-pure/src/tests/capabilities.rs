//! The crate-wide effects of `#![blaze::capabilities]` (docs/v2/plan/design/std.md) over the
//! stand-in toolchain: per effect, a refused crate with its call path and one that passes, the
//! trusted list of `[workspace.metadata.blaze.trusted]`, and what `blaze explain trusted` reads.

use base_db::{Crate, CrateOrigin, CrateWorkspaceData, Db, TrustedCrate, all_crates};

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
};
use crate::{
    Refusal, Rule, check_generic,
    effect_attrs::{AMBIENT, GLOBAL_ALLOC, UNSAFE, capabilities_asked, trusted_reached},
    effects::EffectDef,
};

/// `raw` writes through a pointer in an `unsafe` block; `app`, which asks for capabilities,
/// calls it.
const UNSAFE_DEP: &str = r#"
//- /raw.rs crate:raw deps:std
pub fn poke(at: &mut u8) {
    let ptr = at as *mut u8;
    unsafe { *ptr = 1 }
}
//- /app.rs crate:app deps:std,raw
#![blaze::capabilities]
pub fn run(at: &mut u8) {
    raw::poke(at)
}
"#;

/// Every crate of the workspace (the toolchain's are no workspace's) lists `names` as trusted.
fn trust(db: &mut Db, names: &[&str]) {
    let listed: Box<[TrustedCrate]> = names
        .iter()
        .map(|name| TrustedCrate {
            name: (*name).into(),
            owner: "io".into(),
            reason: "checked by hand".into(),
        })
        .collect();
    let crates: Vec<Crate> = all_crates(&*db)
        .iter()
        .copied()
        .filter(|krate| !matches!(krate.data(&*db).origin, CrateOrigin::Lang(_)))
        .collect();
    for krate in crates {
        let mut data = CrateWorkspaceData::clone(krate.workspace_data(&*db));
        data.trusted = listed.clone();
        krate.set_workspace_data(db, triomphe::Arc::new(data));
    }
}

#[track_caller]
fn refused(db: &Db, krate: &str, name: &str, effect: &EffectDef) -> Refusal {
    let krate = crate_named(db, krate);
    let func = fn_named(db, krate, name);
    blaze_query::db::attach(db, || match check_generic(db, func, effect) {
        Err(refusal) => refusal,
        Ok(proof) => panic!("`{name}` should be refused `{}`, but has it: {proof:?}", effect.name),
    })
}

#[track_caller]
fn passes(db: &Db, krate: &str, name: &str, effect: &EffectDef) {
    let krate = crate_named(db, krate);
    let func = fn_named(db, krate, name);
    blaze_query::db::attach(db, || {
        if let Err(refusal) = check_generic(db, func, effect) {
            panic!("`{name}` should pass `{}`, but {}", effect.name, refusal.said());
        }
    });
}

/// The step names of `refusal`'s chain end in `want`, in order.
#[track_caller]
fn chain_is(refusal: &Refusal, want: &[&str]) {
    let names: Vec<&str> = refusal.chain.iter().map(|it| &*it.name).collect();
    assert_eq!(names.len(), want.len(), "{}: {}", want.join(" → "), refusal.said());
    for (name, want) in names.iter().zip(want) {
        assert!(name.ends_with(want), "step `{name}` should be `{want}`: {}", refusal.said());
    }
}

/// A read of a file is ambient IO, at the call of `std::fs::read`, named with its path.
#[test]
fn ambient_refuses_a_file_read_with_its_path() {
    let db = db_with(
        r#"
//- /app.rs crate:app deps:std
#![blaze::capabilities]
fn load() -> bool {
    std::fs::read("config").is_ok()
}
pub fn start() -> bool {
    load()
}
"#,
    );
    let refusal = refused(&db, "app", "start", &AMBIENT);
    assert_eq!(refusal.rule, Rule::Io, "{}", refusal.said());
    chain_is(&refusal, &["start", "load"]);
    assert!(refusal.what.contains("std::fs::read"), "{}", refusal.said());
    let said = refusal.said();
    assert!(
        said.contains(
            "is in a `#![blaze::capabilities]` crate, which refuses `ambient`, but does input or \
             output through ambient authority"
        ),
        "{said}"
    );
    blaze_query::db::attach(&db, || {
        assert!(capabilities_asked(&db, crate_named(&db, "app")), "`app` asks for capabilities");
    });
}

/// Code that computes on what it is handed reaches nothing ambient.
#[test]
fn ambient_passes_what_is_handed_in() {
    let db = db_with(
        r#"
//- /app.rs crate:app deps:std
#![blaze::capabilities]
pub fn sum(values: &[u32; 3]) -> u32 {
    values[0] ^ values[1] ^ values[2]
}
"#,
    );
    passes(&db, "app", "sum", &AMBIENT);
    passes(&db, "app", "sum", &UNSAFE);
    passes(&db, "app", "sum", &GLOBAL_ALLOC);
}

/// An `unsafe` block of a crate nobody trusts is refused, named with the call path to it.
#[test]
fn unsafe_refuses_an_untrusted_crates_block_with_its_path() {
    let db = db_with(UNSAFE_DEP);
    let refusal = refused(&db, "app", "run", &UNSAFE);
    assert_eq!(refusal.rule, Rule::Unsafe, "{}", refusal.said());
    chain_is(&refusal, &["run", "poke"]);
    assert_eq!(&*refusal.what, "runs an `unsafe` block", "{}", refusal.said());
    assert!(
        refusal.chain.iter().all(|it| it.at.is_some()),
        "each step says where: {:?}",
        refusal.chain
    );
}

/// The same block in a crate the workspace trusts passes, and `blaze explain trusted` names it.
#[test]
fn unsafe_passes_in_a_trusted_crate() {
    let mut db = db_with(UNSAFE_DEP);
    trust(&mut db, &["raw"]);
    passes(&db, "app", "run", &UNSAFE);
    let app = crate_named(&db, "app");
    blaze_query::db::attach(&db, || {
        let reached: Vec<(&str, &str, &str)> = trusted_reached(&db, app)
            .into_iter()
            .map(|it| (&*it.name, &*it.owner, &*it.reason))
            .collect();
        assert_eq!(reached, [("raw", "io", "checked by hand")]);
    });
}

/// The `unsafe` block `format_args!` lowers to is no `unsafe` written in the crate.
#[test]
fn unsafe_passes_a_print() {
    let db = db_with(
        r#"
//- /app.rs crate:app deps:std
#![blaze::capabilities]
pub fn hello() {
    println!("hello");
}
"#,
    );
    passes(&db, "app", "hello", &UNSAFE);
}

/// An `unsafe fn`'s body is `unsafe` code, though no block is written in it.
#[test]
fn unsafe_refuses_an_unsafe_fn() {
    let db = db_with(
        r#"
//- /app.rs crate:app deps:std
#![blaze::capabilities]
pub unsafe fn read(at: *const u8) -> u8 {
    let _ = at;
    0
}
"#,
    );
    let refusal = refused(&db, "app", "read", &UNSAFE);
    assert!(refusal.what.contains("an `unsafe fn`"), "{}", refusal.said());
}

/// `Vec::new` allocates from the global heap through alloc's own code: alloc is trusted for
/// `unsafe` and `ambient`, never for `global_alloc`.
#[test]
fn global_alloc_refuses_a_vec_of_the_global_heap() {
    let db = db_with(
        r#"
//- /app.rs crate:app deps:std
#![blaze::capabilities]
pub fn make() -> usize {
    let made: Vec<u32> = Vec::new();
    let _ = made;
    0
}
"#,
    );
    let refusal = refused(&db, "app", "make", &GLOBAL_ALLOC);
    assert_eq!(refusal.rule, Rule::Alloc, "{}", refusal.said());
    assert!(
        refusal.chain.first().is_some_and(|it| it.name.ends_with("make")),
        "{}",
        refusal.said()
    );
    assert!(refusal.said().contains("allocates from the global heap"), "{}", refusal.said());
    // Neither of the others: alloc's `unsafe` and its allocator shim are its own.
    passes(&db, "app", "make", &UNSAFE);
    passes(&db, "app", "make", &AMBIENT);
}

/// A crate the workspace trusts may allocate: its `Vec::new` is its own.
#[test]
fn global_alloc_passes_through_a_trusted_crate() {
    let mut db = db_with(
        r#"
//- /arena.rs crate:arena deps:std
pub fn scratch() -> usize {
    let made: Vec<u32> = Vec::new();
    let _ = made;
    0
}
//- /app.rs crate:app deps:std,arena
#![blaze::capabilities]
pub fn make() -> usize {
    arena::scratch()
}
"#,
    );
    let _ = refused(&db, "app", "make", &GLOBAL_ALLOC);
    trust(&mut db, &["arena"]);
    passes(&db, "app", "make", &GLOBAL_ALLOC);
}
