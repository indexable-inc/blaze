//! Content keys (docs/v2/plan/design/store.md, "Content keys"): what makes a unit's record
//! key, and the record's own bytes, say nothing of where the checkout, its build directory, the
//! machine's shared standard library directory or blaze's own programs are, so the same
//! sources built in another directory, under another home, by another user or on another
//! machine of the same target find the record.
//!
//! The directories are the build's [`Places`], each a root of blaze-usercode's roots
//! ([`blaze_usercode::roots`], the one home of writing a path against what it is under): the
//! workspace, the build directory, the shared standard library directory, blaze's tools, its
//! launcher and where it keeps the packages it fetches. Three things are written against them:
//!
//! - **The key.** Every string a key encodes ([`Places::cbor`], the unit, its plan, how its
//!   program is run, what its build scripts printed) and every path the front end's identity
//!   of the crate holds ([`base_db::Port`], which [`Places`] is) has each root in it as its
//!   placeholder. The key is also given how the roots sit to each other ([`Places::layout`]):
//!   a relative path from one to another is the same string only where they do.
//! - **The record.** Every string of its body and each native input's path is written the same
//!   way, and written back against the finding build's roots as the record is read
//!   ([`Places::cbor_real`]): a record names its outputs where the build that finds it places
//!   them.
//! - **What ties a record to where it was made.** Once written so, the record's bytes and the
//!   bytes of each file it names are searched for every root, in any spelling
//!   ([`Places::tied`], `Roots::embedded`). A record or an output that still holds one (a
//!   string constant made from `env!("CARGO_MANIFEST_DIR")`, a diagnostic that quotes an
//!   absolute path, an image with a search path linked in) is kept under its key *here*
//!   ([`Places::here`]: the key and each root's real path), which only a build with the same
//!   roots asks. Nothing is guessed about what a crate does with a path: what it made is read.
//!   A record whose bytes hold only roots every checkout of one home shares ([`HOME`]: a
//!   registry crate's object code naming its files under the fetched sources, a build
//!   script's program) is kept under its key *in this home* ([`Places::home`]: the key and
//!   those roots' real paths alone, [`Tied::Home`]), which every checkout built by a daemon
//!   of this home asks: what ties it is where those roots are, never where the checkout is.
//!
//! What this does not cover, and so is never shared (rule keys-cover-every-input): a crate
//! whose code read a path it was given and made something that depends on the path without
//! holding it (its length, a hash of it). The reads that could are a crate's own variables
//! whose value holds a root, read through `env!` or `option_env!`, or given to a proc macro:
//! [`super::reads::Reads`] names them, and a record whose closure made one is kept here.
//!
//! Where blaze keeps fetched packages (`<home>/sources`, [`Role::Sources`]) is a root like the
//! others (2026-11-12): a registry crate's key says nothing of the home, and its front-end
//! records (`hir_def::nameres::frozen`, which writes a literal's paths with this port and
//! refuses bytes that still hold a root) are every home's. A registry crate's code says its
//! files' names by their real path (`file!()`, a panic's location), so its unit record, whose
//! object code holds that path, is tied ([`Places::tied`]) and kept here ([`Places::here`]):
//! one home's, by what it holds and not by its key.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use blaze_cas::Key;
use blaze_ids::Hasher;
use blaze_usercode::roots::{Role, RoleSet, Roots, port::Dirs};

use super::Job;

/// The roots every checkout one daemon builds shares: blaze's tools and launcher, the
/// machine's shared standard library directory and where the home keeps fetched packages.
/// The workspace and the build directory (`<home>/build/<name>-<id of the checkout>`) are
/// each checkout's own.
pub(crate) const HOME: [Role; 4] = [Role::Tools, Role::Launcher, Role::Std, Role::Sources];

/// How far a record tied to the build's directories reaches: which key it is kept under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tied {
    /// What it holds is only roots of [`HOME`], in any spelling ([`Places::held`]): kept under
    /// [`Places::home`], which another checkout of the same home asks too. Its bytes are the
    /// same there: they are made from what the key covers, with the checkout's roots written
    /// as placeholders, and what holds none of the checkout's roots holds nothing of where
    /// it is.
    Home,
    /// Anything else: a root of the checkout's held (or a path between roots of two tops,
    /// which `Roots::embedded` names both of), bytes not read again, or a path the closure's
    /// code was given ([`path_reads`]): kept under [`Places::here`].
    Here,
}

impl Tied {
    /// How far bytes that hold the roots `held` (not empty) reach.
    pub(crate) fn of(held: RoleSet) -> Tied {
        match held.iter().all(|role| HOME.contains(&role)) {
            true => Tied::Home,
            false => Tied::Here,
        }
    }

    /// The farther of the two: what a record tied both ways is kept under.
    pub(crate) fn and(self, other: Tied) -> Tied {
        match (self, other) {
            (Tied::Home, Tied::Home) => Tied::Home,
            _ => Tied::Here,
        }
    }
}

/// The directories of one build that its keys and records are written against, each by what
/// it is.
/// The front end's port of them is [`Dirs`] (blaze-usercode, in the front end's code identity):
/// what writes the paths a front-end record holds; this adds what a unit's record key does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Places {
    dirs: Dirs,
}

/// The domain of [`Places::here`].
const HERE: &str = "blaze v2 cargo unit record here 1";

/// The domain of [`Places::home`].
const HOME_DOMAIN: &str = "blaze v2 cargo unit record in this home 1";

impl Places {
    /// The roots of a build of the workspace at `workspace` whose build directory is `target`,
    /// by a daemon whose shared standard library directory is `std`, tools `tools`, launcher
    /// `launcher` and fetched packages' directory `sources`
    /// (`blaze_manifest::home::sources`). One that is not absolute, not UTF-8 or the file
    /// system's root is no root: what is under it stays as it is in every key.
    pub(crate) fn new(
        workspace: &str,
        target: &str,
        std: &str,
        tools: &std::path::Path,
        launcher: &std::path::Path,
        sources: &std::path::Path,
    ) -> Places {
        let given = [
            (Role::Workspace, Some(workspace)),
            (Role::Target, Some(target)),
            (Role::Std, Some(std)),
            (Role::Tools, tools.to_str()),
            (Role::Launcher, launcher.to_str()),
            (Role::Sources, sources.to_str()),
        ];
        Places { dirs: Dirs::new(given) }
    }

    /// The roots of `job`'s build.
    pub(crate) fn of(job: &Job) -> Places {
        Places::new(
            &job.input.config.invocation.root,
            &job.units().target_dir,
            &job.input.config.std,
            &job.shared.tools,
            &job.shared.launcher,
            &job.shared.sources,
        )
    }

    fn view(&self) -> Roots<'_> {
        self.dirs.view()
    }

    /// The front end's port of these directories ([`base_db::Port`]).
    pub(crate) fn port(&self) -> Dirs {
        // clone: once a lowering, a root each
        self.dirs.clone()
    }

    /// `text` with each root that starts a path in it as its placeholder ([`Dirs::text`]).
    pub(crate) fn text<'t>(&self, text: &'t [u8]) -> Cow<'t, [u8]> {
        self.dirs.text(text)
    }

    /// What `text`, written by [`Places::text`] in some build, is in this one ([`Dirs::real`]).
    pub(crate) fn real<'t>(&self, text: &'t [u8]) -> Result<Cow<'t, [u8]>, String> {
        self.dirs.real(text)
    }

    /// `encoded`, one value in CBOR, appended to `out` with every text string of it written by
    /// [`Places::text`]: what a key hashes and a record keeps of a value that names paths,
    /// whichever of its fields do. An error for bytes that are not one definite-length value.
    pub(crate) fn cbor(&self, encoded: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        recode(encoded, out, &mut |text, out| {
            let ported = self.text(text);
            head(out, 3, ported.len() as u64);
            out.extend_from_slice(&ported);
            Ok(())
        })
    }

    /// The inverse of [`Places::cbor`] for this build's roots: every text string of `encoded`
    /// written back ([`Places::real`]).
    pub(crate) fn cbor_real(&self, encoded: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        recode(encoded, out, &mut |text, out| {
            let real = self.real(text)?;
            head(out, 3, real.len() as u64);
            out.extend_from_slice(&real);
            Ok(())
        })
    }

    /// Whether `bytes` hold one of the roots, in any spelling (`Roots::embedded`): what does
    /// is of this build's directories alone.
    pub(crate) fn tied(&self, bytes: &[u8]) -> bool {
        !self.held(bytes).is_empty()
    }

    /// Which roots `bytes` hold, in any spelling (`Roots::embedded`): none for bytes of no
    /// build's directories alone.
    pub(crate) fn held(&self, bytes: &[u8]) -> RoleSet {
        self.view().embedded(bytes)
    }

    /// How far `bytes` tie what holds them ([`Tied::of`]), if they hold a root.
    pub(crate) fn reach(&self, bytes: &[u8]) -> Option<Tied> {
        let held = self.held(bytes);
        (!held.is_empty()).then(|| Tied::of(held))
    }

    /// How the roots sit to each other (`Roots::layout`), for a key: which is under which and
    /// where, never a root's own path.
    pub(crate) fn layout(&self, hasher: &mut Hasher) {
        self.view().layout(|piece| {
            hasher.update(piece);
        });
    }

    /// The key a record of base key `base` that is tied to this build's directories is under:
    /// `base` and each root's real path. Asked after `base` by every lookup.
    pub(crate) fn here(&self, base: Key) -> Key {
        let mut hasher = Hasher::new(HERE);
        hasher.id(&base.0);
        for (role, root) in self.dirs.roots() {
            hasher.update(&[role.tag()]).part(root.as_bytes());
        }
        Key(hasher.finish())
    }

    /// The key a record of base key `base` tied only to roots of [`HOME`] is under: `base` and
    /// the real path of each of this build's roots of those roles, in role order, so every
    /// checkout a daemon of one home builds asks the same. Asked after `base`, before
    /// [`Places::here`].
    pub(crate) fn home(&self, base: Key) -> Key {
        let mut hasher = Hasher::new(HOME_DOMAIN);
        hasher.id(&base.0);
        for (role, root) in self.dirs.roots().filter(|(role, _)| HOME.contains(role)) {
            hasher.update(&[role.tag()]).part(root.as_bytes());
        }
        Key(hasher.finish())
    }

    /// The key a record of base key `base` tied as `tied` is kept under.
    pub(crate) fn tied_key(&self, base: Key, tied: Tied) -> Key {
        match tied {
            Tied::Home => self.home(base),
            Tied::Here => self.here(base),
        }
    }
}

/// The head of a CBOR item of major type `major` and argument `arg`, in its shortest form.
fn head(out: &mut Vec<u8>, major: u8, arg: u64) {
    let major = major << 5;
    match arg {
        0..=23 => out.push(major | arg as u8),
        24..=0xff => out.extend_from_slice(&[major | 24, arg as u8]),
        0x100..=0xffff => {
            out.push(major | 25);
            out.extend_from_slice(&(arg as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(major | 26);
            out.extend_from_slice(&(arg as u32).to_be_bytes());
        }
        _ => {
            out.push(major | 27);
            out.extend_from_slice(&arg.to_be_bytes());
        }
    }
}

/// How deep a value [`recode`] reads may nest: no record or plan is near it, and bytes that
/// are not one go no deeper before they are refused.
const DEPTH: usize = 256;

/// `input`, one CBOR value of definite lengths, appended to `out` item by item, each text
/// string as `text` writes it (head and all). Bounded by the input's length and [`DEPTH`]; an
/// error says what of the bytes is no such value.
fn recode(
    input: &[u8],
    out: &mut Vec<u8>,
    text: &mut dyn FnMut(&[u8], &mut Vec<u8>) -> Result<(), String>,
) -> Result<(), String> {
    let short = || "a CBOR value cut short".to_owned(); // alloc: error path
    let mut at = 0usize;
    // The items still to read at each depth.
    let mut left: Vec<u64> = vec![1]; // alloc: once per value recoded, its depth
    while let Some(top) = left.last_mut() {
        if *top == 0 {
            left.pop();
            continue;
        }
        *top -= 1;
        let start = at;
        let first = *input.get(at).ok_or_else(short)?;
        at += 1;
        let (major, info) = (first >> 5, first & 0x1f);
        let wide = match info {
            0..=23 => 0,
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            // alloc: error path
            _ => return Err("a CBOR item of indefinite length or a reserved form".to_owned()),
        };
        let arg_bytes = input.get(at..at + wide).ok_or_else(short)?;
        let arg = match wide {
            0 => u64::from(info),
            _ => arg_bytes.iter().fold(0u64, |all, byte| (all << 8) | u64::from(*byte)),
        };
        at += wide;
        match major {
            // Integers, and simple values and floats (whose argument is the value).
            0 | 1 | 7 => out.extend_from_slice(&input[start..at]),
            2 | 3 => {
                let len = usize::try_from(arg).map_err(|_| short())?;
                let end = at.checked_add(len).ok_or_else(short)?;
                let bytes = input.get(at..end).ok_or_else(short)?;
                match major {
                    2 => out.extend_from_slice(&input[start..end]),
                    _ => text(bytes, out)?,
                }
                at = end;
            }
            4 | 5 | 6 => {
                out.extend_from_slice(&input[start..at]);
                let items = match major {
                    4 => arg,
                    5 => arg.checked_mul(2).ok_or_else(short)?,
                    _ => 1,
                };
                if left.len() >= DEPTH {
                    return Err(format!("a CBOR value nested deeper than {DEPTH}")); // alloc: error path
                }
                left.push(items);
            }
            _ => unreachable!("a major type is three bits"),
        }
    }
    match at == input.len() {
        true => Ok(()),
        false => Err("bytes after a CBOR value".to_owned()), // alloc: error path
    }
}

/// Why what `krate` and the crates it depends on were made from may depend on where a root of
/// `places` is without holding it, if anything of it may: a crate of the closure read one of
/// its own variables whose value names a root through `env!` or `option_env!` (the database's
/// `env!` log; `env!("CARGO_MANIFEST_DIR").len()` is a number), or has such a variable and
/// calls a proc macro, which is given the crate's variables and whose reads of them no trace
/// names. A record made from such a closure is kept for the build's directories alone
/// ([`Places::here`]). Asked once the unit is compiled: the log then names every read its code
/// was made from.
///
/// The second is wider than most derives need, and still the only thing that keeps such a
/// record per checkout; narrowing it to the expansions that read a rooted variable or asked a
/// span's file is not sound while either holds:
/// - a macro learns its checkout untraced: the wasm glue enters the directory cargo runs rustc
///   in before the macro runs (blaze-macro-eval's `current_dir`), and `getcwd` answers from
///   wasi-libc's copy, with no host call to trace;
/// - the record's slot keeps each expansion's crate variables as they were made
///   ([`super::reads`]' `Expansions::env`, real paths), and a lookup works the expansions'
///   reads out with those: the root marks every unverified expansion is kept with
///   (`blaze_usercode::roots::Roots::mark`) then answer with the first checkout's roots, so a
///   second checkout finds the record. Removing this clause made the gauntlet's `derive_user`
///   and `gauntlet` units found in a second checkout from expansions verified nowhere.
///
/// It ends when a macro is given paths that name no checkout (store.md, "virtual roots for
/// wasm macros"), or the slot keeps the variables ported and a lookup places them, so a mark
/// answers with this checkout's root and only an expansion verified across checkouts
/// (`blaze_usercode::roots::verify`) leaves the record findable elsewhere.
pub(crate) fn path_reads(db: &base_db::Db, krate: base_db::Crate, places: &Places) -> Option<String> {
    let rooted = |value: &str| matches!(places.text(value.as_bytes()), Cow::Owned(_));
    // alloc: once per unit stored, a crate of its closure each
    let closure: blaze_hash::HashSet<base_db::Crate, std::alloc::Global> =
        krate.transitive_deps(db).iter().copied().collect();
    // A read that gives its value only to an `include!`'s path is none of these: the crate is
    // made from the file, which its key holds by content ([`base_db::Files::env_exposed`]).
    for (by, name) in db.files().env_exposed() {
        if closure.contains(&by) && by.env(db).get(&name).is_some_and(|value| rooted(&value)) {
            // alloc: once per record tied
            return Some(format!(
                "crate {} reads its variable `{name}`, which names a directory of the build",
                by.named(db)
            ));
        }
    }
    for &by in krate.transitive_deps(db) {
        let macros = by.data(db).dependencies.iter().any(|dep| {
            matches!(
                dep.crate_id.data(db).target_kind,
                base_db::TargetKind::Lib { is_proc_macro: true }
            )
        });
        if macros && by.env(db).iter().any(|(_, value)| rooted(value)) {
            // alloc: once per record tied
            return Some(format!(
                "crate {} calls proc macros, which are given its variables, and one of those names a directory of the build",
                by.named(db)
            ));
        }
    }
    None
}

#[cfg(test)]
thread_local! {
    /// A bug a test plants, which its oracle has to catch: the workspace's real path put back
    /// into every record's key ([`planted`]), as it was before keys were written against the
    /// build's directories. Off: no bug planted.
    pub(crate) static PLANTED_PATH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The path a test planted in the key ([`PLANTED_PATH`]): the workspace's as it is; never one
/// outside a test.
pub(crate) fn planted(places: &Places) -> Option<&str> {
    #[cfg(test)]
    {
        if PLANTED_PATH.get() {
            return places.dirs.roots().find(|it| it.0 == Role::Workspace).map(|it| it.1);
        }
    }
    let _ = places;
    None
}
