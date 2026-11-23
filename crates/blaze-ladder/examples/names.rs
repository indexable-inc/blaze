//! A blaze-edition project's names, with the toolchain's crates beneath it: `names <project
//! dir> [<path>...]` resolves the project's crate with bstd, core, alloc and bstd's
//! dependencies (src/crates.rs), prints each crate's module and item counts and notes, and
//! resolves each path given (default: what his `main` names) in the crate root's scope.
//!
//! Run with `mixcheck.sh blaze-ladder run names /Volumes/Projects/tmp/hello-world`.

use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

fn main() {
    // The program's one allocator, made at its top and handed down: a frame, whose memory comes
    // from the OS (no global heap).
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).unwrap_or_else(|e| panic!("names: the frame: {e}"));
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: names <project dir> [<path>...]");
        std::process::exit(2);
    };
    let mut paths: Vec<String> = args.collect();
    if paths.is_empty() {
        paths = ["Cx", "Io", "Error", "Ok", "Result", "bstd::cx::Cx", "std::cx::Cx", "core::option::Option"].map(String::from).to_vec();
    }
    let dir = dir.trim_end_matches('/').to_string();
    let manifest = std::fs::read_to_string(format!("{dir}/Cargo.toml")).expect("the project's Cargo.toml");
    let written = manifest
        .lines()
        .find_map(|l| l.trim().strip_prefix("edition").map(|r| r.trim().trim_start_matches('=').trim().trim_matches('"').to_string()))
        .unwrap_or_else(|| "2021".into());
    let name = manifest
        .lines()
        .find_map(|l| l.trim().strip_prefix("name").map(|r| r.trim().trim_start_matches('=').trim().trim_matches('"').replace('-', "_")))
        .expect("a package name");
    let edition = edition::PackageEdition::of_manifest(&written).expect("a known edition");
    let mut files = Vec::new();
    let mut todo = vec![format!("{dir}/src")];
    while let Some(at) = todo.pop() {
        for e in std::fs::read_dir(&at).expect("src").flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p.display().to_string());
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push((p.display().to_string(), std::fs::read_to_string(&p).expect("UTF-8")));
            }
        }
    }
    let root = format!("{dir}/src/main.rs");
    let shown_from = format!("{dir}/");
    let user = UserCrate { name: &name, dir: &shown_from, root: &root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let home = std::env::var("HOME").expect("HOME");
    let t0 = Instant::now();
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).unwrap_or_else(|e| {
        println!("Names refused: {e}");
        std::process::exit(1);
    });
    let t1 = Instant::now();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| {
        println!("Names refused: {e}");
        std::process::exit(1);
    });
    let crates = inputs.crates();
    println!("program crate: edition {:?}, implicit {:?}, deps {:?}", crates[0].edition, crates[0].implicit, crates[0].deps);
    let names = match inputs.resolve(&crates, erased.bump()) {
        Ok(n) => n,
        Err(e) => {
            println!("Names refused: {e}");
            std::process::exit(1);
        }
    };
    let t2 = Instant::now();
    println!("toolchain read and verified in {:?}; {} crates resolved in {:?}", t1 - t0, crates.len(), t2 - t1);
    for (i, c) in crates.iter().enumerate() {
        let notes = names.notes(i);
        println!("{:>3} {:<28} modules {:>4} items {:>6} notes {:>4}", i, c.name, names.modules(i).count(), names.items(i).count(), notes.len());
        for n in notes.iter().take(3) {
            println!("      note: {n:?}");
        }
    }
    let module = names.root(0);
    for p in &paths {
        let segs: Vec<&str> = p.split("::").collect();
        for ns in [Namespace::Types, Namespace::Values] {
            match names::path(&names, module, ns, Written { kind: PathKind::Plain, segments: &segs }, alloc) {
                Ok(r) => {
                    // Where the definition is: its crate, its module's path and its own name.
                    let krate = names.crate_of(r.def.key);
                    let text = names.item_text(r.def.key);
                    let module = text.and_then(|t| krate.and_then(|c| names.krate(c).path_of(t.module)));
                    println!(
                        "{p} ({ns:?}): {:?} `{}` in {}::{}, consumed {} of {}",
                        r.def.kind,
                        text.and_then(|t| t.name).unwrap_or("?"),
                        krate.map_or("?", |c| crates[c].name),
                        module.unwrap_or("?"),
                        r.consumed,
                        segs.len()
                    )
                }
                Err(e) => println!("{p} ({ns:?}): {e}"),
            }
        }
    }
}
