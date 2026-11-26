//! Pure `#![no_std]`, zero-unsafe, sans-IO progressive directory-size (`dust` / `du`) state
//! machine built on `blaze-own` explicit allocators (`Alloc<'a>`, `AVec`, `AString`).
//!
//! A thin host driver creates a [`DustMachine`], loops on [`DustMachine::next_need`], reads the
//! requested directory entries from the OS (or a deterministic simulation), and feeds them back
//! via [`DustMachine::feed_entries`]. Running byte/file totals are readable after every single
//! step so UIs like `blaze-shell`'s `ll` table can stream live folder sizes without blocking.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use blaze_own::{AString, AVec, Alloc, String, Vec};

/// Explicit traversal bounds so scanning a huge or recursive mount always terminates cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DustLimits {
    /// Maximum total filesystem entries (files + dirs) visited per root scan.
    pub max_entries: u64,
    /// Maximum subdirectory nesting depth below the root.
    pub max_depth: u16,
    /// Maximum queued directories held in the work stack.
    pub max_queued_dirs: usize,
}

impl DustLimits {
    /// Fast interactive limits tuned for live `ll` folder-size streaming (~150k entries cap).
    pub const INTERACTIVE: Self = Self {
        max_entries: 150_000,
        max_depth: 32,
        max_queued_dirs: 32_768,
    };
}

/// Kind of filesystem entry reported by the driver for a directory read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One directory entry fed into [`DustMachine::feed_entry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirEntryMeta<'a> {
    pub name: &'a str,
    pub kind: EntryKind,
    /// Prefer allocated disk bytes (`st_blocks * 512`) or logical `st_size`.
    pub bytes: u64,
    /// Optional `(dev, ino)` for hardlink deduplication when `nlink > 1`.
    pub inode: Option<(u64, u64)>,
    pub nlink: u64,
    /// Entry modification time in Unix nanoseconds (0 if unavailable).
    pub mtime_ns: i64,
}

/// Snapshot of live or completed folder-size & recursive-mtime progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DustProgress {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// Most recent `mtime` (Unix nanoseconds) across all visited descendants (0 if empty).
    pub max_mtime_ns: i64,
    pub done: bool,
    pub capped: bool,
}

/// Next action requested by [`DustMachine::next_need`].
#[derive(Debug, PartialEq, Eq)]
pub enum DustNeed<'a> {
    /// Read the directory at `path` and feed its entries back via [`DustMachine::feed_entry`]
    /// followed by [`DustMachine::finish_dir`].
    ReadDir { path: &'a str, depth: u16 },
    /// Traversal finished (or reached [`DustLimits`]); read final [`DustProgress`].
    Done(DustProgress),
}

struct QueuedDir<'a> {
    path: AString<'a>,
    depth: u16,
}

/// Sans-IO depth-first progressive directory-size accumulator.
pub struct DustMachine<'a> {
    alloc: Alloc<'a>,
    limits: DustLimits,
    queue: AVec<'a, QueuedDir<'a>>,
    active_dir: Option<QueuedDir<'a>>,
    /// Seen `(dev, ino)` pairs for multi-link files (`nlink > 1`) so hardlinks are counted once.
    seen_inodes: AVec<'a, (u64, u64)>,
    progress: DustProgress,
}

impl<'a> DustMachine<'a> {
    /// Start a new progressive scan rooted at `root_path` in `alloc`.
    pub fn new_in(root_path: &str, limits: DustLimits, alloc: Alloc<'a>) -> Self {
        let mut queue = Vec::new_in(alloc);
        queue.push(QueuedDir {
            path: String::from_str_in(root_path, alloc),
            depth: 0,
        });
        Self {
            alloc,
            limits,
            queue,
            active_dir: None,
            seen_inodes: Vec::new_in(alloc),
            progress: DustProgress::default(),
        }
    }

    /// Current live running totals (safe to read after every directory batch!).
    pub fn progress(&self) -> DustProgress {
        self.progress
    }

    /// Ask the machine for the next directory to read (or `Done`).
    pub fn next_need(&mut self) -> DustNeed<'_> {
        if self.progress.done {
            return DustNeed::Done(self.progress);
        }
        if self.active_dir.is_none() {
            match self.queue.pop() {
                Some(next) => {
                    self.active_dir = Some(next);
                }
                None => {
                    self.progress.done = true;
                    return DustNeed::Done(self.progress);
                }
            }
        }
        let cur = self.active_dir.as_ref().expect("active_dir populated above");
        DustNeed::ReadDir {
            path: cur.path.as_str(),
            depth: cur.depth,
        }
    }

    /// Feed one entry discovered inside the currently active directory.
    /// Returns `false` once [`DustLimits::max_entries`] is reached so the driver can stop early.
    pub fn feed_entry(&mut self, entry: DirEntryMeta<'_>) -> bool {
        if self.progress.done {
            return false;
        }
        let total_seen = self.progress.files.saturating_add(self.progress.dirs);
        if total_seen >= self.limits.max_entries {
            self.progress.capped = true;
            self.progress.done = true;
            self.queue.clear();
            self.active_dir = None;
            return false;
        }

        if entry.mtime_ns > self.progress.max_mtime_ns {
            self.progress.max_mtime_ns = entry.mtime_ns;
        }
        match entry.kind {
            EntryKind::File => {
                if entry.nlink > 1
                    && let Some(ino) = entry.inode
                    && !self.record_hardlink(ino)
                {
                    return true;
                }
                self.progress.files = self.progress.files.saturating_add(1);
                self.progress.bytes = self.progress.bytes.saturating_add(entry.bytes);
            }
            EntryKind::Symlink => {
                // Never follow symlinks (prevents cycles & double-counting); count link metadata bytes only.
                self.progress.files = self.progress.files.saturating_add(1);
                self.progress.bytes = self.progress.bytes.saturating_add(entry.bytes);
            }
            EntryKind::Dir => {
                self.progress.dirs = self.progress.dirs.saturating_add(1);
                if let Some(cur) = self.active_dir.as_ref()
                    && cur.depth < self.limits.max_depth
                    && self.queue.len() < self.limits.max_queued_dirs
                    && !entry.name.is_empty()
                    && entry.name != "."
                    && entry.name != ".."
                {
                    let mut child = String::from_str_in(cur.path.as_str(), self.alloc);
                    if !child.as_str().ends_with('/') {
                        child.push('/');
                    }
                    child.push_str(entry.name);
                    let next_depth = cur.depth.saturating_add(1);
                    self.queue.push(QueuedDir {
                        path: child,
                        depth: next_depth,
                    });
                }
            }
            EntryKind::Other => {}
        }
        true
    }

    /// Mark the currently active directory as finished (or unreadable/skipped).
    pub fn finish_dir(&mut self) {
        self.active_dir = None;
        if self.queue.is_empty() {
            self.progress.done = true;
        }
    }

    /// Deduplicate multi-link inodes (capped to avoid unbounded memory on pathological trees).
    fn record_hardlink(&mut self, key: (u64, u64)) -> bool {
        if self.seen_inodes.len() >= 16_384 {
            return true;
        }
        if self.seen_inodes.iter().any(|&k| k == key) {
            return false;
        }
        self.seen_inodes.push(key);
        true
    }
}

/// Binary IEC magnitude tier for subtle visual color-coding of `dust` size bars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeTier {
    /// `< 1 KiB`
    Bytes,
    /// `1 KiB .. 1 MiB`
    KiB,
    /// `1 MiB .. 1 GiB`
    MiB,
    /// `>= 1 GiB`
    GiB,
}

/// Classify `bytes` into its IEC binary order of magnitude (`2^10`, `2^20`, `2^30`).
pub const fn size_tier(bytes: u64) -> SizeTier {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes < KIB {
        SizeTier::Bytes
    } else if bytes < MIB {
        SizeTier::KiB
    } else if bytes < GIB {
        SizeTier::MiB
    } else {
        SizeTier::GiB
    }
}

/// Pure `#![no_std]` IEEE-754 base-2 logarithm for positive finite `x >= 1.0`.
///
/// Exact on powers of two and accurate to `< 1e-4` across `[1, 2^64]`, matching IEC binary
/// orders of magnitude (`+10` = `1 KiB`, `+20` = `1 MiB`, `+30` = `1 GiB`).
fn log2_pos(x: f64) -> f64 {
    if x <= 1.0 {
        return 0.0;
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    // Normalized mantissa `m` in `[1.0, 2.0)`.
    let m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    // Minimax / Padé approximation of `log2(m)` on `[1, 2)` via `t = (m - 1) / (m + 1)`.
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    // `ln(m) = 2 * t * (1 + t^2/3 + t^4/5 + t^6/7)`, scaled by `1 / ln(2)`.
    let ln_m = 2.0 * t * (1.0 + t2 * (1.0 / 3.0 + t2 * (1.0 / 5.0 + t2 * (1.0 / 7.0))));
    exp as f64 + ln_m * core::f64::consts::LOG2_E
}

/// Compute a mathematically sound $\log_2$-scale `dust` bar fraction (`0.0..=1.0`):
///
/// $$\text{frac}(b, m) = \frac{\log_2(b + 1)}{\log_2(m + 1)}$$
///
/// Every logarithm base yields the identical ratio ($\log_b x / \log_b m$), and $\log_2$ aligns
/// naturally with IEC binary byte tiers (`KiB = 2^10`, `MiB = 2^20`, `GiB = 2^30`) so a `92 KiB`
/// `.git/` folder remains clearly visible beside a `44 MiB` `target/` directory instead of collapsing
/// to zero pixels.
pub fn bar_fraction(bytes: u64, max_bytes: u64) -> f32 {
    if bytes == 0 || max_bytes == 0 {
        return 0.0;
    }
    if bytes >= max_bytes {
        return 1.0;
    }
    let num = log2_pos(bytes.saturating_add(1) as f64);
    let den = log2_pos(max_bytes.saturating_add(1) as f64);
    if den <= 0.0 {
        return 0.0;
    }
    (num / den).clamp(0.0, 1.0) as f32
}

/// Pure `#![no_std]` $\log_2$-scale recency heat (`1.0` = just modified / red-hot, `0.0` = `>= 1 year` / cold).
///
/// Maps `age_secs` smoothly across orders of magnitude (`seconds -> minutes -> hours -> days ->
/// weeks -> months -> year`) so `5h ago` and `3d ago` stand out clearly from `13d ago` and `6mo ago`.
pub fn recency_heat(age_secs: u64) -> f32 {
    const FLOOR_SECS: f64 = 60.0; // 1 minute anchor
    const MAX_SECS: f64 = 365.0 * 86_400.0; // 1 year cold floor
    let a = (age_secs as f64).min(MAX_SECS);
    let lo = log2_pos(FLOOR_SECS);
    let hi = log2_pos(MAX_SECS + FLOOR_SECS);
    let cur = log2_pos(a + FLOOR_SECS);
    let t = ((cur - lo) / (hi - lo)).clamp(0.0, 1.0);
    (1.0 - t) as f32
}

/// Format `bytes` in IEC binary units (`8 B`, `44.9 KiB`, `43.9 MiB`, `2.1 GiB`, `1.4 TiB`) into `alloc`.
pub fn format_iec_bytes_in<'a>(bytes: u64, alloc: Alloc<'a>) -> AString<'a> {
    use core::fmt::Write as _;
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    const TIB: u64 = 1024 * 1024 * 1024 * 1024;
    let mut out = String::new_in(alloc);
    let (unit, suffix) = if bytes < KIB {
        let _ = write!(out, "{bytes} B");
        return out;
    } else if bytes < MIB {
        (KIB, "KiB")
    } else if bytes < GIB {
        (MIB, "MiB")
    } else if bytes < TIB {
        (GIB, "GiB")
    } else {
        (TIB, "TiB")
    };
    let tenths = ((bytes as u128 * 10 + (unit as u128 / 2)) / unit as u128) as u64;
    let whole = tenths / 10;
    let frac = tenths % 10;
    if frac == 0 && whole >= 100 {
        let _ = write!(out, "{whole} {suffix}");
    } else {
        let _ = write!(out, "{whole}.{frac} {suffix}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sans_io_dust_accumulates_nested_dirs_and_dedups_hardlinks() {
        let alloc = Alloc::global();
        let mut m = DustMachine::new_in("/repo", DustLimits::INTERACTIVE, alloc);

        let DustNeed::ReadDir { path, depth } = m.next_need() else {
            panic!("expected root ReadDir");
        };
        assert_eq!(path, "/repo");
        assert_eq!(depth, 0);
        assert!(m.feed_entry(DirEntryMeta {
            name: "a.rs",
            kind: EntryKind::File,
            bytes: 1000,
            inode: Some((1, 10)),
            nlink: 2,
            mtime_ns: 1_000_000_000,
        }));
        // Hardlink to the same inode must only be counted once!
        assert!(m.feed_entry(DirEntryMeta {
            name: "a_link.rs",
            kind: EntryKind::File,
            bytes: 1000,
            inode: Some((1, 10)),
            nlink: 2,
            mtime_ns: 1_000_000_000,
        }));
        assert!(m.feed_entry(DirEntryMeta {
            name: "sub",
            kind: EntryKind::Dir,
            bytes: 0,
            inode: None,
            nlink: 1,
            mtime_ns: 500_000_000,
        }));
        m.finish_dir();
        assert!(!m.progress().done);
        assert_eq!(m.progress().bytes, 1000);

        let DustNeed::ReadDir { path, depth } = m.next_need() else {
            panic!("expected sub ReadDir");
        };
        assert_eq!(path, "/repo/sub");
        assert_eq!(depth, 1);
        assert!(m.feed_entry(DirEntryMeta {
            name: "b.bin",
            kind: EntryKind::File,
            bytes: 4096,
            inode: Some((1, 11)),
            nlink: 1,
            mtime_ns: 9_000_000_000,
        }));
        m.finish_dir();

        let DustNeed::Done(final_prog) = m.next_need() else {
            panic!("expected Done");
        };
        assert!(final_prog.done);
        assert!(!final_prog.capped);
        assert_eq!(final_prog.bytes, 5096);
        assert_eq!(final_prog.files, 2);
        assert_eq!(final_prog.dirs, 1);
        assert_eq!(final_prog.max_mtime_ns, 9_000_000_000);
    }

    #[test]
    fn sans_io_dust_respects_entry_cap() {
        let alloc = Alloc::global();
        let limits = DustLimits {
            max_entries: 2,
            max_depth: 4,
            max_queued_dirs: 16,
        };
        let mut m = DustMachine::new_in("/huge", limits, alloc);
        assert!(matches!(m.next_need(), DustNeed::ReadDir { .. }));
        assert!(m.feed_entry(DirEntryMeta {
            name: "f1",
            kind: EntryKind::File,
            bytes: 512,
            inode: None,
            nlink: 1,
            mtime_ns: 100,
        }));
        assert!(m.feed_entry(DirEntryMeta {
            name: "f2",
            kind: EntryKind::File,
            bytes: 512,
            inode: None,
            nlink: 1,
            mtime_ns: 200,
        }));
        assert!(!m.feed_entry(DirEntryMeta {
            name: "f3",
            kind: EntryKind::File,
            bytes: 512,
            inode: None,
            nlink: 1,
            mtime_ns: 300,
        }));
        assert!(m.progress().capped);
        assert!(m.progress().done);
        assert_eq!(m.progress().bytes, 1024);
    }

    #[test]
    fn log2_bar_fraction_and_iec_formatting() {
        let alloc = Alloc::global();
        let max_b = 44 * 1024 * 1024; // 44 MiB
        let f_8b = bar_fraction(8, max_b);
        let f_92k = bar_fraction(92 * 1024, max_b);
        let f_max = bar_fraction(max_b, max_b);
        assert!(f_8b > 0.08 && f_8b < 0.20, "8 B should be a visible stub: {f_8b}");
        assert!(f_92k > 0.55 && f_92k < 0.72, "92 KiB beside 44 MiB should be ~65%: {f_92k}");
        assert_eq!(f_max, 1.0);

        assert_eq!(format_iec_bytes_in(8, alloc).as_str(), "8 B");
        assert_eq!(format_iec_bytes_in(1024, alloc).as_str(), "1.0 KiB");
        assert_eq!(format_iec_bytes_in(44 * 1024 * 1024, alloc).as_str(), "44.0 MiB");
        assert_eq!(size_tier(512), SizeTier::Bytes);
        assert_eq!(size_tier(92 * 1024), SizeTier::KiB);
        assert_eq!(size_tier(44 * 1024 * 1024), SizeTier::MiB);
        assert_eq!(size_tier(2 * 1024 * 1024 * 1024), SizeTier::GiB);

        // Log-scale recency heatmap is strictly decreasing with age.
        let h_now = recency_heat(0);
        let h_5m = recency_heat(300);
        let h_5h = recency_heat(5 * 3600);
        let h_3d = recency_heat(3 * 86_400);
        let h_13d = recency_heat(13 * 86_400);
        let h_1y = recency_heat(365 * 86_400);
        assert_eq!(h_now, 1.0);
        assert!(h_now > h_5m && h_5m > h_5h && h_5h > h_3d && h_3d > h_13d && h_13d > h_1y);
        assert_eq!(h_1y, 0.0);
    }
}
