//! Each allocator against a model (`Vec`, `Box`, `Vec<Option<_>>`) on seeded random operations,
//! the properties its design promises (no move on growth, stale handles detected, rollback,
//! reuse), and its allocation budget with `blaze-alloc-count`: warm, none of them calls the
//! global allocator.

use std::{
    alloc::Layout,
    cell::Cell,
    rc::Rc,
    sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
    thread,
};

use blaze_alloc_count::measure;

use crate::{
    AppendVec, Arena, Block, Built, Column, Frame, HUGE_PAGE, Handle, MemError, Pool, Region,
    RegionVec, Revisions, Slab, Tag, chunks, frame,
    huge::{self, Pages},
    pool,
    revision::PublishError,
    scratch, tag,
};

/// xorshift64*: the tests' operations, the same on every run. Not `blaze_ids::rng`, since
/// `blaze-ids` depends on this crate.
struct Xorshift64Star(u64);

impl Xorshift64Star {
    fn new(seed: u64) -> Xorshift64Star {
        Xorshift64Star(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Counts its drops, to check each value is dropped exactly once.
struct Counted {
    value: u64,
    drops: Rc<Cell<usize>>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

// Region

#[test]
fn region_commits_on_growth_and_never_moves() {
    let region = Region::reserve(64 << 20, Tag::Other).expect("reserve");
    let base = region.base();
    assert_eq!(region.committed(), 0);
    region.ensure(100).expect("commit");
    assert!(region.committed() >= 100);
    let first = region.at(0).as_ptr();
    // SAFETY: the first 100 bytes are committed and this test owns them.
    unsafe { first.write_bytes(0xAB, 100) };
    region.ensure(32 << 20).expect("commit more");
    assert_eq!(region.base(), base, "growing never moves the base");
    // SAFETY: committed earlier, still committed, written above.
    assert_eq!(unsafe { first.read() }, 0xAB, "growing keeps what was written");
    assert!(region.committed() >= 32 << 20);
}

#[test]
fn region_past_its_reservation_is_an_error_naming_the_tag() {
    let region = Region::reserve(1 << 20, Tag::Link).expect("reserve");
    let error = region.ensure(region.reserved() + 1).expect_err("past the reservation");
    assert!(matches!(error, MemError::Exhausted { tag: Tag::Link, .. }), "{error}");
    assert!(error.to_string().starts_with("link: "), "{error}");
}

#[test]
fn region_decommit_after_gives_pages_back_and_commits_again() {
    let mut region = Region::reserve(16 << 20, Tag::Other).expect("reserve");
    region.ensure(8 << 20).expect("commit");
    region.decommit_after(1 << 20).expect("decommit");
    assert!(region.committed() < 8 << 20);
    region.ensure(4 << 20).expect("commit again");
    let at = region.at(3 << 20).as_ptr();
    // SAFETY: committed again just now.
    unsafe { at.write(7) };
    // SAFETY: as above.
    assert_eq!(unsafe { at.read() }, 7);
}

#[test]
fn regions_grow_from_many_threads_at_once() {
    let region = Arc::new(Region::reserve(256 << 20, Tag::Other).expect("reserve"));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let region = Arc::clone(&region);
            thread::spawn(move || {
                for step in 0..64 {
                    let len = (t * 64 + step + 1) * (256 << 10);
                    region.ensure(len).expect("commit");
                    let at = region.at(len - 1).as_ptr();
                    // SAFETY: the byte is committed; each thread writes the last byte of its
                    // own lengths, which other threads write too only with the same value.
                    unsafe { at.write(1) };
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("grower");
    }
    assert!(region.committed() >= 8 * 64 * (256 << 10));
}

/// A length another thread committed is counted by the time `ensure` sees it: else a taker that
/// moves those bytes to another tag (`chunks::take`) takes the region's tag below zero, and
/// the grower's count then overflows.
#[test]
fn a_region_counts_what_it_commits_before_another_thread_sees_it() {
    let region = Arc::new(Region::reserve(256 << 20, Tag::Fuzz).expect("reserve"));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let region = Arc::clone(&region);
            thread::spawn(move || {
                for step in 0..64 {
                    let len = (step * 8 + t + 1) * (64 << 10);
                    region.ensure(len).expect("commit");
                    let live = tag::snapshot().get(Tag::Fuzz).live;
                    assert!(live >= len as u64, "{len} bytes committed, {live} counted");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("grower");
    }
}

// Huge pages

/// Writes byte `i % 251` at each offset `i` of `[from, to)` of `region`.
fn fill(region: &Region, from: usize, to: usize) {
    for i in from..to {
        // SAFETY: the callers commit `[0, to)` first, and this test owns the region.
        unsafe { region.at(i).as_ptr().write((i % 251) as u8) };
    }
}

/// Whether every byte of `[from, to)` is what [`fill`] wrote.
fn filled(region: &Region, from: usize, to: usize) -> bool {
    // SAFETY: as for `fill`, and each byte was written there.
    (from..to).all(|i| unsafe { region.at(i).as_ptr().read() } == (i % 251) as u8)
}

#[test]
fn a_small_huge_region_commits_base_pages() {
    let region = Region::reserve_pages(64 << 20, Pages::Transparent, Tag::Other).expect("reserve");
    assert_eq!(region.base().as_ptr().addr() % HUGE_PAGE, 0, "a huge region starts on a huge page");
    assert_eq!(region.reserved() % HUGE_PAGE, 0, "and ends on one");
    region.ensure(100).expect("commit");
    assert!(
        region.committed() >= 100 && region.committed() < HUGE_PAGE,
        "a small huge region is a few base pages, not a huge page: {}",
        region.committed()
    );
}

#[test]
fn a_huge_region_past_one_huge_page_commits_whole_ones_and_keeps_its_bytes() {
    for pages in [Pages::Base, Pages::Transparent] {
        let mut region = Region::reserve_pages(64 << 20, pages, Tag::Other).expect("reserve");
        region.ensure(100 << 10).expect("commit");
        fill(&region, 0, 100 << 10);
        for len in [HUGE_PAGE + 1, 5 << 20, 33 << 20] {
            region.ensure(len).expect("commit more");
            assert!(region.committed() >= len);
            if pages.is_huge() {
                assert_eq!(region.committed() % HUGE_PAGE, 0, "{pages:?}: {}", region.committed());
            }
        }
        assert!(filled(&region, 0, 100 << 10), "{pages:?}: crossing a huge page kept the bytes");
        fill(&region, 0, 33 << 20);
        region.decommit_after(HUGE_PAGE + 1).expect("decommit");
        if pages.is_huge() {
            assert_eq!(region.committed(), 2 * HUGE_PAGE, "a decommit keeps whole huge pages");
        }
        assert!(filled(&region, 0, HUGE_PAGE + 1), "{pages:?}: the kept bytes stay");
        region.decommit_after(1).expect("decommit to the first pages");
        assert!(region.committed() < HUGE_PAGE);
        region.ensure(3 << 20).expect("commit again");
        // SAFETY: committed again just now; decommitted pages come back zeroed.
        assert_eq!(unsafe { region.at((3 << 20) - 1).as_ptr().read() }, 0);
    }
}

#[test]
fn huge_regions_grow_from_many_threads_at_once() {
    let region = Arc::new(Region::reserve_pages(256 << 20, Pages::Transparent, Tag::Other).expect("reserve"));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let region = Arc::clone(&region);
            thread::spawn(move || {
                for step in 0..64 {
                    let len = (t * 64 + step + 1) * (64 << 10);
                    region.ensure(len).expect("commit");
                    // SAFETY: the byte is committed; every thread writes 1 to it.
                    unsafe { region.at(len - 1).as_ptr().write(1) };
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("grower");
    }
    assert!(region.committed() >= 8 * 64 * (64 << 10));
    assert_eq!(region.committed() % HUGE_PAGE, 0);
}

#[test]
fn a_huge_region_hit_allocates_nothing() {
    let region = Region::reserve_pages(64 << 20, Pages::Transparent, Tag::Other).expect("reserve");
    region.ensure(9 << 20).expect("commit");
    let (_, used) = measure(|| {
        for len in (1..=9 << 20).step_by(4093) {
            region.ensure(len).expect("committed already");
        }
    });
    assert_eq!(used.allocations, 0);
}

#[test]
fn the_huge_pages_setting_is_read_or_named_in_its_error() {
    use std::ffi::OsStr;
    assert_eq!(huge::choose(Some(OsStr::new("off"))).expect("off"), Pages::Base);
    let error = huge::choose(Some(OsStr::new("always"))).expect_err("not a setting");
    assert!(error.to_string().contains("BLAZE_HUGE_PAGES=\"always\""), "{error}");
    let default = huge::choose(None).expect("the default");
    let offered = if crate::os::transparent_offered() { Pages::Transparent } else { Pages::Base };
    assert_eq!(default, offered, "the default is what the system offers with no setup");
    assert_eq!(huge::choose(Some(OsStr::new("transparent"))).expect("transparent"), default);
}

/// A number from a sysfs file, or 0 when it has none.
#[cfg(target_os = "linux")]
fn sysfs_number(path: &str) -> u64 {
    std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse().ok()).unwrap_or(0)
}

/// The `AnonHugePages` of the mapping holding `addr`, in KiB, from `/proc/self/smaps`.
#[cfg(target_os = "linux")]
fn anon_huge_kib(addr: usize) -> u64 {
    let smaps = std::fs::read_to_string("/proc/self/smaps").expect("smaps");
    let mut inside = false;
    for line in smaps.lines() {
        let range = line.split_whitespace().next().and_then(|range| range.split_once('-'));
        if let Some((start, end)) = range
            && let (Ok(start), Ok(end)) = (usize::from_str_radix(start, 16), usize::from_str_radix(end, 16))
        {
            inside = (start..end).contains(&addr);
        } else if inside && let Some(kib) = line.strip_prefix("AnonHugePages:") {
            return kib.trim().trim_end_matches("kB").trim().parse().expect("a size in kB");
        }
    }
    0
}

#[cfg(target_os = "linux")]
#[test]
fn a_transparent_region_is_made_of_huge_pages_on_linux() {
    if !crate::os::transparent_offered() {
        assert_eq!(huge::choose(None).expect("the default"), Pages::Base, "no THP, base pages");
        return;
    }
    let region = Region::reserve_pages(64 << 20, Pages::Transparent, Tag::Other).expect("reserve");
    region.ensure(16 << 20).expect("commit");
    // SAFETY: `[0, 16 MiB)` is committed and this test owns it.
    unsafe { region.base().as_ptr().write_bytes(1, 16 << 20) };
    let kib = anon_huge_kib(region.base().as_ptr().addr());
    assert!(
        kib >= 2 << 10,
        "16 MiB of a transparent region touched, {kib} KiB on huge pages: the kernel made none \
         (fragmented memory, or `defrag` set to `never`)"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_reserved_region_takes_pool_pages_or_names_the_pool() {
    let pool = "/sys/kernel/mm/hugepages/hugepages-2048kB";
    let total = sysfs_number(&format!("{pool}/nr_hugepages"));
    let free = sysfs_number(&format!("{pool}/free_hugepages"));
    let mut region = match Region::reserve_pages(64 << 20, Pages::Reserved, Tag::Other) {
        Ok(region) => region,
        Err(error) => {
            assert_eq!(total, 0, "a pool of {total} pages is there: {error}");
            assert!(error.to_string().contains("nr_hugepages"), "the error names the pool: {error}");
            return;
        }
    };
    region.ensure(100 << 10).expect("the first pages are base pages, outside the pool");
    fill(&region, 0, 100 << 10);
    if free < 2 {
        let error = region.ensure(3 * HUGE_PAGE).expect_err("the pool has too few free pages");
        assert!(error.to_string().contains("MAP_HUGETLB"), "the error names the pool: {error}");
        assert!(region.committed() < HUGE_PAGE, "a refused commit commits nothing");
        assert!(filled(&region, 0, 100 << 10));
        return;
    }
    region.ensure(3 * HUGE_PAGE).expect("two pages from the pool");
    assert_eq!(region.committed(), 3 * HUGE_PAGE);
    assert!(filled(&region, 0, 100 << 10), "the first pages kept their bytes");
    fill(&region, 0, 3 * HUGE_PAGE);
    assert!(filled(&region, 0, 3 * HUGE_PAGE));
    region.decommit_after(HUGE_PAGE + 1).expect("give a pool page back");
    assert_eq!(region.committed(), 2 * HUGE_PAGE);
    region.ensure(3 * HUGE_PAGE).expect("take it again");
    assert!(filled(&region, 0, 2 * HUGE_PAGE), "the kept pages kept their bytes");
    // SAFETY: committed again just now.
    assert_eq!(unsafe { region.at(2 * HUGE_PAGE + 5).as_ptr().read() }, 0, "a page made again is zeroed");
    // SAFETY: nothing reads the purged range before writing it; pool pages are kept.
    unsafe { region.purge(0, 3 * HUGE_PAGE) };
    assert!(filled(&region, HUGE_PAGE, 2 * HUGE_PAGE), "a purge keeps the pool's pages");
    region.decommit_after(0).expect("give everything back");
    assert_eq!(region.committed(), 0);
    let region = Arc::new(region);
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let region = Arc::clone(&region);
            thread::spawn(move || {
                for step in 0..8 {
                    // Up to 32 × 192 KiB, three huge pages.
                    let len = (t * 8 + step + 1) * (192 << 10);
                    region.ensure(len).expect("commit");
                    // SAFETY: committed; every thread writes 1 to it.
                    unsafe { region.at(len - 1).as_ptr().write(1) };
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("grower");
    }
    // SAFETY: committed by the growers, who wrote it.
    assert_eq!(unsafe { region.at(3 * HUGE_PAGE - 1).as_ptr().read() }, 1, "no grower mapped over another's");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn apple_silicon_has_no_superpages_for_anonymous_memory() {
    // SAFETY: a fresh anonymous mapping at an address the kernel picks, unmapped below if made.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            HUGE_PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            libc::VM_FLAGS_SUPERPAGE_SIZE_2MB,
            0,
        )
    };
    if at != libc::MAP_FAILED {
        // SAFETY: the mapping just made, which nothing else knows of.
        unsafe { libc::munmap(at, HUGE_PAGE) };
    }
    assert_eq!(
        at,
        libc::MAP_FAILED,
        "macOS made a 2 MiB superpage on arm64: blaze-mem's huge regions should use it (huge.rs)"
    );
    assert_eq!(huge::choose(None).expect("the default"), Pages::Base);
}

// RegionVec

#[test]
fn region_vec_matches_vec_on_random_operations() {
    let mut rng = Xorshift64Star::new(1);
    let mut column = RegionVec::<u64>::new(1 << 20, Tag::Other).expect("reserve");
    let mut model = Vec::new();
    for _ in 0..200_000 {
        match rng.below(10) {
            0..=5 => {
                let value = rng.next();
                assert_eq!(column.push(value), model.len());
                model.push(value);
            }
            6 => assert_eq!(column.pop(), model.pop()),
            7 => {
                let len = rng.below(model.len() as u64 + 1) as usize;
                column.truncate(len);
                model.truncate(len);
            }
            8 => {
                let values: Vec<u64> = (0..rng.below(100)).map(|_| rng.next()).collect();
                column.extend_from_slice(&values);
                model.extend_from_slice(&values);
            }
            _ => {
                if !model.is_empty() {
                    let i = rng.below(model.len() as u64) as usize;
                    let value = rng.next();
                    column[i] = value;
                    model[i] = value;
                }
            }
        }
        assert_eq!(column.len(), model.len());
    }
    assert_eq!(column.as_slice(), model.as_slice());
}

#[test]
fn region_vec_addresses_are_stable_across_growth() {
    let mut column = RegionVec::<[u8; 24]>::new(1 << 22, Tag::Other).expect("reserve");
    column.push([1; 24]);
    let first = column.as_ptr_at(0).expect("pushed");
    for i in 0..1_000_000u32 {
        column.push([i as u8; 24]);
    }
    assert_eq!(column.as_ptr_at(0), Some(first), "growth never moves an element");
    assert_eq!(column[0], [1; 24]);
}

#[test]
fn region_vec_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let mut column = RegionVec::new(1000, Tag::Other).expect("reserve");
    for value in 0..100 {
        column.push(Counted { value, drops: Rc::clone(&drops) });
    }
    column.truncate(60);
    assert_eq!(drops.get(), 40);
    let last = column.pop().expect("one");
    assert_eq!(last.value, 59);
    drop(last);
    assert_eq!(drops.get(), 41);
    drop(column);
    assert_eq!(drops.get(), 100);
}

#[test]
fn region_vec_full_is_an_error() {
    let mut column = RegionVec::<u64>::new(8, Tag::Other).expect("reserve");
    let max = column.max_len();
    for i in 0..max {
        column.try_push(i as u64).expect("room");
    }
    assert!(matches!(column.try_push(0), Err(MemError::Exhausted { .. })));
}

#[test]
fn region_vec_push_allocates_nothing() {
    let mut column = RegionVec::<u64>::new(1 << 24, Tag::Other).expect("reserve");
    let (_, used) = measure(|| {
        for i in 0..1_000_000 {
            column.push(i);
        }
    });
    assert_eq!(used.allocations, 0, "a region grows by committing pages, not by the allocator");
}

// Column

#[test]
fn column_takes_pushes_from_many_threads_and_reads_them_in_place() {
    let column = Arc::new(Column::<u64>::new(1 << 20, Tag::Intern).expect("reserve"));
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let column = Arc::clone(&column);
            thread::spawn(move || {
                let mut ids = Vec::with_capacity(10_000);
                for i in 0..10_000u64 {
                    let id = column.push(t << 32 | i).expect("room");
                    assert_eq!(column.get(id), Some(&(t << 32 | i)), "readable once pushed");
                    ids.push(id);
                }
                ids
            })
        })
        .collect();
    let mut seen = vec![false; 80_000];
    for thread in threads {
        for id in thread.join().expect("pusher") {
            assert!(!std::mem::replace(&mut seen[id as usize], true), "each index once");
        }
    }
    assert!(seen.iter().all(|&s| s));
    let mut model: Vec<u64> =
        (0..8u64).flat_map(|t| (0..10_000u64).map(move |i| t << 32 | i)).collect();
    let mut got: Vec<u64> = column.iter().map(|(_, &v)| v).collect();
    model.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, model);
    assert_eq!(column.get(90_000), None, "an index never pushed is not readable");
}

#[test]
fn column_blocks_fill_in_order_and_leave_holes_unreadable() {
    let column = Column::<u64>::new(1_000, Tag::Other).expect("reserve");
    let mut a = column.claim(64).expect("room");
    let mut b = column.claim(64).expect("room");
    assert_eq!(column.fill(&mut b, 7).expect("room"), 64);
    assert_eq!(column.fill(&mut a, 3).expect("room"), 0);
    assert_eq!((column.get(64), column.get(0), column.get(1)), (Some(&7), Some(&3), None));
    assert_eq!((a.remaining(), b.remaining()), (63, 63));
    assert_eq!(column.len(), 128);
    assert_eq!(column.push(9).expect("room"), 128);
    assert!(column.claim(1_000).is_err());
    assert!(column.claim(u32::MAX).is_err(), "a claim past any length is refused, not wrapped");
    assert!(Block::EMPTY.is_spent());
}

#[test]
#[should_panic(expected = "only the column that claimed it")]
fn column_block_fills_only_its_column() {
    let one = Column::<u64>::new(64, Tag::Other).expect("reserve");
    let two = Column::<u64>::new(64, Tag::Other).expect("reserve");
    let mut block = one.claim(4).expect("room");
    let _ = two.fill(&mut block, 1);
}

#[test]
fn column_threads_filling_their_own_blocks_publish_every_value_once() {
    let column = Column::<u64>::new(1 << 16, Tag::Other).expect("reserve");
    thread::scope(|scope| {
        for worker in 0..8u64 {
            let column = &column;
            scope.spawn(move || {
                let mut block = Block::EMPTY;
                for n in 0..2_000u64 {
                    if block.is_spent() {
                        block = column.claim(64).expect("room");
                    }
                    let index = column.fill(&mut block, worker << 32 | n).expect("room");
                    assert_eq!(column.get(index), Some(&(worker << 32 | n)));
                }
            });
        }
    });
    let mut seen: Vec<u64> = column.iter().map(|(_, &value)| value).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 16_000);
}

#[test]
fn column_get_mut_changes_a_published_value() {
    let mut column = Column::<u64>::new(1 << 10, Tag::Query).expect("reserve");
    let index = column.push(1).expect("room");
    *column.get_mut(index).expect("published") = 9;
    assert_eq!(column.get(index), Some(&9));
    assert!(column.get_mut(index + 1).is_none());
}

/// Pushes, claims and fills in a seeded order against a `Vec<Option<_>>`: a ready byte per
/// value makes exactly the filled indices readable, holes and indices past the end not, and
/// pushing allocates nothing from the global allocator.
#[test]
fn column_matches_a_model_of_options() {
    let column = Column::<u64>::new(1 << 14, Tag::Other).expect("reserve");
    let mut model: Vec<Option<u64>> = Vec::with_capacity(1 << 14);
    let mut blocks: Vec<Block> = Vec::with_capacity(64);
    let mut rng = Xorshift64Star::new(31);
    let ((), used) = measure(|| {
        for _ in 0..4_000 {
            let value = rng.next();
            match rng.below(3) {
                0 => {
                    let index = column.push(value).expect("room");
                    assert_eq!(index as usize, model.len());
                    model.push(Some(value));
                }
                1 if blocks.len() < blocks.capacity() => {
                    let count = 1 + rng.below(9) as u32;
                    let block = column.claim(count).expect("room");
                    model.extend((0..count).map(|_| None));
                    blocks.push(block);
                }
                _ => {
                    if let Some(block) = blocks.iter_mut().find(|block| !block.is_spent()) {
                        let index = column.fill(block, value).expect("room");
                        model[index as usize] = Some(value);
                    }
                }
            }
        }
    });
    assert_eq!(used.allocations, 0, "pushes, claims and fills allocate nothing");
    assert_eq!(column.len(), model.len());
    for (index, want) in model.iter().enumerate() {
        assert_eq!(column.get(index as u32), want.as_ref(), "index {index}");
    }
    assert_eq!(column.get(model.len() as u32), None);
    let published: Vec<(u32, u64)> = column.iter().map(|(index, &value)| (index, value)).collect();
    let expected: Vec<(u32, u64)> = model
        .iter()
        .enumerate()
        .filter_map(|(index, value)| Some((index as u32, (*value)?)))
        .collect();
    assert_eq!(published, expected, "iteration is index order over the published values");
}

#[test]
fn column_reads_allocate_nothing() {
    let column = Column::<u64>::new(1 << 16, Tag::Intern).expect("reserve");
    for i in 0..10_000 {
        column.push(i).expect("room");
    }
    let (sum, used) = measure(|| (0..10_000u32).filter_map(|i| column.get(i)).sum::<u64>());
    assert_eq!(sum, (0..10_000).sum());
    assert_eq!(used.allocations, 0);
}

// AppendVec

#[test]
fn append_vec_matches_vec_and_never_moves() {
    let mut rng = Xorshift64Star::new(7);
    let column = AppendVec::<u64>::new(1 << 20, Tag::Vfs).expect("reserve");
    let mut model = Vec::new();
    for _ in 0..200_000 {
        let value = rng.next();
        assert_eq!(column.append(value) as usize, model.len());
        model.push(value);
    }
    let first = column.as_slice().as_ptr();
    for i in 0..100_000 {
        column.append(i);
        model.push(i);
    }
    assert_eq!(column.as_slice().as_ptr(), first, "growth never moves an element");
    assert_eq!(column.as_slice(), model.as_slice());
    assert_eq!(column.get(model.len() as u32), None);
}

#[test]
fn append_vec_readers_see_a_written_prefix_while_one_writer_appends() {
    let column = AppendVec::<[u64; 2]>::new(1 << 20, Tag::Vfs).expect("reserve");
    thread::scope(|scope| {
        let column = &column;
        let readers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(move || {
                    let mut seen = 0;
                    while seen < 100_000 {
                        let published = column.as_slice();
                        assert!(published.len() >= seen, "the length only grows");
                        for (i, pair) in published.iter().enumerate().skip(seen) {
                            assert_eq!(*pair, [i as u64, !(i as u64)], "a published value is whole");
                        }
                        seen = published.len();
                    }
                })
            })
            .collect();
        for i in 0..100_000u64 {
            column.append([i, !i]);
        }
        for reader in readers {
            reader.join().expect("reader");
        }
    });
}

#[test]
fn append_vec_full_is_an_error_and_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let column = AppendVec::new(8, Tag::Vfs).expect("reserve");
    for value in 0..8 {
        column.try_append(Counted { value, drops: Rc::clone(&drops) }).expect("room");
    }
    let refused = column.try_append(Counted { value: 8, drops: Rc::clone(&drops) });
    assert!(matches!(refused, Err(MemError::Exhausted { tag: Tag::Vfs, .. })));
    assert_eq!(drops.get(), 1, "the refused value is dropped");
    drop(column);
    assert_eq!(drops.get(), 9);
}

#[test]
fn append_vec_appends_and_reads_allocate_nothing() {
    let column = AppendVec::<u64>::new(1 << 22, Tag::Vfs).expect("reserve");
    let (sum, used) = measure(|| {
        for i in 0..1_000_000 {
            column.append(i);
        }
        column.iter().sum::<u64>()
    });
    assert_eq!(sum, (0..1_000_000).sum());
    assert_eq!(used.allocations, 0);
}

/// A writer's appends, slices and fills against a `Vec` on seeded operations: the same values
/// at the same indices, each bulk append's first index the length before it, and growth never
/// moving an element. Refused ones publish nothing.
#[test]
fn append_vec_writer_and_bulk_appends_match_vec() {
    let mut rng = Xorshift64Star::new(23);
    let column = AppendVec::<u32>::new(1 << 18, Tag::Vfs).expect("reserve");
    let mut model: Vec<u32> = Vec::new();
    let mut scratch: Vec<u32> = Vec::new();
    let mut first = None;
    for _ in 0..1_000 {
        let mut writer = column.writer();
        for _ in 0..rng.below(8) {
            match rng.below(3) {
                0 => {
                    let value = rng.next() as u32;
                    assert_eq!(writer.append(value) as usize, model.len());
                    model.push(value);
                }
                1 => {
                    scratch.clear();
                    scratch.extend((0..rng.below(100)).map(|_| rng.next() as u32));
                    assert_eq!(writer.extend_from_slice(&scratch) as usize, model.len());
                    model.extend_from_slice(&scratch);
                }
                _ => {
                    let count = rng.below(100) as usize;
                    let base = rng.next() as u32;
                    let at = writer.extend_with(count, |i| base ^ i as u32);
                    assert_eq!(at as usize, model.len());
                    model.extend((0..count).map(|i| base ^ i as u32));
                }
            }
        }
        drop(writer);
        assert_eq!(column.extend_from_slice(&[7, 8]) as usize, model.len());
        model.extend_from_slice(&[7, 8]);
        first.get_or_insert(column.as_slice().as_ptr());
    }
    assert_eq!(column.as_slice(), model.as_slice());
    assert_eq!(Some(column.as_slice().as_ptr()), first, "growth never moves an element");
    let left = column.max_len() - model.len();
    let mut writer = column.writer();
    let refused = writer.try_extend_with(left + 1, |i| i as u32);
    assert!(matches!(refused, Err(MemError::Exhausted { tag: Tag::Vfs, .. })));
    assert!(writer.try_extend_from_slice(&vec![0; left + 1]).is_err());
    drop(writer);
    assert_eq!(column.len() as usize, model.len(), "a refused append publishes nothing");
}

#[test]
#[should_panic(expected = "appends must be serialized")]
fn append_vec_second_writer_at_once_stops() {
    let column = AppendVec::<u64>::new(16, Tag::Vfs).expect("reserve");
    let _writer = column.writer();
    column.append(1);
}

#[test]
fn append_vec_writer_appends_allocate_nothing() {
    let column = AppendVec::<u64>::new(1 << 22, Tag::Vfs).expect("reserve");
    let bytes = [3u64; 1000];
    let (_, used) = measure(|| {
        let mut writer = column.writer();
        for i in 0..1_000_000 {
            writer.append(i);
        }
        for _ in 0..1000 {
            writer.extend_from_slice(&bytes);
        }
        writer.extend_with(1000, |i| i as u64);
    });
    assert_eq!(used.allocations, 0);
    assert_eq!(column.len(), 2_001_000);
}

// Frame

#[test]
fn frame_vec_matches_vec_and_grows_in_place() {
    let mut rng = Xorshift64Star::new(2);
    let frame = Frame::new(Tag::Infer).expect("reserve");
    let mut vec: Vec<u32, &Frame> = Vec::new_in(&frame);
    let mut model = Vec::new();
    vec.push(0);
    model.push(0);
    let first = vec.as_ptr();
    for _ in 0..100_000 {
        match rng.below(8) {
            0..=5 => {
                let value = rng.next() as u32;
                vec.push(value);
                model.push(value);
            }
            6 => assert_eq!(vec.pop(), model.pop()),
            _ => {
                vec.shrink_to_fit();
                model.shrink_to_fit();
            }
        }
    }
    assert_eq!(vec.as_slice(), model.as_slice());
    assert_eq!(vec.as_ptr(), first, "the frame's top allocation grows in place");
}

#[test]
fn frame_collect_is_the_copies_it_replaced() {
    /// blaze-link's `arena::collect_in` and hir-ty-wfcheck's `mem::collect`.
    fn kept<'f, T>(frame: &'f Frame, items: impl IntoIterator<Item = T>) -> Vec<T, &'f Frame> {
        let items = items.into_iter();
        let mut out = Vec::with_capacity_in(items.size_hint().0, frame);
        out.extend(items);
        out
    }
    let mut rng = Xorshift64Star::new(5);
    let frame = Frame::new(Tag::Infer).expect("reserve");
    for len in [0, 1, 7, 64, 1000] {
        let values: Vec<u64> = (0..len).map(|_| rng.next()).collect();
        let exact = frame::collect(&frame, values.iter().copied());
        let reference = kept(&frame, values.iter().copied());
        assert_eq!(exact.as_slice(), values.as_slice());
        assert_eq!((exact.as_slice(), exact.capacity()), (reference.as_slice(), reference.capacity()));
        let filtered = frame::collect(&frame, values.iter().copied().filter(|it| it % 3 == 0));
        let reference = kept(&frame, values.iter().copied().filter(|it| it % 3 == 0));
        assert_eq!((filtered.as_slice(), filtered.capacity()), (reference.as_slice(), reference.capacity()));
    }
    let warm = Frame::new(Tag::Infer).expect("reserve");
    drop(frame::collect(&warm, 0..16u32));
    let (_, used) = measure(|| drop(frame::collect(&warm, 0..16u32)));
    assert_eq!(used.allocations, 0, "a frame vector calls no global allocator");
}

#[test]
fn frame_interleaved_vecs_keep_their_contents() {
    let frame = Frame::new(Tag::Infer).expect("reserve");
    let mut a: Vec<u64, &Frame> = Vec::new_in(&frame);
    let mut b: Vec<u8, &Frame> = Vec::new_in(&frame);
    for i in 0..50_000u64 {
        a.push(i * 3);
        b.push(i as u8);
    }
    assert!(a.iter().enumerate().all(|(i, &v)| v == i as u64 * 3));
    assert!(b.iter().enumerate().all(|(i, &v)| v == i as u8));
    let boxed = Box::new_in([7u64; 32], &frame);
    assert_eq!(*boxed, [7; 32]);
}

/// What `f`'s task's frame copied growing blocks, as the task's end reports it.
fn copied_by_task(f: impl FnOnce(&Frame)) -> tag::Copied {
    let before = tag::copied_on_thread();
    frame::task(Tag::Infer, f);
    let after = tag::copied_on_thread();
    tag::Copied { copies: after.copies - before.copies, bytes: after.bytes - before.bytes }
}

#[test]
fn two_frame_vecs_growing_interleaved_in_one_task_report_copies_and_one_alone_none() {
    let alone = copied_by_task(|frame| {
        let mut a: Vec<u64, &Frame> = Vec::new_in(frame);
        for i in 0..10_000 {
            a.push(i);
        }
        assert_eq!(a.len(), 10_000);
    });
    assert_eq!(alone, tag::Copied::default(), "a vector growing alone is the frame's last block and grows in place");

    let line_before = tag::snapshot().get(Tag::Infer);
    let mut last_capacity = 0;
    let interleaved = copied_by_task(|frame| {
        let mut a: Vec<u64, &Frame> = Vec::new_in(frame);
        let mut b: Vec<u64, &Frame> = Vec::new_in(frame);
        for i in 0..10_000 {
            a.push(i);
            b.push(i);
        }
        assert!(a.iter().zip(&b).enumerate().all(|(i, (&a, &b))| a == i as u64 && b == i as u64));
        last_capacity = a.capacity().min(b.capacity());
    });
    assert!(interleaved.copies >= 2, "each vector moved when it grew under the other: {interleaved:?}");
    // The last move of the vector below copied at least half its final capacity.
    assert!(interleaved.bytes >= (last_capacity / 2 * size_of::<u64>()) as u64, "{interleaved:?}");
    let line_after = tag::snapshot().get(Tag::Infer);
    assert!(line_after.grow_copies - line_before.grow_copies >= interleaved.copies, "the tag's line counts them too");
    assert!(line_after.grow_copied - line_before.grow_copied >= interleaved.bytes);

    let mut frame = Frame::new(Tag::Infer).expect("reserve");
    {
        let mut a: Vec<u8, &Frame> = Vec::new_in(&frame);
        let mut b: Vec<u8, &Frame> = Vec::new_in(&frame);
        for i in 0..1000 {
            a.push(i as u8);
            b.push(i as u8);
        }
    }
    let copied = frame.copied();
    assert!(copied.copies > 0);
    let before = tag::copied_on_thread();
    frame.reset();
    assert_eq!(frame.copied(), tag::Copied::default(), "a reset starts the count again");
    assert_eq!(tag::copied_on_thread().copies - before.copies, copied.copies, "a reset reports the frame's copies");
}

#[test]
fn frame_values_are_aligned_and_disjoint() {
    let frame = Frame::new(Tag::Mir).expect("reserve");
    let mut rng = Xorshift64Star::new(3);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for _ in 0..10_000 {
        let size = rng.below(300) as usize;
        let align = 1 << rng.below(13);
        let ptr = frame.alloc_layout(Layout::from_size_align(size, align).expect("layout"));
        assert_eq!(ptr.as_ptr() as usize % align, 0);
        blocks.push((ptr.as_ptr() as usize, size));
    }
    blocks.sort_unstable();
    for pair in blocks.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0, "blocks overlap");
    }
}

#[test]
fn frame_marker_rolls_back_and_reset_frees_all() {
    let mut frame = Frame::new(Tag::Codegen).expect("reserve");
    let kept = frame.alloc_slice_copy(&[1u8; 100]).as_ptr();
    let marker = frame.mark();
    let used = frame.used();
    for _ in 0..1000 {
        frame.alloc(0u64);
    }
    assert!(frame.used() > used);
    frame.reset_to(marker);
    assert_eq!(frame.used(), used);
    let again = frame.alloc(5u64) as *mut u64 as usize;
    assert!(again >= kept as usize + 100, "a rollback keeps what came before the marker");
    frame.reset();
    assert_eq!(frame.used(), 0);
    let text = frame.alloc_str("frame");
    assert_eq!(text, "frame");
}

#[test]
#[should_panic(expected = "a marker of another frame")]
fn frame_marker_of_another_frame_stops() {
    let one = Frame::new(Tag::Codegen).expect("reserve");
    let mut two = Frame::new(Tag::Codegen).expect("reserve");
    let marker = one.mark();
    two.reset_to(marker);
}

#[test]
fn frame_scope_frees_what_it_allocated() {
    let mut frame = Frame::new(Tag::Parse).expect("reserve");
    frame.alloc(1u64);
    let before = frame.used();
    let sum = frame.scope(|frame| {
        let values = frame.alloc_slice_fill_with(1000, |i| i as u64);
        values.iter().sum::<u64>()
    });
    assert_eq!(sum, (0..1000).sum());
    assert_eq!(frame.used(), before);
}

#[test]
fn frame_tasks_nest_with_frames_of_their_own() {
    frame::task(Tag::Infer, |outer| {
        let kept = outer.alloc_slice_copy(&[9u8; 64]);
        let inner_used = frame::task(Tag::Mir, |inner| {
            assert!(!std::ptr::eq(inner, outer), "a nested task gets another frame");
            inner.alloc_slice_copy(&[1u8; 4096]);
            inner.used()
        });
        assert!(inner_used >= 4096);
        assert_eq!(kept, &[9u8; 64], "an inner reset leaves the outer task's memory alone");
    });
    assert!(frame::spare_frames() >= 1, "the frames' chunk is kept warm for the next tasks");
    frame::task(Tag::Infer, |frame| assert_eq!(frame.used(), 0, "a task starts empty"));
}

#[test]
fn frame_current_is_the_innermost_task_and_comes_back_to_the_outer() {
    assert!(!frame::in_task());
    frame::task(Tag::Infer, |outer| {
        let outer_at = outer as *const Frame;
        frame::current(|frame| assert!(std::ptr::eq(frame, outer_at), "the running task's"));
        frame::task(Tag::Mir, |inner| {
            frame::current(|frame| {
                assert!(std::ptr::eq(frame, inner), "a nested task's own");
                frame.alloc_slice_copy(&[3u32; 16]);
            });
            assert!(inner.used() >= 64, "what current allocated is the inner task's");
        });
        frame::current(|frame| assert!(std::ptr::eq(frame, outer_at), "back to the outer"));
    });
    assert!(!frame::in_task());
}

#[test]
#[should_panic(expected = "frame::current outside any frame::task")]
fn frame_current_outside_a_task_stops() {
    frame::current(|frame| frame.used());
}

#[test]
fn frame_task_counts_its_peak_to_its_tag() {
    let before = tag::snapshot().get(Tag::Expand).allocated;
    frame::task(Tag::Expand, |frame| {
        frame.alloc_slice_copy(&[0u8; 10_000]);
    });
    let after = tag::snapshot().get(Tag::Expand);
    assert!(after.allocated >= before + 10_000);
    assert!(after.task_peak >= 10_000);
}

#[test]
fn frame_past_its_reservation_is_an_alloc_error_not_a_crash() {
    use std::alloc::Allocator;
    let frame = Frame::with_reserve(1 << 20, Tag::Codegen).expect("reserve");
    let too_big = Layout::from_size_align(4 << 20, 8).expect("layout");
    assert!(frame.allocate(too_big).is_err());
    assert!(matches!(
        frame.try_alloc_layout(too_big),
        Err(MemError::Exhausted { tag: Tag::Codegen, .. })
    ));
}

#[test]
fn frame_task_allocates_nothing_once_warm() {
    let work = || {
        frame::task(Tag::Infer, |frame| {
            let mut types: Vec<u64, &Frame> = Vec::with_capacity_in(64, frame);
            for i in 0..100_000 {
                types.push(i);
            }
            let names = frame.alloc_str("a name");
            types.len() + names.len()
        })
    };
    work();
    let (_, used) = measure(work);
    assert_eq!(used.allocations, 0, "a warm task allocates only from its frame");
}

/// A task `depth` deep in tasks: it pushes 1 KB through [`frame::current`] and reads it back
/// after the task inside it returns.
fn nest(depth: usize) {
    frame::task(Tag::Infer, |_| {
        let pattern = |i: usize| (depth ^ i) as u8;
        let kept = frame::current(|frame| std::ptr::from_mut(frame.alloc_slice_fill_with(1024, pattern)));
        if depth > 1 {
            nest(depth - 1);
        }
        frame::current(|frame| {
            frame.alloc_slice_fill_with(64, |_| 0xAA);
        });
        // SAFETY: the bytes are in this task's frame, alive until the task ends, and nothing
        // else refers to them.
        let kept = unsafe { &*kept };
        assert!(
            kept.iter().enumerate().all(|(i, &byte)| byte == pattern(i)),
            "task {depth} keeps its bytes past its inner task"
        );
    });
}

#[test]
fn frame_300_nested_tasks_keep_their_bytes_and_cost_nothing_once_warm() {
    let deep = thread::Builder::new().stack_size(64 << 20).spawn(|| {
        // A place of its own, so the chunks other tests carve meanwhile are not counted.
        chunks::set_thread_node(Some(0)).expect("node 0");
        nest(300);
        let warm = chunks::carved_on(Some(0));
        blaze_alloc_count::assert_at_most(0, "300 nested warm tasks", || nest(300));
        let second = chunks::carved_on(Some(0));
        nest(300);
        assert_eq!(second, warm, "a second pass carves no chunk");
        assert_eq!(chunks::carved_on(Some(0)), second, "a third pass carves no chunk");
        assert!(second <= 2 * chunks::CHUNK, "small nested tasks share their outer task's chunk");
    });
    deep.expect("spawn").join().expect("the nested tasks");
}

#[test]
fn frame_inner_task_starts_in_the_rest_of_the_outer_chunk() {
    frame::task(Tag::Infer, |outer| {
        outer.alloc(1u64);
        let held = outer.committed();
        frame::task(Tag::Mir, |inner| {
            let value = inner.alloc(2u64);
            assert_eq!(inner.committed(), 0, "a small inner task takes no block of its own");
            assert!(inner.contains(std::ptr::from_ref(value).cast()));
            let pushed = outer.alloc(3u64);
            assert!(outer.committed() > held, "the outer frame takes another block, not the lent bytes");
            assert_eq!((*value, *pushed), (2, 3));
        });
    });
}

#[test]
fn frame_outer_task_grows_while_an_inner_task_allocates() {
    frame::task(Tag::Infer, |outer| {
        let mut types: Vec<u64, &Frame> = Vec::with_capacity_in(16, outer);
        types.extend(0..16);
        frame::task(Tag::Mir, |inner| {
            let mine = inner.alloc_slice_fill_with(4096, |i| i as u32);
            for i in 16..100_000 {
                types.push(i);
            }
            let more = frame::current(|frame| std::ptr::from_mut(frame.alloc_slice_fill_with(4096, |i| !(i as u32))));
            assert!(mine.iter().enumerate().all(|(i, &v)| v == i as u32), "the outer frame's growth leaves the inner task's bytes alone");
            // SAFETY: in the inner task's frame, alive until it ends; nothing else refers to it.
            let more = unsafe { &*more };
            assert!(more.iter().enumerate().all(|(i, &v)| v == !(i as u32)));
        });
        assert!(types.iter().copied().eq(0..100_000));
        let after = outer.alloc_slice_fill_with(4096, |i| i as u16);
        assert!(types.iter().copied().eq(0..100_000), "what the outer frame takes back is free");
        assert!(after.iter().enumerate().all(|(i, &v)| v == i as u16));
    });
}

#[test]
fn frame_span_grows_in_place_and_comes_back_warm() {
    let grow = || {
        frame::task(Tag::Codegen, |frame| {
            let mut bytes: Vec<u8, &Frame> = Vec::with_capacity_in(2 << 20, frame);
            let first = bytes.as_ptr();
            bytes.resize(24 << 20, 7);
            assert_eq!(bytes.as_ptr(), first, "a span's top allocation grows in place");
            assert!(frame.committed() >= 24 << 20);
            bytes.len()
        })
    };
    grow();
    let spans = frame::spare_frames();
    let ((), used) = measure(|| {
        grow();
    });
    assert_eq!(used.allocations, 0, "a warm span is reused");
    assert_eq!(frame::spare_frames(), spans);
    frame::trim_spare(0);
    assert_eq!(frame::spare_frames(), 0, "trimming to nothing gives every warm block back");
}

#[test]
fn frame_matches_a_model_on_random_allocations_grows_shrinks_and_rollbacks() {
    use std::{alloc::Allocator, ptr::NonNull, slice};

    use crate::Marker;

    struct Live {
        ptr: NonNull<u8>,
        layout: Layout,
        /// What its bytes should be.
        model: Vec<u8>,
    }

    /// Writes bytes `from..` of `live`, in the frame and the model alike.
    fn write(live: &mut Live, from: usize, seed: u64) {
        for i in from..live.layout.size() {
            let byte = (seed as u8) ^ (i as u8).wrapping_mul(7);
            // SAFETY: `ptr` is a live allocation of the frame of `layout.size()` bytes.
            unsafe { live.ptr.as_ptr().wrapping_add(i).write(byte) };
            live.model.push(byte);
        }
    }

    fn check(frame: &Frame, live: &Live) {
        assert_eq!(live.ptr.as_ptr() as usize % live.layout.align(), 0, "aligned");
        // SAFETY: as in `write`; only the test writes it.
        let bytes = unsafe { slice::from_raw_parts(live.ptr.as_ptr(), live.layout.size()) };
        assert!(bytes == live.model.as_slice(), "an allocation keeps its bytes");
        if live.layout.size() > 0 {
            assert!(frame.contains(live.ptr.as_ptr()));
        }
    }

    let mut rng = Xorshift64Star::new(17);
    let mut frame = Frame::with_reserve(1 << 30, Tag::Mir).expect("reserve");
    let mut live: Vec<Live> = Vec::new();
    // A marker and how many allocations were live when it was taken.
    let mut marks: Vec<(Marker, usize)> = Vec::new();
    for step in 0..20_000u64 {
        let op = rng.below(32);
        match op {
            0..=13 => {
                let size = match rng.below(512) {
                    0 => (600 << 10) + rng.below(3 << 20) as usize,
                    _ => rng.below(2048) as usize,
                };
                let layout = Layout::from_size_align(size, 1 << rng.below(8)).expect("layout");
                let ptr = frame.allocate(layout).expect("within the reserve").cast::<u8>();
                let mut it = Live { ptr, layout, model: Vec::new() };
                write(&mut it, 0, step);
                check(&frame, &it);
                live.push(it);
            }
            14..=21 if !live.is_empty() => {
                // A marker taken since the last allocation was made would cut its new bytes.
                let n = live.len();
                marks.retain(|&(_, count)| count < n);
                let last = live.last_mut().expect("not empty");
                let old = last.layout;
                let size = match op {
                    14..=18 => old.size() + rng.below(4096) as usize,
                    _ => rng.below(old.size() as u64 + 1) as usize,
                };
                let new = Layout::from_size_align(size, old.align()).expect("layout");
                let moved = match size >= old.size() {
                    // SAFETY: `last` is a live allocation of the frame with layout `old`.
                    true => unsafe { frame.grow(last.ptr, old, new) },
                    // SAFETY: as above.
                    false => unsafe { frame.shrink(last.ptr, old, new) },
                };
                last.ptr = moved.expect("within the reserve").cast();
                last.layout = new;
                last.model.truncate(size);
                let kept = last.model.len();
                write(last, kept, step);
                check(&frame, last);
            }
            22..=23 if !live.is_empty() => {
                let n = live.len();
                marks.retain(|&(_, count)| count < n);
                let last = live.pop().expect("not empty");
                // SAFETY: a live allocation of the frame, given back once.
                unsafe { frame.deallocate(last.ptr, last.layout) };
            }
            24..=26 => marks.push((frame.mark(), live.len())),
            27..=29 if !marks.is_empty() => {
                let at = rng.below(marks.len() as u64) as usize;
                let (marker, count) = marks[at];
                marks.truncate(at + 1);
                frame.reset_to(marker);
                live.truncate(count);
                if let Some(last) = live.last() {
                    check(&frame, last);
                }
            }
            30 if rng.below(64) == 0 => {
                frame.reset();
                live.clear();
                marks.clear();
                assert_eq!(frame.used(), 0);
            }
            _ => {}
        }
        assert!(frame.used() <= frame.committed() && frame.used() <= frame.peak());
        if step % 512 == 0 {
            for it in &live {
                check(&frame, it);
            }
        }
    }
    for it in &live {
        check(&frame, it);
    }
}

/// The peak an allocation no longer writes is folded in where the cursor moves back: after
/// every operation it equals what writing it on every allocation gave, the most `used()` was
/// since the last reset.
#[test]
fn frame_peak_folded_lazily_is_the_most_used_after_every_operation() {
    use std::{alloc::Allocator, ptr::NonNull};

    let mut rng = Xorshift64Star::new(41);
    let mut frame = Frame::with_reserve(1 << 30, Tag::Mir).expect("reserve");
    let mut live: Vec<(NonNull<u8>, Layout)> = Vec::new();
    let mut marks: Vec<(crate::Marker, usize)> = Vec::new();
    let mut model = 0usize;
    for _ in 0..20_000u64 {
        match rng.below(16) {
            0..=6 => {
                let size = match rng.below(256) {
                    0 => (600 << 10) + rng.below(2 << 20) as usize,
                    _ => rng.below(3000) as usize,
                };
                let layout = Layout::from_size_align(size, 1 << rng.below(7)).expect("layout");
                live.push((frame.allocate(layout).expect("within the reserve").cast(), layout));
            }
            7..=9 if !live.is_empty() => {
                let n = live.len();
                marks.retain(|&(_, count)| count < n);
                let (ptr, old) = live.pop().expect("not empty");
                let size = match rng.below(2) {
                    0 => old.size() + rng.below(5000) as usize,
                    _ => rng.below(old.size() as u64 + 1) as usize,
                };
                let new = Layout::from_size_align(size, old.align()).expect("layout");
                let moved = match size >= old.size() {
                    // SAFETY: `ptr` is a live allocation of the frame with layout `old`.
                    true => unsafe { frame.grow(ptr, old, new) },
                    // SAFETY: as above.
                    false => unsafe { frame.shrink(ptr, old, new) },
                };
                live.push((moved.expect("within the reserve").cast(), new));
            }
            10..=11 if !live.is_empty() => {
                let n = live.len();
                marks.retain(|&(_, count)| count < n);
                let (ptr, layout) = live.pop().expect("not empty");
                // SAFETY: a live allocation of the frame, given back once.
                unsafe { frame.deallocate(ptr, layout) };
            }
            12 => marks.push((frame.mark(), live.len())),
            13 if !marks.is_empty() => {
                let at = rng.below(marks.len() as u64) as usize;
                let (marker, count) = marks[at];
                marks.truncate(at + 1);
                frame.reset_to(marker);
                live.truncate(count);
            }
            14 => {
                let mut inner = 0;
                frame.scope(|frame| {
                    frame.alloc_slice_fill_with(rng.below(4000) as usize, |i| i as u8);
                    inner = frame.used();
                });
                model = model.max(inner);
            }
            15 if rng.below(32) == 0 => {
                frame.reset();
                live.clear();
                marks.clear();
                model = 0;
            }
            _ => {}
        }
        model = model.max(frame.used());
        assert_eq!(frame.peak(), model);
    }
}

#[test]
fn frame_measure_counts_inner_tasks_on_top_of_outer_and_allocates_nothing() {
    let run = || {
        frame::measure(|| {
            frame::task(Tag::Infer, |outer| {
                outer.alloc_slice_copy(&[0u8; 10_000]);
                let ((), inner) = frame::measure(|| {
                    frame::task(Tag::Mir, |inner| {
                        inner.alloc_slice_copy(&[0u8; 50_000]);
                    })
                });
                assert!((50_000..54_096).contains(&inner), "an inner measure counts its task: {inner}");
            })
        })
    };
    run();
    let ((), bytes) = blaze_alloc_count::assert_at_most(0, "a warm measure of nested tasks", run);
    assert!((60_000..68_192).contains(&bytes), "the inner task's bytes on top of the outer's: {bytes}");
    let ((), none) = frame::measure(|| ());
    assert_eq!(none, 0);
}

#[test]
fn frame_cap_waits_past_its_limit_until_bytes_come_back() {
    let (cap, _spills) = frame::Cap::new(100, 100);
    let held = cap.reserve(80).expect("fits");
    let waiter = thread::spawn(move || cap.reserve(40).map(|grant| grant.bytes()));
    while cap.waiting() == 0 {
        thread::yield_now();
    }
    assert_eq!(cap.held(), 80);
    drop(held);
    assert_eq!(waiter.join().expect("the waiter"), Ok(40));
    assert_eq!(cap.held(), 0, "a grant gives its bytes back when it drops");
}

#[test]
fn frame_cap_cancels_the_newest_wait_when_every_holder_waits() {
    let (cap, _spills) = frame::Cap::new(100, 100);
    let mine = cap.reserve(60).expect("fits");
    let (go, ready) = crossbeam_channel::unbounded();
    let older = thread::spawn(move || {
        let first = cap.reserve(30).expect("fits");
        go.send(()).expect("the test waits for it");
        let more = cap.reserve(30).map(|grant| grant.bytes());
        (first.bytes(), more)
    });
    ready.recv().expect("the older thread holds its bytes");
    while cap.waiting() == 0 {
        thread::yield_now();
    }
    assert_eq!(
        cap.reserve(50).map(|grant| grant.bytes()),
        Err(frame::Cancelled),
        "with every holder waiting, the newest wait is cancelled"
    );
    drop(mine);
    assert_eq!(older.join().expect("the older waiter"), (30, Ok(30)), "the older wait goes on");
}

#[test]
fn frame_cancel_unwinds_its_task_and_says_so_until_the_outermost_task_ends() {
    frame::task(Tag::Infer, |_| {
        let unwound = std::panic::catch_unwind(|| frame::task(Tag::Mir, |_| frame::cancel()));
        let payload = unwound.expect_err("a cancel unwinds");
        assert!(payload.is::<frame::Cancelled>(), "its payload is `Cancelled`");
        assert!(frame::cancelling(), "still inside the outer task");
    });
    assert!(!frame::cancelling(), "the outermost task has ended");
}

#[test]
#[should_panic(expected = "frame::cancel outside any frame::task")]
fn frame_cancel_outside_any_task_stops() {
    frame::cancel();
}

#[test]
fn frame_cap_says_each_pass_of_spill_at() {
    let (cap, spills) = frame::Cap::new(1000, 500);
    let a = cap.reserve(600).expect("fits");
    let b = cap.reserve(100).expect("fits");
    assert_eq!(spills.ready().collect::<Vec<_>>(), [100], "one pass, with the bytes over it");
    drop((a, b));
    let c = cap.reserve(700).expect("fits");
    assert_eq!(spills.ready().collect::<Vec<_>>(), [200], "passed again after falling under it");
    drop(c);
}

#[test]
#[should_panic(expected = "a reserve of 200 bytes, more than the cap of 100")]
fn frame_cap_stops_on_a_reserve_past_its_whole_limit() {
    let (cap, _spills) = frame::Cap::new(100, 100);
    _ = cap.reserve(200);
}

#[test]
fn frame_task_past_the_thread_shelf_reserves_from_the_cap() {
    // Every test's task frames reserve from it while it is set, so it never makes one wait.
    let (cap, _spills) = frame::Cap::new(1 << 50, 1 << 50);
    let capped = thread::spawn(move || {
        frame::set_cap(Some(cap));
        frame::task(Tag::Codegen, |frame| {
            let big = frame.alloc_slice_fill_with(8 << 20, |i| i as u8);
            assert!(frame::capped() >= 8 << 20, "a span's pages are reserved from the cap");
            assert!(cap.held() >= 8 << 20);
            assert_eq!(big[12_345], 12_345usize as u8);
        });
        let left = frame::capped();
        frame::set_cap(None);
        left
    });
    assert_eq!(capped.join().expect("the capped task"), 0, "a span on the shelf holds none of the cap");
}

#[test]
fn chunks_refuse_a_numa_node_past_the_last() {
    assert!(matches!(chunks::set_thread_node(Some(chunks::MAX_NODES)), Err(MemError::TooLarge { .. })));
    assert_eq!(chunks::thread_node(), None);
    chunks::set_thread_node(Some(3)).expect("a node in range");
    assert_eq!(chunks::thread_node(), Some(3));
    chunks::set_thread_node(None).expect("unplaced");
    assert_eq!(chunks::thread_node(), None);
}

#[test]
fn region_on_node_zero_is_placed_there() {
    let mut region = Region::reserve_on(4 << 20, 0, Tag::Other).expect("every machine has node 0");
    assert_eq!(region.node(), Some(0));
    region.ensure(1 << 20).expect("commit");
    region.bind_node(0).expect("already on node 0");
    #[cfg(not(windows))]
    {
        let mut unplaced = Region::reserve(4 << 20, Tag::Other).expect("reserve");
        unplaced.bind_node(0).expect("node 0");
        assert_eq!(unplaced.node(), Some(0));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn region_on_a_second_node_is_an_error_on_one_node() {
    let mut region = Region::reserve(4 << 20, Tag::Other).expect("reserve");
    assert!(region.bind_node(1).is_err(), "macOS has one memory node");
    assert_eq!(region.node(), None);
}

// Arena

#[test]
fn arena_values_from_many_threads_live_until_reset() {
    let mut arena = Arena::new(Tag::Query);
    thread::scope(|scope| {
        let arena = &arena;
        let handles: Vec<_> = (0..8u64)
            .map(|t| {
                scope.spawn(move || {
                    let local = arena.local();
                    let mut kept: Vec<&u64> = Vec::new();
                    for i in 0..20_000u64 {
                        kept.push(local.alloc(t * 1_000_000 + i));
                    }
                    let big = local.alloc_slice_copy(&vec![t as u8; 100_000]);
                    let huge = local.alloc_slice_copy(&vec![t as u16; 3 << 20]);
                    (kept, big, huge)
                })
            })
            .collect();
        for (t, handle) in handles.into_iter().enumerate() {
            let (kept, big, huge) = handle.join().expect("allocator");
            for (i, value) in kept.into_iter().enumerate() {
                assert_eq!(*value, t as u64 * 1_000_000 + i as u64);
            }
            assert!(big.iter().all(|&b| b == t as u8));
            assert!(huge.iter().all(|&h| h == t as u16));
        }
    });
    assert!(arena.held() > 0);
    arena.reset();
    assert_eq!(arena.held(), 0);
}

#[test]
fn arena_vec_matches_vec_and_keep_freezes_it() {
    let arena = Arena::new(Tag::Nameres);
    let local = arena.local();
    let mut rng = Xorshift64Star::new(4);
    let mut vec: Vec<u32, _> = Vec::new_in(&local);
    let mut model = Vec::new();
    for _ in 0..100_000 {
        if rng.below(5) == 0 {
            assert_eq!(vec.pop(), model.pop());
        } else {
            let value = rng.next() as u32;
            vec.push(value);
            model.push(value);
        }
    }
    let kept: &[u32] = local.keep(vec);
    drop(local);
    assert_eq!(kept, model.as_slice(), "a kept vector lives as long as its arena");
}

#[test]
fn arena_gives_chunks_back_and_takes_them_again_warm() {
    let mut first = Arena::new(Tag::Vfs);
    {
        let local = first.local();
        for i in 0..100_000u64 {
            local.alloc(i);
        }
    }
    let held = first.held();
    assert!(held > 0);
    first.reset();
    assert_eq!(first.held(), 0, "a reset gives every chunk back");
    // Purged chunks come back zeroed and are taken like warm ones.
    chunks::trim();
    let second = Arena::new(Tag::Vfs);
    let local = second.local();
    let values: Vec<&u64> = (0..100_000u64)
        .map(|i| {
            let value: &u64 = local.alloc(i);
            value
        })
        .collect();
    assert!(values.iter().enumerate().all(|(i, &&v)| v == i as u64));
    assert_eq!(second.held(), held, "the same work holds the same chunks");
    assert!(chunks::carved_bytes() >= held);
}

/// A reset gives back more chunks than one `give_all` run moves: every one leaves the arena's
/// tag, none is lost or given twice (a second arena takes them all again, each once), and the
/// warm request allocates nothing from the global allocator.
#[test]
fn arena_reset_gives_back_every_chunk_in_runs() {
    const HELD: usize = 2 * chunks::GIVE_RUN + 7;
    let whole = Layout::from_size_align(chunks::CHUNK, 8).expect("layout");
    let request = |arena: &Arena| {
        let local = arena.local();
        let mut starts = [0usize; HELD];
        for start in &mut starts {
            *start = local.alloc_layout(whole).as_ptr() as usize;
        }
        starts
    };
    let before = tag::snapshot().get(Tag::Diagnostics).live;
    let mut arena = Arena::new(Tag::Diagnostics);
    let mut first = request(&arena);
    assert_eq!(arena.held(), HELD * chunks::CHUNK, "each whole-chunk allocation holds a chunk");
    assert_eq!(tag::snapshot().get(Tag::Diagnostics).live, before + (HELD * chunks::CHUNK) as u64);
    arena.reset();
    assert_eq!(arena.held(), 0);
    assert_eq!(tag::snapshot().get(Tag::Diagnostics).live, before, "every chunk left the tag");
    let (_, used) = measure(|| {
        let mut second = request(&arena);
        first.sort_unstable();
        second.sort_unstable();
        assert!(second.windows(2).all(|pair| pair[0] != pair[1]), "no chunk handed out twice");
        arena.reset();
    });
    assert_eq!(used.allocations, 0, "a warm request of whole chunks allocates nothing");
    assert_eq!(arena.held(), 0);
    assert_eq!(tag::snapshot().get(Tag::Diagnostics).live, before);
    assert!(first.windows(2).all(|pair| pair[0] != pair[1]));
}

#[test]
fn arena_allocates_nothing_from_the_global_allocator() {
    let request = || {
        let arena = Arena::new(Tag::Query);
        let local = arena.local();
        let mut total = 0;
        for i in 0..50_000u64 {
            total += *local.alloc(i);
        }
        let mut list: Vec<u64, _> = Vec::new_in(&local);
        list.extend(0..10_000);
        total + list.len() as u64
    };
    request();
    let (_, used) = measure(request);
    assert_eq!(used.allocations, 0, "a request's memory comes from chunks, not the allocator");
}

// Revisions

struct Names;

impl Built for Names {
    type Value<'a> = &'a [&'a str];

    fn shorten<'s, 'l: 's>(value: &'s &'l [&'l str]) -> &'s &'s [&'s str] {
        value
    }
}

fn build_names<'a>(local: &crate::ArenaLocal<'a>, names: &[&str]) -> &'a [&'a str] {
    let names: Vec<&'a str> = names
        .iter()
        .map(|name| {
            let name: &'a str = local.alloc_str(name);
            name
        })
        .collect();
    local.alloc_slice_copy(&names)
}

/// One test, so no other test's task guard (the reclaim epoch is process-wide) holds a grace
/// these publishes and collections wait on.
#[test]
fn graces_revisions_publish_read_wait_for_readers_and_drop_once_and_limbo_waits() {
    publish_and_read_through_a_task_guard();
    wait_for_a_task_that_may_read_the_old_one();
    drop_each_value_once();
    limbo_keeps_a_retired_object_until_its_readers_end();
    limbo_matches_a_model_and_allocates_nothing_once_warm();
}

fn limbo_keeps_a_retired_object_until_its_readers_end() {
    let drops = Arc::new(AtomicUsize::new(0));
    let pool = Pool::<DropCount>::new(1024, Tag::Infer).expect("reserve");
    let mut cache = pool.cache();
    let mut limbo = pool::Limbo::new(&pool).expect("reserve");
    let first = cache.alloc(DropCount(Arc::clone(&drops)));
    let index = first.index();
    let reader = blaze_reclaim::enter();
    limbo.retire(first);
    assert_eq!(limbo.collect(), 0, "a task that began before the retirement may still read it");
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(reader);
    assert_eq!(limbo.collect(), 1);
    assert_eq!(drops.load(Ordering::Relaxed), 1, "dropped once its grace passed");
    assert!(limbo.is_empty());
    let second = cache.alloc(DropCount(Arc::clone(&drops)));
    limbo.retire(second);
    drop(limbo);
    assert_eq!(drops.load(Ordering::Relaxed), 2, "a dropped limbo drops what it held");
    drop(cache);
    let mut again = pool.cache();
    let reused = again.alloc(DropCount(Arc::clone(&drops)));
    assert!(reused.index() <= index + 2 * 128, "collected slots go back to the pool");
}

fn limbo_matches_a_model_and_allocates_nothing_once_warm() {
    let pool = Pool::<[u64; 4]>::new(1 << 12, Tag::Infer).expect("reserve");
    let mut cache = pool.cache();
    let mut limbo = pool::Limbo::new(&pool).expect("reserve");
    let mut rng = Xorshift64Star::new(9);
    let mut round = || {
        let mut retired = 0;
        let mut collected = 0;
        for _ in 0..20_000 {
            if rng.below(3) == 0 {
                collected += limbo.collect();
            } else {
                limbo.retire(cache.alloc([rng.next(); 4]));
                retired += 1;
            }
        }
        collected += limbo.collect();
        assert_eq!(collected, retired, "with no reader every grace passes");
        assert!(limbo.is_empty());
    };
    round();
    let (_, used) = measure(&mut round);
    assert_eq!(used.allocations, 0, "retiring and collecting reuse the limbo's column");
    assert!(pool.carved() <= 3 * 128 + 64, "collected slots are reused");
}

fn publish_and_read_through_a_task_guard() {
    let revisions = Revisions::<Names>::new(Tag::Vfs);
    {
        let guard = blaze_reclaim::enter();
        assert!(revisions.read(&guard).is_none(), "nothing before the first publish");
    }
    let one = revisions.publish(|local| build_names(local, &["a", "b"])).expect("publish");
    assert_eq!(one, 1);
    {
        let guard = blaze_reclaim::enter();
        let current = revisions.read(&guard).expect("published");
        assert_eq!(current.number, 1);
        assert_eq!(*current.value, ["a", "b"]);
    }
    let two = revisions.publish(|local| build_names(local, &["c"])).expect("publish");
    assert_eq!(two, 2);
    let three = revisions.publish(|local| build_names(local, &["d", "e", "f"])).expect("publish");
    assert_eq!(three, 3, "no task read revision 1, so its arena is reused at once");
    let guard = blaze_reclaim::enter();
    assert_eq!(*revisions.read(&guard).expect("published").value, ["d", "e", "f"]);
}

fn wait_for_a_task_that_may_read_the_old_one() {
    let revisions = Revisions::<Names>::new(Tag::Vfs);
    revisions.publish(|local| build_names(local, &["one"])).expect("publish");
    let reader = blaze_reclaim::enter();
    let old = revisions.read(&reader).expect("published");
    assert_eq!(*old.value, ["one"]);
    revisions.publish(|local| build_names(local, &["two"])).expect("a second arena is free");
    // Revision 1's arena is next, but this thread's task may still read it.
    assert_eq!(
        revisions.publish(|local| build_names(local, &["three"])),
        Err(PublishError::Readers)
    );
    assert_eq!(*old.value, ["one"], "the old revision is intact while its reader's task lasts");
    drop(reader);
    assert_eq!(revisions.publish(|local| build_names(local, &["three"])), Ok(3));
}

struct Dropping;

impl Built for Dropping {
    type Value<'a> = DropCount;

    fn shorten<'s, 'l: 's>(value: &'s DropCount) -> &'s DropCount {
        value
    }
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn drop_each_value_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let revisions = Revisions::<Dropping>::new(Tag::Vfs);
    for _ in 0..5 {
        let drops = Arc::clone(&drops);
        revisions.publish(move |_| DropCount(drops)).expect("publish");
    }
    assert_eq!(drops.load(Ordering::Relaxed), 3, "each reused slot dropped its old value");
    drop(revisions);
    assert_eq!(drops.load(Ordering::Relaxed), 5);
}

// Pool

#[test]
fn pool_matches_box_on_random_operations() {
    let pool = Pool::<[u64; 4]>::new(1 << 16, Tag::Plane).expect("reserve");
    let mut cache = pool.cache();
    let mut rng = Xorshift64Star::new(5);
    let mut live = Vec::new();
    let mut model: Vec<Box<[u64; 4]>> = Vec::new();
    let mut peak = 0;
    for _ in 0..200_000 {
        match rng.below(6) {
            0..=2 => {
                let value = [rng.next(); 4];
                live.push(cache.alloc(value));
                model.push(Box::new(value));
                peak = peak.max(live.len());
            }
            3 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                let pooled = live.swap_remove(i);
                let expected = model.swap_remove(i);
                assert_eq!(cache.free(pooled), *expected);
            }
            4 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                drop(live.swap_remove(i));
                model.swap_remove(i);
            }
            _ if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                live[i][0] = rng.next();
                model[i][0] = live[i][0];
            }
            _ => {}
        }
    }
    let mut indices: Vec<u32> = live.iter().map(|p| p.index()).collect();
    indices.sort_unstable();
    indices.dedup();
    assert_eq!(indices.len(), live.len(), "live objects have distinct slots");
    for (pooled, boxed) in live.iter().zip(&model) {
        assert_eq!(**pooled, **boxed);
    }
    assert!(pool.carved() as usize <= peak + 2 * 128 + 64, "freed slots are reused");
}

#[test]
fn pool_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let pool = Pool::<Counted>::new(1024, Tag::Plane).expect("reserve");
    let mut cache = pool.cache();
    let a = cache.alloc(Counted { value: 1, drops: Rc::clone(&drops) });
    let b = cache.alloc(Counted { value: 2, drops: Rc::clone(&drops) });
    let c = cache.alloc(Counted { value: 3, drops: Rc::clone(&drops) });
    cache.recycle(a);
    assert_eq!(drops.get(), 1);
    drop(b);
    assert_eq!(drops.get(), 2);
    let value = c.into_inner();
    assert_eq!(drops.get(), 2);
    assert_eq!(value.value, 3);
    drop(value);
    assert_eq!(drops.get(), 3);
}

#[test]
fn pool_caches_on_many_threads_share_slots_in_batches() {
    let pool = Pool::<u64>::new(1 << 20, Tag::Io).expect("reserve");
    thread::scope(|scope| {
        for t in 0..8u64 {
            let pool = &pool;
            scope.spawn(move || {
                let mut cache = pool.cache();
                let mut held = Vec::with_capacity(1000);
                for round in 0..200u64 {
                    for i in 0..1000 {
                        held.push(cache.alloc(t << 40 | round << 20 | i));
                    }
                    for (i, pooled) in held.drain(..).enumerate() {
                        assert_eq!(*pooled, t << 40 | round << 20 | i as u64);
                        if i % 2 == 0 {
                            cache.recycle(pooled);
                        } else {
                            drop(pooled);
                        }
                    }
                }
            });
        }
    });
    assert!(pool.carved() <= 8 * (1000 + 2 * 128) + 64 * 8, "slots are reused across threads");
}

/// One cache only allocates and another only frees, on two threads: every batch crosses
/// between them through the full stack, and the descriptors come back through the empty one
/// four at a time. Every value arrives once, and the descriptors made stay those in flight.
#[test]
fn pool_a_producer_and_a_consumer_move_batches_and_reuse_descriptors() {
    const ROUNDS: u64 = 400;
    const EACH: u64 = 1000;
    let pool = Pool::<u64>::new(1 << 20, Tag::Io).expect("reserve");
    let (send, take) = crossbeam_channel::bounded::<Vec<pool::Pooled<'_, u64>>>(4);
    thread::scope(|scope| {
        let pool = &pool;
        scope.spawn(move || {
            let mut cache = pool.cache();
            for round in 0..ROUNDS {
                let held: Vec<_> = (0..EACH).map(|i| cache.alloc(round * EACH + i)).collect();
                send.send(held).expect("the consumer is there");
            }
        });
        scope.spawn(move || {
            let mut cache = pool.cache();
            let mut next = 0;
            for held in take.iter() {
                for pooled in held {
                    assert_eq!(*pooled, next, "every value arrives once, in order");
                    next += 1;
                    cache.recycle(pooled);
                }
            }
            assert_eq!(next, ROUNDS * EACH);
        });
    });
    // Every free slot in a full batch at once (at most the slots carved over 64), plus two
    // stashes and the descriptors in a cache's hand.
    let in_flight = (7 * EACH as u32 + 4 * 128) / pool::BATCH as u32 + 2 * 8 + 4;
    assert!(pool.descriptors_made() <= in_flight, "{} descriptors made", pool.descriptors_made());
    assert!(pool.carved() as u64 <= 7 * EACH + 4 * 128, "slots are reused across threads");
}

/// The chain push and the pop of some against a `Vec` as the stack, on seeded operations.
#[test]
fn stack_chain_pushes_and_pops_of_some_match_a_vec() {
    use crate::{
        stack::{Links, NONE, Stack},
        sync::atomic::AtomicU32,
    };

    struct Table(Vec<AtomicU32>);
    impl Links for Table {
        fn link(&self, index: u32) -> &AtomicU32 {
            &self.0[index as usize]
        }
    }

    const ITEMS: u32 = 64;
    let links = Table((0..ITEMS).map(|_| AtomicU32::new(NONE)).collect());
    let stack = Stack::new();
    let mut model: Vec<u32> = Vec::new();
    let mut held: Vec<u32> = (0..ITEMS).collect();
    let mut rng = Xorshift64Star::new(5);
    for _ in 0..50_000 {
        match rng.below(4) {
            0 if !held.is_empty() => {
                let item = held.swap_remove(rng.below(held.len() as u64) as usize);
                stack.push(item, &links);
                model.push(item);
            }
            1 if !held.is_empty() => {
                let n = 1 + rng.below(held.len().min(9) as u64) as usize;
                let chain: Vec<u32> = held.drain(held.len() - n..).collect();
                stack.push_all(&chain, &links);
                model.extend(chain.iter().rev());
            }
            2 => {
                let got = stack.pop(&links);
                assert_eq!(got, model.pop());
                held.extend(got);
            }
            _ => {
                let mut out = [NONE; 9];
                let want = rng.below(10) as usize;
                let n = stack.pop_some(&links, &mut out[..want]);
                let expect: Vec<u32> = model.iter().rev().take(want).copied().collect();
                assert_eq!(&out[..n], expect.as_slice());
                model.truncate(model.len() - n);
                held.extend_from_slice(&out[..n]);
            }
        }
    }
    let mut rest = [NONE; ITEMS as usize];
    let n = stack.pop_some(&links, &mut rest);
    assert_eq!(n, model.len());
    assert!(stack.pop(&links).is_none());
}

#[test]
fn pool_raw_round_trip_keeps_the_slot_and_drops_once() {
    let drops = Rc::new(Cell::new(0));
    let pool = Pool::<Counted>::new(64, Tag::Infer).expect("reserve");
    let mut cache = pool.cache();
    let made = cache.alloc(Counted { value: 5, drops: Rc::clone(&drops) });
    let first = made.index();
    let raw = made.into_raw();
    assert_eq!(drops.get(), 0, "the raw value is kept");
    // SAFETY: `raw` came from `into_raw` of a `Pooled` of `pool`, taken back once here.
    let back = unsafe { pool::Pooled::from_raw(&pool, raw) };
    assert_eq!((back.index(), back.value), (first, 5));
    cache.recycle(back);
    assert_eq!(drops.get(), 1);
    let again = cache.alloc(Counted { value: 6, drops: Rc::clone(&drops) });
    assert_eq!(again.index(), first, "the slot is the cache's next");
    let (_, used) = measure(|| {
        // SAFETY: as above, for `again`'s pointer.
        unsafe { pool::Pooled::from_raw(&pool, again.into_raw()) }
    });
    assert_eq!(used.allocations, 0);
}

#[test]
fn pool_full_is_an_error() {
    let pool = Pool::<u64>::new(10, Tag::Io).expect("reserve");
    let mut cache = pool.cache();
    let held: Vec<_> = (0..10).map(|i| cache.alloc(i)).collect();
    assert!(matches!(cache.try_alloc(10), Err(MemError::Exhausted { tag: Tag::Io, .. })));
    drop(held);
}

#[test]
fn pool_allocates_nothing_once_warm() {
    let pool = Pool::<[u64; 8]>::new(1 << 16, Tag::Plane).expect("reserve");
    let mut cache = pool.cache();
    let mut held = Vec::with_capacity(10_000);
    let mut round = || {
        for i in 0..10_000 {
            held.push(cache.alloc([i; 8]));
        }
        for pooled in held.drain(..) {
            cache.recycle(pooled);
        }
    };
    round();
    let (_, used) = measure(&mut round);
    assert_eq!(used.allocations, 0, "a warm pool recycles its slots");
}

// Shared pooled values

#[test]
fn shared_matches_arc_on_random_operations() {
    let pool = Pool::<pool::Counted<[u64; 4]>>::new(1 << 16, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let mut rng = Xorshift64Star::new(11);
    let mut live: Vec<pool::Shared<'_, [u64; 4]>> = Vec::new();
    let mut model: Vec<Arc<[u64; 4]>> = Vec::new();
    for _ in 0..100_000 {
        match rng.below(5) {
            0 => {
                let value = [rng.next(); 4];
                live.push(cache.share(value));
                model.push(Arc::new(value));
            }
            1 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                live.push(live[i].clone());
                model.push(Arc::clone(&model[i]));
            }
            2 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                drop(live.swap_remove(i));
                model.swap_remove(i);
            }
            3 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                let value = rng.next();
                pool::Shared::make_mut(&mut live[i], &mut cache)[1] = value;
                Arc::make_mut(&mut model[i])[1] = value;
            }
            _ if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                assert_eq!(pool::Shared::owners(&live[i]) as usize, Arc::strong_count(&model[i]));
            }
            _ => {}
        }
    }
    for (a, (shared, arc)) in live.iter().zip(&model).enumerate() {
        assert_eq!(**shared, **arc);
        for (other, other_arc) in live.iter().zip(&model).skip(a + 1).take(8) {
            assert_eq!(pool::Shared::ptr_eq(shared, other), Arc::ptr_eq(arc, other_arc));
        }
    }
}

#[test]
fn shared_drops_each_value_once_when_its_last_owner_goes() {
    let drops = Rc::new(Cell::new(0));
    let pool = Pool::<pool::Counted<Counted>>::new(64, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let a = cache.share(Counted { value: 1, drops: Rc::clone(&drops) });
    let b = a.clone();
    let c = b.clone();
    assert_eq!(pool::Shared::owners(&a), 3);
    drop(a);
    drop(c);
    assert_eq!(drops.get(), 0, "an owner is left");
    assert_eq!(b.value, 1);
    drop(b);
    assert_eq!(drops.get(), 1, "the last owner drops it");
}

#[test]
fn shared_make_mut_copies_only_while_shared() {
    let pool = Pool::<pool::Counted<Vec<u32>>>::new(64, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let mut a = cache.share(vec![1, 2, 3]);
    let at = pool::Shared::index(&a);
    pool::Shared::make_mut(&mut a, &mut cache).push(4);
    assert_eq!(pool::Shared::index(&a), at, "one owner changes in place");
    let b = a.clone();
    pool::Shared::make_mut(&mut a, &mut cache).push(5);
    assert_ne!(pool::Shared::index(&a), at, "a shared value is copied first");
    assert_eq!(*a, [1, 2, 3, 4, 5]);
    assert_eq!(*b, [1, 2, 3, 4], "the other owner keeps the old value");
    assert!(pool::Shared::get_mut(&mut a).is_some());
}

#[test]
fn shared_owners_on_many_threads_drop_it_once() {
    let pool = Pool::<pool::Counted<[u64; 2]>>::new(1 << 12, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let values: Vec<_> = (0..256u64).map(|i| cache.share([i, !i])).collect();
    thread::scope(|scope| {
        for _ in 0..8 {
            let owned: Vec<_> = values.iter().cloned().collect();
            scope.spawn(move || {
                for (i, shared) in owned.into_iter().enumerate() {
                    assert_eq!(*shared, [i as u64, !(i as u64)]);
                    let more = shared.clone();
                    drop(shared);
                    drop(more);
                }
            });
        }
    });
    for shared in &values {
        assert_eq!(pool::Shared::owners(shared), 1, "every other owner went");
    }
    drop(values);
    drop(cache);
    let mut cache = pool.cache();
    let again: Vec<_> = (0..256u64).map(|i| cache.share([i, i])).collect();
    assert!(pool.carved() <= 256 + 2 * 128, "the freed slots are reused");
    drop(again);
}

#[test]
fn shared_from_matches_share_for_every_head_length() {
    let pool = Pool::<pool::Counted<[u32; 8]>>::new(64, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    for len in [0, 3, 8] {
        let head: Vec<u32> = (1..=len as u32).collect();
        let mut whole = [9u32; 8];
        whole[..len].copy_from_slice(&head);
        assert_eq!(*cache.share_from(&head, &9), *cache.share(whole), "head of {len}");
    }
    let pool = Pool::<pool::Counted<[String; 5]>>::new(64, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    for len in [0, 3, 5] {
        let head: Vec<String> = (0..len).map(|i| format!("item {i}")).collect();
        let whole: [String; 5] =
            std::array::from_fn(|i| head.get(i).cloned().unwrap_or_else(|| "fill".to_owned()));
        let made = cache.share_from(&head, &"fill".to_owned());
        assert_eq!(*made, whole, "head of {len}");
    }
}

#[test]
fn shared_from_makes_a_megabyte_slot_on_a_small_stack() {
    let pool = Pool::<pool::Counted<[u8; 1 << 20]>>::new(4, Tag::Link).expect("reserve");
    thread::scope(|scope| {
        thread::Builder::new()
            .stack_size(64 << 10)
            .spawn_scoped(scope, || {
                let mut cache = pool.cache();
                let a = cache.share_from(&[], &7);
                let b = cache.share_from(&a[..], &0);
                assert!(a.iter().chain(b.iter()).all(|&byte| byte == 7));
                assert!(!pool::Shared::ptr_eq(&a, &b), "a copy is its own slot");
            })
            .expect("spawn")
            .join()
            .expect("no overflow");
    });
    assert!(pool.carved() <= 2, "a pool of 1 MiB slots carves two at a time, not 64");
}

#[test]
fn shared_from_allocates_nothing_once_warm() {
    let pool = Pool::<pool::Counted<[u64; 64]>>::new(1 << 10, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let mut round = || {
        let first = cache.share_from(&[1, 2, 3], &0);
        let copy = cache.share_from(&first[..], &0);
        drop((first, copy));
    };
    round();
    blaze_alloc_count::assert_at_most(0, "a warm share_from and its copy", &mut round);
}

#[test]
fn shared_from_a_panicking_clone_drops_what_it_wrote_and_returns_the_slot() {
    struct Bomb {
        drops: Rc<Cell<usize>>,
        clones: Rc<Cell<usize>>,
        armed: Rc<Cell<bool>>,
    }
    impl Clone for Bomb {
        fn clone(&self) -> Bomb {
            assert!(!self.armed.get() || self.clones.get() < 4, "the fifth clone panics");
            self.clones.set(self.clones.get() + 1);
            Bomb {
                drops: Rc::clone(&self.drops),
                clones: Rc::clone(&self.clones),
                armed: Rc::clone(&self.armed),
            }
        }
    }
    impl Drop for Bomb {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    let pool = Pool::<pool::Counted<[Bomb; 8]>>::new(1, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let (drops, clones, armed) =
        (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)), Rc::new(Cell::new(true)));
    let fill =
        Bomb { drops: Rc::clone(&drops), clones: Rc::clone(&clones), armed: Rc::clone(&armed) };
    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(cache.share_from(&[], &fill));
    }));
    assert!(made.is_err());
    assert_eq!(drops.get(), 4, "the four written are dropped once each");
    armed.set(false);
    let again = cache.try_share_from(&[], &fill);
    assert!(again.is_ok(), "the pool's one slot came back");
    assert_eq!(pool.carved(), 1);
}

#[test]
fn shared_allocates_nothing_once_warm() {
    let pool = Pool::<pool::Counted<[u64; 8]>>::new(1 << 14, Tag::Link).expect("reserve");
    let mut cache = pool.cache();
    let mut held = Vec::with_capacity(2_000);
    let mut round = || {
        for i in 0..1_000 {
            let shared = cache.share([i; 8]);
            held.push(shared.clone());
            held.push(shared);
        }
        for shared in held.iter_mut().step_by(2) {
            pool::Shared::make_mut(shared, &mut cache)[0] += 1;
        }
        held.clear();
    };
    round();
    let (_, used) = measure(&mut round);
    assert_eq!(used.allocations, 0, "warm shared values reuse their slots");
}

// Slab and handles

#[test]
fn slab_matches_a_model_and_detects_stale_handles() {
    let mut slab = Slab::<u64>::new(1 << 16, Tag::Io).expect("reserve");
    let mut model: Vec<(Handle<u64>, u64)> = Vec::new();
    let mut stale: Vec<Handle<u64>> = Vec::new();
    let mut rng = Xorshift64Star::new(6);
    for _ in 0..200_000 {
        match rng.below(5) {
            0..=1 => {
                let value = rng.next();
                let handle = slab.insert(value);
                model.push((handle, value));
            }
            2 if !model.is_empty() => {
                let i = rng.below(model.len() as u64) as usize;
                let (handle, value) = model.swap_remove(i);
                assert_eq!(slab.remove(handle), Some(value));
                assert_eq!(slab.remove(handle), None, "a handle is stale once removed");
                stale.push(handle);
            }
            3 if !model.is_empty() => {
                let i = rng.below(model.len() as u64) as usize;
                let value = rng.next();
                *slab.get_mut(model[i].0).expect("live") = value;
                model[i].1 = value;
            }
            _ if !stale.is_empty() => {
                let handle = stale[rng.below(stale.len() as u64) as usize];
                assert_eq!(slab.get(handle), None, "a stale handle never reads a new value");
            }
            _ => {}
        }
    }
    assert_eq!(slab.len(), model.len());
    for &(handle, value) in &model {
        assert_eq!(slab.get(handle), Some(&value));
    }
    let mut listed: Vec<_> = slab.iter().map(|(h, &v)| (h, v)).collect();
    listed.sort_unstable_by_key(|&(h, _)| h);
    model.sort_unstable_by_key(|&(h, _)| h);
    assert_eq!(listed, model);
    assert!(slab.slots() <= 1 << 16);
}

#[test]
fn slab_reuses_slots_with_new_generations() {
    let mut slab = Slab::<&str>::new(16, Tag::Io).expect("reserve");
    let a = slab.insert("a");
    slab.remove(a);
    let b = slab.insert("b");
    assert_eq!(a.index(), b.index(), "the freed slot is reused first");
    assert_ne!(a.generation(), b.generation());
    assert_eq!(slab.get(a), None);
    assert_eq!(slab.get(b), Some(&"b"));
    assert_eq!(Handle::<&str>::from_bits(b.to_bits()), b);
    let c = slab.insert_with(|handle| if handle.index() == 1 { "c at 1" } else { "c" });
    assert_eq!(slab.get(c), Some(&"c at 1"));
    slab.clear();
    assert!(slab.is_empty());
    assert_eq!(slab.get(b), None);
    assert_eq!(slab.get(c), None);
}

#[test]
fn slab_refuses_a_made_up_even_generation() {
    let mut slab = Slab::<String>::new(16, Tag::Io).expect("reserve");
    let old = slab.insert("moved out".to_owned());
    assert_eq!(slab.remove(old).as_deref(), Some("moved out"));
    let free = Handle::<String>::from_bits(old.to_bits() + (1 << 32));
    assert_eq!(free.generation() & 1, 0, "the free slot's generation");
    assert_eq!(slab.get(free), None);
    assert!(!slab.contains(free));
    assert_eq!(slab.get_mut(free), None);
    assert_eq!(slab.remove(free), None);
    let never = Handle::<String>::from_bits(u64::from(old.index()));
    assert_eq!(slab.get(never), None, "generation 0, a retired slot's");
}

#[test]
fn slab_handle_at_names_the_slot_s_current_value_only() {
    let mut slab = Slab::<u64>::new(16, Tag::Io).expect("reserve");
    let first = slab.insert(1);
    assert_eq!(slab.handle_at(first.index()), Some(first));
    assert_eq!(slab.remove(first), Some(1));
    assert_eq!(slab.handle_at(first.index()), None, "a free slot has no handle");
    let second = slab.insert(2);
    assert_eq!(second.index(), first.index(), "the slot is reused");
    assert_eq!(slab.handle_at(second.index()), Some(second), "with its new generation");
    assert_eq!(slab.get(first), None, "the old handle stays stale");
    assert_eq!(slab.handle_at(99), None, "a slot never made");
}

#[test]
fn slab_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let mut slab = Slab::new(64, Tag::Io).expect("reserve");
    let handles: Vec<_> =
        (0..10).map(|value| slab.insert(Counted { value, drops: Rc::clone(&drops) })).collect();
    drop(slab.remove(handles[3]));
    assert_eq!(drops.get(), 1);
    drop(slab);
    assert_eq!(drops.get(), 10);
}

#[test]
fn slab_allocates_nothing_once_warm() {
    let mut slab = Slab::<[u64; 4]>::new(1 << 16, Tag::Io).expect("reserve");
    let mut handles = Vec::with_capacity(10_000);
    let mut round = |slab: &mut Slab<[u64; 4]>| {
        for i in 0..10_000 {
            handles.push(slab.insert([i; 4]));
        }
        for handle in handles.drain(..) {
            assert!(slab.get(handle).is_some());
            slab.remove(handle);
        }
    };
    round(&mut slab);
    let (_, used) = measure(|| round(&mut slab));
    assert_eq!(used.allocations, 0, "insert, lookup and remove reuse the slab's slots");
}

// Tags

#[test]
fn tag_counters_track_live_high_water_and_rate() {
    // Only this test touches `Tag::Api`.
    let before = tag::snapshot().get(Tag::Api);
    tag::add(Tag::Api, 1000);
    tag::add(Tag::Api, 500);
    tag::sub(Tag::Api, 1200);
    tag::allocated(Tag::Api, 4000);
    let after = tag::snapshot().get(Tag::Api);
    assert_eq!(after.live, before.live + 300);
    assert!(after.high_water >= before.live + 1500);
    assert_eq!(after.allocated, before.allocated + 4000);

    tag::set_budget(Tag::Api, after.live + 10);
    assert!(!tag::over_budget(Tag::Api));
    assert_eq!(tag::headroom(Tag::Api), 10);
    tag::add(Tag::Api, 11);
    assert!(tag::over_budget(Tag::Api));
    tag::sub(Tag::Api, 311);
    tag::set_budget(Tag::Api, u64::MAX);

    let mut rate = tag::Rate::new();
    let first = rate.sample(1_000_000_000, &tag::snapshot());
    assert_eq!(first[Tag::Api.index()], 0, "no rate from one sample");
    tag::allocated(Tag::Api, 2000);
    let second = rate.sample(1_500_000_000, &tag::snapshot());
    assert_eq!(second[Tag::Api.index()], 4000, "2000 bytes in half a second");
}

#[test]
fn tag_names_round_trip() {
    for tag in Tag::ALL {
        assert_eq!(Tag::from_name(tag.name()), Some(tag));
    }
    assert_eq!(Tag::from_name("nonsense"), None);
}

// Scratch

#[test]
fn scratch_lends_distinct_buffers_when_nested_and_keeps_their_capacity() {
    scratch::with_bytes(|outer| {
        outer.extend_from_slice(b"outer");
        scratch::with_bytes(|inner| {
            assert!(inner.is_empty(), "a nested call gets its own buffer");
            inner.extend_from_slice(&[0; 4096]);
        });
        assert_eq!(outer.as_slice(), b"outer");
    });
    scratch::with_bytes(|buffer| {
        assert!(buffer.is_empty());
        assert!(buffer.capacity() >= 5, "the thread's buffer keeps what it grew to");
    });
}

#[test]
fn scratch_a_shelf_keeps_its_n_largest() {
    let mut shelf = scratch::Shelf::<Vec<u8>, 8>::new();
    assert!(shelf.take().is_none(), "a new shelf is empty");
    for size in 1..=10 {
        shelf.give(Vec::with_capacity(size * 100));
    }
    let mut kept: Vec<usize> = std::iter::from_fn(|| shelf.take()).map(|b| b.capacity()).collect();
    kept.sort_unstable();
    assert_eq!(kept.len(), 8);
    assert!(kept[0] >= 300, "the two smallest went: {kept:?}");
}

#[test]
fn scratch_lent_buffers_return_to_the_shelf_of_the_thread_that_drops_them() {
    let mut lent = scratch::lend_u32s();
    assert!(lent.is_empty());
    lent.extend(0..1000);
    let grown = lent.capacity();
    let built = thread::spawn(move || {
        let lent = lent;
        assert_eq!(lent.len(), 1000, "a lent buffer moves with its owner");
        drop(lent);
        scratch::with_u32s(|ids| (ids.is_empty(), ids.capacity()))
    })
    .join()
    .expect("reader");
    assert_eq!(built, (true, grown), "the dropping thread's shelf keeps it, emptied");
    let kept = scratch::lend_bytes().into_inner();
    assert!(kept.is_empty());
}

#[test]
fn scratch_paths_are_lent_empty_and_kept_warm() {
    scratch::with_path(|path| {
        path.push("/tmp");
        path.push("blaze");
        assert!(path.ends_with("blaze"));
    });
    let (_, used) = measure(|| {
        scratch::with_path(|path| {
            assert!(path.as_os_str().is_empty());
            path.push("/tmp");
            path.push("blaze");
        })
    });
    assert_eq!(used.allocations, 0, "a warm path buffer is reused");
}

#[test]
fn scratch_lent_allocates_nothing_once_warm() {
    // Locals drop in reverse, so each buffer goes back where it was taken from.
    let work = || {
        let mut tokens = scratch::lend_u32s();
        let mut checkpoints = scratch::lend_u32s();
        tokens.extend(0..10_000);
        checkpoints.extend(0..1_000);
        tokens.len() + checkpoints.len()
    };
    work();
    let (_, used) = measure(work);
    assert_eq!(used.allocations, 0, "two lent columns come back warm");
}

#[test]
fn scratch_allocates_nothing_once_warm() {
    let work = || {
        scratch::with_u32s(|ids| {
            ids.extend(0..10_000);
            scratch::with_string(|text| {
                use std::fmt::Write as _;
                let _ = write!(text, "{} ids", ids.len());
                text.len()
            })
        })
    };
    work();
    let (_, used) = measure(work);
    assert_eq!(used.allocations, 0, "a warm scratch buffer is reused, not reallocated");
}
