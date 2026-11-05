//! The general allocator on the pages `blaze-mem`'s regions are on: memo values (bodies,
//! inference, MIR) are read at random through pointers, so on transparent huge pages most of
//! those reads skip a page walk. A reserved hugetlb pool is `blaze-mem`'s alone; mimalloc stays
//! on base pages then.

#![allow(unsafe_code, reason = "sets one mimalloc option word")]

use blaze_mem::huge::{self, Pages};

/// Sets mimalloc's huge page options from `BLAZE_HUGE_PAGES`, every one of them, so neither
/// mimalloc's defaults nor its `MIMALLOC_*` variables decide: segments on transparent huge pages
/// (`MADV_HUGEPAGE`, which still counts under the process's opt-out of unadvised huge pages,
/// [`huge::Thp::AdvisedOnly`]) when they are chosen, base pages otherwise, and never hugetlb
/// pages reserved at start (`reserve_huge_os_pages`), which belong to blaze-mem's pool alone.
/// Measured: with segments on transparent huge pages under the host setting `madvise`, the
/// daemon peaked at 25 GiB where `always` without the opt-out reached 60. Called once, before
/// the first query. An unreadable setting is an error naming it.
pub(crate) fn huge_pages_for_mimalloc() -> std::io::Result<()> {
    let transparent = huge::chosen()? == Pages::Transparent;
    // SAFETY: each sets an option word mimalloc reads when it next maps a segment; mimalloc
    // documents options as settable from any thread at any time.
    unsafe {
        libmimalloc_sys::mi_option_set_enabled(
            libmimalloc_sys::mi_option_large_os_pages,
            transparent,
        );
        libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_reserve_huge_os_pages, 0);
        libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_reserve_huge_os_pages_at, -1);
    }
    Ok(())
}

/// Whether mimalloc maps its segments on huge pages.
#[cfg(test)]
fn enabled() -> bool {
    // SAFETY: reads an option word; callable from any thread.
    unsafe { libmimalloc_sys::mi_option_is_enabled(libmimalloc_sys::mi_option_large_os_pages) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mimalloc_follows_the_pages_chosen() {
        huge_pages_for_mimalloc().expect("BLAZE_HUGE_PAGES is readable");
        let chosen = huge::chosen().expect("chosen once already");
        if chosen == Pages::Transparent {
            assert!(enabled(), "transparent huge pages are chosen, so mimalloc maps on them");
        }
        assert_eq!(
            enabled(),
            chosen == Pages::Transparent,
            "mimalloc's huge pages follow the choice, set either way"
        );
        if std::env::var_os(huge::VAR).is_some_and(|it| it == "off") {
            assert_eq!(chosen, Pages::Base);
            assert!(!enabled(), "BLAZE_HUGE_PAGES=off leaves mimalloc on base pages");
        }
    }
}
