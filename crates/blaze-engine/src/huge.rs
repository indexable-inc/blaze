//! The general allocator and huge pages: mimalloc's arenas are on transparent huge pages where
//! Linux has them, by mimalloc's own doing, and that is kept for its speed at a known price in
//! memory.
//!
//! mimalloc 3 advises every arena it commits `MADV_HUGEPAGE` (its `allow_thp` option, on by
//! default; `allow_large_os_pages` is hugetlb pages alone, which blaze refuses below), and maps
//! its first arena before `main`, so nothing here chooses it and `BLAZE_HUGE_PAGES` does not
//! reach it: the only switch that covers the first arena is libmimalloc-sys's `no_thp`
//! feature, at build time.
//!
//! The price: mimalloc hands out 64 KiB pages and gives them back one at a time; on a huge
//! page the first touch of one makes 2 MiB resident, and giving 64 KiB back leaves the rest
//! resident though mimalloc counts none of it. The gain: fewer TLB misses and a page fault a
//! 2 MiB, not a 4 KiB, as an edit's build takes again what the last one freed (an edit of
//! hello-world allocates and frees 95 MiB of the global heap). Measured 2026-11-11 (lane
//! mem2-1111, Linux aarch64 in an ix VM, a hello-world daemon after a cold build and five
//! edits, two runs each alternated):
//!
//! | mimalloc's arenas | resident | of it mimalloc's arenas | a body edit and run, 3.5 s after the last (median of 30) |
//! |---|---|---|---|
//! | on transparent huge pages | 5,732 and 5,716 MiB | 3,447 MiB, for 2,437 in its pages | 121.0 and 112.5 ms |
//! | built with `no_thp` | 3,959 and 4,066 MiB | 2,440 MiB | 137.7 and 136.7 ms |
//!
//! Run time comes first (decisions D5 and D21), and 17% of an edit is past what memory may
//! cost, so the arenas stay on huge pages. Under a hypervisor a TLB miss walks two page tables,
//! so the gap on bare metal is likely smaller; it was not measured there. macOS has no huge
//! pages for anonymous memory (`blaze_mem::huge`), so none of this is its.

#![allow(unsafe_code, reason = "sets one mimalloc option word")]

/// Sets mimalloc's hugetlb options, so neither its defaults nor its `MIMALLOC_*` variables
/// decide: no arena on hugetlb pages (`allow_large_os_pages`) and none reserved at start
/// (`reserve_huge_os_pages`), which belong to blaze-mem's pool alone
/// (`BLAZE_HUGE_PAGES=reserved`). Transparent huge pages are not these options' (the
/// module's docs). Called once, before the first query.
pub(crate) fn no_hugetlb_for_mimalloc() {
    // SAFETY: each sets an option word mimalloc reads when it next maps an arena; mimalloc
    // documents options as settable from any thread at any time.
    unsafe {
        libmimalloc_sys::mi_option_set_enabled(libmimalloc_sys::mi_option_large_os_pages, false);
        libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_reserve_huge_os_pages, 0);
        libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_reserve_huge_os_pages_at, -1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mimalloc_maps_no_arena_on_hugetlb_pages() {
        no_hugetlb_for_mimalloc();
        // SAFETY: reads an option word; callable from any thread.
        let large = unsafe {
            libmimalloc_sys::mi_option_is_enabled(libmimalloc_sys::mi_option_large_os_pages)
        };
        assert!(!large, "hugetlb pages are blaze-mem's pool's alone");
    }
}
