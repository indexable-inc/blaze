//! `#[blaze::noalloc]` and `#[blaze::nonblocking]` over the stand-in toolchain
//! (docs/v2/plan/lanes/effects.md).

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
};
use crate::{
    check_generic,
    effect_attrs::{NOALLOC, NONBLOCKING},
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
            assert!(said.contains("transmutes `usize`") && said.contains("a function pointer or vtable made of bytes"), "{said}");
        }
    });
}
