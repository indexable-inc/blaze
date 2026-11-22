//! Nanoseconds a call for each strategy of `blaze_sys::macos_kernel` (a measured experiment;
//! the default stays `Public`). Run: `cargo run --release -p blaze-sys --example cost`.
//!
//! Each case runs N iterations hot, seven times, and the best run is printed (the least
//! disturbed by interrupts and other work). Timing is std's `Instant` (an example may use std;
//! on macOS it is `mach_absolute_time`), read only around a run, never inside the loop. Nothing
//! is allocated in a timed loop: buffers, descriptors and the word are made before it.
//!
//! `Trap` traps only getpid, read, pread and close (and write); its open, fstat, clock and
//! wait/wake rows are `Kernel`'s forms (fstat `Public`'s), so those rows measure the same code.

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("cost: the strategies of blaze_sys::macos_kernel are macOS only");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{hint::black_box, sync::atomic::AtomicU32, time::Instant};

    use blaze_sys::{
        Path,
        macos_facts::MacFacts,
        macos_kernel::{Calls, Kernel, MachTime, Public},
        posix::io::{OpenFlags, OwnedFd},
    };

    /// Runs taken of each case; the best is printed.
    const RUNS: usize = 7;

    /// The cases, in the table's order.
    const CASES: [&str; 7] = [
        "getpid",
        "read 1 B /dev/zero",
        "pread 4 KiB warm",
        "open+close",
        "fstat",
        "clock",
        "wake (none) + wait (differs)",
    ];

    /// The best of [`RUNS`] runs of `iterations` calls of `body`, in nanoseconds a call.
    fn best(iterations: u32, mut body: impl FnMut()) -> f64 {
        // One run to warm caches, branch predictors and the cold table's first lookup.
        for _ in 0..iterations.min(1000) {
            body();
        }
        let mut least = f64::INFINITY;
        for _ in 0..RUNS {
            let start = Instant::now();
            for _ in 0..iterations {
                body();
            }
            let per = start.elapsed().as_nanos() as f64 / f64::from(iterations);
            least = least.min(per);
        }
        least
    }

    /// What one strategy's cases are measured on, made before any timing.
    struct Fixture {
        zero: OwnedFd,
        warm: OwnedFd,
        warm_path: std::ffi::CString,
    }

    fn fixture(dir: &std::path::Path) -> Fixture {
        let warm_path = dir.join("warm");
        std::fs::write(&warm_path, [0x5au8; 4096]).expect("write the warm file");
        let warm_path = std::ffi::CString::new(warm_path.into_os_string().into_encoded_bytes()).unwrap();
        let zero = Public.open(Path::new(c"/dev/zero"), OpenFlags::READ, 0).expect("open /dev/zero");
        let warm = Public.open(Path::new(&warm_path), OpenFlags::READ, 0).expect("open the warm file");
        let mut page = [0u8; 4096];
        // Read once, so the page is in the cache before any timing.
        assert_eq!(Public.pread(warm.fd(), &mut page, 0), Ok(4096));
        Fixture { zero, warm, warm_path }
    }

    /// Nanoseconds a call of each case for `calls`.
    fn measure<C: Calls>(calls: &C, fixture: &Fixture, facts: &MacFacts) -> [f64; 7] {
        let mut one = [0u8; 1];
        let mut page = [0u8; 4096];
        let word = AtomicU32::new(1);
        let path = Path::new(&fixture.warm_path);
        // One deadline for every wait: the value differs, so none blocks.
        let deadline = MachTime::now().after(60_000_000_000, facts.timebase);
        [
            best(1_000_000, || {
                black_box(calls.getpid());
            }),
            best(200_000, || {
                black_box(calls.read(fixture.zero.fd(), &mut one)).expect("read");
            }),
            best(200_000, || {
                black_box(calls.pread(fixture.warm.fd(), &mut page, 0)).expect("pread");
            }),
            best(20_000, || {
                let fd = calls.open(path, OpenFlags::READ, 0).expect("open");
                calls.close(fd).expect("close");
            }),
            best(200_000, || {
                black_box(calls.fstat(fixture.warm.fd())).expect("fstat");
            }),
            best(1_000_000, || {
                black_box(calls.now_ns()).expect("clock");
            }),
            best(200_000, || {
                assert_eq!(calls.wake(&word), Ok(false));
                calls.wait(&word, 2, deadline).expect("a wait on a value that differs returns at once");
            }),
        ]
    }

    pub(super) fn main() {
        let facts = MacFacts::probe().expect("this Mac's facts (D185)");
        let dir = std::env::temp_dir().join(format!("blaze-sys-cost-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the temp directory");
        let fixture = fixture(&dir);

        let public = measure(&Public, &fixture, &facts);
        let kernel = Kernel::new(&facts).expect("the Kernel strategy on this Mac");
        let kernel = measure(&kernel, &fixture, &facts);
        #[cfg(target_arch = "aarch64")]
        let trap = {
            let trap = blaze_sys::macos_kernel::Trap::new(&facts).expect("the Trap strategy on this Mac");
            Some(measure(&trap, &fixture, &facts))
        };
        #[cfg(not(target_arch = "aarch64"))]
        let trap: Option<[f64; 7]> = None;

        println!("{facts}");
        println!("timing: std::time::Instant, best of {RUNS} hot runs");
        println!("{:<30} | {:>10} | {:>10} | {:>10} | macOS", "case", "Public ns", "Kernel ns", "Trap ns");
        println!("{:-<30}-|-{:->10}-|-{:->10}-|-{:->10}-|------", "", "", "", "");
        for (at, case) in CASES.iter().enumerate() {
            let trap = match trap {
                Some(trap) => format!("{:.1}", trap[at]),
                None => "-".to_owned(),
            };
            println!("{case:<30} | {:>10.1} | {:>10.1} | {trap:>10} | {}", public[at], kernel[at], facts.os);
        }
        drop(fixture);
        std::fs::remove_dir_all(&dir).expect("remove the temp directory");
    }
}
