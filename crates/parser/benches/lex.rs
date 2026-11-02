//! `LexedStr::new` and `LexedStr::new_hashed` on every `.rs` file of `library/`, against the
//! speed-of-light bounds of the kernel this build has
//! (`docs/v2/plan/lanes/simd-lexer/speed-of-light.md`).
//!
//! The clock is measured, not assumed: a chain of dependent adds runs one add per cycle on
//! every core blaze targets, so its time gives cycles per nanosecond. The lexer's median of 11
//! passes over the corpus, warm, is then cycles per byte. The last line is the verdict: `ok`
//! within 10% of the bound, `bug` past it (and the exit status is 1).

use std::{
    hint::black_box,
    path::{Path, PathBuf},
    time::Instant,
};

use parser::{Edition, LexedStr};

/// Cycles per byte at the bound, by kernel (the speed-of-light doc derives each).
const BOUNDS: [(&str, f64); 5] =
    [("avx512vbmi", 2.0), ("avx2", 2.25), ("neon", 2.25), ("sse2", 2.6), ("scalar", 6.85)];

/// Cycles per byte `LexedStr::new_hashed` adds to the bound on every kernel: hashing each name
/// for the interner (the speed-of-light doc's "Hashing names for the interner").
const HASHED: f64 = 0.45;

/// More than 10% over the bound is a bug.
const SLACK: f64 = 1.10;

/// Adds per run of the clock chain (the `.rept` count in [`chain`]).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const ADDS: u64 = 1000;

/// `ADDS` dependent adds, so `ADDS` cycles.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn chain(mut x: u64) -> u64 {
    // SAFETY: the asm adds to the one register it is given and touches no memory, stack or
    // other register; `add` writes the flags, which `asm!` takes as clobbered by default.
    unsafe {
        std::arch::asm!(".rept 1000", "add {x}, 1", ".endr", x = inout(reg) x, options(nomem, nostack));
    }
    x
}

/// `ADDS` dependent adds, so `ADDS` cycles.
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn chain(mut x: u64) -> u64 {
    // SAFETY: the asm adds to the one register it is given and touches no memory, stack, flags
    // or other register.
    unsafe {
        std::arch::asm!(
            ".rept 1000",
            "add {x}, {x}, #1",
            ".endr",
            x = inout(reg) x,
            options(nomem, nostack, preserves_flags)
        );
    }
    x
}

/// Cycles per nanosecond: the median of 11 timings of 20,000 chains.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn clock() -> f64 {
    const RUNS: u64 = 20_000;
    let mut x = 0;
    let mut samples: Vec<f64> = (0..11)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..RUNS {
                x = chain(black_box(x));
            }
            (RUNS * ADDS) as f64 / start.elapsed().as_nanos() as f64
        })
        .collect();
    black_box(x);
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn clock() -> f64 {
    eprintln!("the clock chain is written for x86-64 and AArch64 only; this target has no cycle count");
    std::process::exit(1);
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            rust_files(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn main() {
    let library = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library");
    let mut paths = Vec::new();
    rust_files(&library, &mut paths);
    let texts: Vec<String> =
        paths.iter().filter_map(|path| String::from_utf8(std::fs::read(path).ok()?).ok()).collect();
    assert!(texts.len() > 1000, "{} files under {}: the corpus is missing", texts.len(), library.display());
    let bytes: usize = texts.iter().map(String::len).sum();

    // The median of 11 warm passes of `lex` (its token count) over the corpus, in nanoseconds.
    let median = |lex: fn(Edition, &str) -> usize| {
        let pass = || texts.iter().map(|text| black_box(lex(Edition::CURRENT, black_box(text)))).sum::<usize>();
        black_box(pass());
        let mut samples: Vec<f64> = (0..11)
            .map(|_| {
                let start = Instant::now();
                black_box(pass());
                start.elapsed().as_nanos() as f64
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    let tokens: usize = texts.iter().map(|text| LexedStr::new(Edition::CURRENT, text).len()).sum();
    let plain = median(|edition, text| LexedStr::new(edition, text).len());
    let hashed = median(|edition, text| LexedStr::new_hashed(edition, text).name_hashes().len());

    let kernel = LexedStr::KERNEL;
    let bound = BOUNDS
        .iter()
        .find(|(name, _)| *name == kernel)
        .unwrap_or_else(|| panic!("no bound for kernel {kernel}"))
        .1;
    let ghz = clock();
    println!("kernel: {kernel}, clock {ghz:.2} GHz");
    println!(
        "{} files, {:.1} MiB, {tokens} tokens ({:.2} B/token)",
        texts.len(),
        bytes as f64 / (1 << 20) as f64,
        bytes as f64 / tokens as f64
    );
    let mut bugs = 0;
    for (what, ns, bound) in [("lex", plain, bound), ("lex hashed", hashed, bound + HASHED)] {
        let per_byte = ns * ghz / bytes as f64;
        println!(
            "{what}: {:.2} ms, {:.2} GB/s, {per_byte:.2} cycles/B; bound {bound:.2} cycles/B ({:.2} GB/s), x{:.3}",
            ns / 1e6,
            bytes as f64 / ns,
            ghz / bound,
            per_byte / bound
        );
        if per_byte > bound * SLACK {
            println!("{what}: {per_byte:.2} cycles/B is past x{SLACK} of the bound, {bound:.2}");
            bugs += 1;
        }
    }
    if bugs == 0 {
        println!("verdict: ok (within x{SLACK} of the {kernel} bounds)");
    } else {
        println!("verdict: bug ({bugs} past x{SLACK} of the {kernel} bounds)");
        std::process::exit(1);
    }
}
