//! The ladder's programs as data (D197 root 6, piece (c); backend-next.md D97 point 1: "every
//! ladder rung runs at every tier and is compared"): each rung's source files, its manifest and
//! its `expected` file, read at compile time from crates/blaze-cli/tests/ladder, the one home of
//! the rungs. Nothing here builds, runs or depends on anything: a leaf every crate can take,
//! so a test of a crate below blaze-ladder (blaze-emit) and a test above it (blaze-rung-tiers)
//! read the same programs without a dependency cycle.
//!
//! WHICH RUNGS: the ones that build on a Rust edition with no toolchain shelf (`#![no_core]`
//! twins and `fn main() {}`), so a test needs nothing outside the tree. A blaze-edition rung
//! needs the shelf and is not here yet; adding one is one row of [`RUNGS`].
//!
//! THE `expected` FORM (crates/blaze-cli/tests/ladder/README.md): line 1 `status <n>`, line 2
//! `stdout`, then every byte the program writes to its standard output. [`Rung::expected`]
//! reads it; a file of another form is refused by name ([`ExpectedError`]), never guessed.

#![no_std]

use core::fmt;

/// One rung: its name (its folder under crates/blaze-cli/tests/ladder), its package edition as
/// its manifest writes it, the manifest's text, every source file as `(path from the package
/// root, text)`, sorted by path, and its `expected` file's bytes.
#[derive(Clone, Copy, Debug)]
pub struct Rung {
    pub name: &'static str,
    pub edition: &'static str,
    pub manifest: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    pub expected: &'static [u8],
}

/// A panic strategy, as a rung's code is built with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    Abort,
    Unwind,
}

/// How the ladder builds every rung's CODE: `panic = abort`, as blaze-ladder's run path builds
/// it (run.rs `build_with`, `build_survey`: `build_from(.., Panic::Abort, ..)`) and as its
/// record tests walk the rungs (tests/code_records.rs `builder_of`, tests/d106_records.rs).
/// Under `unwind` an `extern "C"` body is nounwind, so each call in it lands on a terminate
/// block calling `#[lang = "panic_cannot_unwind"]` (blaze-build walk/nounwind.rs), which no
/// `#![no_core]` rung declares: the walk stops naming that lang item. A rung's manifest names
/// no strategy, so this is the corpus's, one for every rung; a user of the corpus builds with
/// it, never a strategy of its own.
pub const CODE_PANIC: Strategy = Strategy::Abort;

/// What a rung's run must give: the exit status and the standard output, byte for byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Expected {
    pub status: u8,
    pub stdout: &'static [u8],
}

/// Why a rung's `expected` file does not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpectedError {
    /// Line 1 is not `status <n>` with `n` in 0..=255 (a rung whose `expected` says
    /// `panicked ..` is judged elsewhere and is not in [`RUNGS`]).
    Status { rung: &'static str },
    /// Line 2 is not `stdout`.
    Stdout { rung: &'static str },
}

impl fmt::Display for ExpectedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpectedError::Status { rung } => write!(f, "rung {rung}: line 1 of `expected` is not `status <0..=255>`"),
            ExpectedError::Stdout { rung } => write!(f, "rung {rung}: line 2 of `expected` is not `stdout`"),
        }
    }
}

impl core::error::Error for ExpectedError {}

/// Why a rung's manifest does not say its `dev` profile's overflow checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ManifestError {
    /// A line under `[profile.dev]` other than `overflow-checks = true|false`.
    DevKey { rung: &'static str },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::DevKey { rung } => {
                write!(f, "rung {rung}: a line under [profile.dev] that is not `overflow-checks = true|false` (the corpus reads only that key)")
            }
        }
    }
}

impl core::error::Error for ManifestError {}

impl Rung {
    /// The status and standard output its `expected` file says.
    pub fn expected(&self) -> Result<Expected, ExpectedError> {
        let rung = self.name;
        let (first, rest) = split_line(self.expected).ok_or(ExpectedError::Status { rung })?;
        let digits = first.strip_prefix(b"status ").ok_or(ExpectedError::Status { rung })?;
        let status = decimal(digits).ok_or(ExpectedError::Status { rung })?;
        let (second, stdout) = split_line(rest).ok_or(ExpectedError::Stdout { rung })?;
        if second != b"stdout" {
            return Err(ExpectedError::Stdout { rung });
        }
        Ok(Expected { status, stdout })
    }

    /// Whether the manifest's `dev` profile checks overflow: `true` unless `[profile.dev]` says
    /// `overflow-checks = false` (D101: `run` builds with the dev profile). The same reading
    /// as blaze-ladder tests/rungs/x.rs `overflow_of`, refusing what it refuses.
    pub fn dev_overflow_checks(&self) -> Result<bool, ManifestError> {
        let mut in_dev = false;
        let mut on = true;
        for line in self.manifest.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_dev = line == "[profile.dev]";
                continue;
            }
            if !in_dev {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ManifestError::DevKey { rung: self.name });
            };
            if key.trim() != "overflow-checks" {
                return Err(ManifestError::DevKey { rung: self.name });
            }
            on = match value.trim() {
                "true" => true,
                "false" => false,
                _ => return Err(ManifestError::DevKey { rung: self.name }),
            };
        }
        Ok(on)
    }
}

/// `bytes` up to its first `\n` and what follows it; `None` with no `\n`.
fn split_line(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|&b| b == b'\n')?;
    Some((&bytes[..at], &bytes[at + 1..]))
}

/// A decimal number of one to three digits, at most 255.
fn decimal(digits: &[u8]) -> Option<u8> {
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let mut n: u32 = 0;
    for &d in digits {
        if !d.is_ascii_digit() {
            return None;
        }
        n = n * 10 + u32::from(d - b'0');
    }
    u8::try_from(n).ok()
}

/// One rung's row: its folder under crates/blaze-cli/tests/ladder and its source files' paths
/// from the package root, each read by `include_str!` (so a rung's edit rebuilds this crate:
/// the compiler tracks every included file).
macro_rules! rung {
    ($name:literal, $edition:literal, [$($file:literal),+ $(,)?]) => {
        Rung {
            name: $name,
            edition: $edition,
            manifest: include_str!(concat!("../../blaze-cli/tests/ladder/", $name, "/Cargo.toml")),
            files: &[$(($file, include_str!(concat!("../../blaze-cli/tests/ladder/", $name, "/", $file)))),+],
            expected: include_bytes!(concat!("../../blaze-cli/tests/ladder/", $name, "/expected")),
        }
    };
}

/// The rungs, in the ladder's order, then support/, then front2/.
pub const RUNGS: &[Rung] = &[
    rung!("1-empty", "2024", ["src/main.rs"]),
    rung!("2-exit", "2024", ["src/main.rs"]),
    rung!("3-hello", "2024", ["src/main.rs"]),
    rung!("4-modules", "2024", ["src/main.rs", "src/util.rs"]),
    rung!("5-generics", "2024", ["src/main.rs"]),
    rung!("6-aggregates", "2024", ["src/main.rs"]),
    rung!("7-traits", "2024", ["src/main.rs"]),
    rung!("7a-drops", "2024", ["src/main.rs"]),
    rung!("7b-dyn", "2024", ["src/main.rs"]),
    rung!("7c-main-io", "2024", ["src/main.rs"]),
    rung!("support/s1-layout", "2024", ["src/main.rs"]),
    rung!("support/s2-statics", "2024", ["src/main.rs"]),
    rung!("support/s3-extern-c", "2024", ["src/main.rs"]),
    rung!("support/s4-intrinsics", "2024", ["src/main.rs"]),
    rung!("support/s5-thread-local", "2024", ["src/main.rs"]),
    rung!("front2/f1-defaults", "2024", ["src/main.rs"]),
    rung!("front2/f2-generic-self", "2024", ["src/main.rs"]),
    rung!("front2/f4-assoc", "2024", ["src/main.rs"]),
    rung!("front2/f5-closures", "2024", ["src/main.rs"]),
    rung!("front2/f6-literals", "2024", ["src/main.rs"]),
    rung!("front2/x0-int-11-three-way", "2024", ["src/main.rs", "src/prelude.rs"]),
    rung!("front2/x0-int-15-add-wraps-release", "2024", ["src/main.rs"]),
    rung!("front2/x0-adt-10-packed", "2024", ["src/main.rs", "src/prelude.rs"]),
    rung!("front2/x0-atomic-01-orderings", "2024", ["src/main.rs", "src/prelude.rs"]),
];

/// The rung named `name`.
pub fn rung(name: &str) -> Option<&'static Rung> {
    RUNGS.iter().find(|r| r.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rung's `expected` reads, its manifest says its overflow setting, its files are
    /// sorted and each names a `.rs` file, and no name is listed twice.
    /// Expected: passes; a malformed row fails naming the rung.
    #[test]
    fn every_rung_reads() {
        for (i, r) in RUNGS.iter().enumerate() {
            if let Err(error) = r.expected() {
                panic!("{error}");
            }
            if let Err(error) = r.dev_overflow_checks() {
                panic!("{error}");
            }
            assert!(r.files.windows(2).all(|w| w[0].0 < w[1].0), "rung {}: files not sorted", r.name);
            assert!(r.files.iter().all(|(path, _)| path.ends_with(".rs")), "rung {}: a file that is not Rust", r.name);
            assert!(RUNGS[..i].iter().all(|other| other.name != r.name), "rung {} listed twice", r.name);
        }
    }

    /// The reader refuses a status line it cannot read (the planted control of
    /// [`every_rung_reads`]: a reader that accepted anything would pass it vacuously).
    /// Expected: passes.
    #[test]
    fn a_bad_expected_is_refused_by_name() {
        let bad = Rung { name: "planted", edition: "2024", manifest: "", files: &[], expected: b"status 256\nstdout\n" };
        assert_eq!(bad.expected(), Err(ExpectedError::Status { rung: "planted" }));
        let bad = Rung { expected: b"status 0\nout\n", ..bad };
        assert_eq!(bad.expected(), Err(ExpectedError::Stdout { rung: "planted" }));
        let good = Rung { expected: b"status 42\nstdout\nhello\n", ..bad };
        assert_eq!(good.expected(), Ok(Expected { status: 42, stdout: b"hello\n" }));
        let dev = Rung { manifest: "[profile.dev]\npanic = \"abort\"\n", ..good };
        assert_eq!(dev.dev_overflow_checks(), Err(ManifestError::DevKey { rung: "planted" }));
    }
}
