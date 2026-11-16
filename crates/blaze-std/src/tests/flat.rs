//! `bstd::flat`: what the walk for [`AnyAddress`] clears and refuses, decided as the crate
//! compiles. The refusal of a channel of a type with an address inside, where the use site's
//! `const` assertion stops the build, is compiled by `tests/refusals.rs`
//! (`refusals/flat_reference.rs` and `refusals/flat_drop_glue.rs`).

use crate::{
    flat::{AnyAddress, is_flat},
    own::{Verdict, reaches},
};

#[derive(Clone, Copy)]
#[allow(dead_code, reason = "a shape for the walk to decide, never built")]
enum Kind {
    Read,
    Write { len: u64 },
}

#[allow(dead_code, reason = "a shape for the walk to decide, never built")]
struct Job {
    key: [u8; 32],
    size: u64,
    weight: f32,
    kind: Kind,
    next: Option<(u16, char, bool)>,
}

#[allow(dead_code, reason = "shapes for the walk to decide, never built")]
mod refused {
    use std::{alloc::Global, cell::Cell, sync::Arc};

    use crate::clock::Clock;

    pub struct Borrowed {
        pub name: &'static str,
    }
    pub struct Pointer {
        pub at: *const u8,
    }
    pub struct Boxed {
        pub body: Box<u32>,
    }
    pub struct Listed {
        pub items: Vec<u32>,
    }
    pub struct Counted {
        pub shared: Arc<u32>,
    }
    pub struct Celled {
        pub hits: Cell<u32>,
    }
    pub struct Called {
        pub run: fn(u32) -> u32,
    }
    pub struct Wide {
        pub len: usize,
    }
    pub struct Authority {
        pub clock: Clock,
    }
    pub struct Allocating {
        pub alloc: Global,
    }
    /// The address one level down, inside an enum's variant.
    pub enum Deep {
        Plain(u32),
        Holding(Option<Borrowed>),
    }
}

/// Whether the walk refuses `T`, as a constant.
macro_rules! refuses {
    ($ty:ty) => {
        matches!(reaches::<$ty, AnyAddress>(), Verdict::Refused(_))
    };
}

// Tuples are the shape that made the compiler panic when the walk asked `info` of one (rustc
// bug at 16d1ffac17ad, own/raw.rs `shape` says which): each tuple here, `()` and nested ones
// included, is decided without asking it.
const _: () = assert!(is_flat::<Job>());
const _: () = assert!(is_flat::<(u16, u16)>());
const _: () = assert!(is_flat::<()>());
const _: () = assert!(is_flat::<((u8, ()), [(char, bool); 2])>());
const _: () = assert!(is_flat::<[u64; 4]>());
const _: () = assert!(is_flat::<[u8; 0]>());
const _: () = assert!(is_flat::<[[u16; 2]; 3]>());
const _: () = assert!(is_flat::<(u8, i128, Option<core::num::NonZeroU32>)>());

const REFUSED: [bool; 11] = [
    refuses!(refused::Borrowed),
    refuses!(refused::Pointer),
    refuses!(refused::Boxed),
    refuses!(refused::Listed),
    refuses!(refused::Counted),
    refuses!(refused::Celled),
    refuses!(refused::Called),
    refuses!(refused::Wide),
    refuses!(refused::Authority),
    refuses!(refused::Allocating),
    refuses!(refused::Deep),
];

/// A type with drop glue is not flat, though nothing in it is an address.
#[allow(dead_code, reason = "a shape for the walk to decide, never built")]
struct Logged(u32);

impl Drop for Logged {
    fn drop(&mut self) {}
}

const LOGGED_HAS_GLUE: bool = core::mem::needs_drop::<Logged>();

#[test]
fn an_address_anywhere_inside_is_refused() {
    let names = ["a reference", "a raw pointer", "a box", "a vec", "an arc", "a cell", "a function pointer", "a usize", "a capability", "an allocator", "a reference two levels down"];
    for (refused, name) in REFUSED.iter().zip(names) {
        assert!(*refused, "the walk cleared a type holding {name}");
    }
    assert!(LOGGED_HAS_GLUE, "a `Drop` impl is drop glue, which `is_flat` refuses");
}
