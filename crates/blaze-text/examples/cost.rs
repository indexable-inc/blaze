//! Nanoseconds per operation: blaze-text's writers and parsers against `core::fmt` and
//! `str::parse`, on the same values into the same reused buffer. Run with
//! `cargo run --release -p blaze-text --example cost`.
//!
//! Each figure has the loop's own cost taken off: the same loop with the same buffer clear and
//! the same value load, writing nothing, is timed first and subtracted. Values come from a
//! table indexed by `i & 1023`, so no generator or division runs in the timed loop.

use std::{fmt::Write as _, hint::black_box, time::Instant};

use blaze_own::String;
use blaze_text::{Parse, Writes, twins};

const N: u32 = 2_000_000;
const MASK: u32 = 1023;

fn raw(mut body: impl FnMut(usize)) -> f64 {
    for i in 0..N / 10 {
        body(blaze_int::idx(i & MASK));
    }
    let start = Instant::now();
    for i in 0..N {
        body(blaze_int::idx(i & MASK));
    }
    start.elapsed().as_secs_f64() * 1e9 / f64::from(N)
}

struct Bench {
    /// The empty loop's ns per iteration.
    floor: f64,
}

impl Bench {
    fn pair(&self, name: &str, ours: impl FnMut(usize), theirs: impl FnMut(usize)) {
        let a = (raw(ours) - self.floor).max(0.01);
        let b = (raw(theirs) - self.floor).max(0.01);
        println!("{name:<44} ours {a:>7.1} ns  core {b:>7.1} ns  {:>6.2}x", b / a);
    }
}

/// 1024 values with `digits` decimal digits each (0 for a spread of every length).
fn ints(digits: u32) -> std::vec::Vec<u64> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..1024u32)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            match digits {
                0 => x >> (i % 64),
                20 => 10_000_000_000_000_000_000 + x % 8_446_744_073_709_551_615,
                d => {
                    let low = 10u64.pow(d - 1);
                    low + x % (low * 9)
                }
            }
        })
        .collect()
}

fn main() {
    // The program's entry: the process's heap, handed down.
    let alloc = blaze_own::Heap::process().alloc();
    let mut ours = String::with_capacity_in(128, alloc);
    let mut theirs = std::string::String::with_capacity(128);
    let spread = ints(0);
    let floor = raw(|i| {
        ours.clear();
        black_box(spread[i]);
        black_box(ours.as_str());
    });
    println!("loop floor {floor:.2} ns/iteration (subtracted below)\n");
    let bench = Bench { floor };

    for digits in [1u32, 5, 10, 20, 0] {
        let values = ints(digits);
        let name = match digits {
            0 => std::string::String::from("u64 decimal, every length"),
            d => format!("u64 decimal, {d} digits"),
        };
        bench.pair(
            &name,
            |i| {
                ours.clear();
                ours.int(black_box(values[i]));
                black_box(ours.as_str());
            },
            |i| {
                theirs.clear();
                let _ = write!(theirs, "{}", black_box(values[i]));
                black_box(theirs.as_str());
            },
        );
    }
    bench.pair(
        "u64 hex",
        |i| {
            ours.clear();
            ours.hex(black_box(spread[i]));
            black_box(ours.as_str());
        },
        |i| {
            theirs.clear();
            let _ = write!(theirs, "{:x}", black_box(spread[i]));
            black_box(theirs.as_str());
        },
    );

    let floats: std::vec::Vec<f64> = spread.iter().map(|&b| f64::from_bits(b >> 2 | 0x3000_0000_0000_0000)).collect();
    // Everyday magnitudes, 1e-3 to 1e6, where `{}` and `{:?}` mostly agree in shape.
    let everyday: std::vec::Vec<f64> = spread.iter().map(|&b| f64::from_bits(b >> 12 | (1013 + b % 30) << 52)).collect();
    bench.pair(
        "f64 {} (Display), 1e-3..1e6",
        |i| {
            ours.clear();
            ours.float(black_box(everyday[i]));
            black_box(ours.as_str());
        },
        |i| {
            theirs.clear();
            let _ = write!(theirs, "{}", black_box(everyday[i]));
            black_box(theirs.as_str());
        },
    );
    bench.pair(
        "f64 {} (Display), 2^-255..",
        |i| {
            ours.clear();
            ours.float(black_box(floats[i]));
            black_box(ours.as_str());
        },
        |i| {
            theirs.clear();
            let _ = write!(theirs, "{}", black_box(floats[i]));
            black_box(theirs.as_str());
        },
    );
    bench.pair(
        "f64 {:?} (float_debug)",
        |i| {
            ours.clear();
            ours.float_debug(black_box(floats[i]));
            black_box(ours.as_str());
        },
        |i| {
            theirs.clear();
            let _ = write!(theirs, "{:?}", black_box(floats[i]));
            black_box(theirs.as_str());
        },
    );
    bench.pair(
        "f64 fixed, 3 places",
        |i| {
            ours.clear();
            ours.float_fixed(black_box(floats[i]), 3);
            black_box(ours.as_str());
        },
        |i| {
            theirs.clear();
            let _ = write!(theirs, "{:.3}", black_box(floats[i]));
            black_box(theirs.as_str());
        },
    );

    // What call sites do: a line of mixed pieces.
    let names = ["blaze-drive", "reth-primitives", "a", "tempo-node-core-types"];
    bench.pair(
        "\"{}: {} of {} ({:.1}%)\"",
        |i| {
            let (name, done, all) = (names[i & 3], spread[i] >> 40, (spread[i] >> 40) + 7);
            let pct = 100.0 * f64::from(u32::try_from(done).unwrap_or(0)) / f64::from(u32::try_from(all).unwrap_or(1));
            ours.clear();
            ours.str(black_box(name)).str(": ").int(done).str(" of ").int(all).str(" (").float_fixed(pct, 1).str("%)");
            black_box(ours.as_str());
        },
        |i| {
            let (name, done, all) = (names[i & 3], spread[i] >> 40, (spread[i] >> 40) + 7);
            let pct = 100.0 * f64::from(u32::try_from(done).unwrap_or(0)) / f64::from(u32::try_from(all).unwrap_or(1));
            theirs.clear();
            let _ = write!(theirs, "{}: {} of {} ({:.1}%)", black_box(name), done, all, pct);
            black_box(theirs.as_str());
        },
    );

    // A whole site as one expression: a new string each time, against `format!`.
    bench.pair(
        "text_in: \"{}: {} of {}\" into a new string",
        |i| {
            let (name, done) = (names[i & 3], spread[i] >> 40);
            let made = blaze_text::text_in(alloc, |out| out.str(black_box(name)).str(": ").int(done).str(" of ").int(done + 7));
            black_box(made.as_str());
        },
        |i| {
            let (name, done) = (names[i & 3], spread[i] >> 40);
            let made = format!("{}: {} of {}", black_box(name), done, done + 7);
            black_box(made.as_str());
        },
    );
    bench.pair(
        "text_exact_in: the same, one exact allocation",
        |i| {
            let (name, done) = (names[i & 3], spread[i] >> 40);
            let made = blaze_text::text_exact_in(alloc, |out| out.str(black_box(name)).str(": ").int(done).str(" of ").int(done + 7));
            black_box(made.as_str());
        },
        |i| {
            let (name, done) = (names[i & 3], spread[i] >> 40);
            let made = format!("{}: {} of {}", black_box(name), done, done + 7);
            black_box(made.as_str());
        },
    );
    // What was `format_args!` handed on: a closure `Show` against `fmt::Arguments`.
    bench.pair(
        "show_with against format_args!",
        |i| {
            let (name, done) = (names[i & 3], spread[i] >> 40);
            let later = blaze_text::show_with(move |out| _ = out.str(black_box(name)).str(": ").int(done));
            ours.clear();
            ours.show(&later);
            black_box(ours.as_str());
        },
        |i| {
            let (name, done) = (names[i & 3], spread[i] >> 40);
            theirs.clear();
            let _ = theirs.write_fmt(format_args!("{}: {}", black_box(name), done));
            black_box(theirs.as_str());
        },
    );
    // `{:?}` of a derived-`Debug` value, against the debug form read by reflection.
    #[derive(Debug)]
    struct Progress<'a> {
        name: &'a str,
        done: u64,
        state: Option<(u32, bool)>,
    }
    bench.pair(
        "{:?} of a struct: debug against derive",
        |i| {
            let p = Progress { name: names[i & 3], done: spread[i] >> 40, state: Some((7, i & 1 == 0)) };
            ours.clear();
            ours.debug(black_box(&p));
            black_box(ours.as_str());
        },
        |i| {
            let p = Progress { name: names[i & 3], done: spread[i] >> 40, state: Some((7, i & 1 == 0)) };
            theirs.clear();
            let _ = write!(theirs, "{:?}", black_box(&p));
            black_box(theirs.as_str());
        },
    );
    println!();

    for digits in [1u32, 5, 10, 20, 0] {
        let texts: std::vec::Vec<std::string::String> = ints(digits).iter().map(|x| format!("{x}")).collect();
        let length = match digits {
            0 => std::string::String::from("every length"),
            d => format!("{d} digits"),
        };
        bench.pair(
            &format!("u64 parse_all, {length}"),
            |i| _ = black_box(blaze_text::parse_all::<u64>(black_box(texts[i].as_str())).ok()),
            |i| _ = black_box(black_box(texts[i].as_str()).parse::<u64>().ok()),
        );
        // A prefix: the number with text after it, against core on the number alone.
        let lines: std::vec::Vec<std::string::String> = ints(digits).iter().map(|x| format!("{x}, next")).collect();
        bench.pair(
            &format!("u64 parse, a prefix, {length}"),
            |i| _ = black_box(<u64 as Parse>::parse(black_box(lines[i].as_str())).ok()),
            |i| _ = black_box(black_box(texts[i].as_str()).parse::<u64>().ok()),
        );
    }
    let float_texts: std::vec::Vec<std::string::String> = floats.iter().map(|x| format!("{x:?}")).collect();
    bench.pair(
        "f64 parse_all (the text is the token)",
        |i| _ = black_box(blaze_text::parse_all::<f64>(black_box(float_texts[i].as_str())).ok()),
        |i| _ = black_box(black_box(float_texts[i].as_str()).parse::<f64>().ok()),
    );
    bench.pair(
        "f64 parse, a prefix (shortest forms)",
        |i| _ = black_box(<f64 as Parse>::parse(black_box(float_texts[i].as_str())).ok()),
        |i| _ = black_box(black_box(float_texts[i].as_str()).parse::<f64>().ok()),
    );
    println!();

    // The vector routines against their scalar twins ("core" column = the scalar twin), in a
    // longer text, as a parser meets them: where each crosses over.
    println!("vector (ours) against its scalar twin (core column):");
    for len in [4usize, 16, 64, 256] {
        let text: std::vec::Vec<u8> = (0..len).map(|i| b'a' + u8::try_from(i % 26).unwrap_or(0)).chain(*b"\"").collect();
        bench.pair(
            &format!("  next escape byte after {len} plain bytes"),
            |_| _ = black_box(twins::next_special(black_box(&text))),
            |_| _ = black_box(twins::next_special_scalar(black_box(&text))),
        );
    }
}
