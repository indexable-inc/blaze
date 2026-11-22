use core::error::Error;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::mem::size_of;

use crate::union_ty::{Never, TypeSetMeta, UniqueSet};
use crate::{Union, define_union, union_val};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct IoErr(u16);

impl fmt::Display for IoErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "io error {}", self.0)
    }
}

impl Error for IoErr {}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ParseErr(u32);

impl fmt::Display for ParseErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at byte {}", self.0)
    }
}

impl Error for ParseErr {}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TimeoutErr(u64);

impl fmt::Display for TimeoutErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "timeout after {}ms", self.0)
    }
}

impl Error for TimeoutErr {}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct AuthErr(u8);

impl fmt::Display for AuthErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "auth denied (code {})", self.0)
    }
}

impl Error for AuthErr {}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct WrappedErr {
    inner: &'static IoErr,
}

impl fmt::Display for WrappedErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wrapped: {}", self.inner)
    }
}

impl Error for WrappedErr {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.inner)
    }
}

fn assert_fixed_error_union_traits<T>()
where
    T: fmt::Debug
        + fmt::Display
        + Error
        + Clone
        + Copy
        + PartialEq
        + Eq
        + Hash
        + Send
        + Sync
        + Unpin,
{
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = crate::hash::FixedHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

define_union! {
    enum NetErr {
        Io(IoErr) => narrow_io,
        Timeout(TimeoutErr) => narrow_timeout,
    }
}

define_union! {
    enum ServiceErr {
        Auth(AuthErr) => narrow_auth,
        Timeout(TimeoutErr) => narrow_timeout,
        Parse(ParseErr) => narrow_parse,
        Io(IoErr) => narrow_io,
    }
}

define_union! {
    enum PlainErr {
        Io(IoErr),
        Parse(ParseErr),
    }
}

define_union! {
    value enum ScalarVal {
        Num(u32) => narrow_num,
        Flag(bool) => narrow_flag,
        Text(&'static str) => narrow_text,
    }
}

#[test]
fn inline_small_local_tag_layout_and_niche_elision() {
    // 1-member union elides the uninhabited `Never` tail completely.
    assert_eq!(size_of::<Union![u32]>(), size_of::<u32>());
    // Small local u8 tag: `u8 | u16` fits in 4 bytes inline (zero heap indirection).
    assert_eq!(size_of::<Union![u8, u16]>(), 4);
    // `bool | ()` reuses `bool`'s niche and fits in a single byte.
    assert_eq!(size_of::<Union![bool, ()]>(), 1);
    // Concrete `define_union!` enum has a 1-byte local discriminant.
    assert_eq!(size_of::<NetErr>(), 16);
    assert_eq!(size_of::<PlainErr>(), 8);
}

#[test]
fn fixed_auto_traits_display_error_source_copy_eq_hash() {
    assert_fixed_error_union_traits::<Union![IoErr, ParseErr, TimeoutErr]>();
    assert_fixed_error_union_traits::<NetErr>();
    assert_fixed_error_union_traits::<ServiceErr>();

    static ROOT_IO: IoErr = IoErr(503);
    let u: Union![WrappedErr, ParseErr] = union_val!(WrappedErr { inner: &ROOT_IO });
    let copy_u = u;
    assert_eq!(u, copy_u);
    assert_eq!(hash_of(&u), hash_of(&copy_u));
    assert_eq!(alloc::format!("{u}"), "wrapped: io error 503");
    assert!(u.source().is_some());
    assert_eq!(alloc::format!("{}", u.source().unwrap()), "io error 503");

    // Non-Copy member (`alloc::string::String`) still works with Clone/Debug/Display/Eq/Hash.
    let s1: Union![u32, alloc::string::String] = union_val!(alloc::string::String::from("blaze"));
    let s2 = s1.clone();
    assert_eq!(s1, s2);
    assert_eq!(alloc::format!("{s1}"), "blaze");
}

#[test]
fn order_independent_construction_inspection_and_by_value_widening() {
    type Small = Union![IoErr, ParseErr];
    type Reordered = Union![ParseErr, IoErr];
    type Wide = Union![TimeoutErr, ParseErr, IoErr, AuthErr];

    let mut e1: Small = union_val!(IoErr(404));
    assert!(e1.is::<IoErr, _>());
    assert!(!e1.is::<ParseErr, _>());
    assert_eq!(e1.get::<IoErr, _>(), Some(&IoErr(404)));
    e1.get_mut::<IoErr, _>().unwrap().0 = 500;
    assert_eq!(e1.get::<IoErr, _>(), Some(&IoErr(500)));

    // Reorder `IoErr | ParseErr` -> `ParseErr | IoErr` by value.
    let reordered: Reordered = e1.widen();
    assert_eq!(reordered.get::<IoErr, _>(), Some(&IoErr(500)));

    // Widen `IoErr | ParseErr` -> `TimeoutErr | ParseErr | IoErr | AuthErr` by value.
    let wide_io: Wide = e1.widen();
    assert_eq!(wide_io.get::<IoErr, _>(), Some(&IoErr(500)));

    let e2 = union_val!(ParseErr(12) => Small);
    let wide_parse: Wide = e2.widen();
    assert_eq!(wide_parse.get::<ParseErr, _>(), Some(&ParseErr(12)));
    assert_eq!(wide_parse.get::<TimeoutErr, _>(), None);
}

#[test]
fn narrowing_handles_one_variant_and_propagates_widened_remainder() {
    type LowErr = Union![IoErr, ParseErr, TimeoutErr];
    type HighErr = Union![AuthErr, TimeoutErr, ParseErr];

    fn handle_io_or_propagate(err: LowErr) -> Result<u16, HighErr> {
        match err.narrow::<IoErr, _>() {
            Ok(io) => Ok(io.0),
            Err(rest) => Err(rest.widen()),
        }
    }

    // IoErr is handled locally and removed from the return error union.
    assert_eq!(handle_io_or_propagate(union_val!(IoErr(77))), Ok(77));

    // ParseErr and TimeoutErr propagate cleanly via `rest.widen()` into `HighErr` (different order + AuthErr).
    let p = handle_io_or_propagate(union_val!(ParseErr(9))).unwrap_err();
    assert_eq!(p.get::<ParseErr, _>(), Some(&ParseErr(9)));

    let t = handle_io_or_propagate(union_val!(TimeoutErr(250))).unwrap_err();
    assert_eq!(t.get::<TimeoutErr, _>(), Some(&TimeoutErr(250)));

    // Exhaustive narrowing: 3 -> 2 -> 1 -> into_inner() / Never::absurd().
    let u3: LowErr = union_val!(TimeoutErr(999));
    let u2 = u3.narrow::<IoErr, _>().unwrap_err();
    let u1 = u2.narrow::<ParseErr, _>().unwrap_err();
    assert_eq!(u1.into_inner(), TimeoutErr(999));

    let u1_again: Union![TimeoutErr] = union_val!(TimeoutErr(42));
    let extracted: TimeoutErr = match u1_again.narrow::<TimeoutErr, _>() {
        Ok(val) => val,
        Err(never) => {
            let _: Never = never;
            never.absurd()
        }
    };
    assert_eq!(extracted, TimeoutErr(42));
}

#[test]
fn compile_time_uniqueness_and_lifetime_erasure_metadata() {
    assert_eq!(<Union![IoErr, ParseErr, TimeoutErr] as TypeSetMeta>::LEN, 3);
    assert!(<Union![IoErr, ParseErr, TimeoutErr] as UniqueSet>::ALL_DISTINCT);

    // Duplicate members are detected at compile time via `UniqueSet::ALL_DISTINCT`.
    assert!(!<Union![IoErr, IoErr] as UniqueSet>::ALL_DISTINCT);
    assert!(!<Union![IoErr, ParseErr, IoErr] as UniqueSet>::ALL_DISTINCT);

    // Lifetime-erased duplicates (`&'a str | &'b str`) share the same canonical type name.
    fn check_erased_ref_collision<'a, 'b>() -> bool {
        <Union![&'a str, &'b str] as UniqueSet>::ALL_DISTINCT
    }
    assert!(!check_erased_ref_collision());

    // Distinct mutability (`&'a str | &'b mut str`) remains distinct.
    fn check_ref_mut_distinct<'a, 'b>() -> bool {
        <Union![&'a str, &'b mut str] as UniqueSet>::ALL_DISTINCT
    }
    assert!(check_ref_mut_distinct());
}

#[test]
fn define_union_concrete_enum_narrow_and_widen_into() {
    let net_io: NetErr = IoErr(502).into();
    let net_to: NetErr = TimeoutErr(1500).into();
    assert_eq!(alloc::format!("{net_io}"), "io error 502");
    assert_eq!(alloc::format!("{net_to}"), "timeout after 1500ms");

    // Concrete enum -> larger concrete enum widening (`NetErr` -> `ServiceErr`) with reordered variants.
    let svc_io: ServiceErr = net_io.widen_into();
    assert_eq!(svc_io, ServiceErr::Io(IoErr(502)));

    let svc_to: ServiceErr = net_to.widen_into();
    assert_eq!(svc_to, ServiceErr::Timeout(TimeoutErr(1500)));

    // Concrete enum -> structural `Union![...]` widening.
    let wide_u: Union![ParseErr, TimeoutErr, IoErr] = net_to.widen_into();
    assert_eq!(wide_u.get::<TimeoutErr, _>(), Some(&TimeoutErr(1500)));

    // Structural `Union![...]` -> concrete `define_union!` enum widening.
    let structural_small: Union![IoErr, ParseErr] = union_val!(ParseErr(88));
    let svc_from_u: ServiceErr = structural_small.widen_into();
    assert_eq!(svc_from_u, ServiceErr::Parse(ParseErr(88)));

    // Per-variant `narrow_*` methods return the exact narrowed `Union![...]` remainder.
    assert_eq!(svc_io.narrow_io(), Ok(IoErr(502)));
    let rem_after_io: Union![AuthErr, TimeoutErr, ParseErr] = svc_to.narrow_io().unwrap_err();
    assert_eq!(rem_after_io.get::<TimeoutErr, _>(), Some(&TimeoutErr(1500)));
    let rem_after_auth: Union![TimeoutErr, ParseErr] = rem_after_io.narrow::<AuthErr, _>().unwrap_err();
    assert_eq!(rem_after_auth.narrow::<TimeoutErr, _>(), Ok(TimeoutErr(1500)));

    // Shorthand `define_union!` without explicit `=> narrow_*` still supports generic `narrow` and `widen_into`.
    let plain = PlainErr::from(ParseErr(7));
    assert_eq!(plain.narrow::<ParseErr, _>(), Ok(ParseErr(7)));
    let plain_wide: ServiceErr = plain.widen_into();
    assert_eq!(plain_wide, ServiceErr::Parse(ParseErr(7)));

    // Value-union form (non-Error members: u32 | bool | &'static str).
    let sv = ScalarVal::from("hello");
    assert_eq!(alloc::format!("{sv}"), "hello");
    assert_eq!(sv.narrow_text(), Ok("hello"));
    let sv_num = ScalarVal::from(123u32);
    assert_eq!(sv_num.narrow_num(), Ok(123u32));
    let sv_flag = ScalarVal::from(true);
    let rem_sv: Union![bool, &'static str] = sv_flag.narrow_num().unwrap_err();
    assert_eq!(rem_sv.narrow::<bool, _>(), Ok(true));
}
