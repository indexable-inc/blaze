//@ expect: reject E0275
//@ edition: 2021
// A short `Send` goal nested deep, under `#![recursion_limit = "8"]`. `W<..W<Base>..>: Deep`
// with 12 wrappers nests 12 levels down to `Base: Deep`, whose impl needs `L0: Send`, and that
// goal needs 7 levels more (`L0` to `L5`, then `u8`): 19 in all. The next solver overflows at
// 8, reruns the root goal at twice the limit (16), overflows again, and reports E0275; the old
// solver overflows at 8 with the same E0275. The `Send` goal alone needs 6 below it, within the
// shape table's 24 levels and under the limit, so only the depth above it tells the table to
// leave it to the solver: answered without charging depth, the whole goal would need 12, hold
// at 16 and give only the `recursion_depth_exceeding_limit` warning. 19 and 12 against 16
// leave room for a few levels of difference in how the two solvers charge depth.
#![recursion_limit = "8"]

struct L0(L1);
struct L1(L2);
struct L2(L3);
struct L3(L4);
struct L4(L5);
struct L5(u8);

struct W<T>(T);
struct Base;

trait Deep {}
impl<T: Deep> Deep for W<T> {}
impl Deep for Base where L0: Send {}

type Nested = W<W<W<W<W<W<W<W<W<W<W<W<Base>>>>>>>>>>>>;

fn is_deep<T: Deep>() {}

fn main() {
    is_deep::<Nested>();
}
