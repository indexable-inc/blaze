//! What blaze-deflate costs, against the tree's old `miniz_oxide` (flate2's rust backend), on
//! the workspace's own Rust source: MB/s of uncompressed bytes, the best of five runs, and the
//! compressed sizes; then each vector routine against its scalar twin.
//!
//! Run (release, or the numbers mean nothing):
//! `cargo +nightly-2026-10-27 run --release -p blaze-deflate --example cost [megabytes]`

#![feature(allocator_api)]

use std::{hint::black_box, path::Path, time::Instant};

use blaze_deflate::{
    Inflate, InflateInto, Level, Limits, MAX_RATIO,
    simd::{adler_run, adler_run_scalar, match_len, match_len_scalar},
};
use blaze_own::{Alloc, Vec};

const RUNS: usize = 5;

/// The benchmark's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// Every `.rs` file under `dir`, in path order, appended to `out` until it holds `most` bytes.
fn gather(dir: &Path, most: usize, out: &mut std::vec::Vec<u8>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: std::vec::Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if out.len() >= most {
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() && name != "target" && !name.starts_with('.') {
            gather(&path, most, out);
        } else if name.ends_with(".rs") {
            if let Ok(bytes) = std::fs::read(&path) {
                out.extend_from_slice(&bytes);
            }
        }
    }
    out.truncate(most);
}

/// The best of [`RUNS`] timings of `f`, as MB/s of `bytes`.
fn rate(bytes: usize, mut f: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..RUNS {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed().as_secs_f64());
    }
    bytes as f64 / best / 1e6
}

fn line(what: &str, ours: f64, theirs: f64) {
    println!("{what:<34} {ours:>9.1} MB/s  {theirs:>9.1} MB/s  {:>5.2}x", ours / theirs);
}

fn main() {
    let mb: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(8);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut text = std::vec::Vec::new();
    gather(&root.join("crates"), mb << 20, &mut text);
    println!("input: {} bytes of the workspace's Rust source; best of {RUNS}", text.len());
    println!("{:<34} {:>14}  {:>14}  ratio", "", "blaze-deflate", "miniz_oxide");

    let mut fast_size = 0;
    for (level, theirs_level) in [(Level::Fast, 1u8), (Level::Default, 6u8)] {
        let mut ours = Vec::new_in(heap());
        let o = rate(text.len(), || {
            ours.clear();
            blaze_deflate::deflate(&text, level, heap(), &mut ours).expect("deflate");
            black_box(&ours);
        });
        let mut theirs = std::vec::Vec::new();
        let t = rate(text.len(), || theirs = black_box(miniz_oxide::deflate::compress_to_vec(&text, theirs_level)));
        line(&format!("deflate {level:?} (theirs level {theirs_level})"), o, t);
        println!("{:<34} {:>9} B       {:>9} B", "  compressed size", ours.len(), theirs.len());
        if level == Level::Fast {
            fast_size = ours.len();
        }
    }
    // The curve: miniz at each level, and the first whose output is no larger than Fast's.
    println!("miniz_oxide by level (size, MB/s); Fast is {fast_size} B:");
    let mut equal = None;
    for theirs_level in 1..=9u8 {
        let mut theirs = std::vec::Vec::new();
        let t = rate(text.len(), || theirs = black_box(miniz_oxide::deflate::compress_to_vec(&text, theirs_level)));
        println!("  level {theirs_level}: {:>9} B {t:>9.1} MB/s", theirs.len());
        if equal.is_none() && theirs.len() <= fast_size {
            equal = Some((theirs_level, t));
        }
    }
    match equal {
        Some((theirs_level, t)) => println!("  at equal size: level {theirs_level}, {t:.1} MB/s"),
        None => println!("  no level reaches Fast's size"),
    }

    let stream = miniz_oxide::deflate::compress_to_vec(&text, 6);
    let limits = Limits { most_out: text.len() as u64, most_ratio: MAX_RATIO, most_header: 0 };
    let theirs = rate(text.len(), || {
        black_box(miniz_oxide::inflate::decompress_to_vec(&stream).expect("inflate"));
    });
    let mut out = Vec::new_in(heap());
    let ours = rate(text.len(), || {
        out.clear();
        let mut i = Inflate::new_in(limits, heap());
        i.feed(&stream, &mut out).expect("inflate");
        i.finish().expect("ends");
        black_box(&out);
    });
    line("inflate into a vector", ours, theirs);
    let mut flat = std::vec![0u8; text.len()];
    let into = rate(text.len(), || {
        let mut i = InflateInto::new(limits);
        i.feed(&stream, &mut flat).expect("inflate");
        i.finish().expect("ends");
        black_box(&flat);
    });
    line("inflate into a slice", into, theirs);
    assert!(out[..] == text[..] && flat == text, "an inflater read the stream wrongly");
    let careful = rate(text.len(), || {
        let mut i = InflateInto::careful(limits);
        i.feed(&stream, &mut flat).expect("inflate");
        i.finish().expect("ends");
        black_box(&flat);
    });
    line("  its careful twin", careful, theirs);
    let ours_stream = {
        let mut s = Vec::new_in(heap());
        blaze_deflate::deflate(&text, Level::Default, heap(), &mut s).expect("deflate");
        s
    };
    let theirs_own = rate(text.len(), || {
        black_box(miniz_oxide::inflate::decompress_to_vec(&ours_stream).expect("inflate"));
    });
    let into_own = rate(text.len(), || {
        let mut i = InflateInto::new(limits);
        i.feed(&ours_stream, &mut flat).expect("inflate");
        i.finish().expect("ends");
        black_box(&flat);
    });
    line("inflate into a slice (our stream)", into_own, theirs_own);

    println!("{:<34} {:>14}  {:>14}", "", "core::simd", "scalar twin");
    let v = rate(text.len(), || {
        let mut s = (1, 0);
        for run in text.chunks(5_552) {
            s = adler_run(s.0, s.1, run);
            s = (s.0 % 65_521, s.1 % 65_521);
        }
        black_box(s);
    });
    let s = rate(text.len(), || {
        let mut s = (1, 0);
        for run in text.chunks(5_552) {
            s = adler_run_scalar(s.0, s.1, run);
            s = (s.0 % 65_521, s.1 % 65_521);
        }
        black_box(s);
    });
    line("adler-32", v, s);
    // Match lengths as the deflater asks them: the text against itself 1 MiB later, so each
    // compare runs to the cap of 258.
    let mut twice = text.clone();
    twice.extend_from_slice(&text);
    let starts: std::vec::Vec<usize> = (0..text.len().saturating_sub(258)).step_by(97).collect();
    let bytes = starts.len() * 258;
    let v = rate(bytes, || {
        let n: usize = starts.iter().map(|&p| match_len(&twice, p, p + text.len(), 2, 258)).sum();
        black_box(n);
    });
    let s = rate(bytes, || {
        let n: usize = starts.iter().map(|&p| match_len_scalar(&twice, p, p + text.len(), 2, 258)).sum();
        black_box(n);
    });
    line("match length (258-byte matches)", v, s);
}
