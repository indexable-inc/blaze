//! What each piece of blaze-lex costs over `library/` (or the directories given): MB/s and ns a
//! line for rustc_lexer, the lexer, the body skipper and UTF-8 validation (against
//! `core::str::from_utf8`), on every path this machine runs, on one thread and on all of them.
//! The files are read once and timed in memory, best of 5.
//!
//! The floor aimed at (rule 67), on one core at about 3.5 GHz:
//! - UTF-8: memory bandwidth on ASCII, about 0.1 cycle a byte (one load and one OR per vector,
//!   one test per 64 bytes): over 20 GB/s.
//! - The skipper: classification once per byte (two table lookups, an `&` and one mask per
//!   vector, about 0.3 cycle a byte) and one scalar step per structural byte (about one byte in
//!   twelve of Rust is `{ } " ' / #`): about 1 cycle a byte, 3 GB/s.
//! - The lexer: a token every 4 to 5 bytes, each a dispatch on its first byte, a run by
//!   `trailing_ones` and its kind and length: about 12 cycles a token, 3 a byte, 1 GB/s.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use blaze_lex::{Cpu, FrontmatterAllowed, skip_body, tokenize, validate};

fn widest() -> Cpu {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU said it has AVX2 just above.
            return unsafe { Cpu::avx2_unchecked() };
        }
    }
    Cpu::BASELINE
}

fn float(n: usize) -> f64 {
    f64::from(u32::try_from(n).expect("a count of bytes or lines under 4 GiB"))
}

/// The `{` of every fn body: the first `{` after each `fn`, before any `;`.
fn fn_bodies(text: &str) -> Vec<usize> {
    let mut bodies = Vec::new();
    let mut at = 0usize;
    let mut after_fn = false;
    for token in rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No) {
        let len = usize::try_from(token.len).expect("a token's length fits usize");
        match token.kind {
            rustc_lexer::TokenKind::Ident if &text[at..at + len] == "fn" => after_fn = true,
            rustc_lexer::TokenKind::OpenBrace if after_fn => {
                bodies.push(at);
                after_fn = false;
            }
            rustc_lexer::TokenKind::Semi => after_fn = false,
            _ => {}
        }
        at += len;
    }
    bodies
}

struct File {
    text: String,
    lines: usize,
    bodies: Vec<usize>,
}

/// Runs `work` over every file, best of 5, on `threads` threads taking files from one counter.
/// `work` returns the bytes it covered, which the rate is of.
fn best(files: &[File], threads: usize, work: &(dyn Fn(&File) -> (usize, usize) + Sync)) -> (f64, usize, usize) {
    let mut fastest = f64::MAX;
    let (mut bytes, mut lines) = (0, 0);
    for _ in 0..5 {
        let next = AtomicUsize::new(0);
        let covered = AtomicUsize::new(0);
        let covered_lines = AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    let (mut mine, mut mine_lines) = (0, 0);
                    while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let (b, l) = work(file);
                        mine += b;
                        mine_lines += l;
                    }
                    covered.fetch_add(mine, Ordering::Relaxed);
                    covered_lines.fetch_add(mine_lines, Ordering::Relaxed);
                });
            }
        });
        fastest = fastest.min(start.elapsed().as_secs_f64());
        bytes = covered.load(Ordering::Relaxed);
        lines = covered_lines.load(Ordering::Relaxed);
    }
    (fastest, bytes, lines)
}

fn main() {
    let mut stack: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if stack.is_empty() {
        stack.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../library"));
    }
    let mut paths = Vec::new();
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
                paths.push(path);
            }
        }
    }
    paths.sort();
    let files: Vec<File> = paths
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| File { lines: text.lines().count(), bodies: fn_bodies(&text), text })
        .collect();
    let bytes: usize = files.iter().map(|it| it.text.len()).sum();
    let lines: usize = files.iter().map(|it| it.lines).sum();
    println!("{} files, {} lines, {:.1} MB", files.len(), lines, float(bytes) / 1e6);

    let cpus: Vec<Cpu> = widest().and_narrower().collect();
    let most = std::thread::available_parallelism().map_or(1, usize::from);
    let whole = |f: &File| (f.text.len(), f.lines);
    let mut rows: Vec<(String, Box<dyn Fn(&File) -> (usize, usize) + Sync>)> = Vec::new();
    rows.push((
        "rustc_lexer::tokenize".into(),
        Box::new(move |f: &File| {
            std::hint::black_box(rustc_lexer::tokenize(&f.text, rustc_lexer::FrontmatterAllowed::Yes).count());
            whole(f)
        }),
    ));
    for &cpu in &cpus {
        rows.push((
            format!("blaze_lex::tokenize, {}", cpu.name()),
            Box::new(move |f: &File| {
                std::hint::black_box(tokenize(&f.text, FrontmatterAllowed::Yes, cpu).count());
                whole(f)
            }),
        ));
    }
    rows.push((
        "tokenize and count braces, every fn body (rustc_lexer)".into(),
        Box::new(move |f: &File| {
            let mut covered = 0;
            for &body in &f.bodies {
                let mut depth = 0usize;
                let mut at = 0usize;
                for token in rustc_lexer::tokenize(&f.text[body..], rustc_lexer::FrontmatterAllowed::No) {
                    at += usize::try_from(token.len).expect("fits");
                    match token.kind {
                        rustc_lexer::TokenKind::OpenBrace => depth += 1,
                        rustc_lexer::TokenKind::CloseBrace => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                covered += at;
            }
            (covered, 0)
        }),
    ));
    for &cpu in &cpus {
        rows.push((
            format!("blaze_lex::skip_body, every fn body, {}", cpu.name()),
            Box::new(move |f: &File| {
                let mut covered = 0;
                for &body in &f.bodies {
                    covered += skip_body(cpu, &f.text[body..]).map_or(0, |end| end + 1);
                }
                (covered, 0)
            }),
        ));
    }
    rows.push((
        "core::str::from_utf8".into(),
        Box::new(move |f: &File| {
            std::hint::black_box(core::str::from_utf8(std::hint::black_box(f.text.as_bytes())).is_ok());
            whole(f)
        }),
    ));
    for &cpu in &cpus {
        rows.push((
            format!("blaze_lex::validate, {}", cpu.name()),
            Box::new(move |f: &File| {
                std::hint::black_box(validate(cpu, std::hint::black_box(f.text.as_bytes())).is_ok());
                whole(f)
            }),
        ));
    }

    for (name, work) in &rows {
        for threads in [1, most] {
            let (seconds, covered, covered_lines) = best(&files, threads, work.as_ref());
            let per_line = if covered_lines == 0 {
                String::from("-")
            } else {
                format!("{:.1}", seconds * 1e9 / float(covered_lines))
            };
            println!(
                "{name:<58} {threads:>3} threads: {:>9.1} MB/s, {per_line:>6} ns a line ({:.1} MB in {:.2} ms)",
                float(covered) / 1e6 / seconds,
                float(covered) / 1e6,
                seconds * 1e3
            );
            if most == 1 {
                break;
            }
        }
    }
}
