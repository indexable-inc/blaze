//! What a unit's record is filed by beside its key ([`super::compile`]): what its crate and
//! every crate it depends on read that no key can name before they are resolved, with the
//! values read. That is the process variables their `env!` and `option_env!` calls read, the
//! files outside every source root their `include!`, `include_str!` and `include_bytes!` calls
//! read, and what their proc-macro expansions read besides the call (a file outside the source
//! tree, a listing, a variable of the shell), as the expansion server traced it.
//!
//! Those reads are known only once the crates are resolved, so they are kept as a dependency's
//! image keeps its own (`hir_def::deps::filed`): the names in the first free of [`SLOTS`]
//! slots under the record's base key, and the record under the base key, those names and the
//! values they had. A record whose closure read nothing of the kind is under the base key
//! alone. Looking one up asks the base key, then each slot with the values its names have now:
//! a variable's in the job's environment, a file's content ID as it is on disk now
//! ([`base_db::IncludedFile::content_now`], the content ID the front end keeps of what it read:
//! of its bytes, or of why it could not be read), an expansion's worked out again from its
//! names (the daemon's `proc_macro_api::kept::Keeping::reads_now`) with no macro run. So a
//! record is used only while everything it read is as it was, and a variable nothing read (a
//! per-run `BLAZE_PROFILE`, `OLDPWD`) never makes it miss. A file is named by its absolute
//! path, normalized as the front end names it (`base_db::Files::included_by`).
//!
//! `CARGO_PRIMARY_PACKAGE` is of the request, in no plan and no crate's identity
//! ([`base_db::PRIMARY_PACKAGE`]): `--workspace` then `-p one` keys no unit anew. A crate of
//! the closure that read it is named in the slot by its package's manifest
//! (`CARGO_MANIFEST_PATH`, which its identity holds), and valued by whether the job's request
//! makes that package primary ([`blaze_units::query::Units::is_primary_manifest`]), then by the
//! process's variable, as `env!` reads it ([`base_db::env_var`]). Its proc-macro expansions are
//! given it as the front end gives them ([`base_db::given_macro_env`]).
//!
//! The names come from the database's logs: the `env!` log notes each variable an expansion
//! reads, the expansion log each proc-macro expansion's names and values, and each notes the
//! reads a value served from the store in place of resolving was filed by
//! (`hir_def::deps::filed::search`), so once a unit is compiled the logs name everything its
//! code was made from.

#![forbid(unsafe_code)]

#[cfg(test)]
pub(super) mod tests;

use base_db::{Crate, Db};
use blaze_cas::Key;
use blaze_format::cursor::{Put as _, Reader};
use blaze_ids::{ContentId, Hasher};
use blaze_usercode::roots::{Role, Roots};

/// The domain of a record's slots and of a record filed by its reads.
/// 3: the crates that read `CARGO_PRIMARY_PACKAGE` ([`Reads::primary`]). 4: the files outside
/// every source root read ([`Reads::files`]).
const DOMAIN: &str = "blaze v2 cargo unit record reads 4";

/// The first byte of a slot's bytes: the version of [`Reads::encode`].
const VERSION: u8 = 4;

/// Every process variable a record's reads named in a lookup or a filing in this process
/// ([`Reads::hash`]): the `env!` and `option_env!` reads a unit record is found or kept by.
/// What the no-op lane compares of a request's environment for the records' sake
/// ([`named_env`]). The process's, not a job's: a name another checkout's record read is a
/// variable more compared, never one missed. A few names (the variables crates read at
/// compile time), each once.
static NAMED: blaze_core::lock::Lock<Vec<String>> =
    blaze_core::lock::Lock::new("serve.unit.reads.named", Vec::new());

/// Each of `names` noted in [`NAMED`].
fn note_named(names: &[String]) {
    if names.is_empty() {
        return;
    }
    let mut named = NAMED.lock();
    for name in names {
        if !named.contains(name) {
            named.push(name.clone()); // clone: a name, once for the process's life
        }
    }
}

/// Every path a record's reads named in a lookup or a filing in this process
/// ([`Reads::hash`]): the files outside every source root its crates `include!`d and the
/// files its proc-macro expansions read, then the directories those listed, each as the path
/// it is in this checkout; each sorted, each once. The last is why, once an expansion's reads
/// did not decode or named a root it has not: what it read is then not known. The process's, as [`NAMED`] is, and for the same reader: a path here is one the
/// no-op lane never takes for another request's output ([`read_paths`]).
static PATHS: blaze_core::lock::Lock<Noted> =
    blaze_core::lock::Lock::new("serve.unit.reads.paths", (Vec::new(), Vec::new(), None));

/// `path` kept in `sorted`, once.
fn note_path(sorted: &mut Vec<std::path::PathBuf>, path: &std::path::Path) {
    if let Err(at) = sorted.binary_search_by(|it| it.as_path().cmp(path)) {
        sorted.insert(at, path.to_owned()); // alloc: a path, once for the process's life
    }
}

/// What an expansion read under the first bytes of a shape verified across checkouts
/// ([`shape_said`]).
const VERIFIED_SHAPE: &[u8] = b"blaze v2 verified expansion reads 1\0";

/// The paths noted ([`PATHS`]): files, directories, and why they are not known.
type Noted = (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>, Option<String>);

/// The reads of `reads` by path noted in [`PATHS`].
fn note_paths(reads: &Reads) {
    if reads.files.is_empty() && reads.expansions.is_empty() {
        return;
    }
    note_paths_in(&mut PATHS.lock(), reads);
}

/// The reads of `reads` by path noted in `paths`.
fn note_paths_in(paths: &mut Noted, reads: &Reads) {
    for (path, _) in &reads.files {
        note_path(&mut paths.0, std::path::Path::new(path));
    }
    for expansions in &reads.expansions {
        // A shape is written against the calling crate's roots (`proc_macro_api::kept::roots`:
        // its package and `OUT_DIR`, from the variables the slot keeps): each path is noted
        // as the file it is in this checkout, and a root's mark, whose value is the root's
        // path and which names no file, is not a read of the disk.
        let get = |name: &str| expansions.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
        let named = [(Role::Package, get("CARGO_MANIFEST_DIR")), (Role::OutDir, get("OUT_DIR"))];
        let roots = Roots::new(named.into_iter().filter_map(|(role, path)| Some((role, path?))));
        for (shape, _) in &expansions.shapes {
            let rest = match shape.strip_prefix(VERIFIED_SHAPE) {
                Some(rest) if rest.len() >= 32 => &rest[32..],
                _ => shape.as_slice(),
            };
            let shape = match blaze_usercode::index::Shape::decode(rest) {
                Ok(it) => it,
                Err(why) => {
                    // alloc: a reason, once for the process's life
                    paths.2.get_or_insert_with(|| format!("a unit record's proc-macro expansion reads do not decode ({why}): what it read is not known"));
                    continue;
                }
            };
            for (list, at) in [(&shape.files, 0), (&shape.dirs, 1)] {
                for path in list.iter().filter(|it| !blaze_usercode::roots::is_mark(it)) {
                    match roots.real_path(path) {
                        Ok(real) if at == 0 => note_path(&mut paths.0, &real),
                        Ok(real) => note_path(&mut paths.1, &real),
                        Err(why) => {
                            // alloc: a reason, once for the process's life
                            paths.2.get_or_insert_with(|| format!("a unit record's proc-macro expansion read is not a path of this checkout: {why}"));
                        }
                    }
                }
            }
        }
    }
}

/// The files and the directories any record's reads named in this process so far
/// ([`PATHS`]), each as the path it is in this checkout; an error naming why once an
/// expansion's reads did not decode or named a root the expansion has not.
pub(crate) fn read_paths() -> Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>), String> {
    let paths = PATHS.lock();
    if let Some(why) = &paths.2 {
        return Err(why.clone()); // clone: a reason, once a build
    }
    Ok((paths.0.clone(), paths.1.clone())) // clone: once a build that walked and made nothing
}

/// The process variables any record's reads named in this process so far ([`NAMED`]).
pub(crate) fn named_env() -> Vec<String> {
    NAMED.lock().clone() // clone: once a build that walked and made nothing
}

/// How many sets of names one base key keeps. A crate's code reads other names only where a
/// value it read picks them (`env!(concat!(env!("A"), ..))`), so one in practice.
pub(crate) const SLOTS: u32 = hir_def::deps::SLOTS;

/// What a unit's crate closure read, each part sorted, each read once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reads {
    /// The process variables `env!` and `option_env!` read.
    names: Vec<String>,
    /// What the proc-macro expansions read, by the variables of the crate the calls are in.
    expansions: Vec<Expansions>,
    /// The crates whose `env!` or `option_env!` read `CARGO_PRIMARY_PACKAGE`, each by its
    /// package's manifest (`CARGO_MANIFEST_PATH`).
    primary: Vec<String>,
    /// The files outside every source root that `include!`, `include_str!` and
    /// `include_bytes!` read, by absolute path, with the content ID of what was read
    /// ([`base_db::IncludedFile::content`]): known for reads made in this process, none for
    /// reads decoded from a slot.
    files: Vec<(String, Option<ContentId>)>,
}

/// What the proc-macro expansions of the crates of one set of variables read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Expansions {
    /// The crate's variables, sorted. They are in the record's base key already (the crate's
    /// identity); the slot keeps them because a lookup has no database, and a shape's names
    /// are worked out with them (a file's path from `CARGO_MANIFEST_DIR`). Its expansions were
    /// given them and `CARGO_PRIMARY_PACKAGE` when the crate was primary, which is of the
    /// request: added again as the lookup's request has it.
    env: Vec<(String, String)>,
    /// Each expansion's names as its server encodes them, sorted, with the hash of their
    /// values when it ran: known for reads made in this process, none for reads decoded from
    /// a slot.
    shapes: Vec<(Vec<u8>, Option<ContentId>)>,
}

impl Reads {
    /// What `krate` and every crate it depends on read, as `db`'s logs have it. For a
    /// variable, only a crate that is not the standard library's (whose `env!` reads its own
    /// variables alone), and only a name the crate's own variables lack (`env!` reads the
    /// crate's, then the process's; the crate's are in its identity). The expansions' reads are
    /// [`hir_def::deps::closure_expansion_reads`]'s. An expansion of a call the crate still has
    /// whose reads are not known, or two of one crate's that found other values for the same
    /// names, are an error naming the call or crate: no key covers what the unit was made from.
    /// The files are [`base_db::Files::included_by`]'s for each crate of the closure.
    pub(crate) fn of(db: &Db, krate: Crate) -> Result<Reads, String> {
        // alloc: once per unit stored, a file read each
        let mut files: Vec<(String, Option<ContentId>)> = Vec::new();
        for &by in krate.transitive_deps(db) {
            for (path, content) in db.files().included_by(db, by) {
                files.push((path.as_str().to_owned(), Some(content))); // alloc: as above
            }
        }
        files.sort_unstable();
        // Every crate that read a file read the one input the front end keeps of it.
        files.dedup();
        if files.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err("two reads of one file outside every source root found other contents: \
                        an input changed while it was resolved"
                .to_owned());
        }
        // alloc: once per unit stored, a crate of its closure each
        let closure: blaze_hash::HashSet<Crate, std::alloc::Global> =
            krate.transitive_deps(db).iter().copied().collect();
        // alloc: once per unit stored, a variable read each
        let mut names = Vec::new();
        // alloc: once per unit stored, a crate that read `CARGO_PRIMARY_PACKAGE` each
        let mut primary = Vec::new();
        for (by, name) in db.files().env_read() {
            if !closure.contains(&by) || by.data(db).origin.is_lang() {
                continue;
            }
            if name == base_db::PRIMARY_PACKAGE {
                let Some(manifest) = by.env(db).get("CARGO_MANIFEST_PATH") else {
                    let name = by.named(db);
                    // alloc: why the record is not kept, once per unit stored
                    return Err(format!(
                        "crate {name} read `CARGO_PRIMARY_PACKAGE` and has no `CARGO_MANIFEST_PATH` to name its \
                         package by, so no key covers whether it is primary"
                    ));
                };
                primary.push(manifest);
            } else if by.env(db).get(&name).is_none() {
                names.push(name);
            }
        }
        names.sort_unstable();
        names.dedup();
        primary.sort_unstable();
        primary.dedup();
        let read = hir_def::deps::closure_expansion_reads(db, krate).map_err(|refused| {
            format!("no key covers what a proc-macro expansion read: {refused}")
        })?;
        // alloc: once per unit stored, a crate of its closure whose expansions read something each
        let mut expansions: Vec<(Crate, Expansions)> = Vec::with_capacity(read.len());
        for (by, shapes) in read {
            // alloc: a crate's variables, once per unit stored, a crate as above each
            let mut env: Vec<(String, String)> = by
                .env(db)
                .iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect();
            env.sort_unstable();
            let shapes = shapes
                .into_iter()
                .map(|(shape, values)| (shape.into_vec(), Some(values)))
                .collect();
            expansions.push((by, Expansions { env, shapes }));
        }
        expansions.sort_unstable_by(|a, b| a.1.env.cmp(&b.1.env));
        // Crates of one set of variables work a shape's values out alike: one entry for them.
        expansions.dedup_by(|later, kept| {
            let same = later.1.env == kept.1.env;
            if same {
                kept.1.shapes.append(&mut later.1.shapes);
            }
            same
        });
        for (by, it) in &mut expansions {
            it.shapes.sort_unstable();
            it.shapes.dedup();
            if it.shapes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                let name = by.named(db);
                return Err(format!(
                    "two proc-macro expansions in crate {name} read the same names and found other values: \
                     an input changed while it was resolved"
                ));
            }
        }
        Ok(Reads {
            names,
            expansions: expansions.into_iter().map(|(_, it)| it).collect(),
            primary,
            files,
        })
    }

    #[cfg(test)]
    pub(crate) fn named(names: &[&str]) -> Reads {
        let mut names: Vec<String> = names.iter().map(|it| (*it).to_owned()).collect();
        names.sort_unstable();
        names.dedup();
        Reads { names, expansions: Vec::new(), primary: Vec::new(), files: Vec::new() }
    }

    /// The reads of a crate whose code read `CARGO_PRIMARY_PACKAGE`, its package's manifest
    /// `manifest`, and nothing else.
    #[cfg(test)]
    pub(crate) fn primary(manifest: &str) -> Reads {
        Reads {
            names: Vec::new(),
            expansions: Vec::new(),
            primary: vec![manifest.to_owned()],
            files: Vec::new(),
        }
    }

    /// Reads of the files at `files` (absolute paths) with the content IDs read of them.
    #[cfg(test)]
    pub(crate) fn of_files(files: &[(&str, ContentId)]) -> Reads {
        let mut files: Vec<_> =
            files.iter().map(|&(path, id)| (path.to_owned(), Some(id))).collect();
        files.sort_unstable();
        Reads { names: Vec::new(), expansions: Vec::new(), primary: Vec::new(), files }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
            && self.expansions.is_empty()
            && self.primary.is_empty()
            && self.files.is_empty()
    }

    /// The names, as a slot keeps them: [`VERSION`], the variables' count and each, then the
    /// count of sets of crate variables and, for each, its variables' count, each name and
    /// value, its expansions' count and each one's names, then the count of manifests of the
    /// crates that read `CARGO_PRIMARY_PACKAGE` and each, then the files' count and each one's
    /// path. Every string and shape framed by its length, every count and length a `u32` LE.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let names: usize =
            self.names.iter().chain(&self.primary).map(|it| it.len() + 4).sum::<usize>() + 4;
        let files: usize = self.files.iter().map(|(path, _)| path.len() + 4).sum::<usize>() + 4;
        let expansions: usize = (self.expansions.iter())
            .map(|it| {
                8 + it.env.iter().map(|(n, v)| n.len() + v.len() + 8).sum::<usize>()
                    + it.shapes.iter().map(|(s, _)| s.len() + 4).sum::<usize>()
            })
            .sum();
        // alloc: one slot's bytes, once per unit stored that read something
        let mut out = Vec::with_capacity(names + expansions + files + 9);
        out.put_u8(VERSION);
        out.put_u32(self.names.len() as u32);
        for name in &self.names {
            out.put_bytes_u32(name.as_bytes());
        }
        out.put_u32(self.expansions.len() as u32);
        for it in &self.expansions {
            out.put_u32(it.env.len() as u32);
            for (name, value) in &it.env {
                out.put_bytes_u32(name.as_bytes());
                out.put_bytes_u32(value.as_bytes());
            }
            out.put_u32(it.shapes.len() as u32);
            for (shape, _) in &it.shapes {
                out.put_bytes_u32(shape);
            }
        }
        out.put_u32(self.primary.len() as u32);
        for manifest in &self.primary {
            out.put_bytes_u32(manifest.as_bytes());
        }
        out.put_u32(self.files.len() as u32);
        for (path, _) in &self.files {
            out.put_bytes_u32(path.as_bytes());
        }
        out
    }

    /// What [`Reads::encode`] wrote, checked: its version, its lengths against the bytes, and
    /// each part sorted with no repeats. The bytes came from a store, which checked only their
    /// hash.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Reads, &'static str> {
        let short = |_| "a record's reads shorter than their lengths";
        let text = |_| "a record's read variable that is cut short or not UTF-8";
        let mut read = Reader::new(bytes);
        if read.u8().map_err(short)? != VERSION {
            return Err("a record's reads of another version");
        }
        let count = read.count_u32(4).map_err(short)?;
        // alloc: a slot read, once per slot asked
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            names.push(read.str_u32().map_err(text)?.to_owned()); // alloc: as above
        }
        if !names.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err("a record's read variables out of order or repeated");
        }
        let count = read.count_u32(8).map_err(short)?;
        // alloc: as above
        let mut expansions = Vec::with_capacity(count);
        for _ in 0..count {
            let pairs = read.count_u32(8).map_err(short)?;
            let mut env = Vec::with_capacity(pairs); // alloc: as above
            for _ in 0..pairs {
                let name = read.str_u32().map_err(text)?.to_owned(); // alloc: as above
                env.push((name, read.str_u32().map_err(text)?.to_owned())); // alloc: as above
            }
            if !env.windows(2).all(|pair| pair[0].0 < pair[1].0) {
                return Err("a record's crate variables out of order or repeated");
            }
            let shapes = read.count_u32(4).map_err(short)?;
            let mut it = Expansions { env, shapes: Vec::with_capacity(shapes) }; // alloc: as above
            for _ in 0..shapes {
                it.shapes.push((read.bytes_u32().map_err(short)?.to_vec(), None)); // alloc: as above
            }
            if !it.shapes.windows(2).all(|pair| pair[0].0 < pair[1].0) {
                return Err("a record's expansions out of order or repeated");
            }
            expansions.push(it);
        }
        let count = read.count_u32(4).map_err(short)?;
        // alloc: as above
        let mut primary = Vec::with_capacity(count);
        for _ in 0..count {
            primary.push(read.str_u32().map_err(text)?.to_owned()); // alloc: as above
        }
        if !primary.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(
                "a record's crates that read `CARGO_PRIMARY_PACKAGE` out of order or repeated",
            );
        }
        let count = read.count_u32(4).map_err(short)?;
        // alloc: as above
        let mut files = Vec::with_capacity(count);
        for _ in 0..count {
            let path = read
                .str_u32()
                .map_err(|_| "a record's read file whose path is cut short or not UTF-8")?;
            files.push((path.to_owned(), None)); // alloc: as above
        }
        if !files.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            return Err("a record's read files out of order or repeated");
        }
        read.finish().map_err(|_| "bytes after a record's reads")?;
        if !expansions.windows(2).all(|pair| pair[0].env < pair[1].env) {
            return Err("a record's sets of crate variables out of order or repeated");
        }
        Ok(Reads { names, expansions, primary, files })
    }

    /// The hash of the values the reads had as they were made: the variables' in `env` (the
    /// job's environment, `name=value` pairs), `CARGO_PRIMARY_PACKAGE` as `primary` (the job's
    /// [`blaze_units::query::Units::is_primary_manifest`]) has it, each expansion's as it
    /// reported them, each file's content ID as the front end read it. An error for reads
    /// decoded from a slot, which know no values.
    fn values_then(
        &self,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
    ) -> Result<ContentId, String> {
        let unknown = || "a record's reads read from the store, filed as if made here".to_owned();
        self.hash(
            env,
            primary,
            |_, _, then| then.ok_or_else(unknown),
            |_, then| then.ok_or_else(unknown),
        )
    }

    /// The hash of the values the reads have now: the variables' in `env`, the
    /// `CARGO_PRIMARY_PACKAGE` reads' as `primary` has them, each expansion's names worked out
    /// again by `now` with what its crate's expansions are given now: its variables, and
    /// `CARGO_PRIMARY_PACKAGE` when its package is primary ([`base_db::given_macro_env`]),
    /// each file's content ID that `file_now` reads of it.
    fn values_now(
        &self,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
        mut now: impl FnMut(&base_db::Env, &[u8]) -> Result<ContentId, String>,
        mut file_now: impl FnMut(&str) -> ContentId,
    ) -> Result<ContentId, String> {
        let given: Vec<base_db::Env> = (self.expansions.iter())
            .map(|it| {
                let manifest = it.env.iter().find(|(name, _)| name == "CARGO_MANIFEST_PATH");
                // clone: a crate's variables, once per slot asked that holds expansions
                let own: base_db::Env = it.env.iter().cloned().collect();
                base_db::given_macro_env(&own, manifest.is_some_and(|(_, path)| primary(path)))
                    .into_owned()
            })
            .collect();
        self.hash(env, primary, |at, shape, _| now(&given[at], shape), |path, _| Ok(file_now(path)))
    }

    /// Each read by name with the id of its value, in the order [`Reads::hash`] hashes them,
    /// for a person: what `blaze why` and the `unit_reads` view name when a record whose key
    /// is as it was is not found (the values `expansion` and `file` give: those the reads
    /// had as they were made, or those they have now). An expansion is named by what its
    /// shape reads ([`shape_said`]).
    pub(crate) fn named_values(
        &self,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
        mut expansion: impl FnMut(usize, &[u8], Option<ContentId>) -> Result<ContentId, String>,
        mut file: impl FnMut(&str, Option<ContentId>) -> Result<ContentId, String>,
    ) -> Result<Vec<(String, ContentId)>, String> {
        let var =
            |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_bytes());
        let value = |it: Option<&[u8]>| Hasher::new(DOMAIN).opt_part(it).finish();
        // alloc: once a record filed or missed, a read each
        let mut out = Vec::new();
        for name in &self.names {
            out.push((format!("the process variable {name}"), value(var(name)))); // alloc: as above
        }
        for manifest in &self.primary {
            let it = match primary(manifest) {
                true => Some(b"1".as_slice()),
                false => var(base_db::PRIMARY_PACKAGE),
            };
            // alloc: as above
            out.push((format!("CARGO_PRIMARY_PACKAGE of {manifest}"), value(it)));
        }
        for (at, it) in self.expansions.iter().enumerate() {
            let of = it
                .env
                .iter()
                .find(|(name, _)| name == "CARGO_MANIFEST_PATH")
                .map_or("a crate with no CARGO_MANIFEST_PATH", |(_, path)| path.as_str());
            for (shape, then) in &it.shapes {
                // alloc: as above
                let name = format!("a proc-macro expansion of {of} that read {}", shape_said(shape));
                out.push((name, expansion(at, shape, *then)?));
            }
        }
        for (path, then) in &self.files {
            out.push((format!("the included file {path}"), file(path, *then)?)); // alloc: as above
        }
        Ok(out)
    }

    /// [`Reads::named_values`] with the values the reads had as they were made; an error for
    /// reads decoded from a slot.
    pub(crate) fn named_then(
        &self,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<(String, ContentId)>, String> {
        let unknown = || "a record's reads read from the store, named as if made here".to_owned();
        self.named_values(
            env,
            primary,
            |_, _, then| then.ok_or_else(unknown),
            |_, then| then.ok_or_else(unknown),
        )
    }

    /// The hash of the values `env` gives the variables, an unset one apart from every value,
    /// then `CARGO_PRIMARY_PACKAGE` of each crate that read it (`1` when `primary` says its
    /// package is primary, else the process's, as [`base_db::env_var`] reads it), then of each
    /// expansion's that `expansion` gives, with the index of its set of variables, then of each
    /// file's content ID that `file` gives.
    fn hash(
        &self,
        env: &[(String, String)],
        primary: &dyn Fn(&str) -> bool,
        mut expansion: impl FnMut(usize, &[u8], Option<ContentId>) -> Result<ContentId, String>,
        mut file: impl FnMut(&str, Option<ContentId>) -> Result<ContentId, String>,
    ) -> Result<ContentId, String> {
        let var =
            |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_bytes());
        let mut hasher = Hasher::new(DOMAIN);
        hasher.part(b"values");
        // Every lookup and every filing of a record by its reads passes here: the names
        // are noted for the no-op lane, which compares them of a later request.
        note_named(&self.names);
        note_paths(self);
        for name in &self.names {
            hasher.opt_part(var(name));
        }
        hasher.part(b"primary");
        for manifest in &self.primary {
            let value = match primary(manifest) {
                true => Some(b"1".as_slice()),
                false => var(base_db::PRIMARY_PACKAGE),
            };
            hasher.part(manifest.as_bytes()).opt_part(value);
        }
        for (at, it) in self.expansions.iter().enumerate() {
            for (shape, then) in &it.shapes {
                hasher.id(&expansion(at, shape, *then)?);
            }
        }
        hasher.part(b"files");
        for (path, then) in &self.files {
            hasher.part(path.as_bytes()).id(&file(path, *then)?);
        }
        Ok(hasher.finish())
    }
}

/// What an expansion's shape reads, for a person: its files, listings and variables by name
/// (the first few of each), or that it is of an expansion verified across checkouts, whose
/// value moves when the record that retracts it is put.
fn shape_said(shape: &[u8]) -> String {
    const SHOWN: usize = 4;
    const VERIFIED: &[u8] = b"blaze v2 verified expansion reads 1\0";
    let (verified, rest) = match shape.strip_prefix(VERIFIED) {
        Some(rest) if rest.len() >= 32 => (true, &rest[32..]),
        _ => (false, shape),
    };
    let listed = |what: &str, all: Vec<String>| match all.len() {
        0 => String::new(), // alloc: none
        // alloc: once a read named
        n if n > SHOWN => format!(" {what} {} and {} more;", all[..SHOWN].join(", "), n - SHOWN),
        _ => format!(" {what} {};", all.join(", ")), // alloc: as above
    };
    let said = match blaze_usercode::index::Shape::decode(rest) {
        Ok(it) => {
            let paths = |all: &[std::path::PathBuf]| -> Vec<String> {
                all.iter().map(|it| it.display().to_string()).collect() // alloc: as above
            };
            let env = it.env.iter().map(|it| it.to_string_lossy().into_owned()).collect();
            let untraceable = it.untraceable.iter().map(|it| format!("{it:?}")).collect();
            let all = [
                listed("files", paths(&it.files)),
                listed("listings", paths(&it.dirs)),
                listed("variables", env),
                listed("untraceable", untraceable),
            ]
            .concat();
            match all.is_empty() {
                true => "nothing by name".to_owned(), // alloc: as above
                false => all.trim().trim_end_matches(';').to_owned(), // alloc: as above
            }
        }
        // alloc: as above
        Err(_) => format!("a shape of {} bytes that does not decode", rest.len()),
    };
    match verified {
        // alloc: as above
        true => format!("{said} (verified across checkouts: its value moves when it is retracted)"),
        false => said,
    }
}

/// Each slot of `base` that `get` gives with its reads named and the values they have now
/// ([`Reads::named_values`]), as [`find`] works them out: what a record whose key is as it
/// was and that is not found was looked for under. A slot that does not decode, or whose
/// values can't be worked out, is an error naming it.
pub(crate) fn looked_under<'g>(
    base: Key,
    env: &[(String, String)],
    primary: &dyn Fn(&str) -> bool,
    mut now: impl FnMut(&base_db::Env, &[u8]) -> Result<ContentId, String>,
    mut file_now: impl FnMut(&str) -> ContentId,
    mut get: impl FnMut(&Key) -> Option<&'g [u8]>,
) -> Result<Vec<(u32, Vec<(String, ContentId)>)>, String> {
    let mut out = Vec::new(); // alloc: once a miss, a slot each
    for n in 0..SLOTS {
        let Some(record) = get(&slot(base, n)) else { break };
        let reads = Reads::decode(record)
            .map_err(|why| format!("slot {n} of record key {base:?}: {why}"))?;
        let given: Vec<base_db::Env> = (reads.expansions.iter())
            .map(|it| {
                let manifest = it.env.iter().find(|(name, _)| name == "CARGO_MANIFEST_PATH");
                // clone: a crate's variables, once per slot named
                let own: base_db::Env = it.env.iter().cloned().collect();
                base_db::given_macro_env(&own, manifest.is_some_and(|(_, path)| primary(path)))
                    .into_owned()
            })
            .collect();
        let named = reads
            .named_values(
                env,
                primary,
                |at, shape, _| now(&given[at], shape),
                |path, _| Ok(file_now(path)),
            )
            .map_err(|why| format!("slot {n} of record key {base:?}: {why}"))?;
        out.push((n, named));
    }
    Ok(out)
}

/// Slot `n` of base key `base`: where the `n`th set of names its records read is kept.
pub(crate) fn slot(base: Key, n: u32) -> Key {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"slot").id(&base.0).update(&n.to_le_bytes());
    Key(hasher.finish())
}

/// Where the record of base key `base` is for the reads `record` (a slot's bytes) with the
/// values `values`.
pub(crate) fn filed(base: Key, record: &[u8], values: ContentId) -> Key {
    let mut hasher = Hasher::new(DOMAIN);
    hasher.part(b"record").id(&base.0).part(record).id(&values);
    Key(hasher.finish())
}

/// The first key `found` says holds a record: `base`, then each slot's of `base` that `get`
/// gives, its names with the values they have now: a variable's that `env` gives it,
/// `CARGO_PRIMARY_PACKAGE`'s as `primary` says each package is primary, an expansion's that
/// `now` works out with its crate's variables (the daemon's
/// `proc_macro_api::kept::Keeping::reads_now`), a file's content ID that `file_now` reads of
/// it ([`base_db::IncludedFile::content_now`]).
/// None when none does; an error naming the slot when a slot does not decode or `now` fails.
pub(crate) fn find<'g>(
    base: Key,
    env: &[(String, String)],
    primary: &dyn Fn(&str) -> bool,
    mut now: impl FnMut(&base_db::Env, &[u8]) -> Result<ContentId, String>,
    mut file_now: impl FnMut(&str) -> ContentId,
    mut get: impl FnMut(&Key) -> Option<&'g [u8]>,
    mut found: impl FnMut(&Key) -> bool,
) -> Result<Option<Found>, String> {
    if found(&base) {
        return Ok(Some(Found { key: base, slot: None }));
    }
    for n in 0..SLOTS {
        let at = slot(base, n);
        let Some(record) = get(&at) else { break };
        let reads = Reads::decode(record)
            .map_err(|why| format!("slot {n} of record key {base:?}: {why}"))?;
        let values = reads
            .values_now(env, primary, &mut now, &mut file_now)
            .map_err(|why| format!("slot {n} of record key {base:?}: {why}"))?;
        let key = filed(base, record, values);
        if found(&key) {
            return Ok(Some(Found { key, slot: Some(at) }));
        }
    }
    Ok(None)
}

/// Where [`find`] found a record: its key, and the slot whose names it was filed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) key: Key,
    pub(crate) slot: Option<Key>,
}

/// Where a record made with the reads `reads` goes, the values being those they had as they
/// were made (a variable's that `env` gives it, `CARGO_PRIMARY_PACKAGE`'s as `primary` has it):
/// the key, and the slot to keep the names in when no slot of `base` holds them yet. None when
/// every slot holds other names, so the record is not kept. An error for reads read from the
/// store, which know no values.
pub(crate) fn file<'g>(
    base: Key,
    reads: &Reads,
    env: &[(String, String)],
    primary: &dyn Fn(&str) -> bool,
    mut get: impl FnMut(&Key) -> Option<&'g [u8]>,
) -> Result<Option<Filing>, String> {
    if reads.is_empty() {
        return Ok(Some(Filing { key: base, slot: None }));
    }
    let record = reads.encode();
    let key = filed(base, &record, reads.values_then(env, primary)?);
    for n in 0..SLOTS {
        let at = slot(base, n);
        match get(&at) {
            Some(kept) if kept == record.as_slice() => return Ok(Some(Filing { key, slot: None })),
            Some(_) => continue,
            None => return Ok(Some(Filing { key, slot: Some((at, record)) })),
        }
    }
    Ok(None)
}

/// Where [`file`] puts a record: its key, and the slot and bytes of its names when they are
/// kept now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Filing {
    pub(crate) key: Key,
    pub(crate) slot: Option<(Key, Vec<u8>)>,
}
