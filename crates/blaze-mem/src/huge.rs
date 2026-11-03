//! Which pages a region that asks for huge pages ([`Region::reserve_huge`](crate::Region)) is
//! made of: chosen once per process by `BLAZE_HUGE_PAGES`, read at the first such reservation.
//!
//! | `BLAZE_HUGE_PAGES` | pages | what it costs |
//! |---|---|---|
//! | unset or `transparent` | [`Pages::Transparent`] where Linux has transparent huge pages on, else [`Pages::Base`] | nothing to set up; the kernel may give base pages when memory is fragmented |
//! | `reserved` | [`Pages::Reserved`]: Linux's hugetlb pool, `MAP_HUGETLB` | the pool reserved first (`sysctl vm.nr_hugepages`); a region's pages are made when committed, and a commit the pool cannot serve is an error |
//! | `off` | [`Pages::Base`] | base pages, to measure what huge pages save |
//!
//! Whatever the pages, a huge region is reserved aligned to [`HUGE_PAGE`] and commits base
//! pages while it is at most one huge page long, then whole huge pages: the many small tables
//! (a memo column per tracked function and struct) stay a few base pages each, and a table that
//! grows past 2 MiB is made of 2 MiB pages from there. Under [`Pages::Transparent`] the first
//! 2 MiB, made of base pages while the region was small, is collapsed into a huge page when the
//! region outgrows it (`MADV_COLLAPSE`, Linux 6.1; advice, so an older kernel leaves it to
//! khugepaged). Under [`Pages::Reserved`] the first 2 MiB stays base pages, so a small region
//! takes nothing from the pool.
//!
//! macOS has no huge pages for anonymous memory on Apple silicon: XNU takes superpages
//! (`VM_FLAGS_SUPERPAGE_SIZE_2MB`) on x86-64 alone and refuses them on arm64 with
//! `KERN_INVALID_ARGUMENT` (a test holds it to that, so a macOS that starts offering them
//! fails it). What the hardware gives instead is a 16 KiB base page, four times x86-64's TLB
//! reach per entry, and a large second-level TLB, so `transparent` is base pages there and
//! `reserved` is an error. Windows' large pages must be committed whole at reservation
//! (`MEM_LARGE_PAGES`), which commit-on-growth cannot do: base pages there too.
//!
//! [`HUGE_PAGE`]: crate::HUGE_PAGE

use std::{
    ffi::OsStr,
    io,
    sync::atomic::{AtomicU8, Ordering},
};

/// The variable that chooses the pages of every huge region of the process.
pub const VAR: &str = "BLAZE_HUGE_PAGES";

/// What a region's pages are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Pages {
    /// The machine's base pages (4 KiB on x86-64 and most arm64 Linux, 16 KiB on Apple
    /// silicon).
    Base = 1,
    /// Linux's transparent huge pages: the region is advised `MADV_HUGEPAGE`, and each 2 MiB
    /// of it committed whole is made one huge page when first touched, if the kernel has one.
    Transparent = 2,
    /// Linux's reserved hugetlb pool (`MAP_HUGETLB`, 2 MiB): pages made when committed, never
    /// split or swapped, and a commit the pool cannot serve is an error, never base pages.
    Reserved = 3,
}

impl Pages {
    /// Whether the region is made of huge pages past its first 2 MiB.
    pub fn is_huge(self) -> bool {
        self != Pages::Base
    }

    fn from_u8(byte: u8) -> Option<Pages> {
        match byte {
            1 => Some(Pages::Base),
            2 => Some(Pages::Transparent),
            3 => Some(Pages::Reserved),
            _ => None,
        }
    }
}

/// The pages chosen, once read; 0 before. An error is not kept, so each huge reservation after
/// one stops with the same error.
static CHOSEN: AtomicU8 = AtomicU8::new(0);

/// The pages of a region that asks for huge pages in this process: what `BLAZE_HUGE_PAGES`
/// names, checked against what the system has. An error names the variable, its value and
/// what to do.
pub fn chosen() -> io::Result<Pages> {
    if let Some(pages) = Pages::from_u8(CHOSEN.load(Ordering::Relaxed)) {
        return Ok(pages);
    }
    let pages = choose(std::env::var_os(VAR).as_deref())?;
    // Before the choice is published: a reservation that sees it sees the process policy set.
    // Two threads choosing at once both set the same flag.
    THP.store(crate::os::opt_out_unadvised(pages) as u8, Ordering::Relaxed);
    CHOSEN.store(pages as u8, Ordering::Relaxed);
    Ok(pages)
}

/// What the process told Linux about transparent huge pages for memory no region of ours
/// advised, set once with the pages [`chosen`] (`PR_SET_THP_DISABLE`). Every region of ours is
/// advised one way or the other whatever this is: huge regions `MADV_HUGEPAGE`, base ones
/// `MADV_NOHUGEPAGE`, so the host's `transparent_hugepage/enabled` (`always` on an ix VM)
/// cannot put a sparsely touched reservation on 2 MiB pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Thp {
    /// Not chosen yet ([`chosen`] not called).
    #[default]
    Unset = 0,
    /// Huge pages only where advised, as under the host setting `madvise` (Linux 6.18's
    /// `PR_THP_DISABLE_EXCEPT_ADVISED`): the pages chosen are [`Pages::Transparent`].
    AdvisedOnly = 1,
    /// No transparent huge pages in the process: the pages chosen are base or hugetlb ones.
    Off = 2,
    /// The kernel refused the setting (older than 6.18 for [`Thp::AdvisedOnly`]): unadvised
    /// memory follows the host setting; our regions are still advised one by one.
    Unavailable = 3,
    /// Not Linux: no transparent huge pages to opt out of.
    NotLinux = 4,
}

impl Thp {
    fn from_u8(byte: u8) -> Thp {
        match byte {
            1 => Thp::AdvisedOnly,
            2 => Thp::Off,
            3 => Thp::Unavailable,
            4 => Thp::NotLinux,
            _ => Thp::Unset,
        }
    }
}

impl std::fmt::Display for Thp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Thp::Unset => "unset",
            Thp::AdvisedOnly => "advised only",
            Thp::Off => "off",
            Thp::Unavailable => "host setting (kernel refused the opt-out)",
            Thp::NotLinux => "none (not Linux)",
        })
    }
}

/// [`Thp`] as set by [`chosen`]; 0 before.
static THP: AtomicU8 = AtomicU8::new(0);

/// What the process told Linux about transparent huge pages outside our regions.
pub fn thp() -> Thp {
    Thp::from_u8(THP.load(Ordering::Relaxed))
}

/// Keeps `[ptr, ptr + bytes)`, a mapping made outside any [`Region`](crate::Region) (a
/// fiber's stack, touched only at its top), on base pages whatever the host's transparent
/// huge page setting. Advice: nothing on a system without transparent huge pages.
///
/// # Safety
///
/// The range is a live mapping the caller owns.
pub unsafe fn keep_base_pages(ptr: std::ptr::NonNull<u8>, bytes: usize) {
    // SAFETY: the caller's contract.
    unsafe { crate::os::keep_base_pages(ptr, bytes) };
}

/// The pages `value` of `BLAZE_HUGE_PAGES` chooses on this system.
#[cold]
pub(crate) fn choose(value: Option<&OsStr>) -> io::Result<Pages> {
    match value.map(OsStr::to_str) {
        None | Some(Some("" | "transparent")) => Ok(transparent()),
        Some(Some("reserved")) => {
            crate::os::reserved_pool()?;
            Ok(Pages::Reserved)
        }
        Some(Some("off")) => Ok(Pages::Base),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: the error of a misspelled setting, once
            format!(
                "{VAR}={:?} names no pages: use `transparent` (the default), `reserved` (Linux's \
                 hugetlb pool) or `off`",
                value.unwrap_or_default()
            ),
        )),
    }
}

/// Transparent huge pages where the system has them on, else base pages: what the machine
/// offers without setup.
fn transparent() -> Pages {
    if crate::os::transparent_offered() { Pages::Transparent } else { Pages::Base }
}
