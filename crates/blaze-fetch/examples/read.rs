//! Reads a fetched store in place: every unit of a lock's record mapped, opened by its
//! `SourceId` (verified once) and its `Cargo.toml`s read as borrowed slices; then one whole
//! record's files. Prints the time of each part and the lifetime peak footprint.
//!
//! `read <Cargo.lock> <store folder> [whole=<crate name>]`

#![feature(allocator_api)]

use std::time::Instant;

use blaze_fetch::PackView;
use blaze_fetch::plan;
use blaze_manifest::lock::Lock;
use blaze_own::String as OwnString;
use blaze_text::Writes as _;

/// A file mapped read-only and private, unmapped when dropped.
struct Mapped {
    ptr: *mut u8,
    len: usize,
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

impl Mapped {
    fn of(path: &std::path::Path) -> std::io::Result<Mapped> {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        // SAFETY: a read-only private mapping of `len` bytes of an open file, which the store
        // only ever replaces by a rename (the bytes under it never change).
        let ptr = unsafe { mmap(core::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
        if ptr as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Mapped { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: mapped above, `len` bytes, readable while `self` lives.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: mapped by `of` with this length, unmapped once.
        unsafe { munmap(self.ptr, self.len) };
    }
}

/// The lifetime peak `phys_footprint` (`rusage_info_v4`'s `ri_lifetime_max_phys_footprint`).
#[cfg(target_os = "macos")]
fn max_footprint() -> u64 {
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    let mut info = [0u64; 40];
    // SAFETY: flavor 4 writes one rusage_info_v4 (296 bytes), which the buffer outsizes.
    let ok = unsafe { proc_pid_rusage(std::process::id() as i32, 4, info.as_mut_ptr()) };
    if ok != 0 {
        panic!("proc_pid_rusage of this process: {}", std::io::Error::last_os_error());
    }
    info[2 + 28]
}

/// The lifetime peak resident set, in bytes: `getrusage(RUSAGE_SELF)`'s `ru_maxrss`, which
/// Linux counts in kibibytes (as `blaze_core::process` reads a child's).
#[cfg(target_os = "linux")]
fn max_footprint() -> u64 {
    /// `struct rusage` on 64-bit Linux: two 16-byte `timeval`s, then fourteen `long`s,
    /// `ru_maxrss` first.
    #[repr(C)]
    struct Rusage {
        _times: [i64; 4],
        maxrss: i64,
        _rest: [i64; 13],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }
    let mut usage = Rusage { _times: [0; 4], maxrss: 0, _rest: [0; 13] };
    // SAFETY: `Rusage` has `struct rusage`'s layout; `RUSAGE_SELF` is 0. The call writes it
    // and does not keep it.
    if unsafe { getrusage(0, &mut usage) } != 0 {
        panic!("getrusage of this process: {}", std::io::Error::last_os_error());
    }
    usage.maxrss as u64 * 1024
}

fn main() {
    let args: Vec<std::string::String> = std::env::args().collect();
    let (lock_path, store, whole) = match args.as_slice() {
        [_, lock_path, store] => (lock_path, store, None),
        [_, lock_path, store, whole] => (lock_path, store, whole.strip_prefix("whole=")),
        _ => fail("usage: read <Cargo.lock> <store folder> [whole=<crate name>]"),
    };
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Other).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let text = std::fs::read(lock_path).unwrap_or_else(|e| fail(&format!("reading {lock_path}: {e}")));
    let lock = Lock::read(&text, alloc).unwrap_or_else(|e| fail(&format!("{lock_path}: {e}")));
    let plan = plan::plan(&lock, alloc).unwrap_or_else(|e| fail(&shown(&e)));
    let records = std::path::Path::new(store).join("records");
    let at_start = max_footprint();

    // Every record mapped and opened by its id: verified once, here.
    let started = Instant::now();
    let mut maps = Vec::with_capacity(plan.units.len());
    let mut mapped_bytes = 0u64;
    for unit in plan.units.iter() {
        let path = records.join(hex(&unit.key().0));
        let map = Mapped::of(&path).unwrap_or_else(|e| fail(&format!("{}: {e} ({})", path.display(), shown(unit))));
        mapped_bytes += map.len as u64;
        maps.push(map);
    }
    let mapped = started.elapsed();
    // `READ_PUT=1`: records this machine put, their digests not hashed again.
    let put = std::env::var_os("READ_PUT").is_some();
    let open = if put { PackView::open_source_put } else { PackView::open_source };
    println!("open: {}", if put { "open_source_put (form, rows and source; no hash)" } else { "open_source (BLAKE3 of every byte)" });
    let mut views = Vec::with_capacity(maps.len());
    for (unit, map) in plan.units.iter().zip(&maps) {
        let view = open(&unit.source(), map.bytes())
            .unwrap_or_else(|e| fail(&format!("{}: {}", shown(unit), shown(&e))));
        views.push(view);
    }
    let opened = started.elapsed();

    // Every `Cargo.toml`: a crate's at its top, a git commit's wherever its packages are.
    let mut manifests = 0u32;
    let mut manifest_bytes = 0u64;
    let mut touched = 0u64;
    for view in views.iter() {
        for file in view.files() {
            if file.path == "Cargo.toml" || file.path.ends_with("/Cargo.toml") {
                manifests += 1;
                manifest_bytes += file.bytes.len() as u64;
                touched = touched.wrapping_add(file.bytes.iter().map(|b| u64::from(*b)).sum::<u64>());
            }
        }
    }
    let read = started.elapsed();
    let files: usize = views.iter().map(|v| v.len()).sum();
    println!(
        "units: {} records mapped ({:.1} MiB) in {:.1} ms; opened by their ids, each verified once, at {:.1} ms; {files} files listed",
        views.len(),
        mapped_bytes as f64 / (1 << 20) as f64,
        mapped.as_secs_f64() * 1e3,
        opened.as_secs_f64() * 1e3
    );
    println!(
        "manifests: {manifests} Cargo.toml read in place ({:.1} KiB) in {:.2} ms after the opens; {:.1} ms in all (byte sum {touched})",
        manifest_bytes as f64 / 1024.0,
        (read - opened).as_secs_f64() * 1e3,
        read.as_secs_f64() * 1e3
    );
    println!(
        "footprint: lifetime peak phys_footprint {:.1} MiB (at the start {:.1} MiB)",
        max_footprint() as f64 / (1 << 20) as f64,
        at_start as f64 / (1 << 20) as f64
    );

    if let Some(name) = whole {
        // `whole=largest`: the crate of the largest record.
        let largest = (0..maps.len()).filter(|at| matches!(plan.units[*at], plan::Unit::Crate { .. })).max_by_key(|at| maps[*at].len);
        let found = if name == "largest" {
            largest
        } else {
            plan.units.iter().position(|u| matches!(u, plan::Unit::Crate { name: n, .. } if *n == name))
        };
        let Some(at) = found else {
            fail(&format!("no crate {name} in the lock"));
        };
        let unit = &plan.units[at];
        let started = Instant::now();
        let map = Mapped::of(&records.join(hex(&unit.key().0))).unwrap_or_else(|e| fail(&format!("{e}")));
        let view = PackView::open_source(&unit.source(), map.bytes()).unwrap_or_else(|e| fail(&shown(&e)));
        let opened = started.elapsed();
        let mut sum = 0u64;
        let mut bytes = 0u64;
        for file in view.files() {
            bytes += file.bytes.len() as u64;
            sum = sum.wrapping_add(file.bytes.iter().map(|b| u64::from(*b)).sum::<u64>());
        }
        let all = started.elapsed();
        println!(
            "whole: {}: {} files, {:.1} MiB, opened (verified) in {:.1} ms, every byte read in place by {:.1} ms ({:.2} GB/s over the files; byte sum {sum})",
            shown(unit),
            view.len(),
            bytes as f64 / (1 << 20) as f64,
            opened.as_secs_f64() * 1e3,
            all.as_secs_f64() * 1e3,
            bytes as f64 / (all - opened).as_secs_f64().max(1e-9) / 1e9
        );
        println!("footprint: lifetime peak phys_footprint {:.1} MiB", max_footprint() as f64 / (1 << 20) as f64);
    }
}

fn shown<T: blaze_text::Show>(value: &T) -> std::string::String {
    let mut out = OwnString::new_in(std::alloc::Global);
    out.show(value);
    out.as_str().to_owned()
}

fn hex(bytes: &[u8; 32]) -> std::string::String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fail(why: &str) -> ! {
    eprintln!("read: {why}");
    std::process::exit(1);
}
