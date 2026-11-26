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
}

/// Snapshot of live or completed folder-size progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DustProgress {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
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

/// Compute a smooth visual `dust` bar fraction (`0.0..=1.0`) for `bytes` against `max_bytes`.
pub fn bar_fraction(bytes: u64, max_bytes: u64) -> f32 {
    if bytes == 0 || max_bytes == 0 {
        return 0.0;
    }
    ((bytes as f64) / (max_bytes as f64)).clamp(0.0, 1.0) as f32
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
        }));
        // Hardlink to the same inode must only be counted once!
        assert!(m.feed_entry(DirEntryMeta {
            name: "a_link.rs",
            kind: EntryKind::File,
            bytes: 1000,
            inode: Some((1, 10)),
            nlink: 2,
        }));
        assert!(m.feed_entry(DirEntryMeta {
            name: "sub",
            kind: EntryKind::Dir,
            bytes: 0,
            inode: None,
            nlink: 1,
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
        }));
        assert!(m.feed_entry(DirEntryMeta {
            name: "f2",
            kind: EntryKind::File,
            bytes: 512,
            inode: None,
            nlink: 1,
        }));
        assert!(!m.feed_entry(DirEntryMeta {
            name: "f3",
            kind: EntryKind::File,
            bytes: 512,
            inode: None,
            nlink: 1,
        }));
        assert!(m.progress().capped);
        assert!(m.progress().done);
        assert_eq!(m.progress().bytes, 1024);
    }
}
