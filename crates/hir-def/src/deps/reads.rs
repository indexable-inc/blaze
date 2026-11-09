//! What a crate reads that its key ([`super::key`]) cannot name, because it is known only once
//! the crate is resolved: the process variables its `env!` and `option_env!` calls read (those
//! the crate's own variables do not have), and what each of its proc-macro expansions read
//! besides its call (files, listings, variables of the shell), as the expansion server traced
//! it.
//!
//! A crate's [`Reads`] are kept beside its image the way a run of user code keeps its shapes
//! (`blaze_usercode::index`): the names it read, without their values, in the first free of
//! [`SLOTS`] slots under the crate's key ([`slot`]), and the image under the key, those names
//! and the values they had ([`image_key`]). Serving asks each slot in turn, works out the
//! values of its names now, and gets the image they name: one get when nothing changed, and an
//! image is never used once a value it was made from is another. A crate that reads nothing of
//! the kind has its image under its key alone and no slot.
//!
//! A crate's reads hold its dependencies' expansions' too ([`Reads::deps`]): its image holds
//! what they made, and no key covers what they read.
//!
//! `CARGO_PRIMARY_PACKAGE` is of the request, in no crate's key ([`base_db::PRIMARY_PACKAGE`]):
//! each crate of the closure that read it is among the reads by its place in the closure
//! ([`Reads::primary`]), valued as that crate reads it now.
//!
//! An expansion whose reads are not known (one its server could not trace, one that read the
//! clock, one that asked the database about a span, one of a macro whose session does not
//! settle or that moved its session's state on) makes
//! the crate's image leave out an input, so the crate is refused an image ([`Refused`]).

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_format::cursor::{Put, Reader};
use blaze_hash::HashSet;
use blaze_ids::{ContentId, Hasher};
use hir_expand::{HirFileId, MacroCallId, MacroDefKind, proc_macro::ExpansionReads};
use std::alloc::Global;

use crate::{
    Lookup,
    deps::serve::{ImageStore, Refused},
    nameres::crate_def_map,
};

/// The domain of the slots, the images filed by reads, and the reads' encoding.
/// 2: a crate's reads hold its dependencies' ([`Reads::deps`]). 3: and which crates read
/// `CARGO_PRIMARY_PACKAGE` ([`Reads::primary`]).
pub const READS_DOMAIN: &str = "blaze v2 dependency image reads 3";

/// How many sets of names one key keeps: a crate's expansions read other names only as what
/// they read decides it, so one or two in practice.
pub const SLOTS: u32 = 16;

/// The first byte of a slot's bytes: the version of [`Reads::encode`].
const VERSION: u8 = 3;

/// What a crate read besides the inputs of its key: each sorted, with no repeats.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reads {
    /// The process variables `env!` and `option_env!` read.
    env: Vec<Box<str>>,
    /// Each expansion's names, as its server encodes them, with the hash of their values when
    /// it ran: known for reads made in this database, not for reads decoded from a slot.
    macros: Vec<(Arc<[u8]>, Option<ContentId>)>,
    /// What the crates it depends on read that their keys do not cover, as [`Reads::macros`]:
    /// each by its index in the crate's closure ([`Crate::transitive_deps`], which the
    /// crate's key fixes, since it covers every dependency's key in order), its expansions'
    /// names worked out with that crate's variables. A crate's image holds what their
    /// expansions made (a name it imports from a derive's or an attribute's output, the
    /// location of an item in one), and their keys cover only what they were given, never
    /// what their expansions read, nor whether a value verified across checkouts was
    /// retracted since; so without these an image would be served once a dependency's
    /// expansion makes something else (rule 21). Only [`Reads::of`] (an image) has them: a
    /// check's reads ([`super::checked_reads`]) are its own.
    deps: Vec<(u32, Vec<(Arc<[u8]>, Option<ContentId>)>)>,
    /// The crates of the closure whose `env!` or `option_env!` read `CARGO_PRIMARY_PACKAGE`
    /// ([`base_db::PRIMARY_PACKAGE`]), each by its index in the crate's closure as
    /// [`Reads::deps`]: its value is the crate's own (whether it is primary), then the
    /// process's, and no key covers either.
    primary: Vec<u32>,
}

impl Reads {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty()
            && self.macros.is_empty()
            && self.deps.is_empty()
            && self.primary.is_empty()
    }

    /// The crates that read `CARGO_PRIMARY_PACKAGE` ([`Reads::primary`]), by their index in the
    /// closure of the crate whose reads these are.
    pub fn primary(&self) -> &[u32] {
        &self.primary
    }

    /// The names read: the process variables `env!` and `option_env!` read, and each
    /// expansion's names as its server encodes them (an `ExpansionReads::Traced` shape), each
    /// sorted; then its dependencies' expansions' ([`Reads::deps`]), what a rule for public
    /// keys looks at too.
    pub fn names(&self) -> (&[Box<str>], impl Iterator<Item = &[u8]>) {
        let deps =
            self.deps.iter().flat_map(|(_, shapes)| shapes.iter().map(|(shape, _)| &**shape));
        (&self.env, self.macros.iter().map(|(shape, _)| &**shape).chain(deps))
    }

    /// The reads of `krate` as it is resolved in `db`: the proc-macro expansions of every macro
    /// call of its modules, of `calls` (those its signatures name,
    /// [`super::signatures::encode`]) and of each call any of them is in the expansion of; and
    /// every process variable the database noted the crate's `env!`s read. Then what each
    /// crate it depends on read, the same way ([`Reads::deps`]).
    pub(crate) fn of(db: &Db, krate: Crate, calls: &[MacroCallId]) -> Result<Reads, Refused> {
        let mut reads = Reads::own(db, krate, calls)?;
        reads.dependencies(db, krate)?;
        Ok(reads)
    }

    /// [`Reads::of`] but for what its dependencies read.
    fn own(db: &Db, krate: Crate, calls: &[MacroCallId]) -> Result<Reads, Refused> {
        Reads::of_calls(db, krate, image_calls(db, krate, calls))
    }

    /// Adds what each crate `krate` depends on, directly or not, read ([`Reads::deps`]): a
    /// crate served from its image what serving it noted (the reads its image was filed by,
    /// `filed::search`); one resolved here its own reads as [`Reads::of`] works them out, its
    /// modules' expansions, which its def map ran. An expansion of a dependency's whose reads
    /// are not known is a refusal naming it, as one of the crate's own is: what it made is in
    /// the crate's image, and no key covers it.
    fn dependencies(&mut self, db: &Db, krate: Crate) -> Result<(), Refused> {
        for (at, &dep) in krate.transitive_deps(db).iter().enumerate() {
            if dep == krate {
                continue;
            }
            let read = match super::image_of(db, dep) {
                Some(image) => served(db, dep, image.base)?,
                None => Reads::own(db, dep, &[])?,
            };
            self.env.extend(read.env);
            let at = u32::try_from(at).expect("fewer than 2^32 crates");
            if !read.primary.is_empty() {
                self.primary.push(at);
            }
            if !read.macros.is_empty() {
                self.deps.push((at, read.macros));
            }
        }
        self.env.sort_unstable();
        self.env.dedup();
        self.primary.sort_unstable();
        self.primary.dedup();
        Ok(())
    }

    /// The reads of `krate` from the proc-macro expansions of `todo` and of each call any of
    /// them is in the expansion of, and every process variable the database noted the crate's
    /// `env!`s read.
    pub(crate) fn of_calls(
        db: &Db,
        krate: Crate,
        todo: Vec<MacroCallId>,
    ) -> Result<Reads, Refused> {
        let own = krate.env(db);
        let mut reads = Reads::default();
        each_call(db, krate, todo, |call, loc| {
            if !matches!(loc.def.kind, MacroDefKind::ProcMacro(..)) {
                return Ok(());
            }
            match call.expansion_reads(db) {
                ExpansionReads::Nothing => {}
                ExpansionReads::Traced { shape, values } => {
                    // clone: a count of the expansion's names, once per expansion
                    reads.macros.push((shape.clone(), Some(*values)));
                }
                &ExpansionReads::Unknown(why) => {
                    // alloc: why a crate is refused, once per refused crate
                    let call = format!("{:?} of crate {}", call, loc.def.krate.named(db));
                    return Err(Refused::ProcMacro { call, why });
                }
            }
            Ok(())
        })?;
        // Every `env!` and `option_env!` expansion notes its variable as it runs, nested ones
        // too, and the log is never cleared: what the crate's resolution read is in it.
        env_reads(db, krate, own, &mut reads);
        reads.env.sort_unstable();
        reads.env.dedup();
        reads.macros.sort_unstable();
        reads.macros.dedup();
        if reads.macros.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Refused::Reads {
                why: "two expansions read the same names and found other values: an input \
                      changed while the crate was resolved",
            });
        }
        Ok(reads)
    }

    /// The names, as a slot keeps them: the variables, then the expansions' names, each
    /// framed by its length.
    pub fn encode(&self) -> Vec<u8> {
        let len = self.env.iter().map(|it| it.len() + 4).sum::<usize>()
            + self.macros.iter().map(|it| it.0.len() + 4).sum::<usize>();
        // alloc: one slot's bytes, once per crate imaged
        let mut out = Vec::with_capacity(len + 9);
        let count = |len: usize| u32::try_from(len).expect("fewer than 2^32 reads");
        out.put_u8(VERSION);
        out.put_u32(count(self.env.len()));
        for name in &self.env {
            out.put_bytes_u32(name.as_bytes());
        }
        out.put_u32(count(self.macros.len()));
        for (shape, _) in &self.macros {
            out.put_bytes_u32(shape);
        }
        out.put_u32(count(self.deps.len()));
        for (at, shapes) in &self.deps {
            out.put_u32(*at);
            out.put_u32(count(shapes.len()));
            for (shape, _) in shapes {
                out.put_bytes_u32(shape);
            }
        }
        out.put_u32(count(self.primary.len()));
        for at in &self.primary {
            out.put_u32(*at);
        }
        out
    }

    /// What [`Reads::encode`] wrote, its lengths checked against the bytes, its order and its
    /// names checked: the bytes came from a store, which checked only their hash.
    pub fn decode(bytes: &[u8]) -> Result<Reads, &'static str> {
        let short = |_| "reads shorter than their lengths";
        let mut read = Reader::new(bytes);
        if read.u8().map_err(short)? != VERSION {
            return Err("reads of another version");
        }
        let mut reads = Reads::default();
        for _ in 0..read.count_u32(4).map_err(short)? {
            let name = std::str::from_utf8(read.bytes_u32().map_err(short)?)
                .map_err(|_| "a variable not UTF-8")?;
            reads.env.push(name.into()); // alloc: setup: a slot read, once per crate per session
        }
        for _ in 0..read.count_u32(4).map_err(short)? {
            reads.macros.push((read.bytes_u32().map_err(short)?.into(), None)); // alloc: setup, as above
        }
        for _ in 0..read.count_u32(8).map_err(short)? {
            let at = read.u32().map_err(short)?;
            let count = read.count_u32(4).map_err(short)?;
            // alloc: setup, as above
            let mut shapes: Vec<(Arc<[u8]>, Option<ContentId>)> = Vec::with_capacity(count);
            for _ in 0..count {
                shapes.push((read.bytes_u32().map_err(short)?.into(), None));
            }
            if shapes.is_empty() || !shapes.windows(2).all(|pair| pair[0].0 < pair[1].0) {
                return Err("a dependency's expansions empty, out of order or repeated");
            }
            reads.deps.push((at, shapes));
        }
        for _ in 0..read.count_u32(4).map_err(short)? {
            reads.primary.push(read.u32().map_err(short)?);
        }
        read.finish().map_err(|_| "bytes after the reads")?;
        if !reads.primary.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err("the crates that read `CARGO_PRIMARY_PACKAGE` out of order");
        }
        if !reads.deps.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            return Err("dependencies out of order");
        }
        if !reads.env.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err("variables out of order");
        }
        if !reads.macros.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            return Err("expansions out of order");
        }
        Ok(reads)
    }

    /// The values the reads had as `db` resolved `krate`, whose they are: what the image is
    /// filed by.
    pub(crate) fn values_then(&self, db: &Db, krate: Crate) -> ContentId {
        self.values(db, krate, |_, _, then| {
            Ok(then.expect("reads made in this database know their values"))
        })
        .expect("values made in this database do not fail")
    }

    /// The values the reads of `krate` have now: the process variables in `db`, each
    /// expansion's names worked out again by the driver ([`ImageStore::reads_now`]) with the
    /// crate's variables, which its expansions were given, and a dependency's
    /// ([`Reads::deps`]) with that dependency's. `each` is given every one of the crate's own
    /// expansions' names with their values' hash as it is worked out: a dependency's are its
    /// own crate's, noted as that crate is served or resolved.
    pub(crate) fn values_now(
        &self,
        db: &Db,
        krate: Crate,
        // dyn: the driver's store, once per slot read
        store: &dyn ImageStore,
        mut each: impl FnMut(&[u8], ContentId),
    ) -> Result<ContentId, String> {
        // What the expansions were given ([`base_db::macro_env`]).
        let env = base_db::macro_env(db, krate);
        self.values(db, krate, |by, shape, _| {
            if by != krate {
                return store.reads_now(base_db::macro_env(db, by), shape);
            }
            let values = store.reads_now(env, shape)?;
            each(shape, values);
            Ok(values)
        })
    }

    /// The hash of the values of the reads of `krate`, each expansion's as `expansion` gives
    /// it for the crate it is of, the names and the hash it ran with.
    fn values(
        &self,
        db: &Db,
        krate: Crate,
        mut expansion: impl FnMut(Crate, &[u8], Option<ContentId>) -> Result<ContentId, String>,
    ) -> Result<ContentId, String> {
        let mut hasher = Hasher::new(READS_DOMAIN);
        hasher.part(b"values");
        for name in &self.env {
            hasher.opt_part(base_db::process_env_var(db, name));
        }
        for (shape, then) in &self.macros {
            hasher.id(&expansion(krate, shape, *then)?);
        }
        if self.deps.is_empty() && self.primary.is_empty() {
            return Ok(hasher.finish());
        }
        let closure = krate.transitive_deps(db);
        let at_closure = |at: u32| {
            closure.get(at as usize).copied().ok_or_else(|| {
                // alloc: error path
                format!(
                    "reads of dependency {at} of crate {}, whose closure has {} crates: the key leaves out an input",
                    krate.named(db),
                    closure.len()
                )
            })
        };
        hasher.part(b"primary");
        for &at in &self.primary {
            hasher.update(&at.to_le_bytes()).opt_part(base_db::env_var(
                db,
                at_closure(at)?,
                base_db::PRIMARY_PACKAGE,
            ));
        }
        hasher.part(b"dependencies");
        for (at, shapes) in &self.deps {
            let dep = at_closure(*at)?;
            hasher.update(&at.to_le_bytes());
            for (shape, then) in shapes {
                hasher.id(&expansion(dep, shape, *then)?);
            }
        }
        Ok(hasher.finish())
    }
}

/// The macro calls an image of `krate` is made from, before [`each_call`] adds those they are
/// expanded in: `calls` (those its signatures name, [`super::signatures::encode`]) and every
/// macro call of its modules' scopes, those at assoc-item position in their traits and impls
/// too ([`super::signatures::scope_calls`]). One list for what the image's reads cover
/// ([`Reads::of`]) and the files it may read (`serve::include_refusal`), so the two never walk
/// different calls.
pub(super) fn image_calls(db: &Db, krate: Crate, calls: &[MacroCallId]) -> Vec<MacroCallId> {
    // alloc: setup: the calls of one crate, walked once as its image is written
    let mut todo: Vec<MacroCallId> = calls.to_vec();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        todo.extend(super::signatures::scope_calls(db, module.scope(db)));
    }
    todo
}

/// Runs `each` once for every call of `todo` that `krate` makes and every call one of them is in
/// the expansion of, with its location; stops at the first error. An eager macro's argument is
/// expanded as a call of its own (`eager: None`), the parent of the calls inside it: `each`
/// sees it too, and tells it from the real call by its kind.
pub(super) fn each_call<E>(
    db: &Db,
    krate: Crate,
    mut todo: Vec<MacroCallId>,
    mut each: impl FnMut(MacroCallId, &hir_expand::MacroCallLoc) -> Result<(), E>,
) -> Result<(), E> {
    let mut seen = HashSet::new_in(Global);
    while let Some(call) = todo.pop() {
        if !seen.insert(call) {
            continue;
        }
        let loc = call.lookup(db);
        if loc.krate != krate {
            continue;
        }
        if let HirFileId::MacroFile(parent) = loc.kind.file_id() {
            todo.push(parent);
        }
        each(call, loc)?;
    }
    Ok(())
}

/// What `krate`, served from its image under the key `base`, was filed by, as serving it
/// noted ([`super::filed::search`]): the process variables its `env!`s read and its
/// expansions' names, with the values they had.
fn served(db: &Db, krate: Crate, base: ContentId) -> Result<Reads, Refused> {
    let own = krate.env(db);
    let mut reads = Reads::default();
    env_reads(db, krate, own, &mut reads);
    for read in db.files().expansions_read_by(krate, base_db::ReadBy::Served(base)) {
        match read {
            base_db::ExpansionRead::Traced { shape, values } => {
                reads.macros.push((shape.into(), Some(values)))
            }
            // Never noted for a value served: it was filed by its reads, all traced.
            base_db::ExpansionRead::Unknown(why) => {
                // alloc: why a crate is refused, once per refused crate
                let call =
                    format!("an expansion of crate {}, served from its image", krate.named(db));
                return Err(Refused::ProcMacro { call, why });
            }
        }
    }
    reads.env.sort_unstable();
    reads.env.dedup();
    reads.macros.sort_unstable();
    reads.macros.dedup();
    if reads.macros.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(Refused::Reads {
            why: "a dependency was served by two values of the same names",
        });
    }
    Ok(reads)
}

/// What `krate`'s `env!`s read as the `env!` log has it, into `reads`: each process variable
/// its own variables `own` lack, and `CARGO_PRIMARY_PACKAGE` as the crate's own read of it
/// ([`Reads::primary`], at its own place in its closure).
fn env_reads(db: &Db, krate: Crate, own: &base_db::Env, reads: &mut Reads) {
    for (read, name) in db.files().env_read() {
        if read != krate {
            continue;
        }
        if name == base_db::PRIMARY_PACKAGE {
            let at = krate
                .transitive_deps(db)
                .iter()
                .position(|it| *it == krate)
                .expect("a crate's closure holds it");
            reads.primary = vec![u32::try_from(at).expect("fewer than 2^32 crates")]; // alloc: one index, once per crate that read it
        } else if own.get(&name).is_none() {
            reads.env.push(name.into_boxed_str());
        }
    }
}

/// Slot `n` of the crate whose key is `key`: where the `n`th set of names it read is kept.
pub fn slot(key: ContentId, n: u32) -> ContentId {
    let mut hasher = Hasher::new(READS_DOMAIN);
    hasher.part(b"slot").id(&key).update(&n.to_le_bytes());
    hasher.finish()
}

/// Where the image of the crate whose key is `key` is, for the reads `record` (a slot's bytes)
/// and the values `values` they have.
pub fn image_key(key: ContentId, record: &[u8], values: ContentId) -> ContentId {
    let mut hasher = Hasher::new(READS_DOMAIN);
    hasher.part(b"image").id(&key).part(record).id(&values);
    hasher.finish()
}

#[cfg(test)]
impl Reads {
    pub(super) fn for_tests(env: &[&str], macros: &[(&[u8], ContentId)]) -> Reads {
        let mut reads = Reads {
            env: env.iter().map(|&it| it.into()).collect(),
            macros: macros.iter().map(|&(shape, values)| (shape.into(), Some(values))).collect(),
            deps: Vec::new(),
            primary: Vec::new(),
        };
        reads.env.sort_unstable();
        reads.macros.sort_unstable();
        reads
    }

    /// These reads with `deps` as what the crates of the closure read ([`Reads::deps`]) and
    /// `primary` as the crates that read `CARGO_PRIMARY_PACKAGE` ([`Reads::primary`]), each
    /// given in order.
    pub(super) fn with_deps_for_tests(
        mut self,
        deps: &[(u32, &[&[u8]])],
        primary: &[u32],
    ) -> Reads {
        self.deps = deps
            .iter()
            .map(|&(at, shapes)| (at, shapes.iter().map(|&shape| (shape.into(), None)).collect()))
            .collect();
        self.primary = primary.to_vec();
        self
    }

    /// Every part of the reads, their values left out: the variables, the expansions' names,
    /// each dependency's by its index ([`Reads::deps`]), and [`Reads::primary`].
    pub(super) fn parts_for_tests(&self) -> super::tests::ReadsParts {
        let shapes = |shapes: &[(Arc<[u8]>, Option<ContentId>)]| -> Vec<Box<[u8]>> {
            shapes.iter().map(|(shape, _)| Box::<[u8]>::from(&**shape)).collect()
        };
        (
            self.env.clone(),
            shapes(&self.macros),
            self.deps.iter().map(|(at, deps)| (*at, shapes(deps))).collect(),
            self.primary.clone(),
        )
    }
}
