//! `blaze store export`, `blaze store import` and `blaze store install`: packs out of and
//! into this home's store (docs/remote-cache.md, "Packs"). Lane packs-1112.
//!
//! ```text
//! blaze store export [--all] [--raw] --pin <name> --out <dir> [--sign-key <file>]
//! blaze store import <dir or http(s) base> [--pin <name>]... [--unsigned] [--trust <key>]...
//! blaze store install [--from <dir or http(s) base>] [--no-prebuilt] [--lock <Cargo.lock>]
//! blaze store cover [--lock <Cargo.lock>] [<crate directory>]...
//! ```
//!
//! - `export` writes the records the store's pin `<name>` keeps (`toolchain`: the standard
//!   library's; `bstd`; any pin) as one pack under `<dir>`, which is then the directory to
//!   copy to a mirror or a release's bucket as it is. With `--all` the selection is every
//!   record of the store that may travel (everything this home's builds used), and `<name>`
//!   is the pin an importing store keeps them under. `--sign-key` names a file holding an
//!   Ed25519 secret (64 hex digits); the head is signed with it. The body is written as
//!   compressed spans (zstd; the store reads records out uncompressed, so it is their one
//!   compression), or as it is with `--raw`. Nothing is uploaded: that
//!   is someone's own command, with the owner's word.
//! - `import` reads the packs of this blaze's scope at a base and puts their records into
//!   the store, pinned under the manifest's pin before the first is put and each checked
//!   against its manifest before it is kept. A head must be signed by a key `packs.json`
//!   trusts or `--trust` names; `--unsigned` takes a head no one signed, on the user's word.
//! - `install` installs the sealed layer of each tree the toolchain ships (`serve::sealed`)
//!   once a machine, never into a home: from `--from`, else `packs.json`'s `install` and
//!   `sources`, then the engine's own `share/blaze/sealed`, by the cover's pin, signed heads
//!   only. `--no-prebuilt` (or `"prebuilt": false`) asks no base. It also runs by itself the
//!   first time a command starts a daemon on a home that has no store yet ([`first_use`]):
//!   no address is compiled in. With `--lock`, the layers of the registry packages that lock
//!   file pins at a checksum too, by the same rule and code ([`registry`]).
//! - `cover` prints each shipped tree's pin (`sealed-<hex>`, what a publisher exports under),
//!   its directory (where a package names a shipped bstd) and whether its layer is installed,
//!   with `--lock` each registry package's of that lock file too; with directories, whether a crate there is read from a
//!   layer or built from source.
//!
//! With a daemon on the home, which holds the store, the command is asked of it
//! ([`blaze_api::Request::Store`]) and run there over its store; with none, it is run here
//! over the store opened by this process. Either way it is [`run`], over
//! `serve::packs::stored::Records`. This file reads the words and prints.

use std::path::{Path, PathBuf};

use blaze_core::exit::Exit;
use blaze_pack::{Id, Trust, head::Vouched};

use super::say;
use crate::serve::sealed;
use crate::serve::packs::{
    self, Base, Config,
    stored::{self, Records},
};

const USAGE_TEXT: &str = "blaze store export [--all] [--raw] --pin <name> --out <dir> [--sign-key <file>] | blaze store import <dir or http(s) base> [--pin <name>]... [--unsigned] [--trust <key>]... | blaze store install [--from <dir or http(s) base>] [--no-prebuilt] [--lock <Cargo.lock>] | blaze store cover [--lock <Cargo.lock>] [<crate directory>]...";

/// `blaze store`, with the words after it (`export ...`, `import ...`, `install ...`).
pub fn main(words: &[&str], color: bool) -> i32 {
    match asked(words) {
        Ok(out) => {
            blaze_report::out!("{out}");
            0
        }
        Err(what) if what == USAGE_TEXT => {
            blaze_report::err!("{}", say::error(USAGE_TEXT, color));
            Exit::Usage.code()
        }
        Err(what) => {
            blaze_report::err!("{}", say::error(&what, color));
            Exit::Failed.code()
        }
    }
}

/// The command `words`, run by the daemon that holds this home's store, or here when none
/// does.
fn asked(words: &[&str]) -> Result<String, String> {
    // The daemon's directory is not this command's: every path is made whole here.
    let words = whole(words)?;
    match super::daemon::connect(super::daemon::Start::Never) {
        Ok(Some(client)) => {
            drop(client);
            let mut out = String::new();
            super::observe::ask(
                blaze_api::Request::Store { words },
                super::daemon::Start::Never,
                |response| {
                    if let blaze_api::Response::Diagnostic(said) = response.into_owned() {
                        out.push_str(&said.message);
                        out.push('\n');
                    }
                    Ok(())
                },
            )?;
            Ok(out)
        }
        Ok(None) => {
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            let mut store = opened()?;
            run(&words, &mut stored::Driven::new(&mut store))
        }
        Err(error) => Err(format!("{error:#}")),
    }
}

/// `words` with each path in them absolute: the values of `--out`, `--sign-key` and `--from`
/// and an import's place, unless it is an address.
fn whole(words: &[&str]) -> Result<Vec<String>, String> {
    let absolute = |path: &str| match path.starts_with("http://") || path.starts_with("https://") {
        true => Ok(path.to_owned()),
        false => std::path::absolute(path)
            .map(|path| path.display().to_string())
            .map_err(|error| format!("{path}: {error}")),
    };
    let mut out = Vec::with_capacity(words.len());
    let mut at = 0;
    while at < words.len() {
        let word = words[at];
        out.push(word.to_owned());
        match word {
            "--out" | "--sign-key" | "--from" | "--lock" => {
                if let Some(path) = words.get(at + 1) {
                    out.push(absolute(path)?);
                    at += 1;
                }
            }
            "--pin" | "--trust" => {
                if let Some(value) = words.get(at + 1) {
                    out.push((*value).to_owned());
                    at += 1;
                }
            }
            place if at > 0 && matches!(words[0], "import" | "cover") && !place.starts_with('-') => {
                out.pop();
                out.push(absolute(place)?);
            }
            _ => {}
        }
        at += 1;
    }
    Ok(out)
}

fn usage<T>() -> Result<T, String> {
    Err(USAGE_TEXT.to_owned())
}

/// The value after a flag.
fn value<'a>(words: &mut std::slice::Iter<'a, &'a str>) -> Result<&'a str, String> {
    words.next().copied().ok_or_else(|| USAGE_TEXT.to_owned())
}

/// This home's store, opened here: no daemon is on the home ([`asked`] looked).
fn opened() -> Result<blaze_cas::Driven, String> {
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    // Pages are loaded as they are read, within the pool's budget: a store is not read
    // whole into memory to export a pin of it.
    blaze_cas::Driven::open_with(&state, blaze_cas::Loader::Thread { root: state.clone() })
}

fn seed_at(path: &Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("the signing key {}: {error}", path.display()))?;
    Id::from_hex(text.trim())
        .map(|id| id.0)
        .ok_or_else(|| format!("the signing key {} does not hold 64 hex digits", path.display()))
}

/// The command `words` (`export ...`, `import ...`, `install ...`, every path absolute) over
/// `records`: what it did, a line each, or why not. A misuse is [`USAGE_TEXT`].
pub(crate) fn run(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    match words.split_first() {
        Some((&"export", rest)) => export(rest, records),
        Some((&"import", rest)) => import(rest, records),
        Some((&"install", rest)) => install(rest, records),
        Some((&"cover", rest)) => cover(rest),
        _ => usage(),
    }
}

fn export(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    let (mut pin, mut out, mut key, mut all, mut raw) = (None, None, None, false, false);
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "--pin" => pin = Some(value(&mut words)?),
            "--out" => out = Some(PathBuf::from(value(&mut words)?)),
            "--sign-key" => key = Some(PathBuf::from(value(&mut words)?)),
            "--all" => all = true,
            "--raw" => raw = true,
            _ => return usage(),
        }
    }
    let (Some(pin), Some(out)) = (pin, out) else { return usage() };
    let seed = key.as_deref().map(seed_at).transpose()?;
    let scope = packs::scope()?;
    let places = packs::home_places()?;
    let done = stored::export(records, pin, &scope, &out, seed.as_ref(), all, raw, &|bytes| places.tied(bytes))?;
    Ok(format!(
        "exported {} as pin `{pin}` for {scope}: pack {}, {} records, {} bytes, {} head, under {}{}\n",
        if all { "every record that may travel" } else { "the store's pin" },
        done.pack,
        done.records,
        done.bytes,
        if done.signed { "a signed" } else { "an unsigned" },
        out.display(),
        match (done.absent, done.held) {
            (0, 0) => String::new(),
            (absent, held) => format!(
                "; not in the pack: {absent} keys the pin names with no record in the store, {held} records that hold a directory of this machine"
            ),
        }
    ))
}

/// The packs of `pins` at `base`, imported: a line for each.
fn imported(
    records: &mut impl Records,
    base: &Base,
    pins: &[String],
    trust: &Trust,
    unsigned: bool,
) -> Result<String, String> {
    let scope = packs::scope()?;
    let mut said = String::new();
    for pin in pins {
        match stored::import(records, base, &scope, pin, trust, unsigned)? {
            None => said.push_str(&format!("{} has no pack of pin `{pin}` for {scope}\n", base.named())),
            Some(done) => said.push_str(&format!(
                "imported pin `{}` (pack {}) from {}: {} records put ({} bytes), {} already there; vouched for by {}\n",
                done.pin,
                done.pack,
                base.named(),
                done.summary.put,
                done.summary.bytes,
                done.summary.present,
                match done.vouched {
                    Vouched::Signer(key) => format!("the trusted key {}", Id(key)),
                    Vouched::User => "you (`--unsigned`)".to_owned(),
                }
            )),
        }
    }
    Ok(said)
}

fn settings() -> Result<Config, String> {
    match Config::read()? {
        Some(config) => Ok(config),
        None => Config::parse("{}"),
    }
}

fn import(words: &[&str], records: &mut impl Records) -> Result<String, String> {
    let config = settings()?;
    let (mut base, mut pins, mut unsigned) = (None, Vec::new(), false);
    let mut trusted = config.trusted.clone();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "--pin" => pins.push(value(&mut words)?.to_owned()),
            "--unsigned" => unsigned = true,
            "--trust" => {
                let key = Id::from_hex(value(&mut words)?)
                    .ok_or_else(|| "`--trust` takes a public key: 64 hex digits, lower case".to_owned())?;
                trusted.push(key.0);
            }
            flag if flag.starts_with('-') => return usage(),
            place if base.is_none() => base = Some(Base::parse(place)?),
            _ => return usage(),
        }
    }
    let Some(base) = base else { return usage() };
    if unsigned && matches!(base, Base::Url(_)) {
        return Err("`--unsigned` takes a directory you name, never an address: what comes over a network is taken only under a trusted signature".to_owned());
    }
    if pins.is_empty() {
        pins = config.pins;
    }
    imported(records, &base, &pins, &Trust::new(trusted), unsigned)
}

/// `install [--from <base>] [--no-prebuilt]`: the sealed layers of what the toolchain ships,
/// installed once a machine ([`layers`]). It writes the machine's layers, not this home's
/// store, so `records` is not read.
fn install(words: &[&str], _records: &mut impl Records) -> Result<String, String> {
    let config = settings()?;
    let (mut from, mut prebuilt, mut lock) = (None, config.prebuilt, None);
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "--from" => from = Some(Base::parse(value(&mut words)?)?),
            "--no-prebuilt" => prebuilt = false,
            "--lock" => lock = Some(PathBuf::from(value(&mut words)?)),
            _ => return usage(),
        }
    }
    let which = match &lock {
        Some(lock) => registry(lock)?,
        None => shipped()?,
    };
    let mut said = placed_sources(&which.1)?;
    said.extend(layers(&which, &bases(&config, from)?, prebuilt, &config.trust())?);
    Ok(said.join(""))
}

/// Each shipped tree of `placed` that is not on this machine yet, placed once a machine
/// (`toolchain::library::place`, from the release this blaze is): where a package names it
/// (`--bstd <dir>/bstd/crates/blaze-std` for a shipped bstd) and what a miss of its layer is
/// built from. A line for each placed now. A tree that is not under the machine's directory (a
/// registry package, unpacked by its home's fetch; a library this process reads from beside
/// its program) is left as it is.
fn placed_sources(placed: &[sealed::Placed]) -> Result<Vec<String>, String> {
    let machine = crate::serve::machine().map_err(|error| format!("{error:#}"))?;
    let mut said = Vec::new(); // alloc: once a command, a line a tree placed
    for it in placed {
        let (Some(name), Some(digest)) = (it.dir.parent().and_then(Path::file_name), it.dir.file_name()) else { continue };
        let name = name.to_string_lossy();
        if !it.dir.starts_with(&machine) || it.dir.join(&*name).exists() {
            continue;
        }
        let at = crate::toolchain::library::place(&name, &digest.to_string_lossy(), None)?;
        said.push(format!("placed the sealed tree {} at {}\n", sealed::pack_pin(it.cover), at.display()));
    }
    Ok(said)
}

/// `cover [<crate directory>]...`: the pin of each sealed tree the toolchain ships and whether
/// its layer is installed; with directories, for each whether its crate is read from a layer or
/// built from source (`serve::sealed::from_source`).
fn cover(words: &[&str]) -> Result<String, String> {
    // `--lock <Cargo.lock>`: its registry packages' trees after the shipped ones.
    let (lock, words) = match words {
        ["--lock", lock, rest @ ..] => (Some(Path::new(*lock)), rest),
        _ => (None, words),
    };
    if words.iter().any(|it| it.starts_with('-')) {
        return usage();
    }
    let (shared, placed) = match lock {
        Some(lock) => registry(lock)?,
        None => shipped()?,
    };
    let installed = |cover: sealed::Cover| sealed::writer_dir(&shared, cover).join("head").exists();
    let mut out = String::new();
    for it in &placed {
        out.push_str(&format!(
            "{} {}: {}\n",
            sealed::pack_pin(it.cover),
            it.dir.display(),
            if installed(it.cover) { "installed" } else { "not installed: its crates are built in each home that asks" }
        ));
    }
    let dirs: Vec<PathBuf> = words.iter().map(PathBuf::from).collect();
    let crates: Vec<(&str, &Path)> = words.iter().zip(&dirs).map(|(word, dir)| (*word, dir.as_path())).collect();
    let built = sealed::from_source(&crates, &placed, installed);
    for (word, dir) in &crates {
        match sealed::Placed::sealed_at(&placed, dir) {
            Some(it) if !built.contains(word) => out.push_str(&format!("{word}: read from the layer {}\n", sealed::pack_pin(it.cover))),
            Some(it) => out.push_str(&format!("{word}: built from source: the layer {} is not installed\n", sealed::pack_pin(it.cover))),
            None => out.push_str(&format!("{word}: built from source: not under a sealed tree, so its sources can change\n")),
        }
    }
    Ok(out)
}

/// The tags a layer's store asks with in this process: nothing else asks its driver.
const LAYER_TAGS: u64 = 1 << 48;

/// The trees the toolchain ships sealed, dependencies first, each with its cover, and the
/// shared directory their layers are in (`<machine>/std/<makers>`, the daemon's own). The one
/// list that names what ships: the library (`library/DIGEST`), and bstd when the release this
/// blaze is ships its tree (`blaze_new::BSTD_DIGEST`), on the library. Nothing else names
/// either: placing, covering, installing and reading a layer are one code for any tree
/// (`serve::sealed`, `toolchain::library::place`), a lock file's registry packages too
/// ([`registry`]). A bstd a package names by a path into a checkout is no tree of this list,
/// and so is built in its home like any path crate.
pub(crate) fn shipped() -> Result<(PathBuf, Vec<sealed::Placed>), String> {
    let (shared, trees) = shipped_trees()?;
    let placed = sealed::placed(&trees, crate::serve::record_makers()?.id(), host()?)?;
    Ok((shared, placed))
}

/// The host's target triple, which every sealed layer is made for.
fn host() -> Result<&'static str, String> {
    crate::toolchain::host()
        .ok_or_else(|| "this blaze does not know the host's target triple, which its sealed layers are made for".to_owned())
}

/// [`shipped`]'s trees, before their covers: the shared directory and the list.
fn shipped_trees() -> Result<(PathBuf, Vec<sealed::Tree>), String> {
    let said = |error: anyhow::Error| format!("{error:#}"); // alloc: error path
    let machine = crate::serve::machine().map_err(said)?;
    let makers = crate::serve::record_makers()?;
    let shared = machine.join("std").join(format!("{:x}", makers.id()));
    let library = crate::toolchain::library::digest();
    // Where this process reads the library from (a release's `share/blaze/library`, a
    // `BLAZE_LIBRARY`), else where it is placed once a machine.
    let dir = match crate::toolchain::library::found() {
        Ok(found) => found.dir.parent().map_or_else(|| found.dir.clone(), Path::to_path_buf),
        Err(_) => crate::toolchain::library::tree_dir("library", library)?,
    };
    // alloc: once a command, a tree a shipped one
    let mut trees = vec![sealed::Tree { dir, digest: library.to_owned(), on: Vec::new() }];
    if let Some(bstd) = blaze_new::BSTD_DIGEST {
        let dir = crate::toolchain::library::tree_dir("bstd", bstd)?;
        trees.push(sealed::Tree { dir, digest: bstd.to_owned(), on: vec![0] });
    }
    Ok((shared, trees))
}

/// The trees of [`shipped`] and, after them, the registry packages `lock` (a `Cargo.lock`)
/// pins at a checksum, each on the library and on the packages it depends on, unpacked where
/// this home's fetch puts them (`<sources>/registry/src/<registry>/<name>-<version>`, checked
/// against the checksum as it was unpacked). A package not unpacked here, or unpacked under
/// two registries, is the error naming it: which tree it is is not guessed.
pub(crate) fn registry(lock: &Path) -> Result<(PathBuf, Vec<sealed::Placed>), String> {
    let (shared, mut trees) = shipped_trees()?;
    let text = std::fs::read_to_string(lock).map_err(|error| format!("{}: {error}", lock.display()))?;
    let locked = locked_packages(&text).map_err(|why| format!("{}: {why}", lock.display()))?;
    let sources = crate::serve::sources().map_err(|error| format!("{error:#}"))?.join("registry/src");
    let registries: Vec<PathBuf> = match std::fs::read_dir(&sources) {
        Ok(entries) => entries.filter_map(|it| it.ok().map(|it| it.path())).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("{}: {error}", sources.display())),
    };
    let dir_of = |it: &sealed::Locked| {
        let leaf = format!("{}-{}", it.name, it.version);
        let mut found = registries.iter().map(|registry| registry.join(&leaf)).filter(|it| it.join("Cargo.toml").is_file());
        match (found.next(), found.next()) {
            (Some(one), None) => Ok(one),
            (None, _) => Err(format!(
                "{} {} (checksum {}) is not unpacked under {}: a build of the lock file's workspace fetches it",
                it.name,
                it.version,
                it.checksum,
                sources.display()
            )),
            (Some(one), Some(two)) => Err(format!(
                "{} {} is unpacked under two registries, {} and {}: which one the lock file's checksum is of is not guessed",
                it.name,
                it.version,
                one.display(),
                two.display()
            )),
        }
    };
    let first = trees.len();
    trees.extend(sealed::registry(&locked, &[0], first, dir_of)?);
    let placed = sealed::placed(&trees, crate::serve::record_makers()?.id(), host()?)?;
    Ok((shared, placed))
}

/// The packages of a lock file's text that it pins at a checksum, with the dependencies it
/// lists for each.
fn locked_packages(text: &str) -> Result<Vec<sealed::Locked>, String> {
    #[derive(serde::Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
        version: String,
        checksum: Option<String>,
        #[serde(default)]
        dependencies: Vec<String>,
    }
    let lock: Lock = toml::from_str(text).map_err(|error| format!("not a lock file: {error}"))?;
    let pinned = lock.package.into_iter().filter_map(|it| {
        let checksum = it.checksum?;
        Some(sealed::Locked { name: it.name, version: it.version, checksum, dependencies: it.dependencies })
    });
    Ok(pinned.collect())
}

/// Where the sealed layers are asked for, in order: `--from` alone when given; else the
/// settings' `install`, then their `sources`; then the engine's own `../share/blaze/sealed`.
fn bases(config: &Config, from: Option<Base>) -> Result<Vec<Base>, String> {
    let mut bases = Vec::new(); // alloc: once a command, a few bases
    match from {
        Some(from) => bases.push(from),
        None => {
            // clone: once a command, the settings' bases
            for base in config.install.iter().chain(&config.sources) {
                if !bases.contains(base) {
                    bases.push(base.clone()); // clone: once a command
                }
            }
        }
    }
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| format!("finding blaze's own program, beside which its sealed packs can be: {error}"))?;
    if let Some(share) = exe.parent().and_then(Path::parent) {
        bases.push(Base::Dir(share.join("share/blaze/sealed")));
    }
    Ok(bases)
}

/// The layer of each tree the toolchain ships, installed from `bases` when it is not yet
/// ([`sealed::Install`]): a line for each tree installed now; none for one installed already;
/// one at info for one no base had (its crates are built in the home that asks). A base that
/// refuses is the error, naming it and why (AGENTS.md rule 3).
fn layers(
    (shared, placed): &(PathBuf, Vec<sealed::Placed>),
    bases: &[Base],
    prebuilt: bool,
    trust: &Trust,
) -> Result<Vec<String>, String> {
    let store = shared.join("store");
    std::fs::create_dir_all(&store).map_err(|error| format!("making the sealed layers' directory {}: {error}", store.display()))?;
    // Held while layers are written: no collector renames the shared directory away meanwhile.
    let _lease = blaze_gc::lease::Lease::hold(shared).map_err(|why| format!("the sealed layers' directory: {why}"))?;
    let scope = packs::scope()?;
    let mut said = Vec::new(); // alloc: once a command, a line a tree
    for it in placed {
        let pin = sealed::pack_pin(it.cover);
        // clone: once a tree, the bases and the directory for its install to own
        let mut install = sealed::Install::new(it.cover, shared.clone(), bases.to_vec(), prebuilt);
        let mut lock = None;
        let mut step = install.step(None);
        // An install ends within two stats, a lock and one import a base.
        let mut ended = None;
        for _ in 0..4 + 2 * bases.len() {
            let outcome = match step {
                sealed::Step::Stat { head } => sealed::Outcome::Head(head.exists()),
                sealed::Step::Lock { at } => {
                    let _span = tracing::info_span!(target: "blaze", "sealed install: wait for the cover's lock", %pin).entered();
                    let file = std::fs::File::create(&at).map_err(|error| format!("{}: {error}", at.display()))?;
                    lock = Some(stdx::file_lock::Locked::lock(file).map_err(|error| format!("locking {}: {error}", at.display()))?);
                    sealed::Outcome::Locked
                }
                sealed::Step::Import { base, pin } => {
                    let _span = tracing::info_span!(target: "blaze", "sealed install: import", %pin, from = %base.named()).entered();
                    match imported_into_layer(shared, it.cover, &base, &scope, &pin, trust) {
                        Ok(None) => sealed::Outcome::Absent,
                        Ok(Some(done)) => sealed::Outcome::Imported(Ok((u64::from(done.summary.put), done.summary.bytes))),
                        Err(why) => sealed::Outcome::Imported(Err(why)),
                    }
                }
                sealed::Step::Done(installed) => {
                    ended = Some(installed);
                    break;
                }
            };
            step = install.step(Some(outcome));
        }
        drop(lock);
        match ended {
            Some(sealed::Installed::Already) => {}
            Some(sealed::Installed::Now { records, bytes }) => {
                said.push(format!("installed the sealed layer {pin} of {}: {records} records, {bytes} bytes\n", it.dir.display()));
            }
            Some(sealed::Installed::NoPack { looked }) => {
                let looked = if looked.is_empty() { "none: prebuilt records are off".to_owned() } else { looked.join(", ") };
                tracing::info!(target: "blaze", %pin, %looked, "no pack of the sealed layer: its crates are built in the home that asks");
            }
            Some(sealed::Installed::Refused { base, why }) => {
                return Err(format!("the sealed layer {pin} of {} from {base}: {why}", it.dir.display()));
            }
            None => return Err(format!("the sealed layer {pin}'s install did not end in its bound of steps")),
        }
    }
    Ok(said)
}

/// The pack of `pin` at `base`, imported into the layer of `cover` (its own writer directory
/// of `shared`'s store), committed: none when the base has no pack of it. Every record is
/// checked against the signed manifest before it is put (`stored::import`): a record whose
/// bytes do not hash to its entry is the error, naming the base and the record's key, and the
/// layer is not committed.
pub(crate) fn imported_into_layer(
    shared: &Path,
    cover: sealed::Cover,
    base: &Base,
    scope: &str,
    pin: &str,
    trust: &Trust,
) -> Result<Option<stored::Imported>, String> {
    let mut layer = Layer::open(shared, cover)?;
    let done = stored::import(&mut stored::Driven::new(&mut layer), base, scope, pin, trust, false)?;
    stored::Driving::flush(&mut layer)?;
    Ok(done)
}

/// A sealed layer's store, written by this process alone (the cover's lock is held), on a
/// native driver over the shared directory.
struct Layer {
    driver: blaze_core::Native,
    store: blaze_cas::Store,
}

impl Layer {
    fn open(shared: &Path, cover: sealed::Cover) -> Result<Layer, String> {
        let mut driver = blaze_core::native(shared, 2)
            .map_err(|error| format!("the native driver over {}: {error}", shared.display()))?;
        let config = blaze_cas::SharedConfig {
            root: blaze_core::FileName::new("store").expect("a relative name"),
            writer: sealed::writer(cover),
            tags: LAYER_TAGS,
            load_tags: LAYER_TAGS + (1 << 32),
            loader: blaze_cas::Loader::Thread { root: shared.to_owned() },
        };
        let mut effects = Vec::new(); // alloc: once a layer opened
        let mut store = blaze_cas::Store::open_shared(config, &mut effects);
        drive(&mut driver, &mut store, &mut effects)?;
        if let Some(broken) = store.broken() {
            return Err(format!("the sealed layer {}: {broken}", sealed::writer_dir(shared, cover).display()));
        }
        if !store.ready() {
            return Err(format!(
                "the sealed layer {} did not open: its driver went idle first",
                sealed::writer_dir(shared, cover).display()
            ));
        }
        Ok(Layer { driver, store })
    }
}

fn drive(driver: &mut blaze_core::Native, store: &mut blaze_cas::Store, effects: &mut Vec<blaze_core::Effect>) -> Result<(), String> {
    blaze_core::run_until_idle(driver, effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    })
    .map_err(|stalled| format!("the sealed layer's driver: {stalled}"))
}

impl stored::Driving for Layer {
    fn store(&mut self) -> &mut blaze_cas::Store {
        &mut self.store
    }

    fn flush(&mut self) -> Result<(), String> {
        let mut effects = Vec::new(); // alloc: once a flush
        self.store.flush(&mut effects);
        drive(&mut self.driver, &mut self.store, &mut effects)?;
        match self.store.broken() {
            Some(broken) => Err(broken.to_string()),
            None => Ok(()),
        }
    }
}

/// The first use of a home: called as a command is about to start a daemon on a home that has
/// no store yet. The sealed layers of what the toolchain ships are installed once a machine
/// ([`layers`]): a second home finds each by one `exists` and imports nothing. Nothing is put
/// into this home's store: its daemon reads the layers in place behind its own records.
///
/// A base that is set and refused is this command's error, naming it: the user set it, so a
/// first build that silently made the library from source instead would hide that it is down
/// (AGENTS.md rule 3). No base having the pack is a miss: said at info, and the crates are
/// built in the home that asks. `prebuilt` false (the command's `--no-prebuilt`) installs
/// nothing from a pack, as packs.json's `"prebuilt": false` does for every command.
pub fn first_use(prebuilt: bool) -> Result<(), String> {
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    if state.join("store").exists() {
        return Ok(());
    }
    let config = settings()?;
    let _span = tracing::info_span!(target: "blaze", "first use: install the sealed layers").entered();
    let said = layers(&shipped()?, &bases(&config, None)?, config.prebuilt && prebuilt, &config.trust())
        .map_err(|why| format!("installing the sealed layers on this home's first use: {why}"))?;
    for line in said {
        blaze_report::err!("{}", line.trim_end());
    }
    Ok(())
}
