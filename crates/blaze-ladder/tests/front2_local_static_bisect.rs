//! BISECT (hello-e, Class Q): which difference between bstd `main!`'s body-local
//! `static MANIFEST` and the same initializer at module level makes the checker ask a key that
//! front2's key map has no row for. front2_hello_manifest (A1's run, 2026-11-16 19:4x): the
//! module-level OUT_KIND .. MANIFEST all evaluate; the expansion's MANIFEST (declared inside the
//! `fn main` the expansion makes) is refused, "checking <MANIFEST>: 1 gaps: the checker key
//! 0x6986ddff99938e3c94281d66266a6778 .. is in no row of the front end's key map".
//!
//! Each case is one program: a plain `fn main()` (D104 allows it) declaring ONE local static
//! that differs from a control by one thing, reached from `main` by `let _kept = &<static>;`,
//! built by the real command's front (`build_stubbed`, as `hello_d104_gets_past_the_manifest`).
//! The Reference, "Statements", "Item declarations": an item in a block is an item like a
//! module's, so every case's static is expected to check and evaluate as at module level.
//!
//! It ASSERTS NOTHING: it prints one line per case, "case <x>: ok" or "case <x>: <the refusal
//! whole, one line>", then for every 32-hex-digit key and every decimal `ItemKey(..)` in a
//! refusal, "  key 0x..: crate <name>" (the crate whose key prefix, its first 8 bytes, the key
//! begins with; "no crate of this program" when none). One run names the asker. Needs
//! `$HOME/.blaze/v2/toolchain`. NEVER RUN by its writer.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_stubbed};

/// (case, the statements before `let _kept = &<name>;`, the static's name, inside `main`?)
const CASES: [(&str, &str, &str, bool); 10] = [
    ("control-local: plain path, unit variant", "static L: std::grant::Kind = std::grant::Kind::Out;", "L", true),
    ("a: absolute `::std::` path (the expansion's `$crate::` resolves to bstd so)", "static L: ::std::grant::Manifest = ::std::grant::Manifest::new(&[]);", "L", true),
    ("b: `stringify!` in the initializer", "static L: &str = stringify!(out);", "L", true),
    ("c: a doc attribute", "/// The program's manifest: what it takes, as data in the binary.\n    static L: std::grant::Kind = std::grant::Kind::Out;", "L", true),
    ("d: qualified `<Out as Grant>::KIND`", "static L: std::grant::Kind = <std::out::Out as std::grant::Grant>::KIND;", "L", true),
    ("d2: `Param::new` of the qualified constant", "static L: std::grant::Param = std::grant::Param::new(\"out\", <std::out::Out as std::grant::Grant>::KIND);", "L", true),
    ("d3: a `&[Param]` of one, by `Manifest::new`", "static L: std::grant::Manifest = std::grant::Manifest::new(&[std::grant::Param::new(\"out\", std::grant::Kind::Out)]);", "L", true),
    (
        "e: the expansion's one-line printed text",
        "# [doc = \" The program's manifest: what it takes, as data in the binary.\"] static MANIFEST : :: std :: grant :: Manifest = :: std :: grant :: Manifest :: new (& [:: std :: grant :: Param :: new (stringify ! (out), < std :: out :: Out as :: std :: grant :: Grant > :: KIND)]) ;",
        "MANIFEST",
        true,
    ),
    (
        "f: every difference at once, as written",
        "/// The program's manifest: what it takes, as data in the binary.\n    static MANIFEST: ::std::grant::Manifest = ::std::grant::Manifest::new(&[\n        ::std::grant::Param::new(stringify!(out), <std::out::Out as ::std::grant::Grant>::KIND)\n    ]);",
        "MANIFEST",
        true,
    ),
    (
        "control-module: f at module level",
        "/// The program's manifest: what it takes, as data in the binary.\nstatic MANIFEST: ::std::grant::Manifest = ::std::grant::Manifest::new(&[\n    ::std::grant::Param::new(stringify!(out), <std::out::Out as ::std::grant::Grant>::KIND)\n]);",
        "MANIFEST",
        false,
    ),
];

fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// Every key a refusal's text shows: `0x` and 32 hex digits, or `ItemKey(<decimal>)`.
fn keys_in(text: &str) -> Vec<u128> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices("0x") {
        let hex: String = text[at + 2..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() == 32 {
            if let Ok(v) = u128::from_str_radix(&hex, 16) {
                out.push(v);
            }
        }
    }
    for (at, _) in text.match_indices("ItemKey(") {
        let dec: String = text[at + 8..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(v) = dec.parse::<u128>() {
            out.push(v);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[test]
fn local_static_bisect() {
    use core::fmt::Write as _;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::{env, sync::Mutex, thread};
    let home = env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let next = AtomicUsize::new(0);
    let results = Mutex::new([const { None }; CASES.len()]);
    let workers = thread::available_parallelism().map_or(2, |n| n.get().min(6));
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(case, decl, name, local)) = CASES.get(idx) else { break };
                let mut lines = format!("");
                let text = match local {
                    true => format!("fn main() {{\n    {decl}\n    let _kept = &{name};\n}}\n"),
                    false => format!("{decl}\n\nfn main() {{\n    let _kept = &{name};\n}}\n"),
                };
                let files = [SourceFile { path: "src/main.rs", text: &text }];
                let new = NewFront { package: "bisect", edition: "blaze-2026.11", shelf: Some(&shelf), overflow: blaze_build::OverflowChecks::On };
                let texts = match ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind) {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = writeln!(lines, "case {case}: the program's texts: {e}");
                        results.lock().unwrap()[idx] = Some(lines);
                        continue;
                    }
                };
                let user = texts.user();
                let inputs = match Inputs::new(&user, &tc) {
                    Ok(i) => i,
                    Err(e) => {
                        let _ = writeln!(lines, "case {case}: the crates: {e}");
                        results.lock().unwrap()[idx] = Some(lines);
                        continue;
                    }
                };
                let crates = inputs.crates();
                let names_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("names frame");
                let names_erased = blaze_own::Erased::new(&names_frame);
                let names = match inputs.resolve(&crates, names_erased.bump()) {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = writeln!(lines, "case {case}: resolve_crates: {e}");
                        results.lock().unwrap()[idx] = Some(lines);
                        continue;
                    }
                };
                let said = match build_stubbed(&names, &files, &texts, blaze_own::Alloc::global()) {
                    Ok(_) => {
                        let _ = writeln!(lines, "case {case}: ok");
                        results.lock().unwrap()[idx] = Some(lines);
                        continue;
                    }
                    Err(e) => format!("{e}").replace('\n', " "),
                };
                let _ = writeln!(lines, "case {case}: {said}");
                for k in keys_in(&said) {
                    let prefix = k.to_be_bytes();
                    let owner = names.crates().iter().find(|c| c.key.prefix()[..] == prefix[..8]).map_or("no crate of this program", |c| c.name);
                    let _ = writeln!(lines, "  key {k:#034x}: crate {owner}");
                }
                results.lock().unwrap()[idx] = Some(lines);
            });
        }
    });
    for slot in results.into_inner().unwrap() {
        if let Some(block) = slot {
            for line in block.lines() {
                note(line);
            }
        }
    }
}
