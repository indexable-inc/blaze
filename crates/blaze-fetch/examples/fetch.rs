//! `fetch <Cargo.lock> <store>`: every dependency source the lock names, fetched, checked
//! against the lock and kept as one record a unit under `<store>/records/<key>`; a second run
//! fetches nothing. The thin driver over [`blaze_fetch::Machine`]: blaze-http for HTTPS, one
//! file a record (written whole to a temporary name, then renamed), the wall clock.
//!
//! Sent to the network: each crate's own URL on static.crates.io and each git repository's
//! protocol v2 requests, with a User-Agent naming blaze and its version, and nothing else.

#![feature(allocator_api)]

use std::time::{Duration, Instant};

use blaze_core::reserve::{Next, Reserve, Step as Reserving};
use blaze_core::{Done, FileName, Tag};
use blaze_fetch::fetch::{Ask, CRATES};
use blaze_fetch::plan::{self, Unit};
use blaze_fetch::{FetchIo, Failed, Limits, Machine, Method, NetFailure, Outcome, Reply, Reserved, StoreFailure, Woke};
use blaze_ids::ContentId;
use blaze_http::driver::{Fetched, Handle};
use blaze_http::{Client, Config, Kind, Request, Step, mailbox};
use blaze_manifest::lock::Lock;
use blaze_own::String as OwnString;
use blaze_text::Writes as _;

const USER_AGENT: &str = concat!("blaze/", env!("CARGO_PKG_VERSION"));
/// How long one wait lasts when no deadline is due (blaze-cli's fetch has the same bound).
const STEP_MOST_NS: u64 = 1_000_000_000;

/// Where the footprint goes (rule 16): every allocation of the process counted, its live bytes'
/// high-water mark kept, overall and per phase of the main thread (what it was doing when the
/// mark was reached: waiting on the network, looking up, storing, or taking one kind of answer).
/// The HTTP driver's own buffers (the bodies on their way) count in whichever phase they land.
mod footprint {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    pub const PHASES: [&str; 6] =
        ["waiting on the network", "a lookup", "a put", "a crate's answer", "a git advertisement", "a git pack's answer"];
    pub const WAIT: usize = 0;
    pub const LOOKUP: usize = 1;
    pub const PUT: usize = 2;
    pub const CRATE: usize = 3;
    pub const ADVERT: usize = 4;
    pub const PACK: usize = 5;

    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PHASE: AtomicUsize = AtomicUsize::new(WAIT);
    static PEAKS: [AtomicUsize; 6] = [const { AtomicUsize::new(0) }; 6];

    pub struct Counting;

    /// Blocks from this size up are mapped from the kernel and unmapped when freed, so a freed
    /// record's pages leave the footprint at once instead of staying in malloc's free lists
    /// (the cause the footprint's gap over the live heap was traced to).
    const MAPPED: usize = 1 << 20;

    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    fn mapped(layout: Layout) -> bool {
        layout.size() >= MAPPED && layout.align() <= 4096 && cfg!(target_os = "macos")
    }

    /// `len` bytes of fresh zeroed pages, or null.
    fn map(len: usize) -> *mut u8 {
        // PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON (macOS values).
        // SAFETY: an anonymous mapping at no fixed address touches no memory of ours.
        let ptr = unsafe { mmap(core::ptr::null_mut(), len, 1 | 2, 2 | 0x1000, -1, 0) };
        if ptr as isize == -1 {
            return core::ptr::null_mut();
        }
        big_in(ptr as usize, len);
        ptr
    }

    /// Bytes of answers the main thread holds (the last wait's and those kept for a
    /// reservation), and requests sent and not yet answered: what the high-water is made of.
    pub static HELD: AtomicUsize = AtomicUsize::new(0);
    pub static OPEN: AtomicUsize = AtomicUsize::new(0);
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    static AT_PEAK: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

    /// The live blocks of [`MAPPED`] bytes and up: (address, size), 0 for a free slot; and
    /// their sizes copied at the high-water.
    const SLOTS: usize = 1024;
    static BIG: [(AtomicUsize, AtomicUsize); SLOTS] = [const { (AtomicUsize::new(0), AtomicUsize::new(0)) }; SLOTS];
    static BIG_AT_PEAK: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

    fn big_in(ptr: usize, size: usize) {
        for (at, size_at) in &BIG {
            if at.compare_exchange(0, ptr, Relaxed, Relaxed).is_ok() {
                size_at.store(size, Relaxed);
                return;
            }
        }
    }

    fn big_out(ptr: usize) {
        for (at, size_at) in &BIG {
            if at.load(Relaxed) == ptr {
                size_at.store(0, Relaxed);
                at.store(0, Relaxed);
                return;
            }
        }
    }

    fn grew(by: usize) {
        let live = LIVE.fetch_add(by, Relaxed) + by;
        PEAKS[PHASE.load(Relaxed)].fetch_max(live, Relaxed);
        if PEAK.fetch_max(live, Relaxed) < live {
            AT_PEAK[0].store(HELD.load(Relaxed), Relaxed);
            AT_PEAK[1].store(OPEN.load(Relaxed), Relaxed);
            for ((_, size), copy) in BIG.iter().zip(&BIG_AT_PEAK) {
                copy.store(size.load(Relaxed), Relaxed);
            }
        }
    }

    /// At the heap's high-water: the answers' bytes the main thread held, the requests open,
    /// and the sizes of the blocks of 1 MiB and up then live, largest first.
    pub fn at_peak() -> (usize, usize, std::vec::Vec<usize>) {
        let mut big: std::vec::Vec<usize> = BIG_AT_PEAK.iter().map(|it| it.load(Relaxed)).filter(|it| *it > 0).collect();
        big.sort_unstable_by(|a, b| b.cmp(a));
        (AT_PEAK[0].load(Relaxed), AT_PEAK[1].load(Relaxed), big)
    }

    // SAFETY: every call is System's, passed through; only counters are added.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's contract, passed on.
            let ptr = if mapped(layout) { map(layout.size()) } else { unsafe { System.alloc(layout) } };
            if !ptr.is_null() {
                grew(layout.size());
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's contract, passed on; fresh pages are zero.
            let ptr = if mapped(layout) { map(layout.size()) } else { unsafe { System.alloc_zeroed(layout) } };
            if !ptr.is_null() {
                grew(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size(), Relaxed);
            if mapped(layout) {
                big_out(ptr as usize);
                // SAFETY: mapped by `map` with this length, and freed once.
                unsafe { munmap(ptr, layout.size()) };
                return;
            }
            // SAFETY: the caller's contract, passed on.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_layout = Layout::from_size_align(new_size, layout.align()).expect("the caller's contract");
            if mapped(layout) || mapped(new_layout) {
                // SAFETY: the caller's contract; the old block is copied out, then freed once.
                unsafe {
                    let new = self.alloc(new_layout);
                    if !new.is_null() {
                        core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                        self.dealloc(ptr, layout);
                    }
                    return new;
                }
            }
            // SAFETY: the caller's contract, passed on.
            let new = unsafe { System.realloc(ptr, layout, new_size) };
            if !new.is_null() {
                // A moving realloc holds both blocks for a moment: counted as the larger one only.
                if new_size >= layout.size() {
                    grew(new_size - layout.size());
                } else {
                    LIVE.fetch_sub(layout.size() - new_size, Relaxed);
                }
            }
            new
        }
    }

    pub fn phase(phase: usize) {
        PHASE.store(phase, Relaxed);
    }

    pub fn live() -> usize {
        LIVE.load(Relaxed)
    }

    pub fn peaks() -> [usize; 6] {
        core::array::from_fn(|at| PEAKS[at].load(Relaxed))
    }

    /// The process's lifetime peak `phys_footprint` (rule 50's measure; RSS also counts pages
    /// the allocator freed and the kernel has not taken back): `ri_lifetime_max_phys_footprint`
    /// of libproc's `proc_pid_rusage`, `rusage_info_v4`, the 29th 64-bit field after the
    /// 16-byte uuid.
    #[cfg(target_os = "macos")]
    pub fn max_footprint() -> u64 {
        unsafe extern "C" {
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        }
        // rusage_info_v4 is 296 bytes: room for 40 words.
        let mut info = [0u64; 40];
        // SAFETY: flavor 4 writes one rusage_info_v4, which the buffer outsizes.
        let ok = unsafe { proc_pid_rusage(std::process::id() as i32, 4, info.as_mut_ptr()) };
        if ok != 0 {
            panic!("proc_pid_rusage of this process: {}", std::io::Error::last_os_error());
        }
        info[2 + 28]
    }

    /// The process's lifetime peak resident set, in bytes: `getrusage(RUSAGE_SELF)`'s
    /// `ru_maxrss`, which Linux counts in kibibytes (as `blaze_core::process` reads a child's).
    #[cfg(target_os = "linux")]
    pub fn max_footprint() -> u64 {
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
        // SAFETY: `Rusage` has `struct rusage`'s layout; `RUSAGE_SELF` is 0. The call writes
        // it and does not keep it.
        if unsafe { getrusage(0, &mut usage) } != 0 {
            panic!("getrusage of this process: {}", std::io::Error::last_os_error());
        }
        usage.maxrss as u64 * 1024
    }
}

#[global_allocator]
static COUNTING: footprint::Counting = footprint::Counting;

fn main() {
    let args: Vec<std::string::String> = std::env::args().collect();
    // `only=crates` or `only=git`: one kind of unit, for its own peak.
    let (lock_path, store, only) = match args.as_slice() {
        [_, lock_path, store] => (lock_path, store, None),
        [_, lock_path, store, only] => (lock_path, store, only.strip_prefix("only=")),
        _ => {
            eprintln!("usage: fetch <Cargo.lock> <store folder> [only=crates|only=git]");
            std::process::exit(2);
        }
    };
    let started = Instant::now();
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Other).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let bytes = std::fs::read(lock_path).unwrap_or_else(|e| fail(&format!("reading {lock_path}: {e}")));
    let lock = Lock::read(&bytes, alloc).unwrap_or_else(|e| fail(&format!("{lock_path}: {e}")));
    let mut plan = plan::plan(&lock, alloc).unwrap_or_else(|e| fail(&shown(&e)));
    match only {
        None => {}
        Some("crates") => plan.units.retain(|unit| matches!(unit, Unit::Crate { .. })),
        Some("git") => plan.units.retain(|unit| matches!(unit, Unit::Git { .. })),
        Some(other) => fail(&format!("only={other}: not `crates` or `git`")),
    }
    let records = std::path::Path::new(store).join("records");
    std::fs::create_dir_all(&records).unwrap_or_else(|e| fail(&format!("making {}: {e}", records.display())));
    // The reservations' scratch files (`<key>.part-<n>`): a crash's leftovers swept, never
    // relied on.
    for entry in std::fs::read_dir(&records).unwrap_or_else(|e| fail(&format!("listing {}: {e}", records.display()))) {
        let entry = entry.unwrap_or_else(|e| fail(&format!("listing {}: {e}", records.display())));
        if entry.file_name().to_string_lossy().contains(blaze_core::reserve::PART) {
            std::fs::remove_file(entry.path()).unwrap_or_else(|e| fail(&format!("sweeping {}: {e}", entry.path().display())));
        }
    }
    let mut disk = blaze_core::native(std::path::Path::new(store), 2)
        .unwrap_or_else(|e| fail(&format!("the store's driver: {e}")));
    let crates = plan.units.iter().filter(|u| matches!(u, Unit::Crate { .. })).count();
    println!(
        "lock: {} packages ({} in the workspace or by path); {} units: {crates} crates, {} git commits; {} skipped",
        plan.packages,
        plan.paths,
        plan.units.len(),
        plan.units.len() - crates,
        plan.skipped.len()
    );
    for unit in plan.units.iter() {
        if let Unit::Git { packages, .. } = unit {
            println!("  {} ({packages} packages)", shown(unit));
        }
    }
    for skip in plan.skipped.iter() {
        println!("  skipped {}", shown(skip));
    }

    let (cert_file, cert_dir) = (std::env::var_os("SSL_CERT_FILE"), std::env::var_os("SSL_CERT_DIR"));
    let places = blaze_core::trust::Places { cert_file: cert_file.as_deref(), cert_dir: cert_dir.as_deref() };
    let tls = blaze_http::roots::system(places).unwrap_or_else(|e| fail(&format!("the machine's root certificates: {e}")));
    let limits = Limits::REAL;
    let config = Config::new(tls, USER_AGENT, Duration::from_secs(60));
    let handle = Handle::spawn(Client::new(config), limits.in_flight_most as usize)
        .unwrap_or_else(|e| fail(&format!("starting blaze-http's driver: {e}")));
    let (to, answers) = blaze_core::chan::unbounded::<(u64, Fetched)>();

    let mut machine = Machine::new(&plan.units, limits, alloc);
    let git: std::vec::Vec<bool> = plan.units.iter().map(|unit| matches!(unit, Unit::Git { .. })).collect();
    let mut io = Io { started, records: &records, disk: &mut disk, handle: &handle, to: &to, answers: &answers, git: &git, last: None, kept: std::collections::HashMap::new(), wrote: 0 };
    while !machine.is_done() {
        blaze_fetch::drive::step(&mut machine, &mut io, limits.ask_ns);
    }
    let wrote = io.wrote;

    let (mut present, mut fetched, mut failed) = (0u32, 0u32, 0u32);
    let (mut down, mut files, mut unpacked) = (0u64, 0u64, 0u64);
    for (at, outcome) in machine.outcomes().iter().enumerate() {
        match outcome {
            Outcome::Present => present += 1,
            Outcome::Fetched { down: d, files: f, bytes, cache } => {
                fetched += 1;
                if let Some(cache) = cache {
                    let reads = cache.hits + cache.misses;
                    println!(
                        "  {}: {f} files; base cache {} hits of {reads} delta reads ({:.0}%), held at most {:.1} MiB; {:.1} MiB inflated again",
                        shown(&plan.units[at]),
                        cache.hits,
                        100.0 * cache.hits as f64 / reads.max(1) as f64,
                        cache.held_most as f64 / (1 << 20) as f64,
                        cache.inflated as f64 / (1 << 20) as f64
                    );
                }
                down += d;
                files += u64::from(*f);
                unpacked += bytes;
            }
            Outcome::Failed(why) => {
                failed += 1;
                println!("  failed {}", shown(&Failed { unit: &plan.units[at], why }));
            }
            Outcome::Pending => unreachable!("the machine is done"),
        }
    }
    println!(
        "units: {} already present, {fetched} fetched, {failed} failed; downloaded {down} bytes ({:.1} MiB); unpacked {files} files, {unpacked} bytes ({:.1} MiB); records written {wrote} bytes; {} answers after their deadline",
        present,
        down as f64 / (1 << 20) as f64,
        unpacked as f64 / (1 << 20) as f64,
        machine.stale()
    );
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "time: {seconds:.2} s ({:.1} MiB/s down); from {CRATES} and each git repository's https URL",
        down as f64 / (1 << 20) as f64 / seconds
    );
    println!(
        "budget: bytes in flight at most {:.1} MiB of {:.0} MiB (requests' `most`s and records not yet stored)",
        machine.in_flight_peak() as f64 / (1 << 20) as f64,
        limits.bytes_most as f64 / (1 << 20) as f64
    );
    let peaks = footprint::peaks();
    let (at, most) = peaks.iter().enumerate().max_by_key(|(_, peak)| **peak).expect("six phases");
    println!(
        "heap: high-water {:.1} MiB, reached in {}; live at the end {:.1} MiB",
        *most as f64 / (1 << 20) as f64,
        footprint::PHASES[at],
        footprint::live() as f64 / (1 << 20) as f64
    );
    let (held, open, big) = footprint::at_peak();
    let mib = |it: usize| it as f64 / (1 << 20) as f64;
    println!(
        "at the heap's high-water: {:.1} MiB of answers held by the loop, {open} requests open; blocks of 1 MiB and up: {} of {:.1} MiB, the largest {:?} MiB",
        mib(held),
        big.len(),
        mib(big.iter().sum()),
        big.iter().take(12).map(|it| (mib(*it) * 10.0).round() / 10.0).collect::<std::vec::Vec<f64>>()
    );
    println!("footprint: lifetime peak phys_footprint {:.1} MiB", footprint::max_footprint() as f64 / (1 << 20) as f64);
    for (phase, peak) in footprint::PHASES.iter().zip(peaks) {
        println!("  high-water in {phase}: {:.1} MiB", peak as f64 / (1 << 20) as f64);
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

/// The example's IO ([`FetchIo`]): blaze-http's driver thread for HTTPS, one file a record (a
/// put written whole to a temporary name, then renamed; a reservation through
/// `blaze_core::reserve` on the store's driver), the wall clock since the start.
struct Io<'r> {
    started: Instant,
    records: &'r std::path::Path,
    disk: &'r mut blaze_core::Native,
    handle: &'r Handle,
    to: &'r blaze_core::chan::Sender<(u64, Fetched)>,
    answers: &'r blaze_core::chan::Receiver<(u64, Fetched)>,
    /// Per unit, whether it is a git commit (for the footprint's phase).
    git: &'r [bool],
    /// The answer the last wait gave: its unit and body.
    last: Option<(u32, std::vec::Vec<u8>)>,
    /// Answers kept for a reservation, by unit, until it is made.
    kept: std::collections::HashMap<u32, std::vec::Vec<u8>>,
    /// Bytes of records written.
    wrote: u64,
}

/// An answer's bytes leaving [`footprint::HELD`] when this drops.
struct Held(usize);

impl Drop for Held {
    fn drop(&mut self) {
        footprint::HELD.fetch_sub(self.0, std::sync::atomic::Ordering::Relaxed);
    }
}

impl FetchIo for Io<'_> {
    fn now(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }

    fn lookup(&mut self, key: &ContentId) -> bool {
        footprint::phase(footprint::LOOKUP);
        self.records.join(hex(&key.0)).is_file()
    }

    fn put(&mut self, key: &ContentId, record: &[u8]) -> Result<(), StoreFailure> {
        footprint::phase(footprint::PUT);
        let path = self.records.join(hex(&key.0));
        let temporary = self.records.join(format!("{}.part", hex(&key.0)));
        self.wrote += record.len() as u64;
        std::fs::write(&temporary, record)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|e| if e.kind() == std::io::ErrorKind::StorageFull { StoreFailure::Full } else { StoreFailure::Io })
    }

    fn send(&mut self, ask: Ask, method: Method, url: &str, headers: &[(&str, &str)], body: &[u8], most: u64, deadline_ns: u64) {
        let request = Request {
            method: match method {
                Method::Get => blaze_http::Method::Get,
                Method::Post => blaze_http::Method::Post,
            },
            url: url.to_owned(),
            headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
            body: body.to_vec(),
            gzip: false,
            most,
            https_only: true,
            range: None,
        };
        let within = Duration::from_nanos(deadline_ns.saturating_sub(self.now()));
        footprint::OPEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.handle.fetch(request, within, u64::from(ask.unit) << 32 | u64::from(ask.seq), self.to);
    }

    fn wait(&mut self, until_ns: Option<u64>) -> Woke<'_> {
        footprint::phase(footprint::WAIT);
        if let Some((_, body)) = self.last.take() {
            footprint::HELD.fetch_sub(body.len(), std::sync::atomic::Ordering::Relaxed);
        }
        let wait = Duration::from_nanos(until_ns.map_or(STEP_MOST_NS, |at| at.saturating_sub(self.now())));
        match mailbox::answer("a dependency's HTTPS answer", self.answers, wait) {
            Ok(Some((key, fetched))) => {
                footprint::OPEN.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                if let Fetched::Answered { body, .. } = &fetched {
                    footprint::HELD.fetch_add(body.len(), std::sync::atomic::Ordering::Relaxed);
                }
                let ask = Ask { unit: (key >> 32) as u32, seq: key as u32 };
                footprint::phase(match self.git.get(ask.unit as usize) {
                    Some(false) | None => footprint::CRATE,
                    // An advertisement is a few hundred bytes; a pack's answer is never under 64 KiB
                    // here (the phase only names where a peak was, it decides nothing).
                    Some(true) if matches!(&fetched, Fetched::Answered { body, .. } if body.len() < 64 << 10) => footprint::ADVERT,
                    Some(true) => footprint::PACK,
                });
                let reply = match fetched {
                    Fetched::Answered { status, body, .. } => {
                        let (_, body) = self.last.insert((ask.unit, body));
                        Reply::Body { status, bytes: body.as_slice() }
                    }
                    Fetched::Failed(error) => Reply::Failed(failure(&error.kind)),
                    Fetched::TimedOut => Reply::Failed(NetFailure::Timeout),
                };
                Woke::Answer { ask, reply }
            }
            Ok(None) => fail("blaze-http's driver stopped"),
            // The deadline the machine named: its next poll fails or releases what is due.
            Err(_due) => Woke::Due,
        }
    }

    fn keep(&mut self, unit: u32) {
        if let Some((of, body)) = self.last.take()
            && of == unit
        {
            self.kept.insert(unit, body);
        }
    }

    fn reserve(
        &mut self,
        unit: u32,
        key: &ContentId,
        room: u64,
        write: &mut dyn FnMut(&[u8], Result<&mut [u8], StoreFailure>) -> Option<u64>,
    ) -> Reserved {
        footprint::phase(footprint::PUT);
        let body = self.kept.remove(&unit).unwrap_or_else(|| fail("a reservation with no answer kept"));
        // Freed at the end of this call.
        let _held = Held(body.len());
        let name = FileName::new(&format!("records/{}", hex(&key.0))).expect("a name");
        let (mut reserve, first) = Reserve::new(name, room, Tag(u64::from(unit)));
        let mut done = ask(self.disk, first);
        loop {
            match reserve.answer(done) {
                Ok(Next::Write(mut mapping)) => match write(&body, Ok(mapping.bytes_mut())) {
                    Some(len) => {
                        self.wrote += len;
                        let effect = reserve.finish(mapping, len).unwrap_or_else(|e| fail(&e.to_string()));
                        done = ask(self.disk, effect);
                    }
                    // The machine failed the unit: nothing is kept.
                    None => {
                        if let Some(effect) = reserve.abandon(Some(mapping)) {
                            _ = ask(self.disk, effect);
                        }
                        return Reserved::Abandoned;
                    }
                },
                Ok(Next::Ask(effect)) => done = ask(self.disk, effect),
                Ok(Next::Stored) => return Reserved::Kept,
                Ok(Next::Abandoned) => return Reserved::Abandoned,
                Err(error) => {
                    eprintln!("fetch: {error}");
                    let mapped = reserve.step() != Reserving::Mapping;
                    if let Some(effect) = reserve.abandon(None) {
                        _ = ask(self.disk, effect);
                    }
                    return match mapped {
                        false => {
                            _ = write(&body, Err(StoreFailure::Io));
                            Reserved::Abandoned
                        }
                        true => Reserved::Unkept(StoreFailure::Io),
                    };
                }
            }
        }
    }
}

/// What a blaze-http error is to the machine: only no connection, a broken one and no answer
/// in time are transient.
fn failure(kind: &Kind) -> NetFailure {
    match kind {
        Kind::Io { step: Step::Connect | Step::Proxy, .. } => NetFailure::Connect,
        Kind::Io { step: Step::Tls, .. } => NetFailure::Refused,
        Kind::Io { .. } => NetFailure::Reset,
        Kind::NoAnswer { .. } | Kind::Stalled { .. } => NetFailure::Timeout,
        Kind::TooBig { said, .. } => NetFailure::TooBig { said: *said },
        _ => NetFailure::Refused,
    }
}

fn shown<T: blaze_text::Show>(value: &T) -> std::string::String {
    let mut out = OwnString::new_in(std::alloc::Global);
    out.show(value);
    out.as_str().to_owned()
}

/// One store effect run to its completion on the store's driver.
fn ask(disk: &mut blaze_core::Native, effect: blaze_core::Effect) -> Done {
    let mut effects = vec![effect];
    let mut got = None;
    blaze_core::run_until_idle(disk, &mut effects, |completion, _| got = Some(completion.done))
        .unwrap_or_else(|stalled| fail(&format!("the store's driver stalled: {stalled:?}")));
    got.unwrap_or_else(|| fail("the store's driver gave no completion"))
}

fn hex(bytes: &[u8; 32]) -> std::string::String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fail(why: &str) -> ! {
    eprintln!("fetch: {why}");
    std::process::exit(1);
}
