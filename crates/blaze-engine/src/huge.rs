//! The general allocator and huge pages: mimalloc's arenas are on base pages (libmimalloc-sys's
//! `no_thp`), and only blaze-mem's own regions are on transparent huge pages.
//!
//! mimalloc 3 advises every arena it commits `MADV_HUGEPAGE` unless it is built without
//! (`MI_NO_THP`; its run-time `allow_thp` cannot say so for the first arena, which is mapped
//! before `main`, and `allow_large_os_pages` is hugetlb pages alone, which blaze refuses
//! below). With the feature its arenas are advised nothing, and blaze-mem keeps unadvised
//! memory off huge pages whatever the host says (`PR_THP_DISABLE_EXCEPT_ADVISED`, Linux 6.18,
//! `blaze_mem::os`); on an older kernel whose host setting is `always` they follow the host.
//!
//! Why: mimalloc hands out 64 KiB pages and gives them back one at a time; on a huge page the
//! first touch of one makes 2 MiB resident, and giving 64 KiB back leaves the rest resident
//! though mimalloc counts none of it. That was 1.1 to 1.2 GiB of a hello-world daemon for
//! nothing it holds. What huge pages bought was a page fault a 2 MiB as an edit's build takes
//! again what the last one freed (an edit of hello-world allocates and frees 95 MiB of the
//! global heap). Measured 2026-11-12 (lane memstrat-1111, Linux aarch64 in an ix VM of 32
//! cores, home and project on tmpfs, a hello-world daemon after a cold build and five edits,
//! the two binaries alternated, each cell one run's median):
//!
//! | mimalloc's arenas | resident | of it huge pages | body edit and run, a daemon's first 40 | 3.5 s after the last | 8 s after the last | a daemon's edits 96 to 135 |
//! |---|---|---|---|---|---|---|
//! | on transparent huge pages | 5,397, 5,460, 5,325, 5,433 MB | 1,180 to 1,240 | 101.7, 111.2, 113.3, 109.8 ms | 98.6, 117.7, 119.9, 117.1 | 106.2, 107.5, 121.7, 126.8 | 207.0, 210.0 |
//! | built with `no_thp` | 4,143, 4,159, 4,190, 4,192 MB | 76 to 88 | 111.0, 109.7, 115.5, 115.8 ms | 114.4, 117.4, 117.2, 111.5 | 120.2, 127.0, 122.6, 117.7 | 266.4, 251.5 |
//!
//! (n = 40 an edit row, 20 to 30 for 3.5 s and 6 to 20 for 8 s; the last column is of the two
//! later pairs only.) So 1.25 GiB less, with a daemon's first edits and its edits after a
//! pause within the runs' spread. The price is in the last column: an edit of a daemon that
//! has taken a hundred edits is 85% slower than its first on either build (113 to 208 ms on
//! huge pages; why was not looked into, and it is the edit path's to find), and on base
//! pages a quarter slower again. Until that growth is gone this is what `no_thp`
//! costs a long session; if it stays, the next step is the general heap's blocks of a megabyte
//! or more taken from blaze-mem's warm chunks (which are on huge pages and go back whole), not
//! huge pages under mimalloc again.
//!
//! The day before (lane mem2-1111) the same pair read as 17% of an edit 3.5 s after the last.
//! Why the two differ was not found; a guess: this lane's rows moved by more than that with
//! the VM's own disk under the home and while its memory was still being added, until both
//! were taken out (`agents.json`, memstrat-h1). Kept out, measured the same day: a purge
//! delay with the idle purge (300 MB more held, no row of a daemon's first edits faster).
//! macOS has no huge pages for anonymous memory (`blaze_mem::huge`), so none of this is its.

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

    /// The guard on `no_thp`: an arena of mimalloc's is not advised onto transparent huge
    /// pages. A block of 8 MiB is an arena's; its mapping's `VmFlags` in `/proc/self/smaps`
    /// carry `hg` exactly when something advised it `MADV_HUGEPAGE`, which mimalloc built
    /// without the feature does to every arena.
    #[cfg(target_os = "linux")]
    #[test]
    fn mimalloc_advises_no_arena_onto_transparent_huge_pages() {
        const BYTES: usize = 8 << 20;
        // SAFETY: a block of mimalloc's, written, read and freed once, here.
        let block = unsafe { libmimalloc_sys::mi_malloc(BYTES) }.cast::<u8>();
        assert!(!block.is_null(), "mimalloc gave no block of {BYTES} bytes");
        // SAFETY: `block` is `BYTES` long and this test's alone.
        unsafe { block.write_bytes(1, BYTES) };
        let at = block.addr() + BYTES / 2;
        let maps = std::fs::read_to_string("/proc/self/smaps").expect("/proc/self/smaps");
        let mut inside = false;
        let mut flags = None;
        for line in maps.lines() {
            let range = line.split(' ').next().and_then(|it| it.split_once('-')).and_then(|(from, to)| {
                Some((usize::from_str_radix(from, 16).ok()?, usize::from_str_radix(to, 16).ok()?))
            });
            if let Some((from, to)) = range {
                inside = (from..to).contains(&at);
            } else if inside && let Some(rest) = line.strip_prefix("VmFlags:") {
                flags = Some(rest.to_owned());
            }
        }
        // SAFETY: the block, freed once.
        unsafe { libmimalloc_sys::mi_free(block.cast()) };
        let flags = flags.expect("the block's mapping is in /proc/self/smaps with its VmFlags");
        assert!(
            !flags.split(' ').any(|it| it == "hg"),
            "mimalloc's arena is advised MADV_HUGEPAGE (VmFlags:{flags}): libmimalloc-sys is built without `no_thp`"
        );
    }
}
