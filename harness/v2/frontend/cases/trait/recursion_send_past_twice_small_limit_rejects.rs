//@ expect: reject E0275
//@ edition: 2021
// The same 40-level type under `#![recursion_limit = "8"]`. The `Send` goal needs 40 nested
// levels. The next solver overflows at 8, reruns the root goal at twice the limit (16) for the
// `recursion_depth_exceeding_limit` warning, overflows again, and reports the overflow: E0275.
// The old solver overflows at 8 with the same E0275. 40 is far past 16, so neither the rerun
// nor a few levels of difference in how depth is charged can change the answer. The type is
// written out, not built by a macro, so no macro expansion comes near the small limit.
#![recursion_limit = "8"]

struct W<T>(T);

type Deep =
    W<W<W<W<W<W<W<W<W<W<
    W<W<W<W<W<W<W<W<W<W<
    W<W<W<W<W<W<W<W<W<W<
    W<W<W<W<W<W<W<W<W<W<
    u8
    >>>>>>>>>>
    >>>>>>>>>>
    >>>>>>>>>>
    >>>>>>>>>>;

fn is_send<T: Send>() {}

fn main() {
    is_send::<Deep>();
}
