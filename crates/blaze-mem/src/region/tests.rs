//! A sparse region counted by ranges, as the store's buffer pool uses it
//! (docs/v2/plan/design/store-pool.md lane 1): the pool's reservation of terabytes, the kernel's
//! give-back of its pages, and its tag following every reuse and eviction.

use std::ops::Range;

use hegel::{TestCase, generators as gs};

use crate::{Region, Tag, racy, tag};

/// The pool's page (`pool::PAGE` in blaze-cas): 64 KiB, a whole number of base pages on every
/// system blaze runs on.
const PAGE: usize = 64 << 10;

/// The pool's default reservation: 4 TiB, 4096 windows of 1 GiB.
const POOL_RESERVATION: usize = 4 << 40;

fn write_page(region: &Region, at: usize, byte: u8) {
    let bytes = vec![byte; PAGE]; // alloc: a test's page of bytes
    // SAFETY: `[at, at + PAGE)` is a page of the live sparse region this test reused and
    // nothing else references.
    unsafe { racy::copy_in(region.at(at).as_ptr(), &bytes) };
}

/// The page's first and last word, read atomically.
fn ends_of(region: &Region, at: usize) -> [u8; 16] {
    let mut out = [0; 16];
    // SAFETY: inside the live sparse region, every byte of which is readable; the copy is atomic.
    unsafe { racy::copy_out(region.at(at).as_ptr(), &mut out[..8]) };
    // SAFETY: as above.
    unsafe { racy::copy_out(region.at(at + PAGE - 8).as_ptr(), &mut out[8..]) };
    out
}

/// Which of the base pages of `[at, at + len)` are resident, by `mincore`.
#[cfg(target_os = "linux")]
fn resident(region: &Region, at: usize, len: usize) -> usize {
    let page = crate::page_size();
    let mut pages = vec![0u8; len / page]; // alloc: a test's residency vector
    // SAFETY: the range is page-aligned inside the live region; `pages` holds one byte a page.
    let rc = unsafe { libc::mincore(region.at(at).as_ptr().cast(), len, pages.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore: {}", std::io::Error::last_os_error());
    pages.iter().filter(|&&it| it & 1 != 0).count()
}

/// The pool's reservation is granted, costs nothing until touched, and its pages anywhere in it
/// (the first, one 2 TiB in, the last) are made the process's, hold what is written, and go back
/// in one call. Run on macOS and Linux, where the pool reserves this much at the store's open.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_pools_four_tib_reservation_is_granted_and_gives_pages_back() {
    let region = Region::reserve_sparse(POOL_RESERVATION, Tag::Other)
        .unwrap_or_else(|error| panic!("the store pool's 4 TiB reservation: {error}"));
    assert_eq!(region.reserved(), POOL_RESERVATION);
    assert_eq!((region.committed(), region.held()), (0, 0), "a reservation holds nothing");
    let pages = [0, POOL_RESERVATION / 2, POOL_RESERVATION - PAGE];
    for (n, &at) in pages.iter().enumerate() {
        // SAFETY: inside the live region, every byte of which reads zero until written.
        assert_eq!(unsafe { region.at(at).as_ptr().read_volatile() }, 0, "untouched reads zero");
        region.reuse(at, PAGE).expect("reuse a page of the reservation");
        write_page(&region, at, n as u8 + 1);
    }
    assert_eq!(region.held(), 3 * PAGE);
    for (n, &at) in pages.iter().enumerate() {
        assert_eq!(ends_of(&region, at), [n as u8 + 1; 16], "page at {at} keeps its bytes");
    }
    #[cfg(target_os = "linux")]
    for &at in &pages {
        assert_eq!(resident(&region, at, PAGE), PAGE / crate::page_size(), "written, resident");
    }
    let ranges: Vec<Range<usize>> = pages.iter().map(|&at| at..at + PAGE).collect(); // alloc: a test's ranges
    // SAFETY: the three pages this test reused, which nothing references.
    let given = unsafe { region.evict(&ranges) }.expect("this kernel gives pages back");
    assert_eq!(given, 3 * PAGE, "every page given back");
    assert_eq!(region.held(), 0);
    #[cfg(target_os = "linux")]
    for &at in &pages {
        assert_eq!(resident(&region, at, PAGE), 0, "MADV_DONTNEED dropped the page at {at}");
        assert_eq!(ends_of(&region, at), [0; 16], "a page given back reads zeros on Linux");
    }
}

/// The kernel takes the pool's give-back of its own pages: on Linux `process_madvise` of this
/// process with `MADV_DONTNEED` (6.13 for any advice to the caller's own process, 6.14 for
/// `PIDFD_SELF_THREAD_GROUP`), on macOS `MADV_FREE_REUSABLE`. A kernel that refuses fails here
/// naming the call and its release, as the store's open would.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_kernel_gives_a_sparse_regions_pages_back() {
    if let Err(why) = crate::os::check_evict() {
        panic!("{why}");
    }
    let region = Region::reserve_sparse(64 * PAGE, Tag::Other).expect("reserve");
    // Two runs with a gap, one call: the batch the evictor hands the kernel.
    region.reuse(0, 4 * PAGE).expect("reuse");
    region.reuse(8 * PAGE, 2 * PAGE).expect("reuse");
    for at in (0..4).chain(8..10) {
        write_page(&region, at * PAGE, 0x5A);
    }
    // SAFETY: the pages just reused, which nothing references.
    let given = unsafe { region.evict(&[0..4 * PAGE, 8 * PAGE..10 * PAGE]) }.expect("evict");
    assert_eq!(given, 6 * PAGE);
    #[cfg(target_os = "linux")]
    {
        assert_eq!(resident(&region, 0, 10 * PAGE), 0, "the batch left the resident set");
        assert_eq!(ends_of(&region, 0), [0; 16]);
    }
}

/// One step of [`the_tag_follows_reuses_and_evictions`].
#[derive(Debug)]
enum Step {
    /// Reuse the run of unheld pages from `page`, at most `len` of them.
    Reuse { page: usize, len: usize },
    /// Evict every held page in `[page, page + len)`, merged into runs, in one call.
    Evict { page: usize, len: usize },
}

const PAGES: usize = 48;

#[hegel::composite]
fn step(tc: &TestCase) -> Step {
    let page = tc.draw(gs::integers::<usize>().max_value(PAGES - 1));
    let len = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    if tc.draw(gs::booleans()) { Step::Reuse { page, len } } else { Step::Evict { page, len } }
}

/// Every reuse adds its bytes to the region's tag and every eviction takes off what the kernel
/// gave back, so the tag holds exactly the held pages (oracle 4 of store-pool.md's DST,
/// `Tag::Pool` equals the loaded pages times `PAGE`, for the layer below the pool); held pages
/// keep their bytes across other pages' evictions; on Linux a page given back reads zeros; and
/// dropping the region takes the rest off. The only test that moves `Tag::Pool`.
#[hegel::test(test_cases = 64)]
fn the_tag_follows_reuses_and_evictions(tc: TestCase) {
    let before = tag::snapshot().get(Tag::Pool).live;
    let region = Region::reserve_sparse(PAGES * PAGE, Tag::Pool).expect("reserve");
    let mut held: [Option<u8>; PAGES] = [None; PAGES];
    let steps = tc.draw(gs::vecs(step()).max_size(40));
    for (n, step) in steps.into_iter().enumerate() {
        match step {
            Step::Reuse { page, len } => {
                let run = (page..(page + len).min(PAGES)).take_while(|&it| held[it].is_none()).count();
                if run == 0 {
                    continue;
                }
                region.reuse(page * PAGE, run * PAGE).expect("reuse");
                let byte = (n % 250) as u8 + 1;
                for it in page..page + run {
                    write_page(&region, it * PAGE, byte);
                    held[it] = Some(byte);
                }
            }
            Step::Evict { page, len } => {
                let mut ranges: Vec<Range<usize>> = Vec::new(); // alloc: a test's ranges
                for it in page..(page + len).min(PAGES) {
                    if held[it].is_none() {
                        continue;
                    }
                    match ranges.last_mut() {
                        Some(last) if last.end == it * PAGE => last.end += PAGE,
                        _ => ranges.push(it * PAGE..(it + 1) * PAGE),
                    }
                    held[it] = None;
                }
                let asked: usize = ranges.iter().map(Range::len).sum();
                // SAFETY: each range is pages this test reused and has not given back, which
                // nothing references.
                let given = unsafe { region.evict(&ranges) }.expect("evict");
                assert_eq!(given, asked, "the kernel gave back every page of {ranges:?}");
            }
        }
        let holds = held.iter().filter(|it| it.is_some()).count() * PAGE;
        assert_eq!(region.held(), holds, "the region's held bytes after {n} steps");
        assert_eq!(
            tag::snapshot().get(Tag::Pool).live,
            before + holds as u64,
            "Tag::Pool after {n} steps"
        );
        for (it, byte) in held.iter().enumerate() {
            match byte {
                Some(byte) => assert_eq!(ends_of(&region, it * PAGE), [*byte; 16], "page {it}"),
                None if cfg!(target_os = "linux") => {
                    assert_eq!(ends_of(&region, it * PAGE), [0; 16], "page {it}, not held");
                }
                None => {}
            }
        }
    }
    drop(region);
    assert_eq!(tag::snapshot().get(Tag::Pool).live, before, "dropping the region takes its pages off");
}

#[test]
#[should_panic(expected = "counted both")]
fn a_sparse_region_counted_by_its_prefix_refuses_a_range() {
    let region = Region::reserve_sparse(16 * PAGE, Tag::Other).expect("reserve");
    region.ensure(100).expect("commit a prefix");
    let _ = region.reuse(4 * PAGE, PAGE);
}

#[test]
#[should_panic(expected = "counted both")]
fn a_sparse_region_counted_by_ranges_refuses_a_prefix() {
    let region = Region::reserve_sparse(16 * PAGE, Tag::Other).expect("reserve");
    region.reuse(4 * PAGE, PAGE).expect("reuse");
    let _ = region.ensure(100);
}

#[test]
#[should_panic(expected = "must be whole pages")]
fn a_range_not_of_whole_pages_is_refused() {
    let region = Region::reserve_sparse(16 * PAGE, Tag::Other).expect("reserve");
    let _ = region.reuse(100, PAGE);
}

#[test]
#[should_panic(expected = "never reused")]
fn evicting_more_than_is_held_is_refused() {
    let region = Region::reserve_sparse(16 * PAGE, Tag::Other).expect("reserve");
    region.reuse(0, PAGE).expect("reuse");
    // SAFETY: nothing references the region; the call panics before the kernel sees it.
    let _ = unsafe { region.evict(&[0..2 * PAGE]) };
}

#[test]
#[should_panic(expected = "not sparse")]
fn evicting_from_a_region_that_is_not_sparse_is_refused() {
    let region = Region::reserve(16 * PAGE, Tag::Other).expect("reserve");
    region.ensure(PAGE).expect("commit");
    // SAFETY: nothing references the region; the call panics before the kernel sees it.
    let _ = unsafe { region.evict(&[0..PAGE]) };
}
