//! What the library-feature walk (`blaze_scope::lib_features`, the profile's `Lex::next`)
//! costs over the tree's own `library/`: core, alloc, std and the crates of `staged_api` std
//! reaches, under the host's cfg, with every file read into memory first so the time is the
//! walk's alone. Prints ms a whole walk and ns a byte, best of seven, for each way of crossing
//! a comment or a string (`lib_features::Jump`): `Scalar`, a byte at a time (before), and
//! `Vector`, blaze-memchr's search (after); and whether the two listings are equal.
//!
//! The floor: the walk reads every byte once; past comments and strings (most of the bytes:
//! doc comments) a 16-byte NEON compare runs at about 16 to 32 bytes a ns, so the floor is the
//! token bytes' scalar lexing, about 1 ns a token byte.
//!
//!     bash /Volumes/Projects/blaze-int-1112-notes/examplerun.sh blaze-scope cost
//!
//! or `cargo run --release -p blaze-scope --example cost`.

#![feature(allocator_api)]
#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

use core::cell::RefCell;
use std::time::Instant;

use blaze_own::{AString, AVec, Alloc, Heap};
use blaze_scope::lib_features::{Jump, LibFeatures, LibFeaturesOpts, Scalar, Vector};
use blaze_scope::resolve::SourceTexts;
use blaze_scope::task::Cfg;

/// The crates read, as `lib_features`' test of the pinned library reads them.
const CRATES: &[&str] = &["core", "alloc", "std", "std_detect", "unwind", "panic_unwind", "panic_abort"];

/// Files read from disk on first ask and kept, each path once, for the run's life.
struct Loaded {
    files: RefCell<AVec<'static, (AString<'static>, &'static str)>>,
    alloc: Alloc<'static>,
}

impl SourceTexts<'static> for Loaded {
    fn read(&self, _krate: usize, path: &str) -> Option<&'static str> {
        let mut files = self.files.borrow_mut();
        match files.binary_search_by(|(at, _)| at.as_str().cmp(path)) {
            Ok(found) => Some(files[found].1),
            Err(place) => {
                let text: &'static str = std::string::String::leak(std::fs::read_to_string(path).ok()?);
                // Linear, once a file: about 1,500 files on the untimed first walk.
                files.insert_shift(place, (AString::from_str_in(path, self.alloc), text));
                Some(text)
            }
        }
    }
}

/// One whole walk of the crates by `J`: ms, and the listing's names (declared, then
/// mentioned) and files read.
fn walk<J: Jump>(loaded: &Loaded, roots: &[&'static str], cfg: &Cfg<'_, '_>, alloc: Alloc<'static>) -> (f64, AVec<'static, &'static str>, usize) {
    let start = Instant::now();
    let mut features = LibFeatures::new(LibFeaturesOpts { alloc });
    for root in roots {
        if let Err(why) = features.scan_by::<J, _>(loaded, 0, root, cfg) {
            panic!("{root}: {why}");
        }
    }
    let listed = features.listed(alloc);
    let ms = start.elapsed().as_secs_f64() * 1e3;
    let mut names = AVec::new_in(alloc);
    names.extend_from_slice(&listed.declared);
    names.push("--");
    names.extend_from_slice(&listed.mentioned);
    (ms, names, listed.files)
}

fn main() {
    let alloc = Heap::process().alloc();
    let library = std::string::String::leak(std::format!("{}/../../library", env!("CARGO_MANIFEST_DIR")));
    let mut roots: AVec<'static, &'static str> = AVec::new_in(alloc);
    for krate in CRATES {
        roots.push(std::string::String::leak(std::format!("{library}/{krate}/src/lib.rs")));
    }
    let atoms: &[(&str, Option<&str>)] = &[
        ("target_arch", Some("aarch64")),
        ("target_os", Some("macos")),
        ("target_family", Some("unix")),
        ("unix", None),
        ("target_pointer_width", Some("64")),
        ("target_endian", Some("little")),
        ("panic", Some("unwind")),
    ];
    let cfg = Cfg::from_atoms(atoms, alloc);
    let loaded = Loaded { files: RefCell::new(AVec::new_in(alloc)), alloc };
    // The first walk reads the files; the timed ones find them in memory.
    let (_, want, files) = walk::<Scalar>(&loaded, &roots, &cfg, alloc);
    let bytes: usize = loaded.files.borrow().iter().map(|(_, text)| text.len()).sum();
    println!("library/: {files} files, {bytes} bytes");
    println!("search | ms a walk | ns a byte | listing");
    report::<Scalar>("Scalar", &loaded, &roots, &cfg, &want, bytes, alloc);
    report::<Vector>("Vector", &loaded, &roots, &cfg, &want, bytes, alloc);
}

/// The best of seven walks by `J`, and whether each listed `want`.
fn report<J: Jump>(name: &str, loaded: &Loaded, roots: &[&'static str], cfg: &Cfg<'_, '_>, want: &[&'static str], bytes: usize, alloc: Alloc<'static>) {
    let mut best = f64::MAX;
    let mut same = true;
    for _ in 0..7 {
        let (ms, names, _) = walk::<J>(loaded, roots, cfg, alloc);
        best = best.min(ms);
        same &= names.as_slice() == want;
    }
    let per_byte = best * 1e6 / bytes as f64;
    println!("{name:<6} | {best:>9.2} | {per_byte:>9.3} | {}", if same { "equal" } else { "DIFFERS" });
}
