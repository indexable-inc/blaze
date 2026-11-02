//! The pick cache against the probe it stands for (notes: docs/v2/plan/lanes/frontend/fast-method.md,
//! section 5): every hit, in generated bodies and in the method resolution fixtures, is probed
//! again without the cache and must give the same pick; a hit allocates nothing; the table keeps
//! one value per key and state under contention; and the scope buckets equal the assembly they
//! replace.

use std::{cell::Cell, fmt::Write as _};

use base_db::{Db, all_crates};
use blaze_alloc_count::measure;
use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, ModuleId, nameres::crate_def_map,
    resolver::HasResolver,
};
use hir_expand::name::Name;
use hir_expand::fixture;

use super::{
    Answer,
    table::{Key, Table},
};
use crate::{
    InferenceResult,
    method_resolution::{
        MethodError, TraitValueItems,
        probe::{Pick, PickResult},
        scope_sets::{ScopeItem, module_items_named},
    },
};

thread_local! {
    /// Whether a hit on this thread is checked against a fresh probe.
    static VERIFY: Cell<bool> = const { Cell::new(false) };
    /// The hits checked on this thread.
    static HITS: Cell<u64> = const { Cell::new(0) };
}

/// Called on every hit: under [`verified`], probes again without the cache, and measures what
/// handing out the answer allocates.
pub(super) fn on_hit<'db>(
    answer: &Answer,
    result: &PickResult<'db>,
    probe: impl FnOnce() -> PickResult<'db>,
) {
    if !VERIFY.get() {
        return;
    }
    HITS.set(HITS.get() + 1);
    let copies_without_allocating = match answer {
        Answer::Found(pick) | Answer::PrivateMatch(pick) => {
            pick.shadowed_candidates.is_empty() && pick.unstable_candidates.is_empty()
        }
        Answer::NoMatch => true,
        Answer::Ambiguity(_) => false,
    };
    if copies_without_allocating {
        let (_, used) = measure(|| answer.to_result());
        assert_eq!(used.allocations, 0, "a hit on {result:?} allocated {} bytes", used.bytes);
    }
    let fresh = probe();
    assert!(same(result, &fresh), "the cache gave {result:?}, the probe {fresh:?}");
}

/// Runs `f` with every hit on this thread checked; returns the hits.
fn verified(f: impl FnOnce()) -> u64 {
    let was = VERIFY.replace(true);
    let before = HITS.get();
    f();
    VERIFY.set(was);
    HITS.get() - before
}

fn same<'db>(a: &PickResult<'db>, b: &PickResult<'db>) -> bool {
    match (a, b) {
        (Ok(a), Ok(b)) => same_pick(a, b),
        (Err(MethodError::NoMatch), Err(MethodError::NoMatch)) => true,
        (Err(MethodError::Ambiguity(a)), Err(MethodError::Ambiguity(b))) => a == b,
        (Err(MethodError::PrivateMatch(a)), Err(MethodError::PrivateMatch(b))) => same_pick(a, b),
        _ => false,
    }
}

fn same_pick<'db>(a: &Pick<'db>, b: &Pick<'db>) -> bool {
    a.item == b.item
        && a.kind == b.kind
        && a.autoderefs == b.autoderefs
        && a.autoref_or_ptr_adjustment == b.autoref_or_ptr_adjustment
        && a.self_ty == b.self_ty
        && a.receiver_steps == b.receiver_steps
        && a.shadowed_candidates == b.shadowed_candidates
        && a.unstable_candidates == b.unstable_candidates
}

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

/// Every module of the fixture's own crates (not minicore's).
fn modules(db: &Db) -> Vec<ModuleId> {
    let mut modules = Vec::new();
    for &krate in all_crates(db).iter() {
        if krate.data(db).origin.is_local() {
            modules.extend(crate_def_map(db, krate).modules(db).map(|(id, _)| id));
        }
    }
    modules
}

/// Every body of the fixture's own crates: functions, consts and statics in modules and impls.
fn bodies(db: &Db) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    for &krate in all_crates(db).iter() {
        if !krate.data(db).origin.is_local() {
            continue;
        }
        for (_, module) in crate_def_map(db, krate).modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    _ => {}
                }
            }
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    match item {
                        AssocItemId::FunctionId(it) => bodies.push(it.into()),
                        AssocItemId::ConstId(it) => bodies.push(it.into()),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
    }
    bodies
}

/// Infers every body of `fixture` with each hit checked; returns the hits.
fn infer_verified(fixture: &str) -> u64 {
    let db = db_of(fixture);
    let bodies = bodies(&db);
    verified(|| {
        for &body in &bodies {
            let _ = InferenceResult::of(&db, body);
        }
    })
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const TYPES: usize = 6;
const TRAITS: usize = 5;
const METHODS: usize = 4;

/// A crate of `TYPES` structs, `TRAITS` traits of `METHODS` methods each (by reference, by
/// value, by `&mut`), random impls of them, random inherent methods of the same names (some
/// private to a module), a trait in scope only in one module, a `Deref` wrapper, a generic
/// struct, and bodies calling each name on each type through values, references, double
/// references, the wrapper, the generic struct and paths, so most calls repeat a
/// (receiver, name, scope) of another body: hits of every pick kind, ambiguities, private
/// matches and misses.
fn generated(seed: u64) -> String {
    let mut rng = XorShift(seed);
    let mut text = String::from(
        "//- minicore: sized, copy, clone, deref, option\n//- /main.rs crate:main deps:core\n",
    );
    for ty in 0..TYPES {
        writeln!(text, "pub struct S{ty}(pub u32);").unwrap();
    }
    for tr in 0..TRAITS {
        writeln!(text, "pub trait T{tr} {{").unwrap();
        for m in 0..METHODS {
            let receiver = ["&self", "self", "&mut self"][(tr + m) % 3];
            let bound = if receiver == "self" { " where Self: Sized" } else { "" };
            writeln!(text, "    fn m{m}({receiver}) -> u32{bound} {{ {tr} }}").unwrap();
        }
        writeln!(text, "}}").unwrap();
    }
    for ty in 0..TYPES {
        for tr in 0..TRAITS {
            if rng.below(3) == 0 {
                writeln!(text, "impl T{tr} for S{ty} {{}}").unwrap();
            }
        }
        if rng.below(2) == 0 {
            let m = rng.below(METHODS);
            writeln!(text, "impl S{ty} {{ pub fn m{m}(&self) -> u32 {{ 7 }} }}").unwrap();
        }
    }
    writeln!(text, "mod hidden {{").unwrap();
    writeln!(text, "    impl super::S0 {{ fn secret(&self) -> u32 {{ 8 }} }}").unwrap();
    writeln!(text, "    pub trait Far {{ fn m0(&self) -> u32 {{ 9 }} }}").unwrap();
    writeln!(text, "    impl Far for super::S1 {{}}").unwrap();
    writeln!(text, "    fn inside(s: &super::S0, t: &super::S1) -> u32 {{ s.secret() + t.m0() }}").unwrap();
    writeln!(text, "}}").unwrap();
    writeln!(text, "pub struct W(pub S0);").unwrap();
    writeln!(
        text,
        "impl core::ops::Deref for W {{ type Target = S0; fn deref(&self) -> &S0 {{ &self.0 }} }}"
    )
    .unwrap();
    writeln!(text, "pub struct G<T>(pub T);").unwrap();
    writeln!(text, "impl<T> T0 for G<T> {{}}").unwrap();
    for body in 0..24 {
        writeln!(text, "fn b{body}() {{").unwrap();
        for _ in 0..6 {
            let ty = rng.below(TYPES);
            let m = rng.below(METHODS);
            match rng.below(7) {
                0 => writeln!(text, "    let mut s = S{ty}(1); let _ = s.m{m}();").unwrap(),
                1 => writeln!(text, "    let s = S{ty}(1); let r = &s; let _ = r.m{m}();").unwrap(),
                2 => writeln!(text, "    let s = S{ty}(1); let r = &&s; let _ = r.m{m}();").unwrap(),
                3 => writeln!(text, "    let _ = S{ty}::m{m};").unwrap(),
                4 => writeln!(text, "    let w = W(S0(1)); let _ = w.m{m}();").unwrap(),
                5 => writeln!(text, "    let g = G(S{ty}(1)); let _ = g.m{m}();").unwrap(),
                _ => writeln!(text, "    let s = S0(1); let _ = s.secret();").unwrap(),
            }
        }
        writeln!(text, "}}").unwrap();
    }
    text
}

#[test]
fn generated_bodies_pick_as_the_probe_does() {
    let mut hits = 0;
    for seed in [0x9E37_79B9_7F4A_7C15, 0x2545_F491_4F6C_DD1D, 0xD1B5_4A32_D192_ED03] {
        hits += infer_verified(&generated(seed));
    }
    assert!(hits > 0, "the generated bodies repeat their probes");
}

/// The fixtures of a test file: its raw strings but expectations (`expect![[r#"..."#]]`) and
/// fixtures that need proc macros.
fn fixtures(text: &'static str) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("r#\"") {
        let before = &rest[..start];
        let after = &rest[start + 3..];
        let Some(end) = after.find("\"#") else { break };
        let fixture = &after[..end];
        rest = &after[end + 2..];
        if before.trim_end().ends_with("expect![[") || fixture.contains("proc_macros:") {
            continue;
        }
        found.push(fixture);
    }
    found
}

#[test]
fn method_resolution_fixtures_pick_as_the_probe_does() {
    let mut hits = 0;
    let tests = [
        include_str!("../../../../hir-ty-mir/src/tests/method_resolution.rs"),
        include_str!("../../../../hir-ty-mir/src/tests/traits.rs"),
    ];
    for text in tests {
        let fixtures = fixtures(text);
        assert!(!fixtures.is_empty(), "the test file has fixtures");
        for fixture in fixtures {
            hits += infer_verified(fixture);
        }
    }
    assert!(hits > 0, "the fixtures repeat their probes");
}

#[test]
fn scope_buckets_are_the_traits_in_scope_named() {
    let db = db_of(&generated(0x9E37_79B9_7F4A_7C15));
    let names: Vec<Name> = (0..METHODS)
        .map(|m| Name::new_root(&format!("m{m}")))
        .chain(["secret", "absent"].map(Name::new_root))
        .collect();
    for module in modules(&db) {
        for &name in &names {
            // The assembly the bucket replaces: every trait in scope, asked of by name.
            let mut old: Vec<ScopeItem> =
                blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                    let mut values = TraitValueItems::new_in(frame);
                    module
                        .resolver(&db)
                        .traits_in_scope(&db)
                        .into_iter()
                        .filter_map(|trait_| {
                            values.get(&db, trait_, name).map(|item| ScopeItem { trait_, item })
                        })
                        .collect()
                });
            old.sort_by_key(|it| it.trait_);
            let new = module_items_named(&db, module, name).items();
            assert_eq!(new, old.as_slice(), "{module:?}, {}", name.as_str());
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TestKey(u64);

impl Key for TestKey {
    fn hint(&self) -> u64 {
        self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
    }
}

#[test]
fn a_table_hit_allocates_nothing() {
    let table: Table<TestKey, u32, u64> = Table::new(4);
    let guard = blaze_reclaim::enter();
    table.insert(&guard, TestKey(3), 1, 30);
    let (found, used) = measure(|| table.get(&guard, &TestKey(3), 1).copied());
    assert_eq!(found, Some(30));
    assert_eq!(used.allocations, 0, "a hit allocated {} bytes", used.bytes);
    assert_eq!(table.get(&guard, &TestKey(3), 2), None, "another state misses");
    assert_eq!(table.get(&guard, &TestKey(4), 1), None, "another key misses");
}

#[test]
fn a_newer_state_replaces_the_entry() {
    let table: Table<TestKey, u32, u64> = Table::new(4);
    let guard = blaze_reclaim::enter();
    table.insert(&guard, TestKey(3), 1, 30);
    table.insert(&guard, TestKey(3), 2, 31);
    assert_eq!(table.get(&guard, &TestKey(3), 1), None);
    assert_eq!(table.get(&guard, &TestKey(3), 2), Some(&31));
}

#[test]
fn a_full_window_keeps_the_newest_key() {
    let table: Table<TestKey, u32, u64> = Table::new(4);
    let guard = blaze_reclaim::enter();
    for key in 0..1000 {
        table.insert(&guard, TestKey(key), 1, key);
        assert_eq!(table.get(&guard, &TestKey(key), 1), Some(&key));
    }
}

#[test]
fn racing_inserts_keep_one_value_per_key_and_state() {
    // Far more slots than keys, so no key is evicted and each keeps its first value.
    let table: Table<TestKey, u32, u64> = Table::new(10);
    let seen: Vec<Vec<u64>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8u64)
            .map(|thread| {
                let table = &table;
                scope.spawn(move || {
                    let guard = blaze_reclaim::enter();
                    (0..32u64)
                        .map(|key| *table.insert(&guard, TestKey(key), 1, thread))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers.into_iter().map(|it| it.join().expect("a worker finished")).collect()
    });
    let guard = blaze_reclaim::enter();
    for key in 0..32u64 {
        let kept = *table.get(&guard, &TestKey(key), 1).expect("a stored key is found");
        for thread in &seen {
            assert_eq!(thread[key as usize], kept, "every thread got the one value of {key}");
        }
    }
}
