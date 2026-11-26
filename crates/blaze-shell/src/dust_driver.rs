//! Thin OS host driver over the `#![no_std]` sans-IO [`blaze_dust`] progressive directory-size
//! engine: lazily scans visible `ll` / `ls -l` folders on bounded background threads, streaming
//! live byte/file totals back to the Metal UI via [`dust_revision`] without ever blocking a frame.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use blaze_dust::{DirEntryMeta, DustLimits, DustMachine, DustNeed, DustProgress, EntryKind};
use blaze_own::Alloc;

static REVISION: AtomicU64 = AtomicU64::new(0);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
const MAX_CONCURRENT_SCANS: usize = 4;
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Monotonic revision bumped whenever any background folder scan ticks progress or finishes.
pub fn dust_revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

struct CachedDust {
    dir_mtime_ns: u128,
    updated_at: Instant,
    progress: DustProgress,
}

static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedDust>>> = OnceLock::new();
static IN_FLIGHT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// Non-blocking progressive folder size query for `dir_path`.
///
/// Immediately returns any cached or in-progress [`DustProgress`]. If not yet scanned (or stale
/// after 30s / mtime change), queues a background worker driving [`DustMachine`] up to
/// [`MAX_CONCURRENT_SCANS`] concurrent folders.
pub fn request_dir_size(dir_path: &Path) -> Option<DustProgress> {
    if dir_path.as_os_str().is_empty() || dir_path == Path::new("/") {
        return None;
    }
    let dir_mtime_ns = fs::metadata(dir_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(dir_path)
    {
        let fresh = hit.dir_mtime_ns == dir_mtime_ns
            && (!hit.progress.done || hit.updated_at.elapsed() < CACHE_TTL);
        if fresh {
            return Some(hit.progress);
        }
    }

    let inflight = IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()));
    let should_spawn = inflight
        .lock()
        .map(|mut set| {
            if set.contains(dir_path) {
                false
            } else if ACTIVE_WORKERS.load(Ordering::Relaxed) >= MAX_CONCURRENT_SCANS {
                false
            } else {
                set.insert(dir_path.to_path_buf());
                true
            }
        })
        .unwrap_or(false);

    if should_spawn {
        ACTIVE_WORKERS.fetch_add(1, Ordering::Relaxed);
        let root = dir_path.to_path_buf();
        if let Ok(mut guard) = cache.lock() {
            if guard.len() > 256 {
                guard.clear();
            }
            guard.insert(
                root.clone(),
                CachedDust {
                    dir_mtime_ns,
                    updated_at: Instant::now(),
                    progress: DustProgress::default(),
                },
            );
        }
        let spawn_res = std::thread::Builder::new()
            .name("blaze-dust".to_owned())
            .spawn(move || {
                run_dust_scan(&root, dir_mtime_ns);
                if let Some(f) = IN_FLIGHT.get()
                    && let Ok(mut s) = f.lock()
                {
                    s.remove(&root);
                }
                ACTIVE_WORKERS.fetch_sub(1, Ordering::Relaxed);
            });
        if spawn_res.is_err() {
            if let Some(f) = IN_FLIGHT.get()
                && let Ok(mut s) = f.lock()
            {
                s.remove(dir_path);
            }
            ACTIVE_WORKERS.fetch_sub(1, Ordering::Relaxed);
        }
    }

    cache
        .lock()
        .ok()
        .and_then(|g| g.get(dir_path).map(|e| e.progress))
}

fn run_dust_scan(root: &Path, dir_mtime_ns: u128) {
    let alloc = Alloc::global();
    let root_str = root.to_string_lossy();
    let mut machine = DustMachine::new_in(&root_str, DustLimits::INTERACTIVE, alloc);
    let mut last_publish = Instant::now();
    let mut dirs_since_publish = 0u32;

    loop {
        let next_path: Option<PathBuf> = match machine.next_need() {
            DustNeed::ReadDir { path, .. } => Some(PathBuf::from(path)),
            DustNeed::Done(_) => None,
        };
        let Some(dir_path) = next_path else {
            break;
        };

        if let Ok(rd) = fs::read_dir(&dir_path) {
            for entry in rd.flatten() {
                let fname = entry.file_name();
                let name = fname.to_string_lossy();
                let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                let ft = meta.file_type();
                let (kind, bytes, inode, nlink) = if ft.is_symlink() {
                    (EntryKind::Symlink, meta.len(), None, 1)
                } else if ft.is_dir() {
                    (EntryKind::Dir, 0, None, 1)
                } else if ft.is_file() {
                    #[cfg(unix)]
                    let (b, ino, nl) = {
                        use std::os::unix::fs::MetadataExt;
                        let blk_bytes = meta.blocks().saturating_mul(512);
                        let sz = if blk_bytes > 0 { blk_bytes } else { meta.len() };
                        (sz, Some((meta.dev(), meta.ino())), meta.nlink())
                    };
                    #[cfg(not(unix))]
                    let (b, ino, nl) = (meta.len(), None, 1);
                    (EntryKind::File, b, ino, nl)
                } else {
                    (EntryKind::Other, 0, None, 1)
                };
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos().min(i64::MAX as u128) as i64);
                if !machine.feed_entry(DirEntryMeta {
                    name: &name,
                    kind,
                    bytes,
                    inode,
                    nlink,
                    mtime_ns,
                }) {
                    break;
                }
            }
        }
        machine.finish_dir();

        dirs_since_publish += 1;
        if dirs_since_publish >= 24 || last_publish.elapsed() >= Duration::from_millis(25) {
            publish_progress(root, dir_mtime_ns, machine.progress());
            dirs_since_publish = 0;
            last_publish = Instant::now();
        }
    }

    publish_progress(root, dir_mtime_ns, machine.progress());
}

fn publish_progress(root: &Path, dir_mtime_ns: u128, progress: DustProgress) {
    if let Some(cache) = CACHE.get()
        && let Ok(mut guard) = cache.lock()
    {
        guard.insert(
            root.to_path_buf(),
            CachedDust {
                dir_mtime_ns,
                updated_at: Instant::now(),
                progress,
            },
        );
    }
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Format a byte count compactly in IEC binary units (`8 B`, `44.9 KiB`, `43.9 MiB`, `2.1 GiB`).
pub fn format_bytes_compact(bytes: u64) -> String {
    let alloc = Alloc::global();
    blaze_dust::format_iec_bytes_in(bytes, alloc).as_str().to_owned()
}
