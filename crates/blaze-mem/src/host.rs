//! What the host's memory offers, as one plain value: the process's huge-page choice
//! (`BLAZE_HUGE_PAGES`) and the three Linux settings blaze-mem used to read itself (the
//! overcommit mode, the transparent huge page setting and the 2 MiB hugetlb pool's size).
//!
//! blaze-mem sits below the effect layer, so it reads none of them: blaze-core reads them once
//! at boot (the variable from the environment it is given, the files through its `Dir`), parses
//! the TEXT with the functions here, and passes the [`HostMemory`] in
//! ([`huge::chosen_in`](crate::huge::chosen_in) at the program's start; every later
//! [`Region::reserve_huge`](crate::Region::reserve_huge) takes the choice made then). Until
//! every caller does, the old entry points ([`huge::chosen`](crate::huge::chosen)) read them
//! themselves and call the new ones.
//!
//! No `std` here: plain values and text parsers only.

/// Which pages a region that asks for huge pages is made of, as the process chose it
/// (`BLAZE_HUGE_PAGES`; huge.rs's table says what each costs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HugeChoice {
    /// Unset, empty or `transparent`: Linux's transparent huge pages where the host offers
    /// them, else base pages.
    Transparent,
    /// `reserved`: Linux's hugetlb pool, which must have pages.
    Reserved,
    /// `off`: base pages only, to measure what huge pages save.
    Never,
}

impl HugeChoice {
    /// The choice when the variable is not set.
    pub const UNSET: HugeChoice = HugeChoice::Transparent;

    /// The choice the variable's `value` names, or `None` when it names none (the caller's
    /// error names the variable and the value).
    pub fn from_setting(value: &str) -> Option<HugeChoice> {
        match value {
            "" | "transparent" => Some(HugeChoice::Transparent),
            "reserved" => Some(HugeChoice::Reserved),
            "off" => Some(HugeChoice::Never),
            _ => None,
        }
    }
}

/// Linux's `vm.overcommit_memory` (`/proc/sys/vm/overcommit_memory`): only to word the error
/// of a refused reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Overcommit {
    /// `0`: the kernel's heuristic; `MAP_NORESERVE` is honored.
    Heuristic,
    /// `1`: every reservation passes.
    Always,
    /// `2`: every writable reservation is charged whole to the commit limit.
    Strict,
    /// Not read: not Linux, or the file could not be read or parsed.
    Unread,
}

impl Overcommit {
    /// The mode the file's `text` holds; `None` for any other text.
    pub fn parse(text: &str) -> Option<Overcommit> {
        match text.trim() {
            "0" => Some(Overcommit::Heuristic),
            "1" => Some(Overcommit::Always),
            "2" => Some(Overcommit::Strict),
            _ => None,
        }
    }

    /// The mode as an error's text says it.
    pub fn shown(self) -> &'static str {
        match self {
            Overcommit::Heuristic => "0",
            Overcommit::Always => "1",
            Overcommit::Strict => "2",
            Overcommit::Unread => "unread",
        }
    }
}

/// Linux's transparent huge page setting (`/sys/kernel/mm/transparent_hugepage/enabled`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThpSetting {
    /// `[always]`.
    Always,
    /// `[madvise]`: huge pages where a region is advised `MADV_HUGEPAGE`, as ours are.
    Madvise,
    /// `[never]`.
    Never,
    /// No setting: not Linux, a kernel without transparent huge pages, or a file not read.
    Missing,
}

impl ThpSetting {
    /// The setting the file's `text` marks with brackets (`always [madvise] never`); `None`
    /// when it marks none of the three.
    pub fn parse(text: &str) -> Option<ThpSetting> {
        if text.contains("[never]") {
            Some(ThpSetting::Never)
        } else if text.contains("[madvise]") {
            Some(ThpSetting::Madvise)
        } else if text.contains("[always]") {
            Some(ThpSetting::Always)
        } else {
            None
        }
    }

    /// Whether Linux makes transparent huge pages for a region advised `MADV_HUGEPAGE`.
    pub fn offers(self) -> bool {
        matches!(self, ThpSetting::Always | ThpSetting::Madvise)
    }
}

/// The pages the 2 MiB hugetlb pool holds, from the text of
/// `/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages`; `None` for other text.
pub fn parse_pool(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// The host's memory facts, read once at boot and passed down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostMemory {
    /// The process's huge-page choice (`BLAZE_HUGE_PAGES`).
    pub huge_pages: HugeChoice,
    /// `vm.overcommit_memory`, for the error of a refused reservation.
    pub overcommit: Overcommit,
    /// The transparent huge page setting.
    pub thp: ThpSetting,
    /// The 2 MiB hugetlb pool's size in pages; `None` when there is none to read.
    pub huge_pool_2m: Option<u64>,
}

impl HostMemory {
    /// Whether this host makes transparent huge pages for an advised region: Linux only.
    pub fn transparent_offered(&self) -> bool {
        cfg!(target_os = "linux") && self.thp.offers()
    }
}

#[cfg(test)]
mod tests {
    use super::{HostMemory, HugeChoice, Overcommit, ThpSetting};
    use crate::huge::{Pages, pages_for};

    /// `BLAZE_HUGE_PAGES=off` is base pages whatever the host offers (huge.rs's table: `off`
    /// -> [`Pages::Base`], "base pages, to measure what huge pages save"), as the old
    /// `huge::choose(Some("off"))` answered: the decision from a given [`HostMemory`] says no
    /// huge pages for [`HugeChoice::Never`] on every setting and pool, and still says huge
    /// pages are possible for the default where Linux offers them (so a decision that ignored
    /// the choice fails). Prints "test host::tests::never_is_base_pages ... ok".
    #[test]
    fn never_is_base_pages() {
        let settings = [ThpSetting::Always, ThpSetting::Madvise, ThpSetting::Never, ThpSetting::Missing];
        for thp in settings {
            for huge_pool_2m in [None, Some(0), Some(4096)] {
                let host = HostMemory {
                    huge_pages: HugeChoice::Never,
                    overcommit: Overcommit::Heuristic,
                    thp,
                    huge_pool_2m,
                };
                let pages = pages_for(&host).expect("`off` is honored on every host");
                assert_eq!(pages, Pages::Base, "{host:?}");
                assert!(!pages.is_huge(), "{host:?}");
            }
        }
        let offered = HostMemory {
            huge_pages: HugeChoice::UNSET,
            overcommit: Overcommit::Heuristic,
            thp: ThpSetting::Madvise,
            huge_pool_2m: None,
        };
        let expected = if cfg!(target_os = "linux") { Pages::Transparent } else { Pages::Base };
        assert_eq!(pages_for(&offered).expect("the default"), expected);
        assert_eq!(HugeChoice::from_setting("off"), Some(HugeChoice::Never));
        assert_eq!(HugeChoice::from_setting("always"), None);
    }
}
