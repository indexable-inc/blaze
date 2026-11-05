//@ expect: accept
//@ edition: 2021
// `W<W<...u8...>>` 40 levels deep, built by a macro: each level of `Send`, `Sync` and `Unpin`
// is one nested goal (the auto-trait candidate's constituent `T`), so the deepest goal sits at
// depth 40, well under the default recursion limit of 128 in either solver. The shape walk's
// 16-level cap hands the rest of the chain to the solver.
struct W<T>(T);

macro_rules! w8 {
    ($t:ty) => {
        W<W<W<W<W<W<W<W<$t>>>>>>>>
    };
}

type Deep = w8!(w8!(w8!(w8!(w8!(u8)))));

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}
fn is_unpin<T: Unpin>() -> u32 {
    1
}

fn main() {
    let n = is_send::<Deep>() + is_sync::<Deep>() + is_unpin::<Deep>();
    println!("{} {}", n, std::mem::size_of::<Deep>());
}
