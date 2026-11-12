//! The general allocator stays off huge pages, whatever `blaze-mem`'s regions are on.
//!
//! mimalloc hands out 64 KiB pages and gives them back one at a time. On a transparent huge
//! page that does not work: the first touch of a 64 KiB page makes the whole 2 MiB resident,
//! and giving 64 KiB back splits the huge page and leaves the other 1.9 MiB resident though
//! mimalloc counts none of it. Measured 2026-11-11 (lane mem2-1111, Linux aarch64, a
//! hello-world daemon after a cold build and five edits): mimalloc's arenas held 3,447 MiB
//! resident, 1,534 of it in huge pages, for 2,437 MiB in its pages and 1,792 MiB of live
//! blocks; with mimalloc on base pages the daemon's resident set fell from 5,778 to 4,648 MiB.
//! `blaze-mem`'s regions keep their huge pages (`blaze_mem::huge`): a region grows at one end
//! and commits whole huge pages, so nothing in one is free and resident.

#![allow(unsafe_code, reason = "sets one mimalloc option word")]

/// Sets every one of mimalloc's huge page options, so neither mimalloc's defaults nor its
/// `MIMALLOC_*` variables decide: no arena on hugetlb pages (`allow_large_os_pages`), and none
/// reserved at start (`reserve_huge_os_pages`), which belong to blaze-mem's pool alone.
/// Called once, before the first query. Transparent huge pages are not an option's to refuse:
/// mimalloc 3 advises every arena it commits `MADV_HUGEPAGE` unless it is compiled without
/// (`allow_thp` set to 0 at run time comes after the first arena, mapped before `main`, and
/// set from the environment has mimalloc turn huge pages off for the whole process, regions
/// included), so libmimalloc-sys is built with `no_thp` (the workspace's Cargo.toml).
pub(crate) fn base_pages_for_mimalloc() {
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
    fn mimalloc_maps_no_arena_on_huge_pages() {
        base_pages_for_mimalloc();
        // SAFETY: reads an option word; callable from any thread.
        let large = unsafe {
            libmimalloc_sys::mi_option_is_enabled(libmimalloc_sys::mi_option_large_os_pages)
        };
        assert!(!large, "mimalloc's arenas are on base pages whatever BLAZE_HUGE_PAGES chose");
    }
}
