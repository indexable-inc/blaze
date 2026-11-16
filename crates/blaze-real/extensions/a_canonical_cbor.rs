//! canonical_cbor: `#![blaze::canonical_cbor]` on a module whose struct declares its fields out of key order (`bb` before `a`). A working layer refuses it.
//! expect: refuse

#[blaze::canonical_cbor]
mod wire {
    pub struct Row {
        pub bb: i32,
        pub a: i32,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let r = wire::Row { bb: 40, a: 2 };
    r.bb + r.a
}
