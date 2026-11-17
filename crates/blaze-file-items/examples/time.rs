//! Times `outline` over every `.rs` file under the directories given: files, bytes, items,
//! syntax errors, unhandled forms, and the wall time of the outlines alone (reads excluded).
//!
//! A measurement tool for the design's floor (docs/v2/plan/design/bottom.md), not a test.

#![feature(allocator_api)]

use std::{alloc::Global, path::PathBuf, time::Instant};

use blaze_file_items::{Bodies, OneByOne, SideBySide, outline, outline_hashed_by};
use blaze_surface::{Edition, ItemDetail};

/// How many outlines of every file `--profile` runs.
const PROFILE_ROUNDS: usize = 12;

/// The entries `outline` makes of `text`.
fn rows(text: &str, heap: &'static blaze_own::Heap) -> usize {
    outline(text, Edition::E2024, heap.alloc()).items().len()
}

/// The CPU time this thread has used, in seconds: what a round costs whatever else the machine
/// runs beside it, where wall time on a loaded machine also counts the waits for a core.
fn cpu() -> f64 {
    #[repr(C)]
    struct Timespec {
        seconds: i64,
        nanoseconds: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: i32, time: *mut Timespec) -> i32;
    }
    // `CLOCK_THREAD_CPUTIME_ID` in each system's <time.h>.
    let clock = if cfg!(target_os = "macos") { 16 } else { 3 };
    let mut time = Timespec { seconds: 0, nanoseconds: 0 };
    // SAFETY: `clock_gettime` (the libc std links, on a 64-bit macOS or Linux, where `time_t`
    // and `long` are both 64 bits) writes one `timespec` to `time`, which is one.
    let got = unsafe { clock_gettime(clock, &mut time) };
    assert_eq!(got, 0, "the thread's CPU clock reads");
    time.seconds as f64 + time.nanoseconds as f64 * 1e-9
}

/// The machine's one-minute load average, as text.
fn load() -> std::string::String {
    unsafe extern "C" {
        fn getloadavg(averages: *mut f64, count: i32) -> i32;
    }
    let mut averages = [0f64; 3];
    // SAFETY: `getloadavg` (macOS and Linux libc, which std links) writes at most `count`
    // doubles to `averages`, which holds three.
    let got = unsafe { getloadavg(averages.as_mut_ptr(), 3) };
    match got {
        1.. => format!("{:.1}", averages[0]),
        _ => std::string::String::from("unknown"),
    }
}

fn main() {
    let heap = blaze_own::Heap::process();
    let mut files: Vec<PathBuf, Global> = Vec::new_in(Global);
    let mut stack: Vec<PathBuf, Global> = Vec::new_in(Global);
    let mut profile = false;
    for arg in std::env::args_os().skip(1) {
        match arg.to_str() {
            Some("--profile" | "--profile=rows") => profile = true,
            _ => stack.push(PathBuf::from(arg)),
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("not a directory that reads: {}", dir.display());
            std::process::exit(2);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut texts: Vec<std::string::String, Global> = Vec::with_capacity_in(files.len(), Global);
    for file in &files {
        match std::fs::read_to_string(file) {
            Ok(text) => texts.push(text),
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                std::process::exit(2);
            }
        }
    }
    let bytes: usize = texts.iter().map(|it| it.len()).sum();
    let lines: usize = texts.iter().map(|it| it.lines().count()).sum();
    // `--profile` first: `outline` alone, round after round, and nothing else in the process:
    // for a sampling profiler to attach to (the process prints its id), and for the
    // instructions retired of `PROFILE_ROUNDS` outlines of everything (`/usr/bin/time -l`,
    // which mixcheck.sh wraps a run in), the number a speed change is judged by on a loaded
    // machine.
    if profile {
        println!("profile outline: pid {}", std::process::id());
        let (start, start_cpu) = (Instant::now(), cpu());
        let mut items = 0usize;
        for _ in 0..PROFILE_ROUNDS {
            for text in &texts {
                items += rows(text, heap);
            }
        }
        println!(
            "profile outline: {PROFILE_ROUNDS} rounds, {items} items, {:.1} ms a round, {:.1} ms of CPU a round, load {}",
            start.elapsed().as_secs_f64() * 1e3 / PROFILE_ROUNDS as f64,
            (cpu() - start_cpu) * 1e3 / PROFILE_ROUNDS as f64,
            load()
        );
        return;
    }
    // `outline` over everything, best of five, with the machine's load average when the best
    // round ended.
    let mut best = (f64::MAX, std::string::String::new());
    let (mut best_cpu, mut count) = (f64::MAX, 0usize);
    for _ in 0..5 {
        let (start, start_cpu) = (Instant::now(), cpu());
        count = texts.iter().map(|text| rows(text, heap)).sum();
        let (took, took_cpu) = (start.elapsed().as_secs_f64(), cpu() - start_cpu);
        best_cpu = best_cpu.min(took_cpu);
        if took < best.0 {
            best = (took, load());
        }
    }
    println!(
        "outline: {} files, {:.1} MB, {count} items, {:.1} ms best of 5 on one thread ({:.0} MB/s), {:.1} ms of CPU, load {}",
        files.len(),
        bytes as f64 / 1e6,
        best.0 * 1e3,
        bytes as f64 / 1e6 / best.0,
        best_cpu * 1e3,
        best.1,
    );
    for round in 0..3 {
        let (mut items, mut errors, mut unhandled) = (0usize, 0usize, 0usize);
        let start = Instant::now();
        for text in &texts {
            let made = outline(text, Edition::E2024, heap.alloc());
            items += made.items().len();
            errors += made.errors().len();
            unhandled += made.items().iter().filter(|it| matches!(it.detail, ItemDetail::Unhandled { .. })).count();
        }
        let took = start.elapsed();
        println!(
            "round {round}: {} files, {} lines, {:.1} MB, {items} items, {errors} syntax errors, {unhandled} unhandled, {:.1} ms on one thread ({:.0} MB/s, {:.2} us a line)",
            files.len(),
            lines,
            bytes as f64 / 1e6,
            took.as_secs_f64() * 1e3,
            bytes as f64 / 1e6 / took.as_secs_f64(),
            took.as_secs_f64() * 1e6 / lines as f64,
        );
    }
    // The whole-file walk with its digests hashed side by side and one by one (crate::digest's
    // seam), rounds interleaved so the machine's load falls on both alike.
    let mut best = [f64::MAX; 2];
    for _ in 0..3 {
        for (slot, side_by_side) in best.iter_mut().zip([false, true]) {
            let start = Instant::now();
            let mut items = 0usize;
            for text in &texts {
                let made = match side_by_side {
                    false => outline_hashed_by::<OneByOne>(text, Edition::E2024, Bodies::Skipped, heap.alloc()),
                    true => outline_hashed_by::<SideBySide>(text, Edition::E2024, Bodies::Skipped, heap.alloc()),
                };
                items += made.items().len();
            }
            *slot = slot.min(start.elapsed().as_secs_f64());
            std::hint::black_box(items);
        }
    }
    println!(
        "whole-file walk, digests one by one: {:.1} ms; side by side: {:.1} ms (best of 3 each, one thread)",
        best[0] * 1e3,
        best[1] * 1e3
    );
    // BLAKE3 alone on the outline's shape of input: every entry's bytes of at most 1 KiB, one
    // at a time through a streaming hasher, one at a time in one shot, and 64 side by side.
    let mut inputs: Vec<&[u8], Global> = Vec::new_in(Global);
    for text in &texts {
        let made = outline(text, Edition::E2024, heap.alloc());
        for item in made.items() {
            if item.range.len() <= 1024 {
                inputs.push(&text.as_bytes()[item.range.to_usize()]);
            }
        }
    }
    let input_bytes: usize = inputs.iter().map(|input| input.len()).sum();
    let key = blaze_blake3::context_key("blaze-file-items time: inputs");
    let mut best = [f64::MAX; 3];
    let mut out = [[0u8; 32]; 64];
    for _ in 0..3 {
        for (way, slot) in best.iter_mut().enumerate() {
            let start = Instant::now();
            let mut sum = 0u8;
            match way {
                0 => {
                    for input in &inputs {
                        let mut hasher = blaze_blake3::Hasher::new_from_context_key(&key);
                        hasher.update(input);
                        sum ^= hasher.finalize()[0];
                    }
                }
                1 => {
                    for input in &inputs {
                        sum ^= blaze_blake3::derive_key_with(&key, input)[0];
                    }
                }
                _ => {
                    for batch in inputs.chunks(64) {
                        blaze_blake3::derive_key_many(&key, batch, &mut out[..batch.len()]);
                        sum ^= out[0][0];
                    }
                }
            }
            *slot = slot.min(start.elapsed().as_secs_f64());
            std::hint::black_box(sum);
        }
    }
    for (way, took) in ["streaming hasher", "one shot each", "64 side by side"].iter().zip(best) {
        println!(
            "blake3 over {} entries of at most 1 KiB ({:.1} MB): {way} {:.1} ms ({:.0} ns an input, {:.0} MB/s)",
            inputs.len(),
            input_bytes as f64 / 1e6,
            took * 1e3,
            took * 1e9 / inputs.len() as f64,
            input_bytes as f64 / 1e6 / took
        );
    }
    // The entry flags (crate docs, "Flags"), counted outside the timing: how many entries set
    // each, how many set any, and how many files set none.
    let (mut entries, mut flagged, mut clean_files, mut own) = (0usize, 0usize, 0usize, 0usize);
    let mut by_flag = [0usize; 4];
    let mut fns = (0usize, 0usize);
    for text in &texts {
        let made = outline(text, Edition::E2024, heap.alloc());
        clean_files += usize::from(made.flags().is_empty());
        own += usize::from(!made.own_flags().is_empty());
        for item in made.items() {
            entries += 1;
            flagged += usize::from(!item.flags.is_empty());
            for (count, (flag, _)) in by_flag.iter_mut().zip(blaze_surface::ItemFlags::NAMED) {
                *count += usize::from(item.flags.contains(flag));
            }
            if matches!(item.detail, ItemDetail::Fn { .. }) {
                fns.0 += 1;
                fns.1 += usize::from(item.flags.contains(blaze_surface::ItemFlags::UNSAFE));
            }
        }
    }
    let percent = |part: usize, of: usize| 100.0 * part as f64 / of.max(1) as f64;
    for (count, (_, name)) in by_flag.iter().zip(blaze_surface::ItemFlags::NAMED) {
        println!("flag {name}: {count} of {entries} entries ({:.1}%)", percent(*count, entries));
    }
    println!(
        "flags: {flagged} entries set any ({:.1}%); {} of {} fns are unsafe by their own tokens ({:.1}%); {clean_files} of {} files set none; {own} files' own tokens set one",
        percent(flagged, entries),
        fns.1,
        fns.0,
        percent(fns.1, fns.0),
        texts.len(),
    );
    // Half the floor (crate docs, "Why bytes, not tokens"): one BLAKE3 pass over every byte,
    // on one thread. The other half is a skip of every byte, measured by blaze-lex's `cost`.
    let mut best = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let mut sum = 0u8;
        for text in &texts {
            let mut hasher = blaze_ids::Hasher::new("blaze-file-items time: the floor");
            hasher.update(text.as_bytes());
            sum ^= hasher.finish().0[0];
        }
        best = best.min(start.elapsed().as_secs_f64());
        std::hint::black_box(sum);
    }
    println!(
        "blake3 over every byte: {:.1} ms best of 3 on one thread ({:.0} MB/s)",
        best * 1e3,
        bytes as f64 / 1e6 / best
    );
    // The same work on N threads, each taking the next file from one shared counter (so big
    // files do not unbalance a thread): what "divides by N" comes to on this machine. Every
    // outline allocates from the global heap here, so its contention is in the number.
    let most = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut one = 0f64;
    for threads in [1usize, 2, 4, 6, 8, 12, 16, 18, 24, 32, 64, 128] {
        if threads > most {
            break;
        }
        let mut best = f64::MAX;
        let mut items = 0usize;
        for _ in 0..3 {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let made = std::sync::atomic::AtomicUsize::new(0);
            let start = Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        let mut mine = 0usize;
                        loop {
                            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(text) = texts.get(at) else { break };
                            mine += outline(text, Edition::E2024, heap.alloc()).items().len();
                        }
                        made.fetch_add(mine, std::sync::atomic::Ordering::Relaxed);
                    });
                }
            });
            best = best.min(start.elapsed().as_secs_f64());
            items = made.load(std::sync::atomic::Ordering::Relaxed);
        }
        if threads == 1 {
            one = best;
        }
        println!(
            "threads {threads}: {:.1} ms best of 3, {items} items, {:.2}x of one thread, {:.0}% of ideal",
            best * 1e3,
            one / best,
            100.0 * one / best / threads as f64,
        );
    }
}
