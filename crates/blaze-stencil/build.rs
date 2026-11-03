//! Makes the stencil libraries (docs/v2/plan/lanes/fastcompile/stencil.md, "Why a build
//! script"): the generator in `gen/` writes each stencil's LLVM IR, compiles it through
//! llvm-sys and writes one library per variant to `OUT_DIR`. It runs no LLVM tool: llvm-sys, a
//! build-dependency, is built first, and its build script refuses any LLVM but the pinned one
//! (vendor/llvm-sys/pin.rs).
//!
//! Every library is held to the pin, `stencils.json` ([`pin`]), which names each library's id:
//! the BLAKE3 hash of its bytes with the id field zeroed. Each one included is checked to hash
//! to the id its header says, and that id to be the pinned one:
//!
//! - With the `llvm-stencils` feature, the libraries the pinned LLVM made must be the pinned
//!   ones. The build writes the pin of what it made beside them (`OUT_DIR/stencils.json`); when
//!   a change to the generator changes a library, the build stops naming each id and the
//!   command that pins what it made, so the tree's pin is always what its generator makes.
//! - Without it (the seed profile, todo/bootstrap/plan.md, which has no LLVM) the libraries are
//!   copied from the directory `BLAZE_STENCILS` names, such as the `OUT_DIR` of a build with
//!   `llvm-stencils`. The variable says only where the bytes are: a library whose bytes are not
//!   the pinned ones is refused, naming its file, the pinned id and its own.
//!
//! The shape set, the hole forms and the library format are the crate's own files, included
//! here so the generator and the patcher read one definition. They use nothing but `std`,
//! `blaze_ids::bits` and each other.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use format::Variant;

// The generator uses part of what the shared files hold for the patcher.
#[allow(dead_code)]
#[path = "src/enc.rs"]
mod enc;
#[allow(dead_code)]
#[path = "src/table/format.rs"]
mod format;
#[cfg(feature = "llvm-stencils")]
#[path = "gen/mod.rs"]
mod generate;
#[allow(dead_code, reason = "shared with blaze-codegen-code; each profile here uses part of it")]
#[path = "pin.rs"]
mod pin;
#[allow(dead_code)]
#[path = "src/shape.rs"]
mod shape;

/// What this build script is made of and reads, relative to this crate (cargo also reruns it
/// when a build-dependency changes): the generator, the files it includes, and the pin.
const READS: [&str; 7] = [
    "build.rs",
    "gen",
    "pin.rs",
    "src/shape.rs",
    "src/enc.rs",
    "src/table/format.rs",
    pin::PIN_FILE,
];

fn main() {
    for path in READS {
        println!("cargo:rerun-if-changed={path}");
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    // Native builds make the libraries before reading the pin, so a pin that is missing,
    // unparseable or incomplete is a mismatch whose error names the pin of what was made.
    #[cfg(feature = "llvm-stencils")]
    made(&out);
    #[cfg(not(feature = "llvm-stencils"))]
    prebuilt(&out, &pinned().unwrap_or_else(|e| refuse(e)));
}

/// Stops the build with `msg`, one line, as `cargo::error=` takes it (vendor/llvm-sys/pin.rs
/// reports the same way): no panic, no backtrace note.
fn refuse(msg: impl std::fmt::Display) -> ! {
    println!("cargo::error={msg}");
    std::process::exit(1)
}

/// The pin's path, absolute, as errors name it.
fn pin_path() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"))
        .join(pin::PIN_FILE)
}

/// Each variant's pinned id, in [`Variant::ALL`]'s order: the pin names every variant's file
/// once and nothing else. Otherwise why not, naming the pin.
fn pinned() -> Result<Vec<(Variant, [u8; 32])>, String> {
    let path = pin_path();
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("blaze-stencil's pin {}: {e}", path.display()))?;
    let pinned = pin::pin_parse(&text)
        .map_err(|e| format!("blaze-stencil's pin {}: {e}", path.display()))?;
    let files: Vec<&str> = Variant::ALL.iter().map(|variant| variant.file()).collect();
    if let Some((name, _)) = pinned.iter().find(|(name, _)| !files.contains(&name.as_str())) {
        return Err(format!(
            "blaze-stencil's pin {} names {name}, which is no stencil library of this tree ({})",
            path.display(),
            files.join(", ")
        ));
    }
    Variant::ALL
        .iter()
        .map(|variant| {
            let id = pinned.iter().find(|(name, _)| name == variant.file()).map(|(_, id)| *id);
            let id = id.ok_or_else(|| {
                format!("blaze-stencil's pin {} names no id for {}", path.display(), variant.file())
            })?;
            Ok((*variant, id))
        })
        .collect()
}

/// The id of the library `bytes`, read from `path` as `variant`'s, once its structure is
/// checked and the id its header says is checked to be the hash of its bytes.
fn checked_id(path: &Path, variant: Variant, bytes: &[u8]) -> [u8; 32] {
    let reader = format::Reader::new(bytes).unwrap_or_else(|e| {
        refuse(format_args!("{} is no stencil library of this tree's format: {e}", path.display()))
    });
    if reader.variant() != variant {
        refuse(format_args!(
            "{} is the {:?} library, not the {variant:?} one",
            path.display(),
            reader.variant()
        ));
    }
    let (said, hashed) = (reader.id(), reader.hashed_id(blaze_ids::b3::hash));
    if said != hashed {
        refuse(format_args!(
            "{}: its header says its id is {}, and its bytes hash to {}: they were changed after the \
             library was made",
            path.display(),
            pin::pin_hex(&said),
            pin::pin_hex(&hashed)
        ));
    }
    hashed
}

/// The libraries the pinned LLVM makes, written to `out` with the pin of what it made, and
/// then held to the tree's pin. A tree pin that can't be read, parsed or names other files is
/// a mismatch like any other: the pin of what was made is already written, ready to copy.
#[cfg(feature = "llvm-stencils")]
fn made(out: &Path) {
    generate::main(out);
    let made: Vec<(&str, [u8; 32])> = Variant::ALL
        .iter()
        .map(|variant| {
            let path = out.join(variant.file());
            let bytes =
                fs::read(&path).unwrap_or_else(|e| refuse(format_args!("{}: {e}", path.display())));
            (variant.file(), checked_id(&path, *variant, &bytes))
        })
        .collect();
    let made_pin = out.join(pin::PIN_FILE);
    fs::write(&made_pin, pin::pin_text(&made))
        .unwrap_or_else(|e| refuse(format_args!("{}: {e}", made_pin.display())));
    let differ = match pinned() {
        Ok(pinned) => pinned
            .iter()
            .zip(&made)
            .filter(|((_, want), (_, got))| want != got)
            .map(|((variant, want), (_, got))| {
                format!(
                    "{} is pinned as {} and made as {}",
                    variant.file(),
                    pin::pin_hex(want),
                    pin::pin_hex(got)
                )
            })
            .collect::<Vec<String>>()
            .join("; "),
        Err(e) => e,
    };
    if !differ.is_empty() {
        let host = env::var("HOST").unwrap_or_else(|e| format!("<HOST: {e}>"));
        refuse(format_args!(
            "the stencil libraries the pinned LLVM made from this tree on {host} are not the ones {} \
             pins: {differ}. If the change to what the generator makes is meant, pin what it made: \
             `cp {} {}`. If nothing it reads changed, the generator's output is not deterministic \
             (AGENTS.md rule 22): {host} makes other bytes than the host that made the pin",
            pin_path().display(),
            made_pin.display(),
            pin_path().display()
        ));
    }
}

/// The pinned libraries copied from [`pin::PREBUILT`]'s directory into `out`, each refused
/// unless its bytes are the pinned ones.
#[cfg(not(feature = "llvm-stencils"))]
fn prebuilt(out: &Path, pinned: &[(Variant, [u8; 32])]) {
    let var = pin::PREBUILT;
    println!("cargo:rerun-if-env-changed={var}");
    let dir = env::var_os(var).filter(|it| !it.is_empty()).unwrap_or_else(|| {
        let names: Vec<&str> = pinned.iter().map(|(variant, _)| variant.file()).collect();
        refuse(format_args!(
            "blaze-stencil is built without its `llvm-stencils` feature (the seed profile), so its \
             stencil libraries come prebuilt, and {var} is unset: set it to a directory holding {}, \
             such as blaze-stencil's OUT_DIR in a build of this tree with `llvm-stencils`; {} pins \
             which bytes they must be",
            names.join(", "),
            pin_path().display()
        ))
    });
    let dir = PathBuf::from(dir);
    for (variant, want) in pinned {
        let (from, to) = (dir.join(variant.file()), out.join(variant.file()));
        println!("cargo:rerun-if-changed={}", from.display());
        let bytes = fs::read(&from)
            .unwrap_or_else(|e| refuse(format_args!("{var}: {}: {e}", from.display())));
        let id = checked_id(&from, *variant, &bytes);
        if id != *want {
            refuse(format_args!(
                "{var}: {} is not the library {} pins: its id is {}, and the pinned one is {}; point \
                 {var} at the libraries a build of this tree with `llvm-stencils` made",
                from.display(),
                pin_path().display(),
                pin::pin_hex(&id),
                pin::pin_hex(want)
            ));
        }
        // The bytes checked, never the file again: what is included is what was checked.
        fs::write(&to, &bytes)
            .unwrap_or_else(|e| refuse(format_args!("{var}: writing {}: {e}", to.display())));
    }
}
