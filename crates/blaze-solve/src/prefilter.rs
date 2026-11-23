//! The fingerprint prefilter over an impl bucket (solve.md (d)): the reference's
//! `DeepRejectCtxt::args_may_unify` (rustc_type_ir fast_reject.rs), done for a whole bucket at
//! once in `core::simd`.
//!
//! A fingerprint is 16 bytes: for each of the first 4 trait arguments (the self type first),
//! the byte of its head and of its first 3 children. A byte is 0 for a *wildcard* (a term that
//! may unify with anything: on the goal side an inference variable, a non-rigid alias, an
//! error, a bound variable or a region; on the impl side also the impl's own parameters), else
//! `1 + mix(head) % 255` of the head's kind and definition. A rigid term (a param of the
//! goal's scope, a placeholder, a rigid alias) has a byte of its own, so an impl that names a
//! concrete type there cannot match it (fast_reject.rs `types_may_unify_inner`: `Param` and
//! `Alias(IsRigid::Yes)` against a concrete type are rejected).
//!
//! An impl survives when, at every position where the goal's byte is not 0, its byte equals
//! the goal's or is 0. Equal bytes may still fail to unify (a collision, or a deeper
//! difference): the prefilter only drops impls that cannot apply, never one that can.
//!
//! Columns are position-major (`bytes[p * stride + i]` is impl `i`'s byte at position `p`),
//! `stride` a multiple of 32 with zero padding, so 16 or 32 impls are compared per position in
//! one vector compare (`Simd<u8, N>::simd_eq`), ANDed across the goal's set positions, and the
//! survivors read off the bitmask.

use core::simd::cmp::SimdPartialEq;
use core::simd::Simd;

use blaze_own::{Alloc, Vec};

use crate::facts::Portable;
use crate::term::{Children, List, Rigid, Ty, TyKind, VarKind, children};
use crate::tys::Tys;

/// Bytes per fingerprint: 4 arguments of 4 bytes.
pub const WIDTH: usize = 16;

/// A fingerprint.
pub type Fingerprint = [u8; WIDTH];

/// An impl bucket's fingerprint columns.
#[derive(Clone, Copy, Debug)]
pub struct Columns<'w> {
    /// `WIDTH * stride` bytes, position-major.
    pub bytes: &'w [u8],
    /// Row length: a multiple of 32, at least `len`.
    pub stride: u32,
    /// How many impls.
    pub len: u32,
}

impl Columns<'_> {
    pub const EMPTY: Columns<'static> = Columns { bytes: &[], stride: 0, len: 0 };
}

/// The column stride for `len` impls.
pub fn stride_for(len: u32) -> u32 {
    len.div_ceil(32) * 32
}

/// Writes the columns of `fps` into `out` (cleared), `stride_for(fps.len())` wide.
pub fn columns_of(fps: &[Fingerprint], out: &mut Vec<u8, Alloc<'_>>) -> u32 {
    let stride = stride_for(fps.len() as u32);
    out.clear();
    out.resize(WIDTH * stride as usize, 0);
    for (i, fp) in fps.iter().enumerate() {
        for p in 0..WIDTH {
            out[p * stride as usize + i] = fp[p];
        }
    }
    stride
}

/// A byte of `k`'s head: `None` for a wildcard.
fn head(k: TyKind, impl_side: bool) -> Option<u64> {
    let (tag, a, b): (u64, u64, u64) = match k {
        TyKind::Bool => (1, 0, 0),
        TyKind::Char => (2, 0, 0),
        TyKind::Int(i) => (3, i as u64, 0),
        TyKind::Uint(u) => (4, u as u64, 0),
        TyKind::Float(f) => (5, f as u64, 0),
        TyKind::Str => (6, 0, 0),
        TyKind::Never => (7, 0, 0),
        TyKind::Tuple(l) => (8, u64::from(l.len), 0),
        TyKind::Record(s, l) => (35, s, u64::from(l.len)),
        TyKind::Union(l) => (36, u64::from(l.len), 0),
        TyKind::Ref(_, _, m) => (9, m as u64, 0),
        TyKind::RawPtr(_, m) => (10, m as u64, 0),
        TyKind::Array(..) => (11, 0, 0),
        TyKind::Slice(_) => (12, 0, 0),
        TyKind::Pat(_, l) => (34, u64::from(l.len), 0),
        TyKind::Adt(d, _) => (13, d.0 as u64, (d.0 >> 64) as u64),
        TyKind::Foreign(d) => (14, d.0 as u64, (d.0 >> 64) as u64),
        TyKind::FnDef(d, _) => (15, d.0 as u64, (d.0 >> 64) as u64),
        TyKind::FnPtr(l, h) => (16, u64::from(l.len), u64::from(h.bits())),
        TyKind::Closure(d, _) => (17, d.0 as u64, (d.0 >> 64) as u64),
        TyKind::Coroutine(d, _) => (23, d.0 as u64, (d.0 >> 64) as u64),
        TyKind::Dynamic(..) => (18, 0, 0),
        // Bits and type folded: a collision only lets a candidate through to unification.
        TyKind::ConstValue(t, v) => (19, v as u64, (v >> 64) as u64 ^ u64::from(t.code())),
        // A rigid alias is rigid on the goal side; in an impl header every alias may normalize.
        TyKind::Alias(Rigid::Yes, _, d, _) if !impl_side => (20, d.0 as u64, (d.0 >> 64) as u64),
        TyKind::Param(i) | TyKind::ConstParam(i) if !impl_side => (21, u64::from(i), 0),
        TyKind::Placeholder(u, i) => (22, u64::from(u), u64::from(i)),
        TyKind::Infer(v) if v.kind == VarKind::Int || v.kind == VarKind::Float => return None,
        _ => return None,
    };
    let mut h = tag.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ a;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd) ^ b;
    h = (h ^ (h >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 29;
    Some(h)
}

fn byte(h: Option<u64>) -> u8 {
    match h {
        None => 0,
        Some(h) => 1 + (h % 255) as u8,
    }
}

/// The first 3 children of `k`, in `children` order, read with `get`.
fn first_children(k: TyKind, get: impl Fn(List, u32) -> Ty) -> [Ty; 3] {
    let mut out = [Ty::NONE; 3];
    match children(k) {
        Children::None => {}
        Children::One(a) => out[0] = a,
        Children::Two(a, b) => {
            out[0] = a;
            out[1] = b;
        }
        Children::List(l) => {
            for i in 0..l.len.min(3) {
                out[i as usize] = get(l, i);
            }
        }
        Children::ListThen(l, t) => {
            let n = l.len.min(3);
            for i in 0..n {
                out[i as usize] = get(l, i);
            }
            if n < 3 {
                out[n as usize] = t;
            }
        }
    }
    out
}

/// A goal's fingerprint over its trait arguments `args`, resolving variables.
pub fn fingerprint_goal<T: Tys>(tys: &T, args: List) -> Fingerprint {
    let mut fp = [0u8; WIDTH];
    for a in 0..args.len.min(4) {
        let t = tys.shallow_resolve(tys.list_get(args, a));
        let k = tys.kind(t);
        let h = head(k, false);
        fp[a as usize * 4] = byte(h);
        if h.is_none() {
            continue;
        }
        let kids = first_children(k, |l, i| tys.list_get(l, i));
        for (j, &c) in kids.iter().enumerate() {
            if c != Ty::NONE {
                let ck = tys.kind(tys.shallow_resolve(c));
                fp[a as usize * 4 + 1 + j] = byte(head(ck, false));
            }
        }
    }
    fp
}

/// An impl's fingerprint over its header's arguments `args` (portable, over the impl's
/// generics, which are wildcards).
pub fn fingerprint_impl(p: Portable<'_>, args: List) -> Fingerprint {
    let get = |l: List, i: u32| -> Ty { p.lists.get((l.start + i) as usize).copied().unwrap_or(Ty::NONE) };
    let kind = |t: Ty| -> Option<TyKind> { p.nodes.get(t.0 as usize).copied() };
    let mut fp = [0u8; WIDTH];
    for a in 0..args.len.min(4) {
        let Some(k) = kind(get(args, a)) else { continue };
        let h = head(k, true);
        fp[a as usize * 4] = byte(h);
        if h.is_none() {
            continue;
        }
        let kids = first_children(k, get);
        for (j, &c) in kids.iter().enumerate() {
            if let Some(ck) = kind(c) {
                fp[a as usize * 4 + 1 + j] = byte(head(ck, true));
            }
        }
    }
    fp
}

/// Whether the goal's trait arguments `goal` (in the arena) may unify with the impl header's
/// `header` (portable, its parameters wildcards), compared head by head at every depth
/// (fast_reject.rs `types_may_unify_inner`, the deep form of the fingerprint): `false` only
/// where two rigid heads differ, so the header need not be imported and related. A head is
/// [`head`]'s 64-bit hash, a function of the head alone: unequal hashes are unequal heads, and
/// an equal hash of two heads that differ only lets the header through to the relation (no
/// byte folding as in the fingerprint). A wildcard on either side (an inference variable, a
/// non-rigid alias, a region, an impl parameter) matches any subtree, and a repeated impl
/// parameter is not checked for agreement (the relation does that). `work` is scratch.
pub fn header_may_match<T: Tys>(
    tys: &T,
    goal: List,
    p: Portable<'_>,
    header: List,
    work: &mut Vec<(Ty, Ty), Alloc<'_>>,
) -> Result<bool, crate::error::SolveError> {
    if goal.len != header.len {
        return Ok(false);
    }
    let pget = |l: List, i: u32| -> Ty { p.lists.get((l.start + i) as usize).copied().unwrap_or(Ty::NONE) };
    work.clear();
    for i in 0..goal.len {
        work.push((tys.list_get(goal, i), pget(header, i)));
    }
    let mut visited = 0usize;
    while let Some((g, h)) = work.pop() {
        visited += 1;
        if visited > crate::fold::MAX_WALK {
            return Err(crate::error::SolveError::Limit(crate::error::Limit::Walk));
        }
        let Some(&hk) = p.nodes.get(h.0 as usize) else { continue };
        let Some(hh) = head(hk, true) else { continue };
        let gk = tys.kind(tys.shallow_resolve(g));
        let Some(gh) = head(gk, false) else { continue };
        if gh != hh {
            return Ok(false);
        }
        match (children(gk), children(hk)) {
            (Children::One(a), Children::One(b)) => work.push((a, b)),
            (Children::Two(a, x), Children::Two(b, y)) => {
                work.push((a, b));
                work.push((x, y));
            }
            (Children::List(a), Children::List(b)) => {
                for i in 0..a.len.min(b.len) {
                    work.push((tys.list_get(a, i), pget(b, i)));
                }
            }
            (Children::ListThen(a, x), Children::ListThen(b, y)) => {
                for i in 0..a.len.min(b.len) {
                    work.push((tys.list_get(a, i), pget(b, i)));
                }
                work.push((x, y));
            }
            _ => {}
        }
    }
    Ok(true)
}

/// The planted bug of the prefilter's control: a wildcard impl byte is treated as a mismatch.
#[cfg(test)]
fn drops_wildcard() -> bool {
    crate::tests::plant::on(crate::tests::plant::Plant::PrefilterDropsWildcard)
}

#[cfg(not(test))]
fn drops_wildcard() -> bool {
    false
}

/// The survivors of `cols` for the goal fingerprint `g`, appended to `out` in order: the scalar
/// twin of [`filter`], kept as the reference the vector form is tested against.
pub fn filter_scalar(g: &Fingerprint, cols: Columns<'_>, out: &mut Vec<u32, Alloc<'_>>) {
    let drop_wild = drops_wildcard();
    'impls: for i in 0..cols.len as usize {
        for (p, &gb) in g.iter().enumerate() {
            if gb == 0 {
                continue;
            }
            let b = cols.bytes[p * cols.stride as usize + i];
            if b != gb && (b != 0 || drop_wild) {
                continue 'impls;
            }
        }
        out.push(i as u32);
    }
}

fn filter_n<const N: usize>(g: &Fingerprint, cols: Columns<'_>, out: &mut Vec<u32, Alloc<'_>>) {
    let drop_wild = drops_wildcard();
    let stride = cols.stride as usize;
    let zero = Simd::<u8, N>::splat(0);
    let full: u64 = if N == 64 { u64::MAX } else { (1u64 << N) - 1 };
    let mut chunk = 0usize;
    while chunk * N < cols.len as usize {
        let base = chunk * N;
        let mut mask = full;
        for (p, &gb) in g.iter().enumerate() {
            if gb == 0 {
                continue;
            }
            let at = p * stride + base;
            let v = Simd::<u8, N>::from_slice(&cols.bytes[at..at + N]);
            let eq = v.simd_eq(Simd::splat(gb));
            let m = if drop_wild { eq } else { eq | v.simd_eq(zero) };
            mask &= m.to_bitmask();
            if mask == 0 {
                break;
            }
        }
        let rest = cols.len as usize - base;
        if rest < N {
            mask &= (1u64 << rest) - 1;
        }
        while mask != 0 {
            let bit = mask.trailing_zeros();
            out.push((base + bit as usize) as u32);
            mask &= mask - 1;
        }
        chunk += 1;
    }
}

/// The survivors of `cols` for the goal fingerprint `g`, appended to `out` in order: 32 impls
/// per compare where the target has 256-bit vectors (AVX2), else 16 (NEON, SSE2). The compiler
/// picks the instructions from `core::simd`; the width is fixed at compile time from the
/// target's features (std.md, "SIMD", rule 3: a run-time choice would take a `Cpu` value).
pub fn filter(g: &Fingerprint, cols: Columns<'_>, out: &mut Vec<u32, Alloc<'_>>) {
    if cols.len == 0 {
        return;
    }
    if g.iter().all(|&b| b == 0) {
        for i in 0..cols.len {
            out.push(i);
        }
        return;
    }
    #[cfg(target_feature = "avx2")]
    filter_n::<32>(g, cols, out);
    #[cfg(not(target_feature = "avx2"))]
    filter_n::<16>(g, cols, out);
}

/// The vector form at a given width, for the equality tests and the cost example.
pub fn filter_width(g: &Fingerprint, cols: Columns<'_>, out: &mut Vec<u32, Alloc<'_>>, width: u32) {
    match width {
        16 => filter_n::<16>(g, cols, out),
        _ => filter_n::<32>(g, cols, out),
    }
}
