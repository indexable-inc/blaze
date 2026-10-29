//! What a crate of a checkout is, by what is written in it and the identity it is built
//! with: two checkouts with the same sources and identities have the same crate, and what
//! was compiled for one is the code of the other.

pub(crate) use blaze_cargo::content::hex;

use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

use hir_ty_codegen::{CARGO_HOME, ContentId, LibrarySources, moved};
use ide_db::{
    RootDatabase,
    base_db::{Crate, SourceDatabase as _},
};
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use triomphe::Arc;

type Hash = [u8; 32];

/// The crates of one build. An edit makes another crate of the one it is in and of every
/// crate of the checkout that depends on it.
#[derive(Default)]
pub(crate) struct Contents {
    /// Each crate is hashed once: the threads that ask while it is wait for it, rather than
    /// each hashing it and every crate it depends on again.
    crates: Mutex<FxHashMap<Crate, std::sync::Arc<OnceLock<Option<ContentId>>>>>,
    /// Where the build scripts of the crates hashed so far wrote to.
    places: Mutex<FxHashSet<String>>,
    /// Where cargo keeps the sources it fetched, once the crates were asked.
    cargo_home: std::sync::OnceLock<Option<String>>,
    /// Where the sources of the standard library are and were built, once asked.
    library: std::sync::OnceLock<Option<LibrarySources>>,
}

impl Contents {
    /// `None` for a crate that depends on one `stable_crate_id` has nothing for and that is
    /// not of the checkout.
    pub(crate) fn id(
        &self,
        db: &RootDatabase,
        checkouts: (&Path, Option<&Path>),
        krate: Crate,
        stable_crate_id: &dyn Fn(Crate) -> Option<u64>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> Option<ContentId> {
        *self
            .of(krate)
            .get_or_init(|| self.hash(db, checkouts, krate, stable_crate_id, process_reads))
    }

    fn of(&self, krate: Crate) -> std::sync::Arc<OnceLock<Option<ContentId>>> {
        let mut crates = self.crates.lock().unwrap_or_else(|e| e.into_inner());
        crates.entry(krate).or_default().clone()
    }

    /// Where cargo keeps the sources it fetched, when the crates that are not of the
    /// checkout are under one such directory: what is stored names it the same on every
    /// machine. With none, or with more than one, nothing is renamed.
    pub(crate) fn cargo_home(&self, db: &RootDatabase) -> Option<String> {
        let found = || {
            let crates = ide_db::base_db::all_crates(db);
            let outside = crates.iter().filter(|it| !it.data(db).origin.is_local());
            let roots = outside.filter_map(|&it| crate::crate_root_file(db, it.into()));
            let homes: FxHashSet<String> =
                roots.filter_map(|it| Some(cargo_home_of(&it)?.to_str()?.to_owned())).collect();
            let mut homes = homes.into_iter();
            homes.next().filter(|_| homes.next().is_none())
        };
        self.cargo_home.get_or_init(found).clone()
    }

    /// Where the standard library's sources are, blaze's `library/`, and where they were
    /// built, `/rustc/<commit>/library` for the commit of rust-lang/rust the library is a copy
    /// of (`library/UPSTREAM`): rustc names them so in what it built from them.
    pub(crate) fn library_sources(&self) -> Option<LibrarySources> {
        let found = || {
            let library = crate::cargo::toolchain::library::found().ok()?;
            let here = library.dir.to_str()?.to_owned();
            Some(LibrarySources { here, built_in: format!("/rustc/{}/library", library.commit) })
        };
        self.library.get_or_init(found).clone()
    }

    /// Whether data of a crate hashed here has in it where the checkout or the output of a
    /// build script is, as `env!("CARGO_MANIFEST_DIR")` puts it there: code with such data is
    /// of this checkout alone, whatever the crate is named by.
    pub(crate) fn names_place(&self, checkouts: (&Path, Option<&Path>), bytes: &[u8]) -> bool {
        let within =
            |place: &[u8]| !place.is_empty() && bytes.windows(place.len()).any(|it| it == place);
        within(checkouts.0.as_os_str().as_encoded_bytes())
            || checkouts.1.is_some_and(|it| within(it.as_os_str().as_encoded_bytes()))
            || self
                .places
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|it| within(it.as_bytes()))
    }

    fn hash(
        &self,
        db: &RootDatabase,
        checkouts: (&Path, Option<&Path>),
        krate: Crate,
        stable_crate_id: &dyn Fn(Crate) -> Option<u64>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> Option<ContentId> {
        let data = krate.data(db);
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"blaze crate 4");
        // The crate's identity is in what is compiled of it (a `TypeId` of one of its types),
        // so code stored for it is of that identity alone, whatever its sources are.
        match stable_crate_id(krate) {
            Some(id) => field(&id.to_le_bytes()),
            None => field(&[]),
        }
        let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
        field(name.unwrap_or_default().as_bytes());
        field(format!("{:?} {}", data.edition, data.target_kind.is_proc_macro()).as_bytes());
        for attr in &data.crate_attrs {
            field(attr.as_bytes());
        }
        let mut cfgs: Vec<String> =
            krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
        cfgs.sort_unstable();
        field(cfgs.join("\n").as_bytes());

        // The files of a source root can be of more crates than this one: two checkouts
        // whose build scripts write to one directory have their packages in one root.
        let env = krate.env(db);
        let manifest = env.get("CARGO_MANIFEST_DIR")?;
        let out = env.get("OUT_DIR");
        let root = db.file_source_root(data.root_file_id).source_root_id(db);
        let root = db.source_root(root).source_root(db);
        let mut files: Vec<(Written, String, Hash)> = Vec::new();
        for file in root.iter() {
            let Some(path) = root.path_for_file(&file).and_then(|it| it.as_path()) else {
                continue;
            };
            let path = path.to_string();
            let path = Path::new(&path);
            let (by, relative) = if let Ok(relative) = path.strip_prefix(&manifest) {
                (Written::InPackage, relative)
            } else if let Some(Ok(relative)) = out.as_ref().map(|out| path.strip_prefix(out)) {
                (Written::ByBuildScript, relative)
            } else {
                continue;
            };
            let hash = file_hash(db, file, path);
            files.push((by, relative.to_string_lossy().into_owned(), hash));
        }
        files.sort_unstable();
        for (by, path, hash) in &files {
            field(&[*by as u8]);
            field(path.as_bytes());
            field(hash);
        }
        // Sorted by its `Debug`. Where the crate is stands for itself: code that has the place
        // in it is not kept, see [`Contents::names_place`].
        let mut env = format!("{env:?}");
        // The longer path first: the others are often what it starts with.
        let mut places = vec![(manifest.as_str(), "$PACKAGE")];
        places.extend(out.as_deref().map(|out| (out, "$OUT")));
        let (checkout, base) =
            (checkouts.0.to_string_lossy(), checkouts.1.map(Path::to_string_lossy));
        places.push((&*checkout, "$CHECKOUT"));
        places.extend(base.as_deref().map(|base| (base, "$CHECKOUT")));
        places.sort_unstable_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        for (path, stands_for) in places {
            env = env.replace(path, stands_for);
        }
        if let Some(out) = out.clone() {
            self.places.lock().unwrap_or_else(|e| e.into_inner()).insert(out);
        }
        field(env.as_bytes());
        // What `env!` and `option_env!` in it read of the process the build is for.
        field(format!("{:?}", process_reads(krate)).as_bytes());
        // What `include_bytes!` and `include_str!` in it read that no source root has, as it
        // was read: known once the crate is listed, which asking its reads had it be.
        let mut read: Vec<(Written, String, Hash)> = db
            .included_by(krate)
            .into_iter()
            .map(|(path, bytes)| {
                let path = Path::new(path.as_str());
                let (by, path) = if let Ok(relative) = path.strip_prefix(&manifest) {
                    (Written::InPackage, relative)
                } else if let Some(Ok(relative)) = out.as_ref().map(|out| path.strip_prefix(out)) {
                    (Written::ByBuildScript, relative)
                } else {
                    (Written::Elsewhere, path)
                };
                let hash = Sha256::digest(bytes.as_deref().unwrap_or_default()).into();
                (by, path.to_string_lossy().into_owned(), hash)
            })
            .collect();
        read.sort_unstable();
        for (by, path, hash) in &read {
            field(&[*by as u8]);
            field(path.as_bytes());
            field(hash);
        }

        // In the order of their names: the order they are listed in is the build's.
        let mut dependencies: Vec<_> = data.dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            let on = dependency.crate_id;
            field(dependency.name.as_str().as_bytes());
            match on.data(db).origin.is_local() {
                true => field(&self.id(db, checkouts, on, stable_crate_id, process_reads)?.0),
                false => field(&Self::outside(db, on, stable_crate_id(on))),
            }
        }
        Some(ContentId(hasher.finalize().into()))
    }

    /// What stands for a crate from outside the checkout that rustc's build of the program
    /// has no id for, one that only exports others' items or a proc macro: where its source is,
    /// what it is configured with and what it is built on. `known` gives rustc's id of a crate,
    /// if there is one, and where its source starts.
    pub(crate) fn library(
        &self,
        db: &RootDatabase,
        krate: Crate,
        root: &Path,
        known: &dyn Fn(Crate) -> Option<(Option<u64>, std::path::PathBuf)>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> ContentId {
        let hash = || self.hash_library(db, krate, root, known, process_reads);
        if let Some(id) = *self.of(krate).get_or_init(|| Some(hash())) {
            return id;
        }
        // A crate of no checkout hashed as one of a checkout is this from now on.
        let id = hash();
        let known = std::sync::Arc::new(OnceLock::from(Some(id)));
        self.crates.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, known);
        id
    }

    fn hash_library(
        &self,
        db: &RootDatabase,
        krate: Crate,
        root: &Path,
        known: &dyn Fn(Crate) -> Option<(Option<u64>, std::path::PathBuf)>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> ContentId {
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"blaze library 4");
        field(Self::version(db, krate).as_bytes());
        // Where cargo fetched it to is under another directory on another machine.
        let root = root.to_string_lossy();
        let home = self.cargo_home(db);
        let there = home.and_then(|home| moved(&root, &home, CARGO_HOME));
        field(there.as_deref().unwrap_or(&root).as_bytes());
        let mut cfgs: Vec<String> =
            krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
        cfgs.sort_unstable();
        field(cfgs.join("\n").as_bytes());
        field(format!("{:?}", process_reads(krate)).as_bytes());
        let mut dependencies: Vec<_> = krate.data(db).dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            let on = dependency.crate_id;
            field(dependency.name.as_str().as_bytes());
            match known(on) {
                Some((None, root)) => field(&self.library(db, on, &root, known, process_reads).0),
                Some((id, _)) => field(&Self::outside(db, on, id)),
                None => field(&Self::outside(db, on, None)),
            }
        }
        ContentId(hasher.finalize().into())
    }

    /// What stands for a crate from outside the checkout in the hash of one that is built on
    /// it: its name and version, and rustc's id of it when rustc built something of it. The
    /// id is 64 bits and rustc's to choose, so it is never all that tells two crates apart.
    fn outside(db: &RootDatabase, krate: Crate, rustc_id: Option<u64>) -> Hash {
        let mut hasher = Sha256::new();
        let named = Self::version(db, krate);
        hasher.update((named.len() as u64).to_le_bytes());
        hasher.update(named.as_bytes());
        match rustc_id {
            Some(id) => hasher.update([&[1][..], &id.to_le_bytes()].concat()),
            None => hasher.update([0]),
        }
        hasher.finalize().into()
    }

    /// The name and version of a crate, which one from a registry is the same by wherever
    /// it is.
    pub(crate) fn version(db: &RootDatabase, krate: Crate) -> String {
        let extra = krate.extra_data(db);
        let name = extra.display_name.as_ref().map(|it| it.to_string());
        format!("{name:?} {:?}", extra.version)
    }
}

/// What a build script wrote to `out` that the source root of `krate` holds: the path of each
/// file below `out` and the hash of what is in it, in the order of their paths.
pub(crate) fn written_to(db: &RootDatabase, krate: Crate, out: &str) -> Vec<(String, Hash)> {
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    let root = db.source_root(root).source_root(db);
    let mut files: Vec<(String, Hash)> = root
        .iter()
        .filter_map(|file| {
            let path = root.path_for_file(&file)?.as_path()?.to_string();
            let relative = Path::new(&path).strip_prefix(out).ok()?;
            Some((relative.to_string_lossy().into_owned(), file_hash(db, file, Path::new(&path))))
        })
        .collect();
    files.sort_unstable();
    files
}

/// The hash of the file `file` at `path`. One that is not UTF-8 has no text: it is hashed as
/// it is on disk, where an `include_bytes!` reads it from.
pub(crate) fn file_hash(db: &RootDatabase, file: vfs::FileId, path: &Path) -> Hash {
    if db.has_file_text(file) {
        FileHashes::of(db, file)
    } else {
        Sha256::digest(std::fs::read(path).unwrap_or_default()).into()
    }
}

/// The directory cargo keeps what it fetched in, by a file it fetched: sources of a registry
/// are in its `registry/src`, those of git in its `git/checkouts`.
fn cargo_home_of(path: &Path) -> Option<&Path> {
    let fetched = |dir: &&Path| dir.ends_with("registry/src") || dir.ends_with("git/checkouts");
    path.ancestors().filter(fetched).last()?.parent()?.parent()
}

/// Where a file of a crate is, by what its path is hashed from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Written {
    InPackage,
    ByBuildScript,
    /// Read by `include_bytes!` or `include_str!` from anywhere else: by its whole path.
    Elsewhere,
}

/// The hash of each file as it was last read, for as long as the daemon runs: a build hashes
/// only what was edited since the one before.
struct FileHashes {
    files: FxHashMap<vfs::FileId, (Arc<str>, Hash)>,
}

impl FileHashes {
    fn of(db: &RootDatabase, file: vfs::FileId) -> Hash {
        static HASHES: OnceLock<Mutex<FileHashes>> = OnceLock::new();
        let hashes = HASHES.get_or_init(|| Mutex::new(FileHashes { files: FxHashMap::default() }));
        let text = db.file_text(file).text(db);
        let mut hashes = hashes.lock().unwrap_or_else(|e| e.into_inner());
        // The text it was hashed from is kept, so no other text is ever where it is.
        if let Some((known, hash)) = hashes.files.get(&file)
            && Arc::ptr_eq(known, text)
        {
            return *hash;
        }
        let hash: Hash = Sha256::digest(text.as_bytes()).into();
        hashes.files.insert(file, (text.clone(), hash));
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cargo_home_is_told_from_a_file_cargo_fetched() {
        fn home(path: &str) -> Option<&str> {
            cargo_home_of(Path::new(path))?.to_str()
        }
        let registry = "/home/one/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
        assert_eq!(home(&format!("{registry}/dep-0.1.0/src/lib.rs")), Some("/home/one/.cargo"));
        assert_eq!(home("/srv/c/git/checkouts/dep-1a2b/3c4d/src/lib.rs"), Some("/srv/c"));
        // A crate that vendors another has the outermost such directory as cargo's.
        let nested = format!("{registry}/dep-0.1.0/registry/src/inner/lib.rs");
        assert_eq!(home(&nested), Some("/home/one/.cargo"));
        assert_eq!(home("/home/one/checkout/src/lib.rs"), None);
    }
}
