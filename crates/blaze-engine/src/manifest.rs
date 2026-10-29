//! The build manifest (`docs/glancer.md`, item 2; #41): what a full build of a program wrote,
//! kept in the store under a key of everything it was built from, so that a start whose
//! checkout and configuration are the same writes those files again and links, with no
//! inference, MIR, monomorphization or translation. A fresh machine with a warm remote then
//! builds an unchanged checkout in the time it takes to load it and link.
//!
//! The key is a Merkle hash of every crate the workspace loaded (its name, edition, kind,
//! attributes, cfg, environment, the files of its package and of its build script's output
//! for one of the checkout, the version and place for one fetched, and the keys of its
//! dependencies), the program asked for and how (`Args`: the crates, the profile, features and
//! target, what a debugger is told, where the checkout and the outputs are), the standard
//! library's unit it links, and the variables that change what is compiled (`BLAZE_*`,
//! `RUSTFLAGS`, `CARGO_PROFILE_*`, ...). The store adds the toolchain and blaze's own build.
//! What a build reads that only compiling finds out is kept with the files and checked before
//! they are used: the variables `env!` and `option_env!` read of the process, and the files
//! `include_bytes!` and `include_str!` read. A manifest that matches the key and not those is
//! not used.
//!
//! `BLAZE_MANIFEST` says what is done with it: by default a full build of a program the store
//! has a manifest of writes its files; `check` builds in full anyway and fails the build when
//! what it wrote is not the manifest's; `off` neither reads nor writes one.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ide_db::{
    RootDatabase,
    base_db::{Crate, SourceDatabase as _},
};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::{
    Args,
    content::{self, hex},
};

type Hash = [u8; 32];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Use,
    Check,
}

pub(crate) fn mode() -> Option<Mode> {
    match std::env::var("BLAZE_MANIFEST").as_deref() {
        Ok("off" | "0") => None,
        Ok("check") => Some(Mode::Check),
        _ => Some(Mode::Use),
    }
}

/// Variables of the process that change what a build compiles, beside those `env!` reads.
fn configures(name: &str) -> bool {
    const NOT_OF_THE_CODE: &[&str] = &[
        "BLAZE_STATS",
        "BLAZE_PROFILE",
        "BLAZE_PROFILE_CONST",
        "BLAZE_MEMORY",
        "BLAZE_MANIFEST",
        "BLAZE_HOME",
        "BLAZE_PROJECT",
    ];
    let ours = name.starts_with("BLAZE_")
        && !name.starts_with("BLAZE_CACHE")
        && !name.starts_with("BLAZE_REMOTE")
        && !NOT_OF_THE_CODE.contains(&name);
    ours || name == "RUSTFLAGS"
        || name == "CARGO_ENCODED_RUSTFLAGS"
        || name.starts_with("CARGO_PROFILE_")
        || name.starts_with("CARGO_BUILD_")
        || name.starts_with("CARGO_TARGET_")
}

struct Hasher(Sha256);

impl Hasher {
    fn new(domain: &str) -> Hasher {
        let mut it = Hasher(Sha256::new());
        it.field(domain.as_bytes());
        it
    }

    fn field(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    fn finish(self) -> Hash {
        self.0.finalize().into()
    }
}

/// What stands for `krate` in the key: everything of it that what is compiled of it depends
/// on and that loading the workspace knows, and the same of every crate it depends on.
fn crate_key(db: &RootDatabase, krate: Crate, keys: &mut FxHashMap<Crate, Option<Hash>>) -> Hash {
    if let Some(&known) = keys.get(&krate) {
        // A cycle, which only dev-dependencies make, stands for itself by its name.
        return known.unwrap_or_else(|| {
            let mut hasher = Hasher::new("blaze manifest cycle");
            hasher.field(content::Contents::version(db, krate).as_bytes());
            hasher.finish()
        });
    }
    keys.insert(krate, None);
    let data = krate.data(db);
    let mut hasher = Hasher::new("blaze manifest crate 1");
    hasher.field(content::Contents::version(db, krate).as_bytes());
    hasher.field(
        format!(
            "{:?} {:?} {:?} {:?}",
            data.edition, data.target_kind, data.crate_attrs, data.origin
        )
        .as_bytes(),
    );
    let root = crate::crate_root_file(db, krate.into());
    hasher.field(root.as_deref().map(|it| it.as_os_str().as_encoded_bytes()).unwrap_or_default());
    let mut cfgs: Vec<String> =
        krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
    cfgs.sort_unstable();
    hasher.field(cfgs.join("\n").as_bytes());
    let env = krate.env(db);
    let mut vars: Vec<String> = env.iter().map(|(name, value)| format!("{name}={value}")).collect();
    vars.sort_unstable();
    hasher.field(vars.join("\n").as_bytes());
    // A fetched crate is its version and place, as the store names it; one of the checkout,
    // or one a build script wrote into, is what its files say.
    let manifest = env.get("CARGO_MANIFEST_DIR");
    let out = env.get("OUT_DIR");
    let local = data.origin.is_local();
    let source_root = db.file_source_root(data.root_file_id).source_root_id(db);
    let source_root = db.source_root(source_root).source_root(db);
    let mut files: Vec<(String, Hash)> = Vec::new();
    for file in source_root.iter() {
        let Some(path) = source_root.path_for_file(&file).and_then(|it| it.as_path()) else {
            continue;
        };
        let path = path.to_string();
        let of_package = local && manifest.as_deref().is_some_and(|it| path.starts_with(it));
        let of_script = out.as_deref().is_some_and(|it| path.starts_with(it));
        if of_package || of_script {
            let hash = content::file_hash(db, file, Path::new(&path));
            files.push((path, hash));
        }
    }
    files.sort_unstable();
    for (path, hash) in &files {
        hasher.field(path.as_bytes());
        hasher.field(hash);
    }
    let mut dependencies: Vec<(String, Crate)> =
        data.dependencies.iter().map(|it| (it.name.to_string(), it.crate_id)).collect();
    dependencies.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (name, on) in dependencies {
        let key = crate_key(db, on, keys);
        hasher.field(name.as_bytes());
        hasher.field(&key);
    }
    let key = hasher.finish();
    keys.insert(krate, Some(key));
    key
}

/// The key of the program `args` builds, when it is a build a manifest can stand for: one of
/// the daemon's own, with nothing of rustc's and none of the outputs only a harness asks for.
pub(crate) fn key(db: &RootDatabase, args: &Args, table_is_empty: bool) -> Option<Hash> {
    let eligible = table_is_empty
        && args.symbol_files.is_empty()
        && args.report.is_none()
        && args.deny.is_none()
        && args.isolate.is_none()
        && args.also.is_empty()
        && !args.library;
    if !eligible {
        return None;
    }
    let mut hasher = Hasher::new("blaze manifest 1");
    let path = |it: &Option<PathBuf>| {
        it.as_deref().map(|it| it.as_os_str().as_encoded_bytes().to_vec()).unwrap_or_default()
    };
    for krate in args.crates() {
        hasher.field(krate.as_bytes());
    }
    hasher.field(args.out.as_os_str().as_encoded_bytes());
    hasher.field(&path(&args.patch));
    hasher.field(&path(&args.splice));
    hasher.field(&path(&args.exe));
    hasher.field(&path(&args.dep_info));
    hasher.field(args.project.as_os_str().as_encoded_bytes());
    hasher.field(args.checkout.as_os_str().as_encoded_bytes());
    hasher.field(&path(&args.base));
    hasher.field(&[
        args.shared as u8,
        args.staticlib as u8,
        args.cfg_test as u8,
        args.own_target as u8,
        args.unit_test as u8,
    ]);
    for told in [&args.executables, &args.scratch] {
        hasher.field(told.as_ref().map(|it| it.as_str().as_bytes()).unwrap_or_default());
    }
    hasher.field(serde_json::to_string(&args.selection).ok()?.as_bytes());
    hasher.field(format!("{:?}", args.debug).as_bytes());
    hasher.field(hir_ty_codegen::Target::selected().ok()?.triple().as_bytes());
    // The standard library's unit the program is linked over names what the program calls of it.
    match &args.prebuilt {
        Some(unit) => hasher.field(&Sha256::digest(std::fs::read(unit).ok()?)),
        None => hasher.field(b""),
    }
    let mut vars: Vec<(String, String)> =
        std::env::vars().filter(|(it, _)| configures(it)).collect();
    vars.sort_unstable();
    for (name, value) in vars {
        hasher.field(name.as_bytes());
        hasher.field(value.as_bytes());
    }
    let mut keys = FxHashMap::default();
    let mut crates: Vec<(String, Hash)> = ide_db::base_db::all_crates(db)
        .iter()
        .map(|&krate| {
            let key = crate_key(db, krate, &mut keys);
            (content::Contents::version(db, krate), key)
        })
        .collect();
    crates.sort_unstable();
    for (_, key) in crates {
        hasher.field(&key);
    }
    Some(hasher.finish())
}

/// What a build read that the key does not say: the variables `env!` and `option_env!` read
/// of the process, and the files `include_bytes!` and `include_str!` read, with what each was.
#[derive(PartialEq, Eq)]
struct Reads {
    vars: Vec<(String, Option<String>)>,
    files: Vec<(String, Option<Hash>)>,
}

impl Reads {
    fn of(db: &RootDatabase) -> Reads {
        let mut vars: Vec<(String, Option<String>)> = db
            .env_read()
            .into_iter()
            .filter(|(krate, name)| !krate.env(db).contains_key(name))
            .map(|(_, name)| {
                let value = ide_db::base_db::process_env_var(db, &name);
                (name, value)
            })
            .collect();
        vars.sort_unstable();
        vars.dedup();
        let mut files: Vec<(String, Option<Hash>)> = ide_db::base_db::all_crates(db)
            .iter()
            .flat_map(|&krate| db.included_by(krate))
            .map(|(path, bytes)| (path.to_string(), bytes.map(|it| Sha256::digest(&it[..]).into())))
            .collect();
        files.sort_unstable();
        files.dedup();
        Reads { vars, files }
    }

    /// Whether the process and the files are still what they were read as.
    fn hold(&self, db: &RootDatabase) -> Result<(), String> {
        for (name, value) in &self.vars {
            if ide_db::base_db::process_env_var(db, name) != *value {
                return Err(format!("`{name}`, which `env!` read, is not what it was"));
            }
        }
        for (path, hash) in &self.files {
            let now = std::fs::read(path).ok().map(|it| Sha256::digest(&it).into());
            if now != *hash {
                return Err(format!("{path}, which an `include_bytes!` read, is not what it was"));
            }
        }
        Ok(())
    }
}

/// A file a build wrote: where, relative to the directory of its object when it is in it, and
/// the key of what is in it.
struct Written {
    path: String,
    content: Hash,
}

/// The manifest of a build: what it read beside its key, and the files it wrote.
struct Manifest {
    reads: Reads,
    files: Vec<Written>,
}

const MAGIC: &[u8] = b"blaze manifest 1\n";

struct Encoder(Vec<u8>);

impl Encoder {
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.0.extend_from_slice(bytes);
    }

    fn count(&mut self, count: usize) {
        self.0.extend_from_slice(&(count as u64).to_le_bytes());
    }

    fn option(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            Some(bytes) => {
                self.0.push(1);
                self.bytes(bytes);
            }
            None => self.0.push(0),
        }
    }
}

struct Decoder<'a>(&'a [u8]);

impl<'a> Decoder<'a> {
    fn count(&mut self) -> Option<usize> {
        let (it, rest) = self.0.split_first_chunk::<8>()?;
        self.0 = rest;
        usize::try_from(u64::from_le_bytes(*it)).ok()
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.count()?;
        let (it, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(it)
    }

    fn string(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }

    fn hash(&mut self) -> Option<Hash> {
        self.bytes()?.try_into().ok()
    }

    fn option(&mut self) -> Option<Option<&'a [u8]>> {
        let (&tag, rest) = self.0.split_first()?;
        self.0 = rest;
        match tag {
            0 => Some(None),
            1 => Some(Some(self.bytes()?)),
            _ => None,
        }
    }
}

impl Manifest {
    fn encode(&self) -> Vec<u8> {
        let mut out = Encoder(MAGIC.to_vec());
        out.count(self.reads.vars.len());
        for (name, value) in &self.reads.vars {
            out.bytes(name.as_bytes());
            out.option(value.as_deref().map(str::as_bytes));
        }
        out.count(self.reads.files.len());
        for (path, hash) in &self.reads.files {
            out.bytes(path.as_bytes());
            out.option(hash.as_ref().map(|it| &it[..]));
        }
        out.count(self.files.len());
        for file in &self.files {
            out.bytes(file.path.as_bytes());
            out.bytes(&file.content);
        }
        out.0
    }

    fn decode(bytes: &[u8]) -> Option<Manifest> {
        let mut input = Decoder(bytes.strip_prefix(MAGIC)?);
        let mut vars = Vec::new();
        for _ in 0..input.count()? {
            let name = input.string()?;
            let value = match input.option()? {
                Some(it) => Some(String::from_utf8(it.to_vec()).ok()?),
                None => None,
            };
            vars.push((name, value));
        }
        let mut files = Vec::new();
        for _ in 0..input.count()? {
            let path = input.string()?;
            let hash = match input.option()? {
                Some(it) => Some(it.try_into().ok()?),
                None => None,
            };
            files.push((path, hash));
        }
        let mut written = Vec::new();
        for _ in 0..input.count()? {
            written.push(Written { path: input.string()?, content: input.hash()? });
        }
        input.0.is_empty().then_some(Manifest { reads: Reads { vars, files }, files: written })
    }
}

/// The key of a file's content in the store.
fn content_key(content: &Hash) -> Hash {
    let mut hasher = Hasher::new("blaze manifest file 1");
    hasher.field(content);
    hasher.finish()
}

/// Where a file a manifest names is: relative to the directory of the object of the build.
fn place(args: &Args, path: &str) -> PathBuf {
    match args.out.parent() {
        Some(dir) if Path::new(path).is_relative() => dir.join(path),
        _ => PathBuf::from(path),
    }
}

fn named(args: &Args, path: &Path) -> String {
    let inside = args.out.parent().and_then(|dir| path.strip_prefix(dir).ok());
    inside.unwrap_or(path).to_string_lossy().into_owned()
}

/// The files of the program the store has a manifest of under `key`, read and checked: what
/// to write where, or why there are none.
pub(crate) fn lookup(
    db: &RootDatabase,
    args: &Args,
    key: &Hash,
) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let store = blaze_store::Store::global().ok_or("there is no store")?;
    let bytes = store.get(key).ok_or("the store has none of this build")?;
    let manifest = Manifest::decode(&bytes).ok_or("the store's is not one this reads")?;
    manifest.reads.hold(db)?;
    let mut files = Vec::with_capacity(manifest.files.len());
    for file in &manifest.files {
        let bytes = store
            .get(&content_key(&file.content))
            .ok_or_else(|| format!("the store does not have {}", file.path))?;
        if <Hash>::from(Sha256::digest(&bytes)) != file.content {
            return Err(format!("what the store has of {} is not what was written", file.path));
        }
        files.push((place(args, &file.path), bytes));
    }
    Ok(files)
}

/// What the build of `args` under `key` wrote, the files at `wrote`, read to be kept as its
/// manifest by [`Keeping::keep`].
pub(crate) fn keeping(
    db: &RootDatabase,
    args: &Args,
    key: &Hash,
    wrote: &[PathBuf],
) -> Result<Keeping, String> {
    let store = blaze_store::Store::global().ok_or("there is no store")?;
    let mut files = Vec::with_capacity(wrote.len());
    for path in wrote {
        // Read and not mapped: the next build may write the file again, in place, while
        // this is kept.
        let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        files.push((named(args, path), bytes));
    }
    Ok(Keeping { store, key: *key, reads: Reads::of(db), files })
}

/// A manifest and the files it names, to be added to the store.
pub(crate) struct Keeping {
    store: &'static blaze_store::Store,
    key: Hash,
    reads: Reads,
    files: Vec<(String, Vec<u8>)>,
}

impl Keeping {
    /// Keeps it beside what the build does next, as what codegen kept is: the next full
    /// build and the end of the process wait for it. `then` is told how many bytes the
    /// files were and how long keeping them took.
    pub(crate) fn settle(self, then: impl FnOnce(usize, Duration) + Send + 'static) {
        self.store.settle_with(move || {
            let start = Instant::now();
            let bytes = self.keep();
            then(bytes, start.elapsed());
        });
    }

    /// Adds the files and then the manifest that names them, and returns how many bytes
    /// the files are.
    fn keep(self) -> usize {
        let Keeping { store, key, reads, files } = self;
        let mut written = Vec::with_capacity(files.len());
        let mut bytes_kept = 0;
        for (path, bytes) in files {
            let content: Hash = Sha256::digest(&bytes).into();
            store.insert(&content_key(&content), &bytes);
            bytes_kept += bytes.len();
            written.push(Written { path, content });
        }
        let manifest = Manifest { reads, files: written };
        store.insert(&key, &manifest.encode());
        store.flush();
        bytes_kept
    }
}

/// For `BLAZE_MANIFEST=check`: whether the files the full build wrote at `wrote` are those
/// the manifest under `key` has, and which are not when they are not.
pub(crate) fn check(
    db: &RootDatabase,
    args: &Args,
    key: &Hash,
    wrote: &[PathBuf],
) -> Result<bool, String> {
    let kept = match lookup(db, args, key) {
        Ok(kept) => kept,
        Err(_) => return Ok(false),
    };
    let mut differ = Vec::new();
    for (path, bytes) in &kept {
        let now = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        if let Some(why) = unlike(&now, bytes) {
            // Beside the one built, to compare the two.
            let mut beside = path.clone().into_os_string();
            beside.push(".manifest");
            _ = std::fs::write(&beside, bytes);
            differ.push(format!("{} ({why})", path.display()));
        }
    }
    let names: Vec<&PathBuf> = kept.iter().map(|it| &it.0).collect();
    differ.extend(wrote.iter().filter(|it| !names.contains(it)).map(|it| it.display().to_string()));
    match differ.is_empty() {
        true => Ok(true),
        false => Err(format!(
            "the manifest of this build has other files than the build wrote: {}",
            differ.join(", ")
        )),
    }
}

/// How a file a full build wrote is not the manifest's, or `None` when it is the same program.
/// Two builds of one checkout lay a program out in the order their threads finished, and number
/// what they name themselves in that order: an object is compared by what it defines (each
/// section's kind, bytes and relocations, and the global symbols), with a local symbol named
/// without its number and the debug info left out. An executable is linked from those and laid
/// out by them, and is not compared. Anything else is compared byte for byte.
fn unlike(built: &[u8], kept: &[u8]) -> Option<String> {
    use object::{Object, ObjectKind};
    if built == kept {
        return None;
    }
    let parsed = (object::File::parse(built), object::File::parse(kept));
    let (Ok(built), Ok(kept)) = parsed else {
        let at = built.iter().zip(kept).position(|(a, b)| a != b);
        let at = at.unwrap_or(built.len().min(kept.len()));
        return Some(format!(
            "{} bytes, {} kept, first difference at {at}",
            built.len(),
            kept.len()
        ));
    };
    match (built.kind(), kept.kind()) {
        (
            ObjectKind::Executable | ObjectKind::Dynamic,
            ObjectKind::Executable | ObjectKind::Dynamic,
        ) => None,
        (ObjectKind::Relocatable, ObjectKind::Relocatable) => {
            let (built, kept) = (defined(&built), defined(&kept));
            let only = |one: &[String], other: &[String]| -> Vec<String> {
                let only = one.iter().filter(|it| other.binary_search(it).is_err());
                only.map(|it| it.chars().take(200).collect()).collect()
            };
            let (extra, missing) = (only(&built, &kept), only(&kept, &built));
            (!extra.is_empty() || !missing.is_empty()).then(|| {
                let shown = |it: &[String]| it.iter().take(4).cloned().collect::<Vec<_>>().join("; ");
                format!(
                    "{} defined that the kept one has not, as {}; {} it has that this has not, as {}",
                    extra.len(),
                    shown(&extra),
                    missing.len(),
                    shown(&missing)
                )
            })
        }
        (built, kept) => Some(format!("a {built:?} object, {kept:?} kept")),
    }
}

/// What an object defines, sorted, each a line: its sections that are loaded, by kind, size,
/// bytes and relocations, and its global symbols.
fn defined(file: &object::File<'_>) -> Vec<String> {
    use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget, SectionKind, SymbolScope};
    let loaded = |kind: SectionKind| {
        !matches!(
            kind,
            SectionKind::Debug
                | SectionKind::DebugString
                | SectionKind::Metadata
                | SectionKind::Linker
                | SectionKind::Note
                | SectionKind::Other
                | SectionKind::OtherString
        )
    };
    let content = |section: &object::Section<'_, '_>| -> String {
        let digest: Hash = Sha256::digest(section.data().unwrap_or_default()).into();
        format!("{:?} {} {}", section.kind(), section.size(), hex(&digest))
    };
    // A symbol a build numbered (`blaze::<name>::<n>`, `.L<n>`), or a local one, is told by
    // the bytes of the section it is in, which say what it is however it was numbered.
    let named = |symbol: &object::Symbol<'_, '_>| {
        let name = symbol.name().unwrap_or("");
        let numbered = name.starts_with(".L")
            || name.trim_end_matches(|it: char| it.is_ascii_digit()).ends_with("::")
            || name.trim_end_matches(|it: char| it.is_ascii_digit()).ends_with("::n");
        (symbol.is_undefined() || (symbol.scope() != SymbolScope::Compilation && !numbered))
            .then(|| name.to_owned())
    };
    // What a relocation points at.
    let target = |relocation: &object::Relocation| match relocation.target() {
        RelocationTarget::Symbol(index) => match file.symbol_by_index(index) {
            Ok(symbol) => named(&symbol).unwrap_or_else(|| {
                let section = symbol.section_index().and_then(|it| file.section_by_index(it).ok());
                let base = section.as_ref().map_or(0, |it| it.address());
                let place = section.map_or_else(String::new, |it| content(&it));
                format!("({place})+{:x}", symbol.address() - base)
            }),
            Err(_) => "?".to_owned(),
        },
        RelocationTarget::Section(index) => file
            .section_by_index(index)
            .map_or_else(|_| "?".to_owned(), |it| format!("({})", content(&it))),
        RelocationTarget::Absolute => "absolute".to_owned(),
        _ => "other".to_owned(),
    };
    let mut lines = Vec::new();
    for section in file.sections().filter(|it| loaded(it.kind())) {
        // The unwind table has a record of each function in the order they were laid out:
        // what it covers is compared, which its records are made of.
        let unwind = matches!(section.name(), Ok(".eh_frame" | "__eh_frame" | "__compact_unwind"));
        let mut relocations: Vec<String> = section
            .relocations()
            .map(|(at, it)| {
                let at = if unwind { String::new() } else { format!("{at:x} ") };
                format!("{at}{:?} {} {:x}", it.flags(), target(&it), it.addend())
            })
            .collect();
        relocations.sort();
        let what = match unwind {
            true => format!("unwind {}", section.size()),
            false => content(&section),
        };
        lines.push(format!("{what} [{}]", relocations.join(", ")));
    }
    let globals = file.symbols().filter(|it| it.is_definition()).filter_map(|it| named(&it));
    lines.extend(globals.map(|it| format!("symbol {it}")));
    lines.sort();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_reads_back_as_written() {
        let manifest = Manifest {
            reads: Reads {
                vars: vec![("A".to_owned(), Some("1".to_owned())), ("B".to_owned(), None)],
                files: vec![("/x".to_owned(), Some([7; 32])), ("/y".to_owned(), None)],
            },
            files: vec![Written { path: "prog.o".to_owned(), content: [3; 32] }],
        };
        let bytes = manifest.encode();
        let back = Manifest::decode(&bytes).expect("decodes");
        assert!(back.reads == manifest.reads);
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].path, "prog.o");
        assert_eq!(back.files[0].content, [3; 32]);
        assert!(Manifest::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(Manifest::decode(&[bytes.clone(), vec![0]].concat()).is_none());
    }

    #[test]
    fn variables_of_the_store_and_the_log_do_not_change_the_key() {
        assert!(configures("BLAZE_OPT"));
        assert!(configures("RUSTFLAGS"));
        assert!(configures("CARGO_PROFILE_RELEASE_DEBUG"));
        assert!(!configures("BLAZE_STATS"));
        assert!(!configures("BLAZE_CACHE_MAX"));
        assert!(!configures("BLAZE_REMOTE"));
        assert!(!configures("PATH"));
    }

    /// An object of a `main` that calls each of `functions`, local ones named by their place
    /// in that order, as a build numbers what it compiles in the order its threads finish.
    fn object_of(functions: &[&[u8]]) -> Vec<u8> {
        use object::{
            Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind,
            SymbolScope,
            write::{Object, Relocation, Symbol, SymbolSection},
        };
        let mut object = Object::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
        let local = |object: &mut Object<'_>, at: usize, code: &[u8]| {
            let section = object.add_subsection(object::write::StandardSection::Text, b"f");
            let offset = object.append_section_data(section, code, 4);
            object.add_symbol(Symbol {
                name: format!("blaze::f::n{at}").into_bytes(),
                value: offset,
                size: code.len() as u64,
                kind: SymbolKind::Text,
                scope: SymbolScope::Compilation,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            })
        };
        let called: Vec<_> =
            functions.iter().enumerate().map(|(at, code)| local(&mut object, at, code)).collect();
        let main = object.add_subsection(object::write::StandardSection::Text, b"main");
        object.append_section_data(main, &vec![0; 4 * called.len()], 4);
        object.add_symbol(Symbol {
            name: b"main".to_vec(),
            value: 0,
            size: 4 * called.len() as u64,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(main),
            flags: SymbolFlags::None,
        });
        // `main` calls them in the order of their code, not of their numbers.
        let mut order: Vec<usize> = (0..functions.len()).collect();
        order.sort_by_key(|&it| functions[it]);
        for (at, &it) in order.iter().enumerate() {
            let relocation = Relocation {
                offset: 4 * at as u64,
                symbol: called[it],
                addend: 0,
                flags: RelocationFlags::Elf { r_type: object::elf::R_AARCH64_CALL26 },
            };
            object.add_relocation(main, relocation).expect("relocation");
        }
        object.write().expect("object")
    }

    #[test]
    fn objects_alike_but_for_order_and_numbers_are_the_same_program() {
        let (one, two): (&[u8], &[u8]) = (&[1, 0, 0, 0], &[2, 0, 0, 0]);
        let first = object_of(&[one, two]);
        let again = object_of(&[two, one]);
        assert_ne!(first, again);
        assert_eq!(unlike(&first, &again), None);
        let other = object_of(&[one, &[3, 0, 0, 0]]);
        assert!(unlike(&first, &other).is_some());
        assert!(unlike(b"text", b"texts").is_some());
        assert_eq!(unlike(b"text", b"text"), None);
    }
}
