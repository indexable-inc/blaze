//! Large copies into buffers that go straight to the kernel or the network and are not read
//! again soon, with non-temporal stores so they don't evict the cache. Every path leaves the
//! same bytes, so which one runs is dispatch, not a fallback.

/// From this many bytes on, [`append`] copies with non-temporal stores.
pub(crate) const STREAM_FROM: usize = 4 << 20;

/// Appends `src` to `dst`. From [`STREAM_FROM`] bytes on, with non-temporal stores, for a
/// copy that is not read again soon. Leaves `dst` exactly as `dst.extend_from_slice(src)`.
pub(crate) fn append(dst: &mut Vec<u8>, src: &[u8]) {
    if src.len() < STREAM_FROM {
        dst.extend_from_slice(src);
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        avx512(dst, src);
    } else {
        sse2(dst, src);
    }
    #[cfg(target_arch = "aarch64")]
    stnp(dst, src);
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    dst.extend_from_slice(src);
}

/// How many bytes to copy plainly before `to` is aligned to `width` (a power of two), and how
/// many after them fill whole `width`-byte stores.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn split(to: *mut u8, len: usize, width: usize) -> (usize, usize) {
    let head = (to.addr().wrapping_neg() & (width - 1)).min(len);
    (head, (len - head) / width * width)
}

#[cfg(target_arch = "x86_64")]
fn avx512(dst: &mut Vec<u8>, src: &[u8]) {
    assert!(is_x86_feature_detected!("avx512f"));
    dst.reserve(src.len());
    // SAFETY: `dst.len() <= dst.capacity()`, so the offset is inside (or one past) `dst`'s
    // allocation.
    let end = unsafe { dst.as_mut_ptr().add(dst.len()) };
    // SAFETY: the CPU has AVX-512F, asserted above; `reserve` left room for `src.len()` bytes at
    // `end`, and `src` is borrowed shared while `dst` is borrowed mutably, so they don't overlap.
    unsafe { x86::avx512(end, src) };
    // SAFETY: `x86::avx512` wrote all `src.len()` bytes after the old length, and `reserve` made
    // the capacity at least the new length.
    unsafe { dst.set_len(dst.len() + src.len()) };
}

#[cfg(target_arch = "x86_64")]
fn sse2(dst: &mut Vec<u8>, src: &[u8]) {
    dst.reserve(src.len());
    // SAFETY: `dst.len() <= dst.capacity()`, so the offset is inside (or one past) `dst`'s
    // allocation.
    let end = unsafe { dst.as_mut_ptr().add(dst.len()) };
    // SAFETY: `reserve` left room for `src.len()` bytes at `end`, and `src` is borrowed shared
    // while `dst` is borrowed mutably, so they don't overlap.
    unsafe { x86::sse2(end, src) };
    // SAFETY: `x86::sse2` wrote all `src.len()` bytes after the old length, and `reserve` made
    // the capacity at least the new length.
    unsafe { dst.set_len(dst.len() + src.len()) };
}

#[cfg(target_arch = "aarch64")]
fn stnp(dst: &mut Vec<u8>, src: &[u8]) {
    dst.reserve(src.len());
    // SAFETY: `dst.len() <= dst.capacity()`, so the offset is inside (or one past) `dst`'s
    // allocation.
    let end = unsafe { dst.as_mut_ptr().add(dst.len()) };
    // SAFETY: `reserve` left room for `src.len()` bytes at `end`, and `src` is borrowed shared
    // while `dst` is borrowed mutably, so they don't overlap.
    unsafe { arm::stnp(end, src) };
    // SAFETY: `arm::stnp` wrote all `src.len()` bytes after the old length, and `reserve` made
    // the capacity at least the new length.
    unsafe { dst.set_len(dst.len() + src.len()) };
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm_sfence, _mm_stream_si128, _mm512_loadu_si512, _mm512_stream_si512,
    };
    use std::ptr;

    use super::split;

    /// Copies `from` to `to`, the 64-byte-aligned middle with non-temporal stores.
    ///
    /// # Safety
    ///
    /// The CPU has AVX-512F; `to` is valid for writes of `from.len()` bytes and doesn't overlap
    /// `from`.
    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn avx512(to: *mut u8, from: &[u8]) {
        let (head, body) = split(to, from.len(), 64);
        let src = from.as_ptr();
        // SAFETY: `head <= from.len()` (`split`); `src` is valid for reads and `to` for writes of
        // `from.len()` bytes, and they don't overlap (the caller's contract).
        unsafe { ptr::copy_nonoverlapping(src, to, head) };
        let mut at = head;
        while at < head + body {
            // SAFETY: `body` is a multiple of 64 and `at` moves by 64 from `head`, so
            // `at + 64 <= head + body <= from.len()`, and `at` is inside `from`.
            let from_at = unsafe { src.add(at) };
            // SAFETY: the 64 bytes at `from_at` are inside `from`; the load takes any alignment.
            let v = unsafe { _mm512_loadu_si512(from_at.cast()) };
            // SAFETY: `at < from.len()`, for which `to` is valid (the caller's contract).
            let to_at = unsafe { to.add(at) };
            // SAFETY: `to_at` is 64-byte aligned (`split` aligned `to + head`, and `at` moves by
            // 64) and has 64 writable bytes (`at + 64 <= from.len()`).
            unsafe { _mm512_stream_si512(to_at.cast(), v) };
            at += 64;
        }
        _mm_sfence();
        // SAFETY: `at <= from.len()`, so both offsets are inside (or one past) their buffers.
        let (from_at, to_at) = (unsafe { src.add(at) }, unsafe { to.add(at) });
        // SAFETY: the `from.len() - at` bytes from each are in `from` and `to` (the caller's
        // contract), which don't overlap; the non-temporal stores were fenced above.
        unsafe { ptr::copy_nonoverlapping(from_at, to_at, from.len() - at) };
    }

    /// Copies `from` to `to`, the 16-byte-aligned middle with non-temporal stores.
    ///
    /// # Safety
    ///
    /// `to` is valid for writes of `from.len()` bytes and doesn't overlap `from`.
    pub(super) unsafe fn sse2(to: *mut u8, from: &[u8]) {
        let (head, body) = split(to, from.len(), 16);
        let src = from.as_ptr();
        // SAFETY: `head <= from.len()` (`split`); `src` is valid for reads and `to` for writes of
        // `from.len()` bytes, and they don't overlap (the caller's contract).
        unsafe { ptr::copy_nonoverlapping(src, to, head) };
        let mut at = head;
        while at < head + body {
            // SAFETY: `body` is a multiple of 16 and `at` moves by 16 from `head`, so
            // `at + 16 <= head + body <= from.len()`, and `at` is inside `from`.
            let from_at = unsafe { src.add(at) };
            // SAFETY: SSE2 is baseline on x86_64; the 16 bytes at `from_at` are inside `from`,
            // and the load takes any alignment.
            let v = unsafe { _mm_loadu_si128(from_at.cast()) };
            // SAFETY: `at < from.len()`, for which `to` is valid (the caller's contract).
            let to_at = unsafe { to.add(at) };
            // SAFETY: SSE2 is baseline on x86_64; `to_at` is 16-byte aligned (`split` aligned
            // `to + head`, and `at` moves by 16) and has 16 writable bytes
            // (`at + 16 <= from.len()`).
            unsafe { _mm_stream_si128(to_at.cast(), v) };
            at += 16;
        }
        _mm_sfence();
        // SAFETY: `at <= from.len()`, so both offsets are inside (or one past) their buffers.
        let (from_at, to_at) = (unsafe { src.add(at) }, unsafe { to.add(at) });
        // SAFETY: the `from.len() - at` bytes from each are in `from` and `to` (the caller's
        // contract), which don't overlap; the non-temporal stores were fenced above.
        unsafe { ptr::copy_nonoverlapping(from_at, to_at, from.len() - at) };
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use std::arch::asm;
    use std::ptr;

    use super::split;

    /// Copies `from` to `to`, the 64-byte-aligned middle with non-temporal pair stores. No
    /// barrier: the bytes are only read later through `to`, on this thread or after whatever
    /// synchronization hands `to` over, which orders them like any other store.
    ///
    /// # Safety
    ///
    /// `to` is valid for writes of `from.len()` bytes and doesn't overlap `from`.
    pub(super) unsafe fn stnp(to: *mut u8, from: &[u8]) {
        let (head, body) = split(to, from.len(), 64);
        let src = from.as_ptr();
        // SAFETY: `head <= from.len()` (`split`); `src` is valid for reads and `to` for writes of
        // `from.len()` bytes, and they don't overlap (the caller's contract).
        unsafe { ptr::copy_nonoverlapping(src, to, head) };
        let mut at = head;
        while at < head + body {
            // SAFETY: `body` is a multiple of 64 and `at` moves by 64 from `head`, so
            // `at + 64 <= head + body <= from.len()`, and `at` is inside both buffers.
            let (from_at, to_at) = (unsafe { src.add(at) }, unsafe { to.add(at) });
            // SAFETY: NEON is baseline on aarch64; the asm reads the 64 bytes at `from_at`, in
            // `from`, and writes the 64 at `to_at`, in `to` (the caller's contract), which don't
            // overlap.
            unsafe {
                asm!(
                    "ldp {a:q}, {b:q}, [{src}]",
                    "ldp {c:q}, {d:q}, [{src}, #32]",
                    "stnp {a:q}, {b:q}, [{dst}]",
                    "stnp {c:q}, {d:q}, [{dst}, #32]",
                    src = in(reg) from_at,
                    dst = in(reg) to_at,
                    a = out(vreg) _,
                    b = out(vreg) _,
                    c = out(vreg) _,
                    d = out(vreg) _,
                    options(nostack, preserves_flags),
                )
            };
            at += 64;
        }
        // SAFETY: `at <= from.len()`, so both offsets are inside (or one past) their buffers.
        let (from_at, to_at) = (unsafe { src.add(at) }, unsafe { to.add(at) });
        // SAFETY: the `from.len() - at` bytes from each are in `from` and `to` (the caller's
        // contract), which don't overlap.
        unsafe { ptr::copy_nonoverlapping(from_at, to_at, from.len() - at) };
    }
}

#[cfg(test)]
fn plain(dst: &mut Vec<u8>, src: &[u8]) {
    dst.extend_from_slice(src);
}

/// Every path this CPU can run, the plain copy first, for the differential tests. Each
/// streams whatever the length; only [`append`] applies [`STREAM_FROM`].
#[cfg(test)]
pub(crate) fn paths() -> Vec<(&'static str, fn(&mut Vec<u8>, &[u8]))> {
    let mut out: Vec<(&'static str, fn(&mut Vec<u8>, &[u8]))> = vec![("extend_from_slice", plain)];
    #[cfg(target_arch = "x86_64")]
    {
        out.push(("sse2", sse2));
        if is_x86_feature_detected!("avx512f") {
            out.push(("avx512f", avx512));
        }
    }
    #[cfg(target_arch = "aarch64")]
    out.push(("stnp", stnp));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = blaze_ids::rng::SplitMix64::new(seed);
        (0..len).map(|_| rng.next_u64() as u8).collect()
    }

    /// `copy` of `src` onto a `dst` holding `prefix`, with `spare` bytes of capacity beyond
    /// what the copy needs, against `extend_from_slice`.
    fn check(name: &str, copy: fn(&mut Vec<u8>, &[u8]), prefix: &[u8], src: &[u8], spare: usize) {
        let mut expected = prefix.to_vec();
        expected.extend_from_slice(src);
        let mut got = Vec::with_capacity(prefix.len() + src.len() + spare);
        got.extend_from_slice(prefix);
        copy(&mut got, src);
        assert!(
            got == expected,
            "{name}: {} onto {} with {spare} spare",
            src.len(),
            prefix.len()
        );
    }

    #[test]
    fn every_path_copies_short_inputs() {
        let buffer = noise(1100, 1);
        let prefix = noise(70, 2);
        let mut copies = paths();
        copies.push(("append", append));
        for (name, copy) in copies {
            for len in (0..=300).chain([511, 512, 513, 1000, 1023, 1024, 1025]) {
                for offset in 0..16 {
                    for kept in [0, 1, 7, 16, 33, 64, 70] {
                        for spare in [0, 9] {
                            let src = &buffer[offset..offset + len];
                            check(name, copy, &prefix[..kept], src, spare);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_path_copies_around_the_threshold() {
        let buffer = noise(STREAM_FROM + 64 + 16, 3);
        let prefix = noise(70, 4);
        let mut copies = paths();
        copies.push(("append", append));
        for (name, copy) in copies {
            for len in [STREAM_FROM - 1, STREAM_FROM, STREAM_FROM + 1, STREAM_FROM + 63] {
                for offset in [0, 13] {
                    for kept in [0, 5, 64] {
                        let src = &buffer[offset..offset + len];
                        check(name, copy, &prefix[..kept], src, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn append_onto_an_empty_vec_is_the_bytes() {
        let src = noise(STREAM_FROM + 3, 5);
        let mut dst = Vec::new();
        append(&mut dst, &src);
        assert!(dst == src);
    }
}
