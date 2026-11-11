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
