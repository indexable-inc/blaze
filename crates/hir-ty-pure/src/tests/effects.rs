//! `#[blaze::noalloc]` and `#[blaze::nonblocking]` over the stand-in toolchain
//! (docs/v2/plan/design/effects.md).

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
};
use crate::{
    PURE, Rule, check_generic,
    effect_attrs::{NOALLOC, NONBLOCKING, NOPANIC},
};

/// The pinned core's `unsafe` precondition checks (`unchecked_add`'s, under every
/// `Range<usize>` loop) build their message with `fmt::Arguments::from_str`, which transmutes a
/// `usize` into the `NonNull<rt::Argument>` of arguments holding function pointers. A pointer
/// made of a number has no provenance, so no code is reached through it: the check passes. A
/// function pointer made of a number is still refused.
#[test]
fn a_pointer_made_of_a_number_reaches_no_code() {
    let db = db_with(
        r#"
//- /lib.rs crate:kernel deps:std
pub struct Argument {
    formatter: fn(&u8) -> bool,
}

/// `core::fmt::Arguments` at the pin: a message's length packed into the pointer to its
/// arguments.
pub struct Message {
    args: *const Argument,
}

fn from_len(len: usize) -> Message {
    Message { args: unsafe { core::mem::transmute(len) } }
}

/// `unchecked_add`'s precondition check: the message, then an abort.
fn precondition_check(a: usize, b: usize) {
    if a >= b {
        let _message = from_len(41);
        core::panicking::panic("unsafe precondition(s) violated: unchecked_add");
    }
}

#[blaze::noalloc]
#[blaze::nonblocking]
pub fn count(n: usize) -> usize {
    let mut i = 0;
    while i < n {
        precondition_check(i, n);
        i += 1;
    }
    i
}

#[blaze::noalloc]
#[blaze::nonblocking]
pub fn jump(at: usize) -> fn() {
    unsafe { core::mem::transmute(at) }
}
"#,
    );
    let krate = crate_named(&db, "kernel");
    let (count, jump) = (fn_named(&db, krate, "count"), fn_named(&db, krate, "jump"));
    blaze_query::db::attach(&db, || {
        for effect in [&NOALLOC, &NONBLOCKING] {
            if let Err(refusal) = check_generic(&db, count, effect) {
                panic!("`count` should be {}, but {}", effect.name, refusal.said());
            }
            let Err(refusal) = check_generic(&db, jump, effect) else {
                panic!("`jump` makes a function pointer of a number, so it is not {}", effect.name);
            };
            let said = refusal.said();
            assert!(
                said.contains("transmutes `usize`")
                    && said.contains("a function pointer or vtable made of bytes"),
                "{said}"
            );
        }
    });
}

/// At the pin a `Vec`'s buffer is a `NonNull<u8>`, whose field is the pattern type
/// `pattern_type!(*const u8 is !null)`: a raw pointer in a toolchain type's field, looked
/// through as the toolchain's own, not one the function takes (blaze checking blaze, E4:
/// every `Compute::compute`, which takes `&mut Reads` and its `Vec<u8>`, was refused). A raw
/// pointer of the crate's own is still refused.
#[test]
fn a_raw_pointer_in_a_toolchain_pattern_type_is_the_toolchains() {
    let db = hir_expand::fixture::db(
        r#"
//- minicore: sized, copy, non_null
//- /lib.rs crate:kernel
pub struct Keys {
    buf: core::ptr::NonNull<u8>,
    len: usize,
}

pub struct Raw {
    buf: *const u8,
}

pub fn keys(keys: &mut Keys, input: &u8) -> u8 {
    let _ = keys;
    *input
}

pub fn raw(raw: &mut Raw, input: &u8) -> u8 {
    let _ = raw;
    *input
}
"#,
    )
    .into_db();
    let krate = crate_named(&db, "kernel");
    let (keys, raw) = (fn_named(&db, krate, "keys"), fn_named(&db, krate, "raw"));
    blaze_query::db::attach(&db, || {
        if let Err(refusal) = check_generic(&db, keys, &PURE) {
            panic!(
                "`keys` takes a `NonNull` of the toolchain, so it should be pure, but {}",
                refusal.said()
            );
        }
        let Err(refusal) = check_generic(&db, raw, &PURE) else {
            panic!("`raw` takes a raw pointer of its own, so it is not pure");
        };
        let said = refusal.said();
        assert!(said.contains("takes the raw pointer `*const u8`"), "{said}");
    });
}

/// The pinned library's `unsafe` precondition checks (`assert_unsafe_precondition!`'s
/// `precondition_check`, which aborts) are not a panic of a safe toolchain function that
/// reaches one: its contract is that it upholds the precondition, and the toolchain is trusted
/// for `unsafe` (`wrapping_shl`, slice iteration, `NonNull::new`, `MaybeUninit::zeroed` and
/// `CStr::to_bytes` at the pin, each a safe function over an `unsafe fn` with a check). A
/// workspace call of the `unsafe fn` still counts: there the workspace upholds it, and a build
/// with the checks on aborts when it does not. A panic that is no precondition check counts
/// wherever it is, and so does a function that only has the check's name.
#[test]
fn an_unsafe_precondition_check_is_a_panic_only_to_who_must_uphold_it() {
    let db = db_with(
        r#"
//- /lib.rs crate:kernel deps:std
pub fn through_a_safe_function(n: usize) -> usize {
    std::unchecked::to_bytes(n)
}

pub fn through_a_closure_of_a_safe_function() -> usize {
    std::unchecked::zeroed()
}

pub fn calls_the_unsafe_function(n: usize) -> usize {
    // SAFETY: none given: the fixture's point.
    unsafe { std::unchecked::new_unchecked(n) }
}

pub fn calls_the_checked_function_itself(n: usize) {
    // SAFETY: as above.
    unsafe { std::unchecked::assert_unchecked(n != 0) }
}

pub fn a_real_panic(value: Option<usize>) -> usize {
    std::unchecked::unwrap(value)
}

pub fn only_the_name(n: usize) {
    std::unchecked::precondition_check(n != 0)
}
"#,
    );
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        for clean in ["through_a_safe_function", "through_a_closure_of_a_safe_function"] {
            if let Err(refusal) = check_generic(&db, fn_named(&db, krate, clean), &NOPANIC) {
                panic!("`{clean}` should be nopanic, but {}", refusal.said());
            }
        }
        for (refused, names) in [
            ("calls_the_unsafe_function", "new_unchecked"),
            ("calls_the_checked_function_itself", "assert_unchecked"),
        ] {
            let Err(refusal) = check_generic(&db, fn_named(&db, krate, refused), &NOPANIC) else {
                panic!("`{refused}` calls an `unsafe fn` whose precondition it must uphold");
            };
            let said = refusal.said();
            assert_eq!(refusal.rule, Rule::Panic, "{said}");
            assert!(
                said.contains(names)
                    && said.contains("an `unsafe fn` of the toolchain whose precondition check"),
                "{said}"
            );
        }
        for (refused, names) in [("a_real_panic", "unwrap"), ("only_the_name", "precondition_check")]
        {
            let Err(refusal) = check_generic(&db, fn_named(&db, krate, refused), &NOPANIC) else {
                panic!("`{refused}` reaches a panic that is no precondition check");
            };
            let said = refusal.said();
            assert_eq!(refusal.rule, Rule::Panic, "{said}");
            assert!(said.contains(names) && said.contains("which panics"), "{said}");
        }
    });
}

/// What the tests of a `dyn` the root takes share: a trait with a method whose
/// `#[blaze::may(..)]` says what any object's may do and one with none, and a holder of one.
const PROBED: &str = r#"
//- minicore: sized, copy, phantom_data, drop, unsize, coerce_unsized, dispatch_from_dyn
//- /lib.rs crate:kernel
pub trait Probe {
    /// Allocates, in any impl, and nothing else.
    #[blaze::may(alloc)]
    fn listed(&self, at: u32);
    fn unlisted(&self, at: u32);
}

pub struct World<'a> {
    pub probe: &'a dyn Probe,
    pub clock: u32,
}

pub fn only_stores<'a>(probe: &'a dyn Probe, clock: u32) -> World<'a> {
    World { probe, clock }
}

pub fn reads_beside_it(world: &World<'_>) -> u32 {
    world.clock
}

fn inner(world: &World<'_>) -> u32 {
    world.clock
}

pub fn hands_it_on(world: &World<'_>) -> u32 {
    inner(world)
}

pub fn calls_listed(probe: &dyn Probe) {
    probe.listed(1)
}

pub fn calls_unlisted(probe: &dyn Probe) {
    probe.unlisted(1)
}

fn calls_inside(world: &World<'_>) {
    world.probe.unlisted(2)
}

pub fn hands_it_to_a_caller(world: &World<'_>) {
    calls_inside(world)
}

#[blaze::nopanic(trusted = "the fixture's: it answers for its own code")]
#[blaze::nonblocking(trusted = "the fixture's: it answers for its own code")]
fn trusted_inside(world: &World<'_>) {
    world.probe.unlisted(3)
}

pub fn hands_it_to_a_trusted_function(world: &World<'_>) {
    trusted_inside(world)
}
"#;

/// A function that takes a `dyn` is refused for what it does through it, not for taking it:
/// one that stores it, reads beside it or hands it to a function that calls nothing through
/// it is clean. A call through it has the effects the trait's method declares
/// (`#[blaze::may(alloc)]`: no panic and no wait in any impl, an allocation in some). A call
/// of a method with no list is refused naming the method and the `dyn` taken, wherever in
/// the root's graph it is made, under a callee trusted for the effect too: that one answers
/// for its own code, not for the object it is handed.
#[test]
fn a_dyn_parameter_is_refused_only_for_a_call_through_it() {
    let db = hir_expand::fixture::db(PROBED).into_db();
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        for clean in ["only_stores", "reads_beside_it", "hands_it_on"] {
            for effect in [&NOPANIC, &NOALLOC, &NONBLOCKING] {
                if let Err(refusal) = check_generic(&db, fn_named(&db, krate, clean), effect) {
                    panic!("`{clean}` should be {}, but {}", effect.name, refusal.said());
                }
            }
        }
        // Exactly the declared effects: `alloc` and no other.
        let listed = fn_named(&db, krate, "calls_listed");
        for effect in [&NOPANIC, &NONBLOCKING] {
            if let Err(refusal) = check_generic(&db, listed, effect) {
                panic!("`calls_listed` should be {}, but {}", effect.name, refusal.said());
            }
        }
        let Err(refusal) = check_generic(&db, listed, &NOALLOC) else {
            panic!("`Probe::listed` may allocate, so `calls_listed` is not noalloc");
        };
        assert!(refusal.said().contains("listed"), "{}", refusal.said());
        for refused in
            ["calls_unlisted", "hands_it_to_a_caller", "hands_it_to_a_trusted_function"]
        {
            for effect in [&NOPANIC, &NONBLOCKING] {
                let Err(refusal) = check_generic(&db, fn_named(&db, krate, refused), effect)
                else {
                    panic!("`{refused}` calls `Probe::unlisted` through a `dyn` it is given");
                };
                let said = refusal.said();
                assert_eq!(refusal.rule, Rule::Unchecked, "{said}");
                assert!(
                    said.contains("unlisted")
                        && said.contains("through a `dyn`")
                        && said.contains("dyn Probe"),
                    "{said}"
                );
            }
        }
    });
}
