//! The key of a dependency's image: a hash of every input its def map, lang items and
//! signatures are made from that is known before the crate is resolved, written so that two
//! checkouts of the crate at different paths, on one machine or two, give one key.
//!
//! - The crate ([`crate_inputs`]): its root file's path, edition, origin, crate attributes,
//!   target kind and crate types, the directory its proc macros run in, version, display name,
//!   potential cfg, cfg (features are cfg), env, and the workspace's target, toolchain and
//!   const-eval budget ([`base_db::CrateWorkspaceData::content`]); each dependency as its key,
//!   its name and whether it is a prelude, sysroot or private dependency. Each of
//!   [`base_db::BuiltCrateData`]'s and [`ExtraCrateData`]'s fields is named where it is hashed,
//!   so a field added to either does not build until the key covers it. One is named and left
//!   out on purpose: `manifest_kind`, the kind of the manifest target a `--test` crate is built
//!   from. Nothing in an image reads it (def maps, lang items and signatures are made from
//!   `target_kind`, crate types and cfg), and the root file's path above already tells one
//!   package's targets apart (`tests/fetch.rs` and `examples/fetch.rs`), so hashing it would
//!   only split one image into two equal ones.
//! - Each file of the source root of the crate's root file: its path and its content ID in the
//!   source root ([`base_db::Files::file_content`], which has no path in it), sorted by path:
//!   its text's, or for a file that is not UTF-8, that of the bytes the view loader gave it
//!   ([`base_db::IncludedFile::content`]), the bytes `include_str!` and `include_bytes!` read.
//! - Of a proc-macro crate, each macro's name and kind and the content ID of the library it is
//!   built into (`ProcMacroExpander::code`): what its expansions are a function of besides
//!   their call. A proc-macro crate whose library is not known is not [`eligible`], nor is any
//!   crate that depends on it.
//! - `build`, which names the front end that reads and writes the image (it decides what a
//!   crate resolves to): blaze hands over the content identity of the front end's code and of
//!   how it was built (`blaze_codegen_code::FRONT_END`), never its executable's bytes; and
//!   [`PRODUCER`], the version of each encoding in the image.
//!
//! **Paths.** A path is written relative to the crate's package directory
//! (`CARGO_MANIFEST_DIR`, else the directory of its root file) and its `OUT_DIR`: each of
//! those, wherever it is in a path or a variable's value, is written as a marker ([`Base`]).
//! A variable whose value is another absolute path is written as a marker as well. The image
//! is written only if none of its parts holds one of the paths a marker stands for
//! ([`Base::held_by`]), so a key that leaves a path out never names an image that depends on
//! it.
//!
//! What the crate reads that is known only once it is resolved (the process variables its
//! `env!`s read, what its proc-macro expansions read) is not in the key: it is the crate's
//! [`super::reads`], which file the image under the key and the values those reads have.
//!
//! **Files.** Every file an image depends on is a file of the source root of a crate whose key
//! the key covers (its own or a dependency's), so its text is in the key wherever in that
//! source root it is: a `#[path]` or an `include!` into another directory of the package
//! (`../shared/m.rs`), and a build script's `OUT_DIR` files, which a run puts in its package's
//! source root (`blaze_lower`'s `apply_run`). A frozen def map names its files by their path in
//! their owner's source root, and is looked up there alone when read. A crate whose module
//! files or `include!`s read a file anywhere else (a `#[path]` into another package, a path no
//! source root has), an `include!` among an impl's or a trait's items too
//! (`signatures::scope_calls`), is refused an image (`serve::refusal`,
//! `serve::include_refusal`): that file is known only once the crate is resolved, after the
//! key is made. So is a crate with a `mod` whose file is not there (`Refused::ModuleFileMissing`):
//! the key covers the files that are, and would not move once that one appears.

use base_db::{Crate, Db, ExtraCrateData, TargetKind};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use hir_expand::proc_macro::{ProcMacroKind, ProcMacros};
use std::alloc::Global;

use crate::{
    deps::{image::IMAGE_SCHEMA, reads, serve::Refused, signatures},
    nameres::frozen,
};

/// The domain of an image key.
pub const KEY_DOMAIN: &str = "blaze v2 dependency image key 6";

/// What makes the images: the version of each encoding in them, so an image of another
/// encoding is never found.
#[expect(
    clippy::disallowed_types,
    reason = "the process's one producer content id: all image keys use the same encoding identity"
)]
pub static PRODUCER: std::sync::LazyLock<ContentId> = std::sync::LazyLock::new(|| {
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher.part(b"producer");
    hasher.id(&IMAGE_SCHEMA.0);
    hasher.part(frozen::ENCODING.as_bytes());
    hasher.id(&signatures::SCHEMA.0);
    hasher.part(reads::READS_DOMAIN.as_bytes());
    hasher.finish()
});

/// Whether `krate` is read from an image: a crate no workspace member, none of whose
/// dependencies is one either (a member's def map changes with every edit, so nothing that
/// names it is kept), and every proc-macro crate among them with its library known.
pub fn eligible(db: &Db, krate: Crate) -> bool {
    let mut memo = HashMap::new_in(Global);
    walk(db, krate, &mut |_, _| ContentId::default(), &mut memo).is_some()
}

/// The key of each of `crates` that is [`eligible`], by `build`, the front end that reads and
/// writes the images. A crate's key covers its dependencies' keys, so they are made too.
///
/// One walk of the crate graph: each crate is visited once, whether eligible and its key
/// decided together, so the work is the crates' files and their dependency edges.
pub fn keys(db: &Db, crates: &[Crate], build: ContentId) -> HashMap<Crate, ContentId, Global> {
    // alloc: setup: the keys of a session, once
    walked(db, crates, build).into_iter().filter_map(|(krate, key)| Some((krate, key?))).collect()
}

/// What the key of a crate was made from that a rule for public keys looks at
/// (`blaze_global::Rule::image`), from the walk that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInputs {
    pub key: ContentId,
    pub build: ContentId,
    /// The crate, then each crate its key covers (its dependencies, theirs, and so on), each
    /// once.
    pub closure: Vec<Crate>,
    /// The `OUT_DIR` of each crate of `closure` that has one, as its variables give it, in the
    /// order of `closure`.
    pub out_dirs: Vec<String>,
}

/// [`keys`], each with what it was made from: the same walk, so the closure is exactly the
/// crates whose keys the key hashed.
pub fn key_inputs(
    db: &Db,
    crates: &[Crate],
    build: ContentId,
) -> HashMap<Crate, KeyInputs, Global> {
    let memo = walked(db, crates, build);
    let inputs = |(&krate, key): (&Crate, &Option<ContentId>)| {
        // alloc: setup: once per crate a driver publishes
        let mut closure = vec![krate];
        let mut at = 0;
        while let Some(&next) = closure.get(at) {
            for dep in &next.data(db).dependencies {
                if !closure.contains(&dep.crate_id) {
                    closure.push(dep.crate_id);
                }
            }
            at += 1;
        }
        // alloc: setup, as above
        let out_dirs = closure.iter().filter_map(|it| it.env(db).get("OUT_DIR")).collect();
        Some((krate, KeyInputs { key: (*key)?, build, closure, out_dirs }))
    };
    // alloc: setup, as above
    memo.iter().filter_map(inputs).collect()
}

/// The walk of [`keys`] and [`key_inputs`]: each crate of `crates` and of their dependencies,
/// with its key or `None` when it is not [`eligible`].
fn walked(db: &Db, crates: &[Crate], build: ContentId) -> Memo {
    let mut memo = HashMap::new_in(Global);
    let mut hash = |krate, memo: &Memo| key(db, krate, build, memo);
    for &krate in crates {
        walk(db, krate, &mut hash, &mut memo);
    }
    memo
}

/// Each crate walked: its key, or `None` when it is not [`eligible`].
type Memo = HashMap<Crate, Option<ContentId>, Global>;

/// `krate`'s key by `hash` of it and the memo holding its dependencies' keys, if it and every
/// crate it depends on is eligible, its dependencies' first. The crate graph has no cycles,
/// which [`Crate::transitive_deps`] relies on too, so the walk ends.
fn walk(
    db: &Db,
    krate: Crate,
    hash: &mut impl FnMut(Crate, &Memo) -> ContentId,
    memo: &mut Memo,
) -> Option<ContentId> {
    if let Some(&key) = memo.get(&krate) {
        return key;
    }
    let data = krate.data(db);
    let mut eligible = !data.origin.is_local() && libraries_known(db, krate);
    for dep in &data.dependencies {
        eligible &= walk(db, dep.crate_id, hash, memo).is_some();
    }
    let key = eligible.then(|| hash(krate, memo));
    memo.insert(krate, key);
    key
}

/// Whether every proc macro of `krate` (none, for a crate that is not a proc-macro crate) has
/// the content ID of its library.
fn libraries_known(db: &Db, krate: Crate) -> bool {
    let is_proc_macro =
        matches!(krate.data(db).target_kind, TargetKind::Lib { is_proc_macro: true });
    match ProcMacros::get_for_crate(db, krate) {
        None => !is_proc_macro,
        Some(macros) => macros
            .macros()
            .is_some_and(|macros| macros.iter().all(|it| it.expander.code().is_some())),
    }
}

/// The key of `krate` from its inputs and its dependencies' keys in `memo`, in the order the
/// crate lists them: each is there, since [`walk`] found every dependency eligible.
fn key(db: &Db, krate: Crate, build: ContentId, memo: &Memo) -> ContentId {
    let base = Base::of(db, krate);
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher.id(&PRODUCER).id(&build);
    crate_inputs(db, krate, &base, memo, &mut hasher);
    files(db, krate, &base, &mut hasher);
    libraries(db, krate, &mut hasher);
    hasher.finish()
}

/// The crate's own inputs, its dependencies by their keys in `memo`.
fn crate_inputs(db: &Db, krate: Crate, base: &Base, memo: &Memo, hasher: &mut Hasher) {
    let base_db::BuiltCrateData {
        root_file_id,
        edition,
        dependencies,
        origin,
        crate_attrs,
        target_kind,
        // Nothing an image holds reads it, and the root's path below tells the targets of one
        // package apart.
        manifest_kind: _,
        crate_types,
        proc_macro_cwd,
    } = krate.data(db);
    let ExtraCrateData { version, display_name, potential_cfg_options, built_for, build_script_runs } =
        krate.extra_data(db);

    let root = db.file_source_root(*root_file_id).source_root_id(db);
    let root_path = db.source_root(root).source_root_ref(db).path_for_file(root_file_id);
    match root_path {
        // alloc: setup: the root's path as text, once per crate per session
        Some(path) => base.part(hasher, &path.to_string()),
        None => {
            hasher.update(&[0xff]);
        }
    }
    // The root's own content ID: the root may be an alias ([`base_db::Files::alias_of`]), its
    // file's path and text with a module appended (a test harness, a proc macro's wasm glue),
    // which [`files`] does not list, since a source root lists only the file it aliases.
    hasher.id(&db.file_content(*root_file_id));
    hasher.update(&(edition.number() as u64).to_le_bytes());
    hasher.update(&(dependencies.len() as u64).to_le_bytes());
    for dep in dependencies {
        hasher
            .id(&memo[&dep.crate_id].expect("`walk` keys every dependency first"))
            .part(dep.name.as_str().as_bytes())
            .update(&[dep.is_prelude() as u8, dep.is_sysroot() as u8, dep.is_private() as u8]);
    }
    origin.hash_into(hasher);
    hasher.update(&(crate_attrs.len() as u64).to_le_bytes());
    for attr in crate_attrs.iter() {
        hasher.part(attr.as_bytes());
    }
    hasher.update(&[target_kind.tag(), crate_types.bits()]);
    base.part(hasher, proc_macro_cwd.as_str());
    hasher.opt_part(version.as_deref());
    match display_name {
        None => hasher.update(&[0]),
        Some(name) => hasher
            .update(&[1])
            .part(name.crate_name().as_str().as_bytes())
            .part(name.canonical_name().as_str().as_bytes()),
    };
    match potential_cfg_options {
        None => hasher.update(&[0]),
        Some(cfg) => hasher.update(&[1]).id(&base_db::cfg_options_content(cfg)),
    };
    built_for.hash_into(hasher);
    hasher.update(&(build_script_runs.len() as u64).to_le_bytes());
    for run in build_script_runs {
        hasher.part(run.as_bytes());
    }
    hasher
        .id(&krate.workspace_data(db).content())
        .id(&base_db::cfg_options_content(krate.cfg_options(db)));
    // alloc: setup: the crate's variables sorted, once per crate per session
    let mut env: Vec<(&str, &str)> = krate.env(db).iter().collect();
    env.sort_unstable();
    hasher.update(&(env.len() as u64).to_le_bytes());
    for (name, value) in env {
        hasher.part(name.as_bytes());
        base.value(hasher, value);
    }
}

/// Each file of the source root of `krate`'s root file, as its path and its content ID in the
/// source root ([`base_db::Files::file_content`]): its text's, or for a file that is not UTF-8,
/// that of the bytes the view loader gave it, which `include_str!` and `include_bytes!` read.
/// Reading them writes nothing, so a key made before or after any other query is the same.
fn files(db: &Db, krate: Crate, base: &Base, hasher: &mut Hasher) {
    let file = krate.data(db).root_file_id;
    let root = db.file_source_root(file).source_root_id(db);
    let root = db.source_root(root).source_root_ref(db);
    // alloc: setup: the directory frozen images name this crate's files from
    let root_path = root.path_for_file(&file).map(|it| it.to_string());
    let dir = root_path.as_deref().and_then(|it| it.rsplit_once('/')).map(|it| it.0);
    // alloc: setup: the root's files, once per crate per session
    let mut files: Vec<(Vec<u8>, ContentId)> = root
        .iter()
        .map(|file| {
            // alloc: setup: one path a file, once per crate per session
            let path = root.path_for_file(&file)
                .unwrap_or_else(|| panic!("source-root file {file:?} has no path"))
                .to_string();
            // An unmarked absolute path alone misses how frozen images restore it: from
            // the crate root's directory. Moving that directory while keeping the outside
            // file in place must move the key too (#1637).
            let generated = base.out.as_deref().is_some_and(|out| {
                path.strip_prefix(out).is_some_and(|rest| rest.starts_with('/'))
            });
            let path = match dir {
                Some(dir) if !generated => {
                    crate::nameres::frozen::relative(dir, &path).into_bytes()
                }
                _ => base.portable(&path),
            };
            (path, db.file_content(file))
        })
        // alloc: setup, as above
        .collect();
    files.sort_unstable();
    hasher.update(&(files.len() as u64).to_le_bytes());
    for (path, content) in &files {
        hasher.part(path).id(content);
    }
}

/// The proc macros of `krate`, if it is a proc-macro crate: each one's name, kind and library.
fn libraries(db: &Db, krate: Crate, hasher: &mut Hasher) {
    let Some(macros) = ProcMacros::get_for_crate(db, krate).and_then(|it| it.macros()) else {
        hasher.update(&[0]);
        return;
    };
    hasher.update(&[1]).update(&(macros.len() as u64).to_le_bytes());
    for it in macros {
        let kind = match it.kind {
            ProcMacroKind::CustomDerive => 0,
            ProcMacroKind::Bang => 1,
            ProcMacroKind::Attr => 2,
        };
        let code = it.expander.code().expect("`walk` keys only crates whose libraries are known");
        hasher.part(it.name.as_str().as_bytes()).update(&[kind, it.disabled as u8]).id(&code);
    }
}

/// The paths a crate's key writes as markers: its package directory, its `OUT_DIR`, and each
/// other absolute path a variable of its holds.
pub(crate) struct Base {
    dir: Option<String>,
    out: Option<String>,
    others: Vec<String>,
}

/// What stands for the package directory, `OUT_DIR` and any other absolute path in a key. No
/// path or variable holds a NUL.
const DIR: &[u8] = b"\0PKG";
const OUT: &[u8] = b"\0OUT_DIR";
const OTHER: &[u8] = b"\0ABSOLUTE";

/// The shortest path [`Base::held_by`] looks for: a shorter one (`/a` in a test) is a few bytes
/// that any binary part holds by chance. A real package directory is far longer.
const SHORTEST_HELD: usize = 8;

impl Base {
    pub(crate) fn of(db: &Db, krate: Crate) -> Base {
        let trimmed = |it: &str| -> Option<String> {
            let it = it.trim_end_matches(['/', '\\']);
            // alloc: setup: once per crate per session
            (!it.is_empty()).then(|| it.to_owned())
        };
        let env = krate.env(db);
        let dir = env.get("CARGO_MANIFEST_DIR").and_then(|it| trimmed(&it)).or_else(|| {
            let root = krate.data(db).root_file_id;
            let source_root = db.file_source_root(root).source_root_id(db);
            let path = db.source_root(source_root).source_root_ref(db).path_for_file(&root)?;
            // alloc: setup, as above
            let path = path.to_string();
            let (dir, _) = path.rsplit_once('/')?;
            trimmed(dir)
        });
        let out = env.get("OUT_DIR").and_then(|it| trimmed(&it));
        // alloc: setup, as above
        let mut base = Base { dir, out, others: Vec::new() };
        for (_, value) in env.iter() {
            if base.marked(value.as_bytes()).is_none() && absolute(value) {
                base.others.push(value.to_owned()); // alloc: setup, as above
            }
        }
        base.others.sort_unstable();
        base.others.dedup();
        base
    }

    /// The markers this writes in place of paths, longest first so a path below another is
    /// replaced whole.
    fn markers(&self) -> impl Iterator<Item = (&str, &'static [u8])> {
        let mut found = [(self.out.as_deref(), OUT), (self.dir.as_deref(), DIR)];
        found.sort_by_key(|(path, _)| std::cmp::Reverse(path.map_or(0, str::len)));
        found.into_iter().filter_map(|(path, marker)| Some((path?, marker)))
    }

    /// The first place in `text` one of the paths is, the path's length and its marker.
    fn marked(&self, text: &[u8]) -> Option<(usize, usize, &'static [u8])> {
        let mut first: Option<(usize, usize, &'static [u8])> = None;
        for (path, marker) in self.markers() {
            if let Some(at) = find(text, path.as_bytes())
                && first.is_none_or(|(was, _, _)| at < was)
            {
                first = Some((at, path.len(), marker));
            }
        }
        first
    }

    /// `text` with each of the package directory and `OUT_DIR` in it written as its marker.
    pub(crate) fn portable(&self, text: &str) -> Vec<u8> {
        let mut text = text.as_bytes();
        // alloc: setup: one path or value of a key, once per crate per session
        let mut out = Vec::with_capacity(text.len());
        while let Some((at, len, marker)) = self.marked(text) {
            out.extend_from_slice(&text[..at]);
            out.extend_from_slice(marker);
            text = &text[at + len..];
        }
        out.extend_from_slice(text);
        out
    }

    /// Hashes a path as [`Base::portable`] writes it.
    fn part(&self, hasher: &mut Hasher, path: &str) {
        hasher.part(&self.portable(path));
    }

    /// Hashes a variable's value: as [`Base::portable`] writes it, or as [`OTHER`] for another
    /// absolute path.
    fn value(&self, hasher: &mut Hasher, value: &str) {
        match self.others.binary_search_by(|it| it.as_str().cmp(value)) {
            Ok(_) => hasher.part(OTHER),
            Err(_) => hasher.part(&self.portable(value)),
        };
    }

    /// Why an image whose parts are `parts` would depend on a path its key does not hold: the
    /// first part that holds one, and the path.
    pub(crate) fn held_by(&self, parts: &[Vec<u8>; 3]) -> Option<Refused> {
        let paths = self.dir.iter().chain(&self.out).chain(&self.others);
        for path in paths.filter(|it| it.len() >= SHORTEST_HELD) {
            for (part, bytes) in super::Part::ALL.into_iter().zip(parts) {
                if find(bytes, path.as_bytes()).is_some() {
                    // clone: why a crate is refused, once per refused crate
                    return Some(Refused::MachinePath { part, path: path.clone() });
                }
            }
        }
        None
    }
}

/// Whether `value` is an absolute path on some machine: `/..`, `C:\..`, `C:/..` or `\\..`.
pub fn absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    match bytes {
        [b'/', ..] => true,
        [b'\\', b'\\', ..] => true,
        [drive, b':', b'\\' | b'/', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// Where `needle` first is in `haystack`: a scan for its first byte and a compare at each.
/// Run over an image's parts once, as the image is written.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let mut at = 0;
    while at + needle.len() <= haystack.len() {
        let found =
            haystack[at..=haystack.len() - needle.len()].iter().position(|&it| it == first)?;
        let start = at + found;
        if haystack[start + 1..start + needle.len()] == *rest {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

#[cfg(test)]
pub(super) fn base_for_tests(dir: &str, out: Option<&str>) -> Base {
    Base { dir: Some(dir.to_owned()), out: out.map(str::to_owned), others: Vec::new() }
}
