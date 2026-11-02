//! Random reads over 1 GiB of a region on base pages, transparent huge pages and the reserved
//! pool: a dependent chase through one random cycle of its 64-byte lines, the access pattern of
//! a memo, node or intern lookup, timed and counted in page walks per read.
//!
//! The bound: 1 GiB is 512 huge pages, inside the second-level TLB of every machine blaze
//! targets (Skylake and later 1536 entries, Zen 2 and later 2048, Neoverse N1 1280), so on
//! huge pages a read costs one DRAM access and no page walk; on 4 KiB pages it is 262144
//! pages, and nearly every read walks. The target is the bound within 10%: at most 0.1 walks
//! per read on huge pages. The last line is the verdict, and the exit status is 1 when a huge
//! mode is off the bound, 2 when this machine has no counter for page walks (a VM without a
//! virtual PMU, or `perf_event_paranoid` above 2), so the bound is unmeasured.
//!
//! Run on the first build: `blaze bench -p blaze-mem --bench huge` (Linux; 1 GiB free, and for
//! the reserved mode `sysctl vm.nr_hugepages=600`).

use std::{hint::black_box, time::Instant};

use blaze_mem::{Region, Tag, huge::Pages};

const BYTES: usize = 1 << 30;
const LINE: usize = 64;
const LINES: usize = BYTES / LINE;
/// Reads timed per mode: 8 Mi, about a second on base pages.
const HOPS: usize = 1 << 23;
/// Walks per read a huge mode may have: the bound (0) within 10% of a read.
const TARGET_WALKS: f64 = 0.1;

/// splitmix64, for the cycle: the same on every run.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next()) * n as u128) >> 64) as usize
    }
}

/// One cycle through every line (Sattolo's shuffle): line `i` holds the line read after it.
fn cycle() -> Vec<u32> {
    let mut next: Vec<u32> = (0..LINES as u32).collect();
    let mut rng = SplitMix64(0x6875_6765);
    for i in (1..LINES).rev() {
        next.swap(i, rng.below(i));
    }
    next
}

/// A counter of the page walks of this thread's loads, or why there is none.
#[cfg(target_os = "linux")]
mod walks {
    use std::io;

    /// `perf_event_attr` up to `config1`: `PERF_ATTR_SIZE_VER0`, which every kernel takes.
    #[repr(C)]
    struct Attr {
        kind: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }

    const _: () = assert!(size_of::<Attr>() == 64);

    const PERF_TYPE_RAW: u32 = 4;
    /// `disabled`, `exclude_kernel` and `exclude_hv`.
    const FLAGS: u64 = 1 | 1 << 5 | 1 << 6;
    const IOC_ENABLE: u32 = 0x2400;
    const IOC_DISABLE: u32 = 0x2401;
    const IOC_RESET: u32 = 0x2403;

    pub struct Walks {
        fd: libc::c_int,
        pub event: &'static str,
    }

    /// The event that counts a load's page walk (a miss of every TLB level) here.
    #[cfg(target_arch = "x86_64")]
    fn event() -> Option<(u32, u64, &'static str)> {
        const PERF_TYPE_HW_CACHE: u32 = 3;
        // SAFETY: `cpuid` leaf 0 exists on every x86-64 core and only reads registers.
        let id = unsafe { std::arch::x86_64::__cpuid(0) };
        if [id.ebx, id.edx, id.ecx] == [0x6874_7541, 0x6974_6E65, 0x444D_4163] {
            // "AuthenticAMD": Zen's PMCx045 with every "L2 TLB miss" unit (bits 4 to 7).
            return Some((PERF_TYPE_RAW, 0xF045, "ls_l1_d_tlb_miss.all_l2_miss"));
        }
        // dTLB, read, miss: Intel's DTLB_LOAD_MISSES that cause a walk.
        Some((PERF_TYPE_HW_CACHE, 3 | 1 << 16, "dTLB-load-misses"))
    }

    /// Arm's common event `DTLB_WALK`.
    #[cfg(target_arch = "aarch64")]
    fn event() -> Option<(u32, u64, &'static str)> {
        Some((PERF_TYPE_RAW, 0x34, "dtlb_walk"))
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    fn event() -> Option<(u32, u64, &'static str)> {
        None
    }

    impl Walks {
        pub fn open() -> io::Result<Walks> {
            let Some((kind, config, event)) = event() else {
                return Err(io::Error::new(io::ErrorKind::Unsupported, "no page walk event known for this architecture"));
            };
            let attr = Attr {
                kind,
                size: size_of::<Attr>() as u32,
                config,
                sample_period: 0,
                sample_type: 0,
                read_format: 0,
                flags: FLAGS,
                wakeup_events: 0,
                bp_type: 0,
                config1: 0,
            };
            let (this_thread, any_cpu, no_group, no_flags): (libc::c_long, libc::c_long, libc::c_long, libc::c_long) =
                (0, -1, -1, 0);
            // SAFETY: the kernel reads `size` bytes of `attr`, a live value of that size; every
            // other argument is a full-width integer, as the variadic `syscall` reads them.
            let fd = unsafe {
                libc::syscall(libc::SYS_perf_event_open, &raw const attr, this_thread, any_cpu, no_group, no_flags)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Walks { fd: fd as libc::c_int, event })
        }

        /// The walks while `f` runs.
        pub fn count(&self, f: impl FnOnce()) -> u64 {
            let none: libc::c_ulong = 0;
            // SAFETY: `fd` is this counter's; the ioctls read no argument memory.
            unsafe {
                libc::ioctl(self.fd, IOC_RESET as _, none);
                libc::ioctl(self.fd, IOC_ENABLE as _, none);
            }
            f();
            let mut count = 0u64;
            // SAFETY: as above; `read` writes the 8-byte count into `count`.
            unsafe {
                libc::ioctl(self.fd, IOC_DISABLE as _, none);
                libc::read(self.fd, (&raw mut count).cast(), 8);
            }
            count
        }
    }

    impl Drop for Walks {
        fn drop(&mut self) {
            // SAFETY: the counter's own descriptor, closed once.
            unsafe { libc::close(self.fd) };
        }
    }
}

/// The region of `pages` with the cycle written in, or why it cannot be made.
fn made(pages: Pages, next: &[u32]) -> Result<Region, String> {
    let region = Region::reserve_pages(BYTES, pages, Tag::Other).map_err(|error| error.to_string())?;
    #[cfg(target_os = "linux")]
    if pages == Pages::Base {
        // So a machine with transparent huge pages `always` still measures base pages.
        // SAFETY: the region's whole reservation, holding nothing yet; advice only.
        unsafe { libc::madvise(region.base().as_ptr().cast(), region.reserved(), libc::MADV_NOHUGEPAGE) };
    }
    region.ensure(BYTES).map_err(|error| error.to_string())?;
    for (line, &to) in next.iter().enumerate() {
        // SAFETY: `[0, BYTES)` is committed, and line `line` starts inside it, 4-aligned.
        unsafe { region.at(line * LINE).cast::<u32>().as_ptr().write(to) };
    }
    Ok(region)
}

/// `hops` reads along the cycle from line 0.
fn chase(region: &Region, hops: usize) -> u32 {
    let mut at = 0u32;
    for _ in 0..hops {
        // SAFETY: every line index in the cycle is below `LINES`, inside the committed bytes.
        at = unsafe { region.at(at as usize * LINE).cast::<u32>().as_ptr().read_volatile() };
    }
    black_box(at)
}

#[cfg(target_os = "linux")]
fn main() {
    let next = cycle();
    let walks = walks::Walks::open();
    match &walks {
        Ok(walks) => println!("page walks counted by `{}`", walks.event),
        Err(why) => println!("no page walk counter ({why}): timing only"),
    }
    let mut off = Vec::new();
    for pages in [Pages::Base, Pages::Transparent, Pages::Reserved] {
        let region = match made(pages, &next) {
            Ok(region) => region,
            Err(why) => {
                println!("{pages:?}: not measured: {why}");
                continue;
            }
        };
        chase(&region, HOPS / 8);
        let start = Instant::now();
        let walked = match &walks {
            Ok(walks) => Some(walks.count(|| _ = chase(&region, HOPS))),
            Err(_) => {
                chase(&region, HOPS);
                None
            }
        };
        let ns = start.elapsed().as_nanos() as f64 / HOPS as f64;
        match walked {
            Some(walked) => {
                let per = walked as f64 / HOPS as f64;
                println!("{pages:?}: {ns:.1} ns and {per:.3} page walks per read");
                if pages.is_huge() && per > TARGET_WALKS {
                    off.push(format!("{pages:?} at {per:.3} walks per read"));
                }
            }
            None => println!("{pages:?}: {ns:.1} ns per read"),
        }
    }
    if walks.is_err() {
        println!("verdict: unmeasured: no page walk counter on this machine");
        std::process::exit(2);
    }
    if off.is_empty() {
        println!("verdict: within the bound ({TARGET_WALKS} walks per read on huge pages)");
    } else {
        println!("verdict: off the bound ({TARGET_WALKS} walks per read): {}", off.join(", "));
        std::process::exit(1);
    }
}

/// Elsewhere only base pages exist for anonymous memory (`blaze_mem::huge`): the reads are
/// timed, with no huge mode to hold to the bound.
#[cfg(not(target_os = "linux"))]
fn main() {
    let next = cycle();
    let region = made(Pages::Base, &next).unwrap_or_else(|why| panic!("{why}"));
    chase(&region, HOPS / 8);
    let start = Instant::now();
    chase(&region, HOPS);
    let ns = start.elapsed().as_nanos() as f64 / HOPS as f64;
    println!("Base: {ns:.1} ns per read");
    println!("verdict: unmeasured: {} has no huge pages for anonymous memory", std::env::consts::OS);
    std::process::exit(2);
}
