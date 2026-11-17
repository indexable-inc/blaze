//! `vendor_from` (D112 N2): the fetch step from blaze's fetch store, in place of `cargo vendor`.
//! No network: every package is written from a verified record, and a package the store does
//! not hold is a refusal naming it.
//!
//! What is written, under `<work>/vendor/<name>-<version>/` (cargo's directory source, the
//! layout `cargo vendor --versioned-dirs` makes): the files of the package's record, found at
//! `<store>/records/<key>` by `SourceId::Crate { name, version, sha256 }.key()` (the key of
//! blaze-fetch's plan's `Unit::key()`, from the lock's text alone) and opened by
//! `PackView::open_source`, which refuses a record that is not that `.crate`'s files; and the
//! `.cargo-checksum.json` cargo's directory source checks: `{"files":{<path>:<SHA-256 of its
//! bytes>,..},"package":"<the lock's checksum>"}`, compact, the files in the record's order
//! (sorted by path bytes), as `cargo vendor` writes it.
//!
//! Which packages: from the checkout's lock, the closure of each registry crate asked for, and
//! each dependency the build's member declares (all of a macro crate's manifest; a script's
//! build-dependency tables), with that package's closure: the one the lock pins under the
//! crate, or every version the lock pins of an optional one it does not activate. From std's
//! own lock (`library/Cargo.lock`, what `-Zbuild-std` builds), every crates.io package. A
//! workspace, path or git package is not vendored, as with `cargo vendor` before (the build's
//! source replacement covers crates.io only; a git root's files come from the store by
//! [`super::git_origin`]).
//!
//! Refused, every one named before anything is written: a dependency the lock pins no version
//! of; a registry package blaze-fetch keys no record for (no checksum, another registry); a
//! package whose record the store does not hold. Not seen here: an optional dependency deeper
//! down that only a feature of the member turns on and the lock does not pin: the offline build
//! names it.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use blaze_fetch::{Mode, PackView, Skip, SourceId, Unit};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::Alloc;
use blaze_text::Shown;

use super::{Failure, Origin, Plan, SCRIPT, hex, io, read, sha256, write};

/// Which lock pinned a package.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Which {
    /// The checkout's `Cargo.lock`.
    Checkout,
    /// The toolchain's `library/Cargo.lock`: std's own dependencies.
    Std,
}

impl fmt::Display for Which {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Which::Checkout => "the checkout's lock",
            Which::Std => "std's lock (library/Cargo.lock)",
        })
    }
}

/// A package the build needs that cannot be vendored from the store, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Missing {
    /// A crates.io package the lock pins whose record (`records/<key>`) the store does not hold.
    NotStored { which: Which, name: String, version: String, key: String },
    /// A dependency the crate `of` (`name version`) declares that the lock pins no version of.
    NotPinned { name: String, of: String },
    /// A registry package the lock pins that blaze-fetch keys no record for: its words.
    NoKey { which: Which, name: String, version: String, why: String },
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Missing::NotStored { which, name, version, key } => {
                write!(f, "{name} {version} (pinned by {which}): the store holds no record {key}")
            }
            Missing::NotPinned { name, of } => {
                write!(f, "{name}: a dependency {of} declares, of which the checkout's lock pins no version, so no record can be keyed")
            }
            Missing::NoKey { which, name, version, why } => write!(f, "{name} {version} (pinned by {which}): no record is keyed for it: {why}"),
        }
    }
}

/// A crate the run builds, as the store fetch starts from it.
pub(super) struct Root<'r> {
    pub(super) name: &'r str,
    pub(super) version: &'r str,
    /// A workspace or git package's manifest (what it inherits written in); `None` for a
    /// registry crate, whose manifest is read from its record.
    pub(super) manifest: Option<&'r str>,
}

/// A lock as given: where it was read (for errors) and its text.
#[derive(Clone, Copy)]
pub(super) struct LockText<'t> {
    pub(super) path: &'t Path,
    pub(super) text: &'t str,
}

/// What [`from_store`] wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Vendored {
    /// Package folders written.
    pub(super) packages: usize,
    /// Of them, pinned by std's lock only.
    pub(super) std_only: usize,
}

/// One package to write: the lock's pin.
struct Pinned {
    name: String,
    version: String,
    sha256: [u8; 32],
    which: Which,
}

impl Pinned {
    fn source(&self) -> SourceId<'_> {
        SourceId::Crate { name: &self.name, version: &self.version, sha256: self.sha256 }
    }
}

impl Plan<'_> {
    /// Step one with `vendor_from`: `<work>/vendor` written from the fetch store `store`, and
    /// `<work>/fetch/Cargo.lock` (the lock each build's own is made from) the checkout's.
    /// `cut` is the part of a crate's manifest its build's member takes: all of it for a macro
    /// crate, the build-dependency tables for a script. No network, no program started.
    pub(super) fn vendor_from(&self, store: &Path, cut: fn(&str) -> String, alloc: Alloc<'_>) -> Result<(), Failure> {
        write(&self.work.join("fetch").join("Cargo.lock"), &self.lock)?;
        let roots: Vec<Root<'_>> = self
            .located()
            .map(|(_, wanted, origin)| Root {
                name: &wanted.name,
                version: &wanted.version,
                manifest: match origin {
                    Origin::Registry { .. } => None,
                    Origin::Path { manifest, .. } | Origin::Git { manifest, .. } => Some(manifest.as_str()),
                },
            })
            .collect();
        let checkout_path = self.args.checkout.join("Cargo.lock");
        let std_path = self.tool.library().join("Cargo.lock");
        let std_text = read(&std_path)?;
        let vendored = from_store(
            store,
            &self.work.join("vendor"),
            LockText { path: &checkout_path, text: &self.lock },
            LockText { path: &std_path, text: &std_text },
            &roots,
            cut,
            alloc,
        )?;
        println!(
            "{SCRIPT}: vendored {} packages from the fetch store {} ({} pinned by std's lock only); no network",
            vendored.packages,
            store.display(),
            vendored.std_only
        );
        Ok(())
    }
}

/// The packages `roots` reach in `checkout` and every crates.io package of `std`, each written
/// under `vendor` from its record in `store` with its `.cargo-checksum.json`. Every package
/// that cannot be is named in one [`Failure::Unvendored`] before anything is written.
pub(super) fn from_store(
    store: &Path,
    vendor: &Path,
    checkout: LockText<'_>,
    std: LockText<'_>,
    roots: &[Root<'_>],
    cut: fn(&str) -> String,
    alloc: Alloc<'_>,
) -> Result<Vendored, Failure> {
    let refused = |path: &Path, why: String| Failure::Manifest { path: path.to_owned(), why };
    let lock = Lock::read(checkout.text.as_bytes(), alloc).map_err(|why| refused(checkout.path, why.to_string()))?;
    let plan = blaze_fetch::plan::plan(&lock, alloc).map_err(|why| refused(checkout.path, Shown(&why).to_string()))?;
    let std_lock = Lock::read(std.text.as_bytes(), alloc).map_err(|why| refused(std.path, why.to_string()))?;
    let std_plan = blaze_fetch::plan::plan(&std_lock, alloc).map_err(|why| refused(std.path, Shown(&why).to_string()))?;
    let unit_of = units_by_package(&lock, &plan.units);
    let std_unit_of = units_by_package(&std_lock, &std_plan.units);
    let mut missing: Vec<Missing> = Vec::new();

    // The checkout's packages the builds reach, walked with a work list.
    let mut todo: Vec<LockId> = Vec::new();
    for root in roots {
        let entry = format!("{} {}", root.name, root.version);
        let id = lock.entry(&entry).ok_or_else(|| Failure::NotLocked { name: root.name.to_owned(), version: root.version.to_owned() })?;
        let manifest = match root.manifest {
            Some(manifest) => manifest.to_owned(),
            None => {
                todo.push(id);
                // A registry root's manifest is in its record; with no record (or no key) the
                // walk below names it, and its declared dependencies wait for it.
                let Some(unit) = unit_of[id.index()].map(|at| &plan.units[at]) else { continue };
                let Some(manifest) = manifest_in_record(store, unit)? else { continue };
                manifest
            }
        };
        let declared = &lock.package(id).deps;
        for name in dependency_packages(&cut(&manifest)) {
            let locked: Vec<LockId> = declared.iter().copied().filter(|dep| lock.package(*dep).name.as_str() == name).collect();
            if !locked.is_empty() {
                todo.extend(locked);
                continue;
            }
            // Not activated under this crate: an optional dependency, which cargo resolves for
            // a workspace member all the same.
            let named: Vec<LockId> = lock.named(&name).collect();
            if named.is_empty() {
                missing.push(Missing::NotPinned { name, of: entry.clone() });
            }
            todo.extend(named);
        }
    }
    let mut seen = vec![false; lock.packages.len()];
    while let Some(id) = todo.pop() {
        if std::mem::replace(&mut seen[id.index()], true) {
            continue;
        }
        todo.extend(lock.package(id).deps.iter().copied());
    }

    let mut pinned: Vec<Pinned> = Vec::new();
    for (at, reached) in seen.iter().enumerate() {
        if *reached {
            pin(&lock, &plan, &unit_of, at, Which::Checkout, &mut pinned, &mut missing);
        }
    }
    for at in 0..std_lock.packages.len() {
        pin(&std_lock, &std_plan, &std_unit_of, at, Which::Std, &mut pinned, &mut missing);
    }
    // One folder per name and version: a stable sort keeps the checkout's pin first.
    pinned.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    let mut kept: Vec<Pinned> = Vec::with_capacity(pinned.len());
    for package in pinned {
        match kept.last() {
            Some(held) if held.name == package.name && held.version == package.version => {
                if held.sha256 != package.sha256 {
                    return Err(refused(
                        std.path,
                        format!(
                            "{} {} is pinned by {} to {} and by {} to {}: one folder, two sources",
                            package.name,
                            package.version,
                            held.which,
                            hex(&held.sha256),
                            package.which,
                            hex(&package.sha256)
                        ),
                    ));
                }
            }
            _ => kept.push(package),
        }
    }
    let std_only = kept.iter().filter(|it| it.which == Which::Std).count();

    let records = store.join("records");
    for package in &kept {
        let key = hex(&package.source().key().0);
        let record = records.join(&key);
        match std::fs::metadata(&record) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(Missing::NotStored { which: package.which, name: package.name.clone(), version: package.version.clone(), key });
            }
            Err(error) => return Err(Failure::Io { path: record, error }),
        }
    }
    if !missing.is_empty() {
        missing.sort_by_key(|it| it.to_string());
        missing.dedup();
        return Err(Failure::Unvendored { store: store.to_owned(), missing });
    }

    for package in &kept {
        let record = records.join(hex(&package.source().key().0));
        let bytes = std::fs::read(&record).map_err(io(&record))?;
        let view = PackView::open_source(&package.source(), &bytes).map_err(|why| {
            refused(&record, format!("the record of {} {}: {}", package.name, package.version, Shown(&why)))
        })?;
        write_package(&vendor.join(format!("{}-{}", package.name, package.version)), &view, &hex(&package.sha256), &record)?;
    }
    Ok(Vendored { packages: kept.len(), std_only })
}

/// Each lock package's crate unit, by the package's index.
fn units_by_package(lock: &Lock<'_>, units: &[Unit<'_, '_>]) -> Vec<Option<usize>> {
    let mut out = vec![None; lock.packages.len()];
    for (at, unit) in units.iter().enumerate() {
        if let Unit::Crate { package, .. } = unit {
            out[package.index()] = Some(at);
        }
    }
    out
}

/// The `at`th package of `lock` as a pin to write, or why it cannot be one; a workspace, path
/// or git package is none. `plan` is blaze-fetch's of `lock`, `unit_of` its units by package.
fn pin(
    lock: &Lock<'_>,
    plan: &blaze_fetch::Plan<'_, '_>,
    unit_of: &[Option<usize>],
    at: usize,
    which: Which,
    pinned: &mut Vec<Pinned>,
    missing: &mut Vec<Missing>,
) {
    let package = &lock.packages[at];
    if !matches!(package.source, LockSource::Registry { .. }) {
        return;
    }
    let skipped: &[Skip<'_>] = &plan.skipped;
    match unit_of[at].map(|unit| &plan.units[unit]) {
        Some(Unit::Crate { name, version, sha256, .. }) => {
            pinned.push(Pinned { name: (*name).to_owned(), version: version.as_str().to_owned(), sha256: *sha256, which });
        }
        Some(Unit::Git { .. }) | None => {
            let name = package.name.as_str();
            let why = skipped
                .iter()
                .find(|skip| match skip {
                    Skip::NoChecksum { name: skipped } | Skip::OtherRegistry { name: skipped, .. } | Skip::GitNotHttps { name: skipped, .. } => {
                        *skipped == name
                    }
                })
                .map_or_else(|| "blaze-fetch's plan of the lock has no unit for it".to_owned(), |skip| Shown(skip).to_string());
            missing.push(Missing::NoKey { which, name: name.to_owned(), version: package.version.to_string(), why });
        }
    }
}

/// A registry crate's `Cargo.toml` from its record; `None` when the store holds no record (the
/// walk names it).
fn manifest_in_record(store: &Path, unit: &Unit<'_, '_>) -> Result<Option<String>, Failure> {
    let record = store.join("records").join(hex(&unit.key().0));
    let bytes = match std::fs::read(&record) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Failure::Io { path: record, error }),
    };
    let refuse = |why: String| Failure::Manifest { path: record.clone(), why };
    let view = PackView::open_source(&unit.source(), &bytes).map_err(|why| refuse(format!("{}", Shown(&why))))?;
    let file = view.find("Cargo.toml").ok_or_else(|| refuse("the crate's record holds no Cargo.toml".to_owned()))?;
    let text = std::str::from_utf8(file.bytes).map_err(|error| refuse(format!("its Cargo.toml is not UTF-8: {error}")))?;
    Ok(Some(text.to_owned()))
}

/// One package's folder, written afresh from `view` (anything there before is removed: the
/// folder is the run's own), and its `.cargo-checksum.json` with `checksum` as `package`.
fn write_package(dir: &Path, view: &PackView<'_>, checksum: &str, record: &Path) -> Result<(), Failure> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Failure::Io { path: dir.to_owned(), error }),
    }
    let mut files = String::new();
    for file in view.files() {
        let path: PathBuf = dir.join(file.path);
        match file.mode {
            Mode::File => write(&path, file.bytes)?,
            Mode::Exec => {
                use std::os::unix::fs::PermissionsExt as _;
                write(&path, file.bytes)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(io(&path))?;
            }
            Mode::Link => {
                return Err(Failure::Manifest {
                    path: record.to_owned(),
                    why: format!("{} is a link: a crate's link is not vendored yet (cargo's directory source checks files)", file.path),
                });
            }
        }
        if !files.is_empty() {
            files.push(',');
        }
        let quoted = serde_json::to_string(file.path).map_err(|error| Failure::Manifest { path: record.to_owned(), why: error.to_string() })?;
        files.push_str(&format!("{quoted}:\"{}\"", sha256(file.bytes)));
    }
    write(&dir.join(".cargo-checksum.json"), format!("{{\"files\":{{{files}}},\"package\":\"{checksum}\"}}"))
}

/// The package names `manifest`'s dependency tables declare (`[dependencies]`,
/// `[build-dependencies]`, their `[target.*]` forms and `[..dependencies.NAME]` tables; never
/// `dev-dependencies`), a renamed one by its `package`; sorted, each once.
fn dependency_packages(manifest: &str) -> Vec<String> {
    /// Where the reader is: outside a dependency table, in a list of them, or in one's table.
    enum At {
        Elsewhere,
        List,
        One(usize),
    }
    // (the key it is declared by, its package)
    let mut declared: Vec<(String, String)> = Vec::new();
    let mut at = At::Elsewhere;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            at = match dependency_table(header) {
                None => At::Elsewhere,
                Some(None) => At::List,
                Some(Some(name)) => {
                    declared.push((name.to_owned(), name.to_owned()));
                    At::One(declared.len() - 1)
                }
            };
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        match at {
            At::Elsewhere => {}
            At::One(held) => {
                if key == "package" {
                    declared[held].1 = value.trim_matches('"').to_owned();
                }
            }
            At::List => {
                // `name = ..`, or a dotted `name.package = ..` / `name.version = ..`.
                let (name, rest) = match key.split_once('.') {
                    Some((name, rest)) => (name.trim().trim_matches('"'), Some(rest.trim())),
                    None => (key.trim_matches('"'), None),
                };
                let package = match rest {
                    Some("package") => Some(value.trim_matches('"').to_owned()),
                    Some(_) => None,
                    None => inline_package(value),
                };
                match declared.iter_mut().find(|(held, _)| held == name) {
                    Some(held) => {
                        if let Some(package) = package {
                            held.1 = package;
                        }
                    }
                    None => declared.push((name.to_owned(), package.unwrap_or_else(|| name.to_owned()))),
                }
            }
        }
    }
    let mut out: Vec<String> = declared.into_iter().map(|(_, package)| package).collect();
    out.sort();
    out.dedup();
    out
}

/// A dependency table's header read: `Some(None)` for a list of dependencies, `Some(Some(name))`
/// for one dependency's table, `None` for any other table (`dev-dependencies` among them).
fn dependency_table(header: &str) -> Option<Option<&str>> {
    let rest = match header.strip_prefix("target.") {
        None => header,
        Some(target) => {
            // The target's cfg or triple: quoted (dots and all inside), or up to the next dot.
            let end = match target.chars().next() {
                Some(quote @ ('\'' | '"')) => target[1..].find(quote)? + 2,
                _ => target.find('.')?,
            };
            target[end..].strip_prefix('.')?
        }
    };
    let rest = rest.strip_prefix("build-").unwrap_or(rest);
    let rest = rest.strip_prefix("dependencies")?;
    if rest.is_empty() {
        return Some(None);
    }
    Some(Some(rest.strip_prefix('.')?.trim_matches('"')))
}

/// The `package = "x"` of an inline table's text, if it has one.
fn inline_package(value: &str) -> Option<String> {
    let inner = value.strip_prefix('{')?;
    inner.split(',').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == "package").then(|| value.trim().trim_end_matches('}').trim().trim_matches('"').to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock's checksum of the fake crate's `.crate` (any 64 hex digits: no `.crate` is read).
    const CHECKSUM: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const MANIFEST: &str = "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    const LIB: &str = "pub fn one() -> u32 {\n    1\n}\n";
    /// The fixture: what cargo's directory source checks, compact, `files` (each file's SHA-256,
    /// by `shasum -a 256` of the bytes above) before `package` (the lock's checksum).
    const CHECKSUM_JSON: &str = "{\"files\":{\"Cargo.toml\":\"1ea0298919290d9b20cb2aa15ca55459619b1333b0cdeeea530ace925789e34b\",\"src/lib.rs\":\"1afe9973bc88e19065efa92c68f873d5c8436c344948d1b05c0c4b2bb42ae4cd\"},\"package\":\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"}";
    const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";
    const NO_STD: &str = "version = 4\n";

    fn sha(text: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (at, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).expect("hex");
        }
        out
    }

    /// A fresh folder of the test's own.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-store-vendor-{}-{name}", std::process::id()));
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{}: {error}", dir.display()),
        }
        dir
    }

    /// A store holding one record: `tiny 0.1.0`'s two files, made by blaze-fetch's builder.
    fn store_with_tiny(store: &Path, alloc: Alloc<'_>) {
        let limits = blaze_fetch::pack::Limits { most_files: 16, most_bytes: 1 << 20, most_path: 256 };
        let mut builder = blaze_fetch::pack::Builder::new(blaze_fetch::pack::Kind::Crate, sha(CHECKSUM), limits, 0, alloc).expect("a builder");
        builder.file("Cargo.toml", Mode::File, MANIFEST.as_bytes()).expect("Cargo.toml");
        builder.file("src/lib.rs", Mode::File, LIB.as_bytes()).expect("src/lib.rs");
        let record = builder.finish().expect("the record");
        let key = SourceId::Crate { name: "tiny", version: "0.1.0", sha256: sha(CHECKSUM) }.key();
        let path = store.join("records").join(hex(&key.0));
        std::fs::create_dir_all(store.join("records")).expect("records/");
        std::fs::write(&path, &record[..]).expect("the record written");
    }

    fn package(name: &str, version: &str, checksum: &str, deps: &[&str]) -> String {
        let mut out = format!("\n[[package]]\nname = {name:?}\nversion = {version:?}\n");
        if !checksum.is_empty() {
            out.push_str(&format!("source = {REGISTRY:?}\nchecksum = {checksum:?}\n"));
        }
        if !deps.is_empty() {
            out.push_str("dependencies = [\n");
            for dep in deps {
                out.push_str(&format!(" {dep:?},\n"));
            }
            out.push_str("]\n");
        }
        out
    }

    /// N2's fixture test: a store with one record, and the vendored folder and its
    /// `.cargo-checksum.json` byte-equal to what cargo's directory source wants. Prints
    /// "vendored from the store: tiny 0.1.0, byte-equal to the fixture".
    #[test]
    fn a_stored_crate_is_vendored_byte_equal_to_the_fixture() {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = folder("fixture");
        let (store, vendor) = (dir.join("store"), dir.join("work/vendor"));
        store_with_tiny(&store, alloc);
        let lock = format!("version = 4\n{}", package("tiny", "0.1.0", CHECKSUM, &[]));
        let roots = [Root { name: "tiny", version: "0.1.0", manifest: None }];
        let vendored = match from_store(
            &store,
            &vendor,
            LockText { path: Path::new("Cargo.lock"), text: &lock },
            LockText { path: Path::new("library/Cargo.lock"), text: NO_STD },
            &roots,
            |manifest| manifest.to_owned(),
            alloc,
        ) {
            Ok(vendored) => vendored,
            Err(failure) => panic!("refused: {failure}"),
        };
        assert_eq!(vendored, Vendored { packages: 1, std_only: 0 });
        let folder = vendor.join("tiny-0.1.0");
        let text = |path: &str| std::fs::read_to_string(folder.join(path)).unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(text("Cargo.toml"), MANIFEST);
        assert_eq!(text("src/lib.rs"), LIB);
        assert_eq!(text(".cargo-checksum.json"), CHECKSUM_JSON);
        println!("vendored from the store: tiny 0.1.0, byte-equal to the fixture");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the store lacks is refused by name, and nothing is written: `gone 1.0.0` (under
    /// `tiny`, no record) and `absent` (declared by the workspace member, pinned by no lock
    /// entry). Prints "refused by name: gone 1.0.0, absent".
    #[test]
    fn a_package_missing_from_the_store_is_refused_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let dir = folder("missing");
        let (store, vendor) = (dir.join("store"), dir.join("work/vendor"));
        store_with_tiny(&store, alloc);
        let gone = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let lock = format!(
            "version = 4\n{}{}{}",
            package("gone", "1.0.0", gone, &[]),
            package("member", "0.1.0", "", &["tiny"]),
            package("tiny", "0.1.0", CHECKSUM, &["gone"]),
        );
        let member = "[package]\nname = \"member\"\nversion = \"0.1.0\"\n\n[dependencies]\ntiny = \"0.1\"\nabsent = { version = \"1\", optional = true }\n\n[dev-dependencies]\nnot-built = \"1\"\n";
        let roots = [Root { name: "member", version: "0.1.0", manifest: Some(member) }];
        let result = from_store(
            &store,
            &vendor,
            LockText { path: Path::new("Cargo.lock"), text: &lock },
            LockText { path: Path::new("library/Cargo.lock"), text: NO_STD },
            &roots,
            |manifest| manifest.to_owned(),
            alloc,
        );
        let Err(Failure::Unvendored { missing, .. }) = result else { panic!("a store without gone 1.0.0 was not refused") };
        let named: Vec<String> = missing.iter().map(|it| it.to_string()).collect();
        assert_eq!(named.len(), 2, "{named:?}");
        assert!(named.iter().any(|it| it.starts_with("gone 1.0.0 (pinned by the checkout's lock)")), "{named:?}");
        assert!(named.iter().any(|it| it.starts_with("absent: a dependency member 0.1.0 declares")), "{named:?}");
        assert!(!vendor.join("tiny-0.1.0").exists(), "a refused fetch wrote tiny");
        println!("refused by name: gone 1.0.0, absent");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tables read, renames by `package`, and `dev-dependencies` left out.
    #[test]
    fn dependency_tables_are_read_by_package() {
        let manifest = "[package]\nname = \"m\"\n\n[dependencies]\nserde = \"1\"\nrenamed = { package = \"real\", version = \"2\" }\n\n[dependencies.tabled]\nversion = \"3\"\npackage = \"tabled-real\"\n\n[target.'cfg(target_os = \"linux\")'.build-dependencies]\ncc = \"1\"\n\n[dev-dependencies]\nproptest = \"1\"\n";
        assert_eq!(dependency_packages(manifest), ["cc", "real", "serde", "tabled-real"]);
    }
}
