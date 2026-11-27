//! `engine-check`'s stored answer (engine.md E6, the incremental half): the engine's answer and
//! causes as pages (blaze-engine pages.rs), the leaf table of what the request read (sweep.rs
//! `Leaves`), and a sealed pointer naming both (seal.rs), under one root of the store per
//! checkout ([`root_name`]).
//!
//! THE TOOLS are not in the root's name: the answer is one per checkout whichever blaze wrote it.
//! Every row's key holds its own kind's producer (blaze-engine `Tools::key`), and each file's
//! plan digest holds every kind's (pages.rs `plan_digest`), so a request under other tools reads
//! the same answer: a file whose plan digest moved is in its edit, a task whose key moved is
//! entered, and the rest stand by their keys and reads (a moved names solver enters the names,
//! and a body whose scopes came out the same is not entered). Only the certificate, which reads
//! no row, needs the tools: the pointer keeps their digest ([`tools_digest`]), and it stands on
//! equal tools alone. Before (BEL6), the root hashed the tools, so every new blaze build (every
//! producer that moved) started from no answer and entered every task: a whole check of tempo
//! per new binary.
//!
//! Each changed page gets a CAS reach index containing its key, row payloads, and the
//! expansion records its names rows own. The checkout pins the sealed pointer alone,
//! and `<name>.pages` pins the root, printed lines and all live page indexes. The store's
//! generation flip publishes both roots together (pages_sim.rs).
//!
//! The read, before any work: the root name's one key is the sealed pointer; the sweep
//! follows its leaf table; nothing moved, nothing owed and no findings kept is the certificate,
//! from the pointer and the printed lines' record (engine_lines.rs, one get), no page read.
//! Else [`open_root`] reads the directory. [`Stored`] authenticates a page on its first
//! lookup. All consumers share that page's proof for the rest of the request. A refused
//! page stops the edit. A pointer of an older form is reported and rebuilt (rule 26).
//!
//! ```text
//! the root name   engine.<16 hex of H(checkout)>
//! it names        the sealed pointer's key; `<it>.pages` reaches the root, lines and page
//!                 indexes, with their payloads
//! sealed pointer  seal.rs form: last = "BEL8" | answer root key [32] | lines key [32] |
//!                 files u32 | printed u8 (bit 0 names checked, 1 bodies whole, 2 every
//!                 printed line is in the lines record) | findings u32 | owed u32 (the
//!                 certificate's facts) | tools digest [32] | headers u8 (0 current, 1
//!                 absent) | since u64 | 128 x producer header shard key [32] (all zero when
//!                 absent) | the journal's places; leaves = the BCL table
//! ```
//!
//! BEL6 had no tools digest: its root's name held the tools. A BEL6 pointer is an old home's
//! answer, reported and rebuilt ([`Unreadable::Older`]).
//!
//! BEL4 named pages without stored reverse-reader runs or page-local payload reach indexes.
//! BEL5 named 128 shard keys and no state: it could not say that the `Ready` driver's commit
//! (D187: pages alone, no shard read or written) left the shards behind the pages, so a BEL5
//! pointer is an old home's answer, reported and rebuilt ([`Unreadable::Older`]), never read
//! as BEL6 (the form is the pointer's place, D160: the magic is checked before any byte).

use core::fmt;
use core::fmt::Write as _;
use std::sync::OnceLock;

use blaze_cas::plain::Snapshot;
use blaze_core::Driver;
use blaze_drive::{Key, Name};
use blaze_engine::{StoredPages, Tools};
use blaze_engine::pages::{self, Header, HeaderPages, Headers, HeadersPlant, IndexedHeaders, Kept, PagesError, Root, Sealed, VerifiedPage, Writer};

/// The store's proof that a held record's bytes are its key's (rule 74): what the pages take
/// in place of hashing each page at its open (blaze-engine pages.rs, "Checked once").
impl pages::StoreProof for crate::store::plain_place::Checked<'_> {}
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::Cx;
use bstd::mem::Frame;

use crate::store::{CommandStore, Sharing, Store as _};
use crate::sweep::{Leaves, Rows};
use crate::{CliError, seal};

/// The pointer's form inside its seal.
/// BEL8: a file's lint row is ONE (engine_lints.rs, sparse rows: its units' keys in its record,
/// no row a unit); a BEL7 answer's unit rows are an old home's, reported and rebuilt.
pub(crate) const POINTER_MAGIC: &[u8; 4] = b"BEL8";
/// The magic, the answer's root, the lines record's key, then the [`Facts`]: files u32, printed
/// u8, findings u32, owed u32; then the tools' digest ([`tools_digest`]). The header index's
/// state ([`Headers`]) follows: its tag, its `since`, then every shard's key; then the
/// journal's places, of their own length.
const TOOLS_AT: usize = 4 + 32 + 32 + 4 + 1 + 4 + 4;
const HEADERS_AT: usize = TOOLS_AT + 32;
const SHARDS_AT: usize = HEADERS_AT + 1 + 8;
const POINTER_BYTES: usize = SHARDS_AT + 32 * HeaderPages::COUNT;
const HEADERS_CURRENT: u8 = 0;
const HEADERS_ABSENT: u8 = 1;
const POINTER_DOMAIN: &str = "blaze-cli engine-check pointer";

/// Formats an answer root name from `prefix` and `input` under the checkout root domain.
fn root_name_with<'f>(prefix: &str, input: &[u8], frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let id = ContentId::of("blaze-cli engine-check root 2", input);
    let mut name = blaze_own::String::new_in(frame);
    name.push_str(prefix);
    // A home made before the root stopped hashing the tools holds its answers under names no
    // request asks for again: dead roots, whose records the store's cleanup takes when their
    // pins expire, `ROOT_LIFE_NS` after their last renewal (store/plain_place.rs `pin`).
    // Nothing reads or migrates them (AGENTS.md: an old home's
    // answer is rebuilt, never migrated).
    for byte in &id.0[..8] {
        // A frame string's write fails only when the frame is full: the name is then short and
        // names no root, so the request answers in full.
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// The root the checkout's stored answer hangs from, whichever tools wrote it (the module's doc,
/// "THE TOOLS"; a machine's path is hashed here alone: a root is local, never shared).
pub(crate) fn root_name<'f>(checkout: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    root_name_with("engine.", checkout.as_bytes(), frame)
}

/// Whether `name` is the checkout's own root (`checkout_root`) or one of its auxiliary / scoped
/// roots (`<checkout_root>.*`), so `blaze clean` can discard this checkout's cached answers
/// without touching any other checkout or shared store data.
pub(crate) fn is_checkout_root(name: &str, checkout_root: &str) -> bool {
    name == checkout_root
        || (name.starts_with(checkout_root) && name.as_bytes().get(checkout_root.len()) == Some(&b'.'))
}

/// The root a check of `scope` hangs from: equal to [`root_name`] for whole-workspace checks, and
/// a disjoint `"<root_name>.s.<hex>"` root for scoped `blaze test` / `blaze run` / `blaze export`
/// requests so scoped checks never overwrite the whole-workspace answer while still supporting
/// certificate hits, incremental edits, and per-checkout cleanup (`blaze clean`).
pub(crate) fn scoped_root_name<'f>(checkout: &str, scope: crate::engine_check::CheckScope<'_>, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    match scope {
        crate::engine_check::CheckScope::Whole => root_name(checkout, frame),
        crate::engine_check::CheckScope::Test { packages, bin, bin_roots } => {
            let mut key = blaze_own::Vec::new_in(frame);
            let push_part = |buf: &mut blaze_own::Vec<u8, &'f Frame>, bytes: &[u8]| {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                buf.extend_from_slice(&len.to_le_bytes());
                buf.extend_from_slice(bytes);
            };
            push_part(&mut key, checkout.as_bytes());
            push_part(&mut key, b"test");
            let pkg_count = u64::try_from(packages.len()).unwrap_or(u64::MAX);
            key.extend_from_slice(&pkg_count.to_le_bytes());
            for package in packages {
                push_part(&mut key, package.as_bytes());
            }
            match bin {
                Some(name) => {
                    key.push(1);
                    push_part(&mut key, name.as_bytes());
                }
                None => key.push(0),
            }
            let root_count = u64::try_from(bin_roots.len()).unwrap_or(u64::MAX);
            key.extend_from_slice(&root_count.to_le_bytes());
            for root in bin_roots {
                push_part(&mut key, root.as_bytes());
            }
            let prefix = bstd::mem::format_in!(frame, "{}.s.", root_name(checkout, frame).as_str());
            root_name_with(prefix.as_str(), key.as_slice(), frame)
        }
        crate::engine_check::CheckScope::Bin { package, root } => {
            let mut key = blaze_own::Vec::new_in(frame);
            let push_part = |buf: &mut blaze_own::Vec<u8, &'f Frame>, bytes: &[u8]| {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                buf.extend_from_slice(&len.to_le_bytes());
                buf.extend_from_slice(bytes);
            };
            push_part(&mut key, checkout.as_bytes());
            push_part(&mut key, b"bin");
            push_part(&mut key, package.as_bytes());
            push_part(&mut key, root.as_bytes());
            let prefix = bstd::mem::format_in!(frame, "{}.s.", root_name(checkout, frame).as_str());
            root_name_with(prefix.as_str(), key.as_slice(), frame)
        }
    }
}

/// Unroots every stored answer root (whole-workspace, scoped, reach, journal, grains, members,
/// scripts, view) belonging to `checkout` from `lent`, and removes any pointer owned by this
/// checkout from [`BASES_ROOT`] / [`BASES_KEPT_ROOT`] so a subsequent check does not immediately
/// reload the just-discarded answer as a workspace base. Returns the number of roots unrooted.
pub(crate) fn unroot_checkout<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    checkout: &str,
    frame: &Frame,
) -> Result<u32, CliError> {
    let prefix = root_name(checkout, frame);
    let mut removed_pointers: blaze_own::Vec<Key, &Frame> = blaze_own::Vec::new_in(frame);
    if let Some(root_rec_key) = lent.root_named(prefix.as_str())
        && let Some(held) = lent.get(&root_rec_key).map_err(crate::store::PlaceError::from)?
        && held.bytes.len() == 32
    {
        removed_pointers.push(key_at(held.bytes, 0));
    }
    let roots = lent.roots();
    let mut unrooted = 0u32;
    for (name, _) in roots.iter() {
        if is_checkout_root(name.as_str(), prefix.as_str()) {
            lent.unroot(name.as_str());
            unrooted = unrooted.saturating_add(1);
        }
    }
    if unrooted == 0 {
        return Ok(0);
    }
    if !removed_pointers.is_empty()
        && let Some(bases_rec_key) = lent.root_named(BASES_ROOT)
        && let Some(held_keys) = lent.get(&bases_rec_key).map_err(crate::store::PlaceError::from)?
        && held_keys.bytes.len() == 32
        && let Some(held_bases) = lent.get(&cas_key_at(held_keys.bytes, 0)).map_err(crate::store::PlaceError::from)?
        && let Some(bases) = decode_bases(held_bases.bytes, frame)
    {
        let mut kept: blaze_own::Vec<Base, &Frame> = blaze_own::Vec::with_capacity_in(bases.len(), frame);
        for base in bases.iter() {
            if removed_pointers.scan_for(&base.pointer).is_none() {
                kept.push(*base);
            } else {
                unroot_base_slot(lent, base.workspace, base.layout, frame);
            }
        }
        if kept.len() != bases.len() {
            if kept.is_empty() {
                lent.unroot(BASES_ROOT);
                if lent.has_root(BASES_KEPT_ROOT) {
                    lent.unroot(BASES_KEPT_ROOT);
                }
            } else {
                let bytes = encode_bases(&kept, frame);
                let record = blaze_cas::Key(ContentId::of(BASES_DOMAIN, &bytes));
                lent.put_parts(
                    &record,
                    blaze_cas::plain::Namespace::LOCAL,
                    blaze_keyed::kind::ReservedTag::Drive.tag(),
                    &[bytes.as_slice()],
                ).map_err(crate::store::PlaceError::from)?;
                lent.root_for(BASES_ROOT, &[record], blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
                let mut kept_indexes: blaze_own::Vec<blaze_cas::Key, &Frame> = blaze_own::Vec::with_capacity_in(kept.len(), frame);
                for base in kept.iter() {
                    kept_indexes.push(kept_index_for_base(lent, base.pointer)?);
                    renew_base_slot(lent, base.workspace, base.layout, frame);
                }
                lent.root_through(BASES_KEPT_ROOT, &kept_indexes, blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
            }
        }
    }
    lent.commit().map_err(crate::store::PlaceError::from)?;
    Ok(unrooted)
}

/// Auxiliary per-answer record keys (`<root>.members` and `<root>.grains`) carried beside a
/// saved workspace base so cross-worktree adoption (`adopt_certified`) and `OtherCheckout` starts
/// inherit standing member reach and `ReadGrains` without rebuilding them.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct BaseAux {
    pub(crate) members: Option<Key>,
    pub(crate) grains: Option<Key>,
}

/// Whether a saved answer should update the home's shared workspace bases (`BASES_ROOT`): whole-
/// workspace checks keep their base (with optional auxiliary `.members`/`.grains` keys); scoped
/// checks skip it so a partial answer never replaces a workspace base.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SaveBase {
    Keep,
    KeepWith(BaseAux),
    Skip,
}

/// The control of main.rs tests `a_moved_body_checker_enters_its_bodies_alone` (rule 23):
/// [`RootPlant::PerTools`] reads no stored answer written under other tools, as the root did
/// when its name hashed the tools (BEL6).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RootPlant {
    None,
    PerTools,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static ROOT_PLANT: core::cell::Cell<RootPlant> = const { core::cell::Cell::new(RootPlant::None) };
}

/// The root that named the home's latest sealed pointer, whichever checkout saved it: since
/// 2026-11-21 only the tests' control ([`BasePlant::OnePointer`]); a checkout with no answer of
/// its own starts from its workspace's base ([`BASES_ROOT`], [`load_base`]). What held of the
/// one pointer holds of a base: a checkout with no answer of its own starts from it: every row
/// is valid by its key and reads against today's facts, and the pointer's leaves are
/// checkout-relative, so the sweep follows them here, every row looked at (its journal places
/// are the other checkout's and are not read), and the certificate never stands on it
/// (engine_check.rs, `StoredBase::OtherCheckout`). Its pages stay reachable while the checkout
/// that wrote it keeps them under its own `<root>.pages`; a page that is gone makes the answer
/// unreadable, reported, and the request answers in full (rule 3: a stated path).
#[cfg_attr(not(test), allow(dead_code, reason = "the bases' test control alone (`BasePlant::OnePointer`)"))]
pub(crate) const LATEST_ROOT: &str = "engine.latest";

// ---- The bases: one list of answers to start from, keyed by the workspace (thoughts2 1.5) ----
//
// Until 2026-11-21 a checkout with no answer of its own started from [`LATEST_ROOT`], the
// home's ONE latest pointer: working in repository B replaced the base a new worktree of
// repository A started from, which then entered every body B's answer did not hold. Now the
// home keeps [`BASES_ROOT`]: one record of up to [`BASES_MOST`] entries (workspace, lock digest,
// layout digest, pointer key), at most [`BASES_PER_WORKSPACE`] a workspace, newest first, and a
// checkout with none of its own starts from the newest entry of its lock digest, else of its
// workspace ([`pick_base`]). The single pointer is kept only as the tests' control
// ([`BasePlant`]).
//
// THE WORKSPACE'S IDENTITY ([`workspace_of_lock`]): the digest of the names of the workspace's
// path packages: a lock's packages with no `source` (cargo writes `source` for every registry
// and git package, none for a path package: the members and the path dependencies in the
// repository), or, for a workspace whose layout reads no lock (nothing outside it, units.rs),
// the members' own lock (graph.rs `members_lock`, the same form). It is equal for two
// worktrees of one repository (one lock, or one with the same members on another branch),
// different for two repositories with different members, holds no path, and moves with no
// checkout's place. Two repositories whose members share every name (two forks, or two `cli`
// + `core` pairs) share one list: harmless, since the pick is by an equal lock digest first,
// and every row of any base is valid by its key and reads against today's facts; such a base
// costs entered tasks, never a wrong answer. Versions are left out: a version bump on a branch
// keeps its workspace.
//
// KNOWN WITH NO READ: a save learns its workspace from the request's own read of the lock
// ([`Rows::workspace`]), and on a request whose layout stood (no manifest moved, nothing read)
// from the entry of its layout digest ([`layout_of`]: the manifest rows' paths and ids, which
// the rows hold), else of its lock digest. A request that read nothing and matches no entry
// keeps none ([`BaseKept::Unknown`]: its last base stays, older, still valid). The start
// ([`load_base`], only for a checkout with no answer of its own, after the bases record is
// read) reads the checkout's `Cargo.lock` once: its workspace's entry of that lock, else its
// workspace's newest. With no lock on disk, or no entry of its workspace, it reads the root
// `Cargo.toml` once (the first file the layout reads anyway) and starts from the newest entry
// whose root manifest has those bytes ([`Base::root`], [`pick_by_root`]): a members-only
// workspace (blaze itself; a fixture with no lock) whose save learned its workspace from the
// members' own lock finds its base too, as it did from the one pointer before 2026-11-21
// (main.rs `a_second_worktree_answers_from_the_first_ones_records`).
//
// THE BOUND (rule 18): [`BASES_PER_WORKSPACE`] (4) layouts a workspace and [`BASES_MOST`] (32)
// entries in all; past either the oldest entry (by the record's own counter, never a clock:
// rule 25) is dropped. Each save rewrites the record (about 4.4 KiB at the cap) and pins it
// under [`BASES_ROOT`] and the pointers it lists under [`BASES_KEPT_ROOT`], both through the
// store's root commit (`store.pin`: the pointer/root commit path of blaze-cas plain
// pointer_tests.rs and root_commit_tests.rs), so the list moves with the answer it names, in
// one commit. Two roots, because a root's record holds its keys SORTED (blaze-cas
// `Plain::root_for`, `sorted_keys`): under one root the bases record was wherever its key
// sorted among the pointers, and a read of the root's first key found a pointer, "not of this
// form", on 2026-11-21 (engine_bases_worktrees_tests.rs, every save `Replaced`).

/// The root that names the bases record alone: one key.
pub(crate) const BASES_ROOT: &str = "engine.bases";
/// The root that names every pointer the bases record lists, so cleanup keeps each answer a
/// base names (in key order; the record says which is whose).
pub(crate) const BASES_KEPT_ROOT: &str = "engine.bases.kept";
/// The bases record's form: magic, entries u32, then each entry (workspace 32, lock 32 (zero
/// for a layout that read none), layout 32, root manifest 32 (zero for none), pointer 32, its
/// counter u64), newest first. `BEB1` (no root manifest) is another form: rewritten.
const BASES_MAGIC: &[u8; 4] = b"BEB2";
const BASES_DOMAIN: &str = "blaze-cli engine-check bases 2";
const BASE_ENTRY: usize = 32 * 5 + 8;
/// The checkout's root manifest, the first file every layout reads (its row's path).
const ROOT_MANIFEST: &str = "Cargo.toml";
/// No root manifest row: the entry's [`Base::root`].
const NO_ROOT: ContentId = ContentId([0; 32]);
/// Layouts a workspace keeps a base for.
pub(crate) const BASES_PER_WORKSPACE: usize = 4;
/// Entries the record keeps in all.
pub(crate) const BASES_MOST: usize = 32;
/// The digest's domain of a workspace's identity.
const WORKSPACE_DOMAIN: &str = "blaze-cli workspace of a lock 1";
/// The digest's domain of a layout's manifests ([`layout_of`]).
const LAYOUT_DOMAIN: &str = "blaze-cli engine-check layout of the manifests 1";
/// The checkout's lock, from its root (units.rs `LOCK`).
const LOCK_FILE: &str = "Cargo.lock";
/// No lock read: the entry's lock digest.
const NO_LOCK: ContentId = ContentId([0; 32]);

/// A checkout's workspace ([`workspace_of_lock`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Workspace {
    pub(crate) id: ContentId,
}

impl Workspace {
    /// The workspace of the lock whose bytes are `text` (a `Cargo.lock`, or graph.rs
    /// `members_lock`); `None` for a lock of no path package.
    pub(crate) fn of_lock(text: &[u8], frame: &Frame) -> Option<Workspace> {
        Some(Workspace { id: workspace_of_lock(text, frame)? })
    }
}

/// The digest of the rows' manifests (each `Manifest` row's path, flags and id, by path): equal
/// for two checkouts whose layouts read equal files; `None` with no manifest row.
pub(crate) fn layout_of(rows: &Rows<'_>, frame: &Frame) -> Option<ContentId> {
    let mut manifests: blaze_own::Vec<(&str, u8, ContentId), &Frame> = blaze_own::Vec::new_in(frame);
    manifests.extend(rows.rows.iter().filter(|leaf| leaf.kind == crate::sweep::LeafKind::Manifest).map(|leaf| (rows.path(leaf), leaf.flags, leaf.id)));
    if manifests.is_empty() {
        return None;
    }
    manifests.sort_unstable();
    let mut h = blaze_ids::Hasher::new(LAYOUT_DOMAIN);
    for (path, flags, id) in manifests.iter() {
        h.part(path.as_bytes()).part(&[*flags]).part(&id.0);
    }
    Some(h.finish())
}

/// The digest of the lock's path packages' names (the module's "THE WORKSPACE'S IDENTITY"),
/// sorted and each once; `None` when it has none. Read line by line in cargo's own form
/// (`[[package]]`, then `name = "x"` and, for a fetched package, `source = "..."`): only the
/// names and whether a source is written are read, so a lock this reader cannot follow gives
/// fewer names (a weaker identity, never a wrong answer: the module's doc).
pub(crate) fn workspace_of_lock(text: &[u8], frame: &Frame) -> Option<ContentId> {
    let mut names: blaze_own::Vec<&[u8], &Frame> = blaze_own::Vec::new_in(frame);
    let mut in_package = false;
    let mut name: Option<&[u8]> = None;
    let mut sourced = false;
    fn flush<'t>(in_package: bool, name: Option<&'t [u8]>, sourced: bool, names: &mut blaze_own::Vec<&'t [u8], &Frame>) {
        if let (true, Some(name), false) = (in_package, name, sourced) {
            names.push(name);
        }
    }
    for line in text.split(|b| *b == b'\n') {
        let line = line.trim_ascii();
        if line.starts_with(b"[") {
            flush(in_package, name.take(), sourced, &mut names);
            in_package = line == b"[[package]]";
            sourced = false;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"name") {
            let rest = rest.trim_ascii_start();
            if let Some(value) = rest.strip_prefix(b"=") {
                let value = value.trim_ascii();
                name = value.strip_prefix(b"\"").and_then(|it| it.strip_suffix(b"\""));
            }
        } else if line.strip_prefix(b"source").is_some_and(|rest| rest.trim_ascii_start().starts_with(b"=")) {
            sourced = true;
        }
    }
    flush(in_package, name, sourced, &mut names);
    if names.is_empty() {
        return None;
    }
    names.sort_unstable();
    names.dedup();
    let mut h = blaze_ids::Hasher::new(WORKSPACE_DOMAIN);
    for name in names.iter() {
        h.part(name);
    }
    Some(h.finish())
}

/// One base: the answer `pointer` a checkout of `workspace` saved with lock `lock` ([`NO_LOCK`]
/// for a layout that read none) and layout `layout` ([`layout_of`]), and its place in the
/// record's order (`seq`: larger is newer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Base {
    pub(crate) workspace: ContentId,
    pub(crate) lock: ContentId,
    pub(crate) layout: ContentId,
    /// The id of the root manifest's bytes (its row's, `sweep::FILE_DOMAIN`), [`NO_ROOT`] for
    /// rows with none: how a checkout with no lock on disk finds its base ([`pick_by_root`]).
    pub(crate) root: ContentId,
    pub(crate) pointer: Key,
    pub(crate) seq: u64,
}

/// The id of the rows' root manifest ([`Base::root`]), with no read: its row's.
pub(crate) fn root_of(rows: &Rows<'_>) -> ContentId {
    rows.rows
        .iter()
        .find(|leaf| leaf.kind == crate::sweep::LeafKind::Manifest && !leaf.absent() && rows.path(leaf) == ROOT_MANIFEST)
        .map_or(NO_ROOT, |leaf| leaf.id)
}

/// The base a checkout whose root manifest has the id `root` starts from when its lock names
/// none (sans-IO): the newest entry of an equal root manifest; never a zero-root match.
#[cfg_attr(not(test), allow(dead_code, reason = "tested sans-IO entry point beside candidate_bases"))]
pub(crate) fn pick_by_root(bases: &[Base], root: ContentId) -> Option<Base> {
    bases.iter().find(|it| it.root == root && root != NO_ROOT).copied()
}

/// The bases record's entries, newest first; `None` for bytes not of this form (the record is
/// then written anew, [`BaseKept::Replaced`]).
pub(crate) fn decode_bases<'f>(bytes: &[u8], frame: &'f Frame) -> Option<blaze_own::Vec<Base, &'f Frame>> {
    let rest = bytes.strip_prefix(BASES_MAGIC.as_slice())?;
    let count = usize::try_from(u32::from_le_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &rest[4..];
    if count > BASES_MOST || rest.len() != count * BASE_ENTRY {
        return None;
    }
    let mut out = blaze_own::Vec::with_capacity_in(count, frame);
    for entry in rest.chunks_exact(BASE_ENTRY) {
        let id = |at: usize| ContentId(entry[at..at + 32].try_into().expect("32 bytes"));
        let seq = u64::from_le_bytes(entry[160..168].try_into().expect("eight bytes"));
        out.push(Base { workspace: id(0), lock: id(32), layout: id(64), root: id(96), pointer: Key(id(128)), seq });
    }
    // Newest first, each seq once: else not a record this writer made.
    out.windows(2).all(|pair| pair[0].seq > pair[1].seq).then_some(out)
}

/// The record's bytes of `bases` (newest first).
pub(crate) fn encode_bases<'f>(bases: &[Base], frame: &'f Frame) -> blaze_own::Vec<u8, &'f Frame> {
    let mut out = blaze_own::Vec::with_capacity_in(8 + bases.len() * BASE_ENTRY, frame);
    out.extend_from_slice(BASES_MAGIC);
    out.extend_from_slice(&u32::try_from(bases.len()).unwrap_or(u32::MAX).to_le_bytes());
    for base in bases {
        out.extend_from_slice(&base.workspace.0);
        out.extend_from_slice(&base.lock.0);
        out.extend_from_slice(&base.layout.0);
        out.extend_from_slice(&base.root.0);
        out.extend_from_slice(&base.pointer.0.0);
        out.extend_from_slice(&base.seq.to_le_bytes());
    }
    out
}

/// `bases` with `now` as its newest entry (sans-IO; `now.seq` is ignored and set past every
/// other): the entry of the same workspace and layout replaced, then the oldest dropped past
/// [`BASES_PER_WORKSPACE`] of one workspace and past [`BASES_MOST`] in all.
pub(crate) fn with_base<'f>(bases: &[Base], now: Base, frame: &'f Frame) -> blaze_own::Vec<Base, &'f Frame> {
    let seq = bases.iter().map(|it| it.seq).max().map_or(0, |most| most.saturating_add(1));
    let mut out = blaze_own::Vec::with_capacity_in(bases.len() + 1, frame);
    out.push(Base { seq, ..now });
    for base in bases {
        if out.len() == BASES_MOST {
            break;
        }
        let same = base.workspace == now.workspace && base.layout == now.layout;
        let of_workspace = out.iter().filter(|it| it.workspace == base.workspace).count();
        if !same && of_workspace < BASES_PER_WORKSPACE {
            out.push(*base);
        }
    }
    out
}

/// The base a checkout of `workspace` with lock `lock` starts from (sans-IO): the newest of its
/// workspace and lock digest, else the newest of its workspace, else none.
#[cfg_attr(not(test), allow(dead_code, reason = "tested sans-IO entry point beside candidate_bases"))]
pub(crate) fn pick_base(bases: &[Base], workspace: Workspace, lock: ContentId) -> Option<Base> {
    let mine = |it: &&Base| it.workspace == workspace.id;
    bases.iter().filter(mine).find(|it| it.lock == lock && lock != NO_LOCK).or_else(|| bases.iter().find(mine)).copied()
}

/// Every candidate base in `bases` matching `(workspace, lock)` or `root` in priority order
/// (sans-IO, newest first within each tier, deduplicated by pointer):
/// 1. Same `workspace` and equal non-zero `lock` digest;
/// 2. Same `workspace` (other lock digest);
/// 3. If neither workspace tier matched, equal non-zero `root` manifest digest.
pub(crate) fn candidate_bases<'f>(
    bases: &[Base],
    workspace: Option<Workspace>,
    lock: ContentId,
    root: ContentId,
    frame: &'f Frame,
) -> blaze_own::Vec<Base, &'f Frame> {
    fn push_unique<'f>(out: &mut blaze_own::Vec<Base, &'f Frame>, base: Base) {
        if !out.iter().any(|existing: &Base| existing.pointer == base.pointer) {
            out.push(base);
        }
    }
    let mut out = blaze_own::Vec::new_in(frame);
    if let Some(ws) = workspace {
        if lock != NO_LOCK {
            for &base in bases {
                if base.workspace == ws.id && base.lock == lock {
                    push_unique(&mut out, base);
                }
            }
        }
        for &base in bases {
            if base.workspace == ws.id {
                push_unique(&mut out, base);
            }
        }
    }
    if out.is_empty() && root != NO_ROOT {
        for &base in bases {
            if base.root == root {
                push_unique(&mut out, base);
            }
        }
    }
    out
}

/// The workspace a save's rows are of, with no read (the module's "KNOWN WITH NO READ"): the
/// request's own read, else the entry of an equal layout, else of an equal lock digest.
pub(crate) fn workspace_of_rows(bases: &[Base], read: Option<Workspace>, layout: ContentId, lock: ContentId) -> Option<Workspace> {
    read.or_else(|| bases.iter().find(|it| it.layout == layout).map(|it| Workspace { id: it.workspace }))
        .or_else(|| bases.iter().find(|it| it.lock == lock && lock != NO_LOCK).map(|it| Workspace { id: it.workspace }))
}

/// What a save did with the bases ([`Saved::base`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum BaseKept {
    /// Not a save that keeps one (a test's plain encode).
    #[default]
    NotAsked,
    /// This answer is its workspace's newest base for its layout.
    Kept { entries: u32 },
    /// The record read was not of this form, and was written anew with this answer alone.
    Replaced,
    /// The rows hold no manifest: no layout, no workspace.
    NoWorkspace,
    /// No workspace was had: this request read no lock (nor the members' own) and no entry has
    /// its layout or lock digest, or what it read names no path package. Known again when the
    /// layout is next read; the last base stays.
    Unknown,
}

impl fmt::Display for BaseKept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaseKept::NotAsked => f.write_str("base not kept (not asked)"),
            BaseKept::Kept { entries } => write!(f, "kept as its workspace's newest base ({entries} bases in the home)"),
            BaseKept::Replaced => f.write_str("kept as the one base: the home's bases record was not of this form"),
            BaseKept::NoWorkspace => f.write_str("base not kept: the rows hold no manifest"),
            BaseKept::Unknown => f.write_str("base not kept: this request read no lock and no base has its layout or lock"),
        }
    }
}

/// The tests' control of the bases (rule 23):
/// - [`BasePlant::OnePointer`] keeps and reads the one [`LATEST_ROOT`] as before 2026-11-21, so a
///   second repository's answer replaces the first's.
/// - [`BasePlant::NoStaleFallback`] skips cross-worktree base fallback when the checkout's own
///   stored answer is unreadable or from older tools, proving the stale-answer fallback test
///   catches the pre-fix regression.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BasePlant {
    None,
    OnePointer,
    NoStaleFallback,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static BASE_PLANT: core::cell::Cell<BasePlant> = const { core::cell::Cell::new(BasePlant::None) };
}

#[cfg(test)]
pub(crate) fn base_plant() -> BasePlant {
    BASE_PLANT.get()
}

/// The digest of every kind's tool, in kind order: what the pointer keeps so the certificate
/// stands only under the tools that wrote the answer (the module's doc, "THE TOOLS").
pub(crate) fn tools_digest(tools: &Tools) -> ContentId {
    let mut h = blaze_ids::Hasher::new("blaze-cli engine-check pointer tools 1");
    for tool in tools.0.iter() {
        h.update(&tool.0.0);
    }
    h.finish()
}

/// Which CAS reach root of a stored answer was missing or expired when validated at load time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MissingReachRoot {
    /// `<prefix>.answer` (pins root directory, radix leaf shards, lines, headers).
    Answer,
    /// `<prefix>.reach` (flat or single-root page reach index).
    Reach,
    /// `<prefix>.reach.<shard:02x>` (sharded radix page reach root).
    Shard(u16),
    /// `<prefix>.reach.delta` (sharded radix incremental delta reach root).
    Delta,
}

impl fmt::Display for MissingReachRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MissingReachRoot::Answer => f.write_str("its `.answer` root is missing or expired"),
            MissingReachRoot::Reach => f.write_str("its `.reach` root is missing or expired"),
            MissingReachRoot::Shard(shard) => write!(f, "its `.reach.{shard:02x}` shard root is missing or expired"),
            MissingReachRoot::Delta => f.write_str("its `.reach.delta` root is missing or expired"),
        }
    }
}

/// Why the stored answer is not read: the request answers in full and writes it anew.
#[derive(Debug)]
pub(crate) enum Unreadable {
    /// The root names other than one key (its pointer's).
    Names { bytes: usize },
    /// A record the root names is not in the store.
    Missing { key: Key },
    /// The pointer is of an older form (`BEL5`, `BEL4`, or older still): an old home's answer.
    Older,
    /// The printed lines' record (engine_lines.rs).
    Lines(crate::engine_lines::LinesError),
    /// The pointer's write was cut short.
    Torn(seal::Torn),
    /// A pointer of this form whose header index field no writer makes: a tag of no state, or an
    /// absent index naming a shard key.
    Headers { tag: u8 },
    /// A page or the answer's root (pages.rs).
    Pages(PagesError),
    /// One of the stored answer's CAS reach roots is missing or expired (e.g. partially evicted
    /// during compaction or unrooted after a crash), so its pages' `beside_key` reach indexes can
    /// no longer be assumed held.
    MissingReach { what: MissingReachRoot },
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Unreadable::Older = self {
            return write!(f, "the stored engine answer is an old home's answer (its pointer is of an older form than `BEL8`), rebuilt (rule 26)");
        }
        write!(f, "the stored engine answer is not read (engine_answer.rs `load`): ")?;
        match self {
            Unreadable::Names { bytes } => write!(f, "its root names {bytes} bytes, not its pointer's one key")?,
            Unreadable::Missing { key } => write!(f, "{key:?}, which its root names, is not in the store")?,
            Unreadable::Older => {}
            Unreadable::Lines(why) => write!(f, "{why}")?,
            Unreadable::Torn(torn) => write!(f, "{torn}")?,
            Unreadable::Headers { tag } => write!(f, "its producer header index field (tag {tag}) is no state a writer makes (engine_answer.rs `load_pointer`)")?,
            Unreadable::Pages(why) => write!(f, "{why}")?,
            Unreadable::MissingReach { what } => write!(f, "{what}")?,
        }
        write!(f, "; answering in full and writing it anew")
    }
}

/// What the certificate prints, kept in the pointer so a request with nothing changed reads no
/// page: the files outlined, whether the crates' names were checked, the rows with findings,
/// and the cells and tasks owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Facts {
    pub(crate) files: u32,
    pub(crate) printed: Printed,
    pub(crate) findings: u32,
    pub(crate) owed: u32,
}

/// What the last request printed that the answer's rows do not say, from the driver: which
/// stages it checked whole, and whether every line it printed beyond the closing one is in the
/// lines record (`replayable`: no note of a count the rows do not keep). The certificate stands
/// only after a replayable request: it prints the lines record and its coverage.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Printed {
    pub(crate) names: bool,
    /// Every body's TYPES were checked to the end (engine_lines.rs `not_checked_whole`). Whether
    /// their borrows were too is not kept here: it is a count of the lines record itself (its
    /// `Class::Borrows` notes, `Tally::borrows`), which the certificate prints whole, so the
    /// two can never disagree.
    pub(crate) bodies_whole: bool,
    pub(crate) replayable: bool,
}

impl Printed {
    fn byte(self) -> u8 {
        u8::from(self.names) | u8::from(self.bodies_whole) << 1 | u8::from(self.replayable) << 2
    }

    fn of_byte(byte: u8) -> Printed {
        Printed { names: byte & 1 != 0, bodies_whole: byte & 2 != 0, replayable: byte & 4 != 0 }
    }
}

/// The pointer as read: the answer's root, the certificate's facts, and the leaf table of what
/// the last request read.
pub(crate) struct Pointer<'m> {
    /// The sealed pointer's own key, as the root named it when read: what a journal
    /// checkpoint extends (journal_checkpoint.rs).
    pub(crate) key: Key,
    pub(crate) root: Key,
    /// The producer header index: current with the pages, or absent since a ready commit.
    pub(crate) headers: Headers,
    /// The printed lines' record (engine_lines.rs).
    pub(crate) lines: Key,
    pub(crate) facts: Facts,
    /// The digest of the tools that wrote the answer ([`tools_digest`]): the certificate stands
    /// only when today's are equal ([`Pointer::certifies_under`]).
    pub(crate) tools: ContentId,
    pub(crate) leaves: Leaves<'m>,
    /// Where the journal of changes stood when the request that wrote it began its reads.
    pub(crate) journal: crate::journal_sweep::Stored<'m>,
}

impl Pointer<'_> {
    /// THE ONE PLACE the tools decide anything of a stored answer (the module's doc, "THE
    /// TOOLS"): whether the certificate, which reads no row, may stand under `tools`. Equal
    /// digests: yes. Else the request takes the phase path over this same answer, where each
    /// row stands or is entered by its own key (its kind's producer) and reads.
    pub(crate) fn certifies_under(&self, tools: &Tools) -> bool {
        self.tools == tools_digest(tools)
    }
}

fn key_at(bytes: &[u8], at: usize) -> Key {
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    Key(ContentId(id))
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Only the directory the pointer names. The certificate does not need even this read.
/// For 2-level radix roots (`bits > RADIX_FLAT_MAX_BITS`), reads only the top root and its
/// directory sub-records — ZERO page leaf shards are read upfront.
pub(crate) fn open_root<'a, D: Driver, S: Sharing>(
    cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, key: Key, alloc: SharedAlloc<'a>,
) -> Result<Result<Root<'a>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    let mut store_err: Option<CliError> = None;
    let decoded = pages::decode_root_with(
        Sealed::by_store(key, held.bytes, held.proof),
        |sub_key| match store.get(cx, sub_key) {
            Ok(found) => found.map(|h| Ok(Sealed::by_store(sub_key, h.bytes, h.proof))),
            Err(err) => {
                store_err.get_or_insert(err);
                None
            }
        },
        alloc,
    );
    if let Some(err) = store_err {
        return Err(err);
    }
    Ok(decoded.map_err(Unreadable::Pages))
}

pub(crate) fn page_error(why: PagesError) -> CliError {
    blaze_engine::EngineError::from(blaze_engine::last::LastError::Pages(why)).into()
}

// A small directory of slots, with one block allocated only if a lookup reaches it. The
// snapshot owns the mappings even while workers publish later generations of the store.
const BLOCK: usize = 64;
type PageBox<'a> = blaze_own::Box<VerifiedPage<'a>, SharedAlloc<'a>>;
type PageSlot<'a> = OnceLock<Option<PageBox<'a>>>;
type PageBlock<'a> = blaze_own::Box<[PageSlot<'a>; BLOCK], SharedAlloc<'a>>;
/// The producer header shards' slots, one each, in the request's memory: 128 slots of about a
/// hundred bytes, which held inline made `Stored` some 12 KiB, copied at each move of it through
/// the command's top-level check frame in a debug build (the stack overflow of
/// `a_changed_archive_checksum_enters_the_body_through_the_graph`, wave-1119 t26).
type HeaderSlots<'a> = blaze_own::Box<[OnceLock<Result<IndexedHeaders<'a>, PagesError>>; HeaderPages::COUNT], SharedAlloc<'a>>;

/// Hot atomic read/byte counters for [`Stored`], each padded to its own 128-byte cache line
/// (`bstd::sync::CachePadded`, rule 86) and boxed off-stack so `Stored` stays `<= 1 KiB` by value
/// while eliminating 128-core false sharing across parallel readers.
struct StoredMetrics {
    task_reads: bstd::sync::CachePadded<core::sync::atomic::AtomicUsize>,
    header_reads: bstd::sync::CachePadded<core::sync::atomic::AtomicUsize>,
    /// The bytes of the task and header pages read, and of those hashed (zero on the store's
    /// proof): the `pages:` line ([`PagesRead`]).
    task_bytes: bstd::sync::CachePadded<core::sync::atomic::AtomicU64>,
    header_bytes: bstd::sync::CachePadded<core::sync::atomic::AtomicU64>,
    hashed_bytes: bstd::sync::CachePadded<core::sync::atomic::AtomicU64>,
}

/// One cache-line-isolated shard of [`Stored::asked`]: holds both its own ask counter and its
/// capped entry list so parallel workers pick a shard by `Name`'s hash bytes with zero shared
/// global `asks.fetch_add(1)` contention across 128 cores (rule 86).
struct AskedShard<'a> {
    asks: core::sync::atomic::AtomicUsize,
    entries: blaze_core::lock::Lock<blaze_own::Vec<(Name, blaze_engine::Ask), SharedAlloc<'a>>>,
}

pub(crate) struct Stored<'a> {
    pub(crate) root: &'a Root<'a>,
    snapshot: &'a Snapshot<SharedAlloc<'a>>,
    /// The producer header index the pointer names: its shards read when current, else every
    /// header and planner row read from its row's page (blaze-engine headers.rs `Headers`).
    headers: &'a Headers,
    header_pages: HeaderSlots<'a>,
    alloc: SharedAlloc<'a>,
    blocks: blaze_own::Vec<OnceLock<PageBlock<'a>>, SharedAlloc<'a>>,
    /// Lazy cache of decoded 256-page radix leaf shards (`bits > RADIX_FLAT_MAX_BITS`); empty
    /// for flat roots. Kept off-stack in a `Vec` so `Stored` stays `<= 1 KiB` by value.
    radix_shards: blaze_own::Vec<OnceLock<Option<pages::RadixShard<'a>>>, SharedAlloc<'a>>,
    refused: OnceLock<PagesError>,
    /// How a page's bytes are known to be its key's: the store's proof handed through
    /// ([`pages::Check::Store`], the command's path: no byte hashed, a page's head alone checked)
    /// or every byte hashed and every entry walked ([`pages::Check::Hashed`], `--verify`).
    check: pages::Check,
    metrics: blaze_own::Box<StoredMetrics, SharedAlloc<'a>>,
    /// Each name asked, sharded across [`ASKED_SHARDS`] 128-byte `CachePadded` shards indexed
    /// directly by `name`'s hash bytes (zero shared atomic counter): what [`Stored::asked`] tells
    /// by kind, repeats and distinct names apart (rule 17).
    asked: AskedShards<'a>,
}

/// Shards for [`Stored::asked`] so parallel 128-core body workers never serialize on one global
/// lock or a shared `asks` atomic counter (power of two for bitmask indexing).
const ASKED_SHARDS: usize = 32;

/// Per-shard cap on kept ask entries (rule 18: bounded table): `4096` per shard (`131,072` across
/// all shards), so even a hot repeated `Name` stays told while asks past the cap are counted in
/// `AskedShard::asks` (`AskedByKind::untold`).
const MOST_ASKS_PER_SHARD: usize = 4096;

type AskedShards<'a> = blaze_own::Box<[bstd::sync::CachePadded<AskedShard<'a>>; ASKED_SHARDS], SharedAlloc<'a>>;

/// A request's rows asked of the stored answer, by kind ([`Stored::asked`]): each kind's asks
/// (repeats included) and distinct names, and the asks past the cap not told; and by purpose
/// ([`blaze_engine::Ask`]): each purpose's asks and its distinct reads (a name and purpose, a
/// cell ask's cell too), so a repeat is the same read asked again, never another cell's read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AskedByKind {
    pub(crate) asks: [u32; blaze_drive::key::KINDS],
    pub(crate) distinct: [u32; blaze_drive::key::KINDS],
    pub(crate) by_purpose: [u32; blaze_engine::Ask::PURPOSES],
    pub(crate) purpose_distinct: [u32; blaze_engine::Ask::PURPOSES],
    /// Each kind's asks by purpose (repeats included): which of a kind's asks repeat (rule 17:
    /// a row asked 9 times is told as its purposes before anyone guesses which reader asks).
    pub(crate) kind_purpose: [[u32; blaze_engine::Ask::PURPOSES]; blaze_drive::key::KINDS],
    pub(crate) untold: u32,
}

impl Default for AskedByKind {
    fn default() -> AskedByKind {
        AskedByKind {
            asks: [0; blaze_drive::key::KINDS],
            distinct: [0; blaze_drive::key::KINDS],
            by_purpose: [0; blaze_engine::Ask::PURPOSES],
            purpose_distinct: [0; blaze_engine::Ask::PURPOSES],
            kind_purpose: [[0; blaze_engine::Ask::PURPOSES]; blaze_drive::key::KINDS],
            untold: 0,
        }
    }
}

impl AskedByKind {
    /// The purposes whose distinct reads differ between `self` and `other`, by word: what a
    /// world test names when a request's reads grow.
    #[cfg(test)]
    pub(crate) fn purposes_moved(&self, other: &AskedByKind) -> std::vec::Vec<&'static str> {
        (0..blaze_engine::Ask::PURPOSES)
            .filter(|&at| self.purpose_distinct[at] != other.purpose_distinct[at])
            .map(|at| blaze_engine::Ask::WORDS[at])
            .collect()
    }
}

/// `Kind asks/distinct` for each kind asked, then the untold asks: the profile's `rows asked:`
/// words and the world test's message.
impl core::fmt::Display for AskedByKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (asks, distinct): (u32, u32) = (self.asks.iter().sum(), self.distinct.iter().sum());
        write!(f, "rows asked {asks} ({distinct} distinct):")?;
        for kind in blaze_drive::Kind::ALL {
            let at = kind.index();
            if self.asks[at] > 0 {
                write!(f, " {kind:?} {}/{}", self.asks[at], self.distinct[at])?;
                // Its asks by purpose, when it asked any row more often than it has rows.
                if self.asks[at] > self.distinct[at] {
                    let mut sep = " (";
                    for (purpose, &count) in self.kind_purpose[at].iter().enumerate() {
                        if count > 0 {
                            write!(f, "{sep}{} {count}", blaze_engine::Ask::WORDS[purpose])?;
                            sep = ", ";
                        }
                    }
                    write!(f, ")")?;
                }
            }
        }
        write!(f, "; by purpose (asks/distinct reads):")?;
        for at in 0..blaze_engine::Ask::PURPOSES {
            if self.by_purpose[at] > 0 {
                write!(f, " {} {}/{}", blaze_engine::Ask::WORDS[at], self.by_purpose[at], self.purpose_distinct[at])?;
            }
        }
        if self.untold > 0 {
            write!(f, "; {} past the cap not told", self.untold)?;
        }
        Ok(())
    }
}

/// What a request read of its stored answer's pages ([`Stored::pages_read_line`]): the profile's
/// `pages: N read, M bytes, K hashed` line, the one the engine's simulation prints for its cases.
/// Its floor on an edit is the pages the edited rows, their files' plans and the producer header
/// pages they name sit on, and `K` is 0 on the command's path.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct PagesRead {
    pub(crate) task: usize,
    pub(crate) header: usize,
    pub(crate) bytes: u64,
    pub(crate) hashed: u64,
}

impl fmt::Display for PagesRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pages: {} read ({} task, {} producer header), {} bytes, {} hashed", self.task + self.header, self.task, self.header, self.bytes, self.hashed)
    }
}

impl<'a> Stored<'a> {
    /// `check`: [`pages::Check::Store`] on the command's path (each page's bytes with the
    /// store's proof, plain_place.rs `snapshot_get`), [`pages::Check::Hashed`] for an explicit
    /// verify (every byte hashed, every entry walked: `VerifiedPage::open_whole`).
    pub(crate) fn new(root: &'a Root<'a>, snapshot: &'a Snapshot<SharedAlloc<'a>>, headers: &'a Headers, check: pages::Check, alloc: SharedAlloc<'a>) -> Self {
        use bstd::sync::CachePadded;
        use core::sync::atomic::{AtomicU64, AtomicUsize};
        let total_pages = root.page_count();
        let blocks = blaze_own::Vec::from_iter_in((0..total_pages.div_ceil(BLOCK)).map(|_| OnceLock::new()), alloc);
        let radix_shards = blaze_own::Vec::from_iter_in((0..root.page_shards.len()).map(|_| OnceLock::new()), alloc);
        let metrics = blaze_own::Box::new_in(
            StoredMetrics {
                task_reads: CachePadded::new(AtomicUsize::new(0)),
                header_reads: CachePadded::new(AtomicUsize::new(0)),
                task_bytes: CachePadded::new(AtomicU64::new(0)),
                header_bytes: CachePadded::new(AtomicU64::new(0)),
                hashed_bytes: CachePadded::new(AtomicU64::new(0)),
            },
            alloc,
        );
        Self {
            root, snapshot, headers, header_pages: blaze_own::Box::new_in(core::array::from_fn(|_| OnceLock::new()), alloc), alloc, blocks, radix_shards, refused: OnceLock::new(), check,
            metrics,
            asked: blaze_own::Box::new_in(
                core::array::from_fn(|_| {
                    CachePadded::new(AskedShard {
                        asks: AtomicUsize::new(0),
                        entries: blaze_core::lock::Lock::new("engine.answer: the rows asked by name", blaze_own::Vec::with_capacity_in(0, alloc)),
                    })
                }),
                alloc,
            ),
        }
    }

    /// Selects a `CachePadded` ask shard directly from `name`'s hash bytes, eliminating shared
    /// global `asks.fetch_add(1)` cache-line contention across 128 workers.
    #[inline]
    fn ask_shard_index(name: Name) -> usize {
        let b0 = usize::from(name.subject.0[0]);
        let b1 = usize::from(name.subject.0[1]);
        let kind = name.kind.index();
        (b0 ^ b1.rotate_left(3) ^ kind) & (ASKED_SHARDS - 1)
    }

    /// One ask of `name`'s page for `ask`: counted in its hash-selected `CachePadded` shard and
    /// kept under the per-shard cap for [`Stored::asked`].
    fn note_ask(&self, name: Name, ask: blaze_engine::Ask) {
        let Some(shard) = self.asked.get(Self::ask_shard_index(name)) else { return };
        let slot = shard.asks.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if slot >= MOST_ASKS_PER_SHARD {
            return;
        }
        let mut asked = shard.entries.lock();
        // A refused growth leaves the name untold, counted by `shard.asks` (rule 3: the count
        // says so, `AskedByKind::untold`).
        let _ = asked.try_push((name, ask));
    }

    /// The rows asked so far by kind (repeats and distinct names apart) and by purpose (asks
    /// and distinct reads apart) ([`AskedByKind`]).
    pub(crate) fn asked(&self) -> AskedByKind {
        use core::sync::atomic::Ordering::Relaxed;
        let mut by = AskedByKind::default();
        let mut total_asks = 0usize;
        let mut kept_cap = 0usize;
        for shard in self.asked.iter() {
            let shard_asks = shard.asks.load(Relaxed);
            total_asks = total_asks.saturating_add(shard_asks);
            kept_cap = kept_cap.saturating_add(shard_asks.min(MOST_ASKS_PER_SHARD));
        }
        let mut sorted: blaze_own::Vec<(Name, blaze_engine::Ask), SharedAlloc<'a>> = blaze_own::Vec::with_capacity_in(kept_cap, self.alloc);
        let mut kept = 0usize;
        for shard in self.asked.iter() {
            let asked = shard.entries.lock();
            kept = kept.saturating_add(asked.len());
            for &(name, ask) in asked.iter() {
                if let Some(slot) = by.asks.get_mut(name.kind.index()) {
                    *slot += 1;
                }
                if let Some(slot) = by.by_purpose.get_mut(ask.purpose()) {
                    *slot += 1;
                }
                if let Some(slot) = by.kind_purpose.get_mut(name.kind.index()).and_then(|row| row.get_mut(ask.purpose())) {
                    *slot += 1;
                }
                let _ = sorted.try_push((name, ask));
            }
        }
        by.untold = u32::try_from(total_asks.saturating_sub(kept)).unwrap_or(u32::MAX);
        sorted.sort_unstable();
        let mut last_name = None;
        let mut last_read = None;
        for &(name, ask) in sorted.iter() {
            if last_name != Some(name) {
                by.distinct[name.kind.index()] += 1;
            }
            if last_read != Some((name, ask)) {
                by.purpose_distinct[ask.purpose()] += 1;
            }
            last_name = Some(name);
            last_read = Some((name, ask));
        }
        by
    }

    /// How many times a reader asked this answer for a row by name ([`StoredPages::header`],
    /// [`StoredPages::page`]), repeats included: what a request's reads of the answer scale
    /// with, apart from the page packing (a page holds the rows its names' leading bits pick, so
    /// one set of names asked spans more pages in a larger answer: `pages_read` is not the measure).
    pub(crate) fn asks(&self) -> usize {
        use core::sync::atomic::Ordering::Relaxed;
        let mut total = 0usize;
        for shard in self.asked.iter() {
            total = total.saturating_add(shard.asks.load(Relaxed));
        }
        total
    }

    /// The page holding `name`'s row (or `READERS` entry for `Ask::Readers`, or arbitrary page for
    /// `Ask::Witness`), asked by name for `ask` (counted in [`Stored::asks`] and told by purpose in
    /// [`Stored::asked`]).
    pub(crate) fn named_page(&self, name: Name, ask: blaze_engine::Ask) -> Result<Option<&VerifiedPage<'a>>, PagesError> {
        self.note_ask(name, ask);
        self.page_at(pages::page_for_ask(name, ask, self.root.bits) as usize)
    }

    /// Page `page`'s record under `key` from the snapshot, sealed by this request's `check`:
    /// the store's proof handed through, or every byte hashed (and counted as hashed).
    fn sealed(&self, key: Key, page: u32, bytes_read: &core::sync::atomic::AtomicU64) -> Result<pages::Sealed<'a>, PagesError> {
        use core::sync::atomic::Ordering::Relaxed;
        let held = crate::store::plain_place::snapshot_get(self.snapshot, key)
            .map_err(|_| PagesError::PageRecord { page, key })?
            .ok_or(PagesError::Missing { page, key })?;
        bytes_read.fetch_add(held.bytes.len() as u64, Relaxed);
        match self.check {
            pages::Check::Store => Ok(Sealed::by_store(key, held.bytes, held.proof)),
            pages::Check::Hashed => {
                self.metrics.hashed_bytes.fetch_add(held.bytes.len() as u64, Relaxed);
                Sealed::hashed(key, held.bytes).map_err(|torn| torn.at(page))
            }
        }
    }

    /// The record under `key` in this answer's snapshot, in place, under the store's proof (the
    /// one check of its segment, rule 74): what a row of the answer names, read for the request.
    pub(crate) fn record_bytes(&self, key: Key) -> Result<Option<&'a [u8]>, blaze_cas::plain::SnapshotError> {
        Ok(crate::store::plain_place::snapshot_get(self.snapshot, key)?.map(|held| held.bytes))
    }

    /// The `pages:` line's counts so far.
    pub(crate) fn pages_read_line(&self) -> PagesRead {
        use core::sync::atomic::Ordering::Relaxed;
        let (task, header) = self.read_counts();
        PagesRead {
            task,
            header,
            bytes: self.metrics.task_bytes.load(Relaxed) + self.metrics.header_bytes.load(Relaxed),
            hashed: self.metrics.hashed_bytes.load(Relaxed),
        }
    }

    pub(crate) fn answer(&self) -> blaze_engine::Answer<'_> {
        let mut answer = blaze_engine::Answer::from_pages(self, self.root.serial, self.alloc);
        answer.owed_tasks.extend(self.root.owed_tasks.iter().map(|(&name, &plan)| (name, plan)));
        answer.owed_cells.extend(self.root.owed_cells.iter().map(|(&id, &at)| (id, at)));
        answer.with_findings.extend(self.root.findings.keys().copied());
        answer
    }

    /// Shard `index`, opened once. Asked only of a current index (headers.rs `Headers::header`,
    /// `Headers::planner_row`, `Headers::after` ask a shard of nothing else); asked of an
    /// absent one it is refused by name, never answered from a stale key.
    fn headers_at(&self, index: usize) -> Result<Option<&IndexedHeaders<'a>>, PagesError> {
        let key = match self.headers {
            Headers::Current(keys) => keys.keys[index],
            Headers::Absent { since } => {
                return Err(*self.refused.get_or_init(|| PagesError::HeadersAbsent { page: u32::try_from(index).unwrap_or(u32::MAX), since: *since }));
            }
        };
        if key == Key::default() {
            return Ok(None);
        }
        let proof = self.header_pages[index].get_or_init(|| {
            self.metrics.header_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let page = index as u32;
            IndexedHeaders::open(self.sealed(key, page, &self.metrics.header_bytes)?, index)
        });
        match proof {
            Ok(page) => Ok(Some(page)),
            Err(error) => Err(*self.refused.get_or_init(|| *error)),
        }
    }

    /// The units planner and script ordering consume these full prior results: from a current
    /// shard, or from the row's page when the index is absent.
    pub(crate) fn planner_row<'r>(&self, name: Name, alloc: SharedAlloc<'r>) -> Result<Option<blaze_engine::Row<'r>>, PagesError> {
        self.headers.planner_row(name, |index| self.headers_at(index), || self.page(name), alloc)
    }

    /// The header index this answer names (the profile's `headers:` word).
    pub(crate) fn headers(&self) -> &'a Headers {
        self.headers
    }

    /// Lock-free read of a committed record from this answer's snapshot (for parallel standing
    /// outline read-backs in `engine_check::plan`).
    pub(crate) fn snapshot_record(&self, key: Key) -> Result<Option<&'a [u8]>, blaze_cas::plain::SnapshotError> {
        Ok(crate::store::plain_place::snapshot_get(self.snapshot, key)?.map(|held| held.bytes))
    }

    pub(crate) fn read_counts(&self) -> (usize, usize) {
        use core::sync::atomic::Ordering::Relaxed;
        (self.metrics.task_reads.load(Relaxed), self.metrics.header_reads.load(Relaxed))
    }

    #[cfg(test)]
    pub(crate) fn pages_read(&self) -> usize {
        let (tasks, headers) = self.read_counts();
        tasks + headers
    }

    /// Resolves page `index`'s key: O(1) slice index for flat roots, or lazy 8 KiB leaf-shard
    /// fetch-and-cache for 2-level radix roots (`bits > RADIX_FLAT_MAX_BITS`).
    fn page_key(&self, index: usize) -> Result<Key, PagesError> {
        let page = u32::try_from(index).unwrap_or(u32::MAX);
        if !self.root.is_radix() {
            return self
                .root
                .pages
                .get(index)
                .copied()
                .ok_or_else(|| *self.refused.get_or_init(|| PagesError::Count { page: pages::ROOT_PAGE, count: page }));
        }
        let (shard_idx, slot_idx) = Root::shard_of(index);
        let Some(&shard_key) = self.root.page_shards.get(shard_idx) else {
            return Err(*self.refused.get_or_init(|| PagesError::Count { page: pages::ROOT_PAGE, count: page }));
        };
        if shard_key == Key::default() {
            return Ok(Key::default());
        }
        let Some(shard_cell) = self.radix_shards.get(shard_idx) else {
            return Err(*self.refused.get_or_init(|| PagesError::Count { page: pages::ROOT_PAGE, count: page }));
        };
        let cached = shard_cell.get_or_init(|| {
            let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
            let opened = self
                .sealed(shard_key, pages::ROOT_PAGE, &self.metrics.task_bytes)
                .and_then(|sealed| pages::decode_radix_shard(sealed, shard_u32, self.root.bits, self.alloc));
            match opened {
                Ok(shard) => Some(shard),
                Err(why) => {
                    self.refused.get_or_init(|| why);
                    None
                }
            }
        });
        match cached {
            Some(shard) => Ok(shard.keys.get(slot_idx).copied().unwrap_or_default()),
            None => Err(self.refused.get().copied().unwrap_or(PagesError::Count { page: pages::ROOT_PAGE, count: page })),
        }
    }

    /// Reads an auxiliary radix record from this answer's snapshot, sealed by `self.check`
    /// (for `pages::encode_changes_radix` to read ONLY the touched leaf shards), propagating any
    /// snapshot error rather than silently converting it to `None`.
    fn get_radix_record(&self, key: Key) -> Result<pages::Got<'a>, blaze_cas::plain::SnapshotError> {
        let Some(held) = crate::store::plain_place::snapshot_get(self.snapshot, key)? else {
            return Ok(None);
        };
        Ok(match self.check {
            pages::Check::Store => Some(Ok(Sealed::by_store(key, held.bytes, held.proof))),
            pages::Check::Hashed => Some(Sealed::hashed(key, held.bytes)),
        })
    }

    pub(crate) fn page_at(&self, index: usize) -> Result<Option<&VerifiedPage<'a>>, PagesError> {
        let page = u32::try_from(index).unwrap_or(u32::MAX);
        let key = self.page_key(index)?;
        if key == Key::default() {
            return Ok(None);
        }
        let block = self.blocks[index / BLOCK].get_or_init(|| blaze_own::Box::new_in(core::array::from_fn(|_| OnceLock::new()), self.alloc));
        let proof = block[index % BLOCK].get_or_init(|| {
            self.metrics.task_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // On the store's proof the head alone is checked, no byte hashed and no entry walked
            // (pages/view.rs `open`); an explicit verify hashes and walks every entry.
            let opened = self.sealed(key, page, &self.metrics.task_bytes).and_then(|sealed| match self.check {
                pages::Check::Store => VerifiedPage::open(sealed, page, self.root.bits),
                pages::Check::Hashed => VerifiedPage::open_whole(sealed, page, self.root.bits),
            });
            match opened {
                Ok(page) => Some(blaze_own::Box::new_in(page, self.alloc)),
                Err(why) => {
                    self.refused.get_or_init(|| why);
                    None
                }
            }
        });
        match proof {
            Some(page) => Ok(Some(page)),
            None => Err(*self.refused.get().expect("a refused page records its error before publishing its slot")),
        }
    }
}

impl StoredPages for Stored<'_> {
    fn header(&self, name: Name) -> Result<Option<Header>, PagesError> {
        self.note_ask(name, blaze_engine::Ask::Header);
        self.headers.header(name, |index| self.headers_at(index), || self.page_at(pages::content_page_of(name, self.root.bits) as usize))
    }
    fn page(&self, name: Name) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.named_page(name, blaze_engine::Ask::Unsaid)
    }
    fn page_for(&self, name: Name, ask: blaze_engine::Ask) -> Result<Option<&VerifiedPage<'_>>, PagesError> {
        self.named_page(name, ask)
    }

    fn refusal(&self) -> Option<PagesError> {
        self.refused.get().copied()
    }

    fn finding(&self, name: Name) -> Option<u32> {
        self.root.findings.get(&name).copied()
    }

    fn visit_rows(&self, visit: &mut dyn FnMut(Name) -> Result<(), blaze_engine::LastError>) -> Result<(), blaze_engine::LastError> {
        // In `BER6`, `ROWS` live exclusively in the lower content half (`0 .. half_pages(bits)`).
        if !self.root.is_radix() {
            let content_pages = usize::try_from(pages::half_pages(self.root.bits))
                .unwrap_or(self.root.pages.len())
                .min(self.root.pages.len());
            for index in 0..content_pages {
                if let Some(page) = self.page_at(index).map_err(blaze_engine::LastError::Pages)? {
                    for name in page.row_names() {
                        visit(name)?;
                    }
                }
            }
        } else {
            let content_shards = self.root.page_shards.len() / 2;
            for (shard_idx, &shard_key) in self.root.page_shards.iter().take(content_shards).enumerate() {
                if shard_key == Key::default() {
                    continue;
                }
                let base = shard_idx << pages::RADIX_LEAF_BITS;
                for slot in 0..pages::RADIX_LEAF_PAGES {
                    if let Some(page) = self.page_at(base + slot).map_err(blaze_engine::LastError::Pages)? {
                        for name in page.row_names() {
                            visit(name)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Exhaustive inspection for commands and tests which explicitly ask for all rows.
pub(crate) fn load_answer<'m, 'a, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, root_key: Key, alloc: SharedAlloc<'a>) -> Result<Result<(Kept<'a>, Root<'a>), Unreadable>, CliError> {
    // Each record through the store; a store's own error stops the command after the decode.
    let mut failed: Option<CliError> = None;
    let root = match open_root(cx, store, root_key, alloc)? {
        Ok(root) => root,
        Err(why) => return Ok(Err(why)),
    };
    let decoded = pages::decode_at(
        &root,
        |key| match store.get(cx, key) {
            Ok(found) => found.map(|held| Ok(Sealed::by_store(key, held.bytes, held.proof))),
            Err(error) => {
                failed.get_or_insert(error);
                None
            }
        },
        alloc,
    );
    if let Some(error) = failed {
        return Err(error);
    }
    let kept = match decoded {
        Ok(kept) => kept,
        Err(why) => return Ok(Err(Unreadable::Pages(why))),
    };
    Ok(Ok((kept, root)))
}

/// Opens an answer root directory directly from a lent store handle without creating a
/// [`CommandStore`] (used by pre-check warm selection lookup in `engine_check.rs`).
pub(crate) fn open_root_from_lent<'a, D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    key: Key,
    alloc: SharedAlloc<'a>,
) -> Result<Result<Root<'a>, Unreadable>, CliError> {
    let Some(held) = lent.get(&blaze_cas::Key(key.0)).map_err(crate::store::PlaceError::from)? else {
        return Ok(Err(Unreadable::Missing { key }));
    };
    let sealed_root = match Sealed::hashed(key, held.bytes) {
        Ok(s) => s,
        Err(why) => return Ok(Err(Unreadable::Pages(why.at(pages::ROOT_PAGE)))),
    };
    let mut store_err: Option<CliError> = None;
    let decoded = pages::decode_root_with(
        sealed_root,
        |sub_key| match lent.get(&blaze_cas::Key(sub_key.0)) {
            Ok(Some(h)) => Some(Sealed::hashed(sub_key, h.bytes)),
            Ok(None) => None,
            Err(err) => {
                store_err.get_or_insert(crate::store::PlaceError::from(err).into());
                None
            }
        },
        alloc,
    );
    if let Some(err) = store_err {
        return Err(err);
    }
    Ok(decoded.map_err(Unreadable::Pages))
}

/// The pointer the root `name` names, read directly from a lent store handle without creating a
/// [`CommandStore`]: `None` when absent.
pub(crate) fn load_pointer_from_lent<'m, D: Driver>(
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let Some(root_key) = lent.root_named(name) else { return Ok(None) };
    let Some(keys) = lent.get(&root_key).map_err(crate::store::PlaceError::from)? else { return Ok(None) };
    if keys.bytes.len() != 32 {
        return Ok(Some(Err(Unreadable::Names { bytes: keys.bytes.len() })));
    }
    let first = key_at(keys.bytes, 0);
    let Some(held) = lent.get(&blaze_cas::Key(first.0)).map_err(crate::store::PlaceError::from)? else {
        return Ok(Some(Err(Unreadable::Missing { key: first })));
    };
    decode_pointer_bytes(first, held.bytes).map(Some)
}

/// The pointer the root `name` names, read in place: `None` when there is none.
pub(crate) fn load_pointer<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, name: &str) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let Some(keys) = store.named(name)? else { return Ok(None) };
    if keys.len() != 32 {
        return Ok(Some(Err(Unreadable::Names { bytes: keys.len() })));
    }
    pointer_at(cx, store, key_at(keys, 0)).map(Some)
}

/// The pointer record `first`, read in place (a root's one key, or a base's, [`load_base`]).
fn pointer_at<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, first: Key) -> Result<Result<Pointer<'m>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, first)? else { return Ok(Err(Unreadable::Missing { key: first })) };
    decode_pointer_bytes(first, held.bytes)
}

/// Decodes a sealed pointer record `sealed` at key `first` in place (shared by [`load_pointer`]
/// and `daemon_server.rs` when arming `KqueueVnode` directly from the open store mmap).
pub(crate) fn decode_pointer_bytes<'m>(first: Key, sealed: &'m [u8]) -> Result<Result<Pointer<'m>, Unreadable>, CliError> {
    if !sealed.starts_with(seal::SEAL_MAGIC) {
        return Ok(Err(Unreadable::Older));
    }
    let (pointer, leaves) = match seal::open(sealed, seal::Plant::None) {
        Ok(opened) => opened,
        Err(torn) => return Ok(Err(Unreadable::Torn(torn))),
    };
    if pointer.len() < POINTER_BYTES || !pointer.starts_with(POINTER_MAGIC) {
        return Ok(Err(Unreadable::Older));
    }
    let Some(journal) = crate::journal_sweep::Stored::of(&pointer[POINTER_BYTES..]) else {
        return Ok(Err(Unreadable::Older));
    };
    let (leaves, older) = Leaves::of(leaves)?;
    if older.is_some() {
        return Ok(Err(Unreadable::Older));
    }
    let facts = Facts { files: word(pointer, 68), printed: Printed::of_byte(pointer[72]), findings: word(pointer, 73), owed: word(pointer, 77) };
    if pointer[72] > 7 {
        return Ok(Err(Unreadable::Older));
    }
    let keys = HeaderPages { keys: core::array::from_fn(|i| key_at(pointer, SHARDS_AT + i * 32)) };
    let since = u64::from_le_bytes(pointer[HEADERS_AT + 1..SHARDS_AT].try_into().expect("eight bytes"));
    let headers = match pointer[HEADERS_AT] {
        HEADERS_CURRENT if since == 0 => Headers::Current(keys),
        HEADERS_ABSENT if keys.keys.iter().all(|key| *key == Key::default()) => Headers::Absent { since: blaze_engine::Gen(since) },
        tag => return Ok(Err(Unreadable::Headers { tag })),
    };
    let tools = key_at(pointer, TOOLS_AT).0;
    Ok(Ok(Pointer { key: first, root: key_at(pointer, 4), headers, lines: key_at(pointer, 36), facts, tools, leaves, journal }))
}

/// The printed lines' record the pointer names, every entry checked (engine_lines.rs
/// `decode_in_frame`), decompressing `BZR1`-enveloped lines into `frame` while leaving raw/small
/// records zero-copy.
pub(crate) fn load_lines_in<'m, 'f, 'b, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    key: Key,
    frame: &'f Frame,
    alloc: SharedAlloc<'a>,
) -> Result<Result<crate::engine_lines::Last<'b, 'a>, Unreadable>, CliError>
where
    'm: 'b,
    'f: 'b,
{
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    let bytes: &'m [u8] = held.bytes;
    Ok(crate::engine_lines::decode_in_frame(bytes, frame, alloc).map_err(Unreadable::Lines))
}

/// The printed lines' record the pointer names, every entry checked (engine_lines.rs `decode`).
pub(crate) fn load_lines<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    key: Key,
    alloc: SharedAlloc<'static>,
) -> Result<Result<crate::engine_lines::Last<'m, 'static>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    let bytes: &'m [u8] = held.bytes;
    Ok(crate::engine_lines::decode(bytes, alloc).map_err(Unreadable::Lines))
}

/// What one write put.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Saved {
    /// The new pages and their reach indexes, the root, the lines and the sealed pointer.
    pub(crate) puts: u32,
    /// Task pages in the new root and their total bytes.
    pub(crate) pages: u32,
    pub(crate) bytes: u64,
    /// Task pages this write put.
    pub(crate) new_pages: u32,
    /// The bytes of the task pages this write put, and of the largest: a commit costs these
    /// (each page is merged whole, keyed by its bytes, then put), so the profile names a page
    /// that is large for what moved on it (a hot cell's reader list, pages.rs `READERS`).
    pub(crate) new_page_bytes: u64,
    pub(crate) largest_new_page: u64,
    pub(crate) new_headers: u32,
    /// The new root's key (pages.rs `Root`): a function of every page's key, the owed sets and
    /// the findings, so two drivers' answers of one checkout are equal when their roots are.
    pub(crate) root: Key,
    /// The digest of the root's page keys in order, the zero key of an empty slot too
    /// ([`PAGES_DOMAIN`]): the world test names which side moved when two roots differ. Made in
    /// a test build alone: 2 MB hashed a commit on tempo's 65,536 pages (edit12 profile,
    /// c0340wave89), and the root's key already covers every page's.
    pub(crate) pages_digest: Option<ContentId>,
    /// The pages this commit keyed into the reach root, and why it was made whole if it was
    /// ([`page_indexes`]).
    pub(crate) rekeyed: Rekeyed,
    /// Of a commit folded into the last answer: the entries its delta wrote and the pages they
    /// fell on (blaze-engine pages.rs `Encoded::changed_entries`), each page read once as its
    /// witness; (0, 0) for a whole encode.
    pub(crate) changed: (u32, u32),
    /// What the save did with the home's bases ([`BASES_ROOT`]).
    pub(crate) base: BaseKept,
    /// The newly sealed pointer's key, lines key, and certificate facts (for the resident daemon
    /// to update `HotAnswer` and `CheckoutWatcher` without tearing down `kqueue`).
    pub(crate) pointer_key: Key,
    pub(crate) lines_key: Key,
    pub(crate) facts: Facts,
}

/// Which pages a commit keys into the answer's reach root ([`page_indexes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rekey {
    /// Only the pages whose key moved since the last answer: every other page's index is
    /// carried from the last commit's reach root, read in place.
    Changed,
    /// Every page, as a cold commit does: the planted control of the page edit test (a commit
    /// that re-keys what did not move).
    #[cfg(test)]
    All,
}

/// What [`page_indexes`] did: the pages it keyed (their index computed now: one digest for the
/// new key, one for the old a page replaced), and why the reach root was made whole if it was.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Rekeyed {
    pub(crate) pages: u32,
    pub(crate) whole: Option<WhyWhole>,
}

/// Why a commit's reach root was made from every page rather than carried: each a first commit
/// of its kind, said on the profile's line, never a quiet second route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WhyWhole {
    /// No last answer: the first commit of this checkout's answer.
    NoLast,
    /// The last answer's paging differs (its `bits`): every page's index moved.
    Paging,
    /// No unexpired reach root or answer root under the checkout's names (`{name}.reach`,
    /// `{name}.answer`): the first commit of this form, or one past the roots' life.
    NoRoot,
    /// The answer root names another answer than the one this commit is made over: another
    /// command committed between this one's read and its write.
    NotLast,
    /// The planted control ([`Rekey::All`]).
    Planted,
}

impl fmt::Display for Rekeyed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.whole {
            None => write!(f, "{} page(s) re-keyed, the rest carried from the last reach root", self.pages),
            Some(why) => write!(f, "{} page(s) keyed, the reach root made whole ({why:?})", self.pages),
        }
    }
}

/// The checkout's reach root's version: the pages' indexes alone (the root, lines, header
/// shards and member rows under [`ANSWER_ROOT`]). The form before it (`{name}.pages`, the direct
/// index among the pages') is never read: it expires by its life.
pub(crate) const REACH_ROOT: &str = "reach";
pub(crate) const ANSWER_ROOT: &str = "answer";
/// Page slots per reach shard once an answer spans more than one shard (`> 512` pages):
/// at Tempo's 65,536 pages, 128 base shards (`{name}.reach.00`..`{name}.reach.7f`) plus one
/// bounded delta root (`{name}.reach.delta`, capped at [`REACH_DELTA_CAP`]) let incremental edits
/// renew all 128 base shards in O(1) (`Lent::renew_root`, 0 CAS probes) and probe only the delta's
/// changed page indexes (e.g. 4 probes for a 4-page edit, 82 for an 82-page edit) instead of
/// probing `touched_shards * 512` keys. Once the delta would exceed [`REACH_DELTA_CAP`], changed
/// shards are folded and `{name}.reach.delta` is reset to empty. Small answers (`<= 512` pages)
/// keep the single `{name}.reach` root.
pub(crate) const REACH_SHARD_PAGES: usize = 512;
pub(crate) const REACH_SHARDS_MOST: usize = (1usize << blaze_drive::answers::MOST_BITS) / REACH_SHARD_PAGES;
/// Maximum page indexes accumulated in `{name}.reach.delta` before folding into changed base shards.
pub(crate) const REACH_DELTA_CAP: usize = 512;
const REACH_DELTA_SUFFIX: &str = "delta";

const PAGES_DOMAIN: &str = "blaze-cli engine-check page keys";

/// What a request wrote of the stored answer and read of the last (engine_check.rs
/// `Checked::stored`): the oracle of the drivers' world test (engine_ready_edit_tests.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StoredKeys {
    /// The new root's key ([`Saved::root`]).
    pub(crate) root: Key,
    /// The digest of its page keys in order ([`Saved::pages_digest`], a test build's alone) and
    /// how many there are.
    pub(crate) pages: (Option<ContentId>, u32),
    /// What the commit keyed into its reach root ([`Saved::rekeyed`]).
    pub(crate) rekeyed: Rekeyed,
    /// Task pages of the last answer this request read (`Stored::read_counts`'s first).
    pub(crate) pages_read: usize,
    /// Producer header shards of the last answer this request read (its second): 0 under the
    /// ready driver, which answers every header from the row's page.
    pub(crate) header_shards_read: usize,
    /// Rows of the last answer this request asked by name, by kind, repeats and distinct names
    /// apart ([`Stored::asked`]).
    pub(crate) asked: AskedByKind,
    /// The commit's delta: entries written and the pages they fell on ([`Saved::changed`]).
    pub(crate) changed: (u32, u32),
}

/// The profile's `answer keys:` line: what two drivers' runs of one edit are compared by.
impl core::fmt::Display for StoredKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (digest, count) = self.pages;
        write!(f, "answer keys: root {}, {count} page(s)", self.root)?;
        if let Some(digest) = digest {
            write!(f, " digest {}", Key(digest))?;
        }
        write!(f, "; read of the last: {} task page(s), {} producer header shard(s); reach: {}", self.pages_read, self.header_shards_read, self.rekeyed)
    }
}

impl StoredKeys {
    pub(crate) fn of(saved: &Saved, last: Option<&Stored<'_>>) -> StoredKeys {
        let (pages_read, header_shards_read) = last.map_or((0, 0), Stored::read_counts);
        StoredKeys { root: saved.root, pages: (saved.pages_digest, saved.pages), rekeyed: saved.rekeyed, pages_read, header_shards_read, asked: last.map_or_else(AskedByKind::default, Stored::asked), changed: saved.changed }
    }
}

/// `kept` and the leaf table of `rows` stored under the root `name` over `last` (its pages not
/// put again), by the phase path: the producer header shards its rows sit on rewritten when
/// current (blaze-engine headers.rs `Headers::after`); visible from the store's commit after
/// the answer.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_edit<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    (last_root, workers): (Option<Key>, core::num::NonZeroU32),
    save_base: SaveBase,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, last, Writer::Phases, rows, journal, alloc, frame, row_keys, (last_root, Rekey::Changed), workers, save_base)
}

/// [`save_edit`] keying every page into the reach root: the page edit test's planted control.
#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_edit_rekeying_all<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    last_root: Option<Key>,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, last, Writer::Phases, rows, journal, alloc, frame, &[], (last_root, Rekey::All), core::num::NonZeroU32::MIN, SaveBase::Keep)
}

/// The `Ready` driver's commit of an edit (D187, engine_ready_check.rs): `kept`'s changed rows
/// merged into only the pages they sit on over `last` (pages.rs `encode_changes`), the lines,
/// then the sealed pointer last, in one store commit. No producer header shard is read or
/// written: the pointer names the index [`Headers::Absent`] (since this request, or since the
/// commit that first left it so), and every later header lookup reads the row's page.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save_changes<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: &Stored<'_>,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    (last_root, workers): (Option<Key>, core::num::NonZeroU32),
    save_base: SaveBase,
) -> Result<Saved, CliError> {
    save(cx, store, lent, name, toolchain, tools, kept, printed, lines, Some(last), Writer::Ready, rows, journal, alloc, frame, row_keys, (last_root, Rekey::Changed), workers, save_base)
}

const SAVE_TASK_FRAME: usize = 64 << 10;
/// Reusable per-worker bump frame reserved for `BEP6` page dictionary & merge scratch.
/// Virtual reservation only; pages are committed lazily and trimmed back to [`PAGE_SCRATCH_KEEP`].
const PAGE_SCRATCH_FRAME: usize = 1 << 30;
/// Retained warm capacity for per-worker page encoding scratch frames after trimming.
const PAGE_SCRATCH_KEEP: usize = 256 << 10;
/// High-water trim threshold for per-worker page encoding scratch frames.
const PAGE_SCRATCH_TRIM: usize = 2 << 20;
/// Minimum changed/non-empty answer pages to fan out across workers (0..=3 run inline with zero
/// thread-spawn overhead on 1-line incremental edits).
const PAGE_PAR_MIN: usize = 4;
/// Minimum dirty producer header shards to fan out across workers (0..=1 run inline with zero
/// thread-spawn overhead on 1-line incremental edits).
const HEADER_PAR_MIN: usize = 2;

/// Runs independent encoding jobs in parallel when `jobs.len() >= min_par` and more than one
/// worker is active, returning results in exact `jobs` index order (Rule 25). Below `min_par` or
/// on 1 worker, runs inline with zero thread-spawn or atomic overhead.
fn run_jobs_par<'a, J, R, F>(
    cx: &Cx,
    workers: core::num::NonZeroU32,
    min_par: usize,
    span: &'static str,
    jobs: &[J],
    alloc: SharedAlloc<'a>,
    run: F,
) -> Result<blaze_own::Vec<R, SharedAlloc<'a>>, CliError>
where
    J: Sync,
    R: Send,
    F: Fn(&J) -> R + Sync,
{
    let mut out = blaze_own::Vec::with_capacity_in(jobs.len(), alloc);
    let active_workers = workers.get().min(cx.workers());
    if jobs.len() < min_par || active_workers <= 1 {
        for job in jobs {
            out.push(run(job));
        }
        return Ok(out);
    }
    let n = jobs.len();
    let worker_count = usize::try_from(active_workers).unwrap_or(1).min(n);
    // Dynamic work stealing balances skewed page sizes (e.g. hot `READERS` pages) and skewed
    // header shards (64 heavy `Signature` shards vs tiny `Units`/`Fetch` shards) across 18/128
    // cores. Batching 8 pages at a time when `n >= worker_count * 16` amortizes atomic fetch_add.
    let batch = if n >= worker_count.saturating_mul(16) { 8 } else { 1 };
    let next = core::sync::atomic::AtomicUsize::new(0);
    let next_ref = &next;
    let run_ref = &run;
    let scope = cx.scope();
    scope.narrow_workers(workers, bstd::task::Class::Foreground);
    let mut handles = blaze_own::Vec::with_capacity_in(worker_count, alloc);
    let est_per_worker = (n / worker_count).saturating_add(batch);
    for _ in 0..worker_count {
        handles.push(scope.may_overlap(span, SAVE_TASK_FRAME, move |_cx: &Cx| -> Result<_, core::convert::Infallible> {
            let mut local = blaze_own::Vec::with_capacity_in(est_per_worker, alloc);
            loop {
                let start = next_ref.fetch_add(batch, core::sync::atomic::Ordering::Relaxed);
                if start >= n {
                    break;
                }
                let end = start.saturating_add(batch).min(n);
                if let Some(slice) = jobs.get(start..end) {
                    for (offset, job) in slice.iter().enumerate() {
                        local.push((start + offset, run_ref(job)));
                    }
                }
            }
            Ok(local)
        }));
    }
    let mut pool_err: Option<CliError> = None;
    let mut slots: blaze_own::Vec<Option<R>, SharedAlloc<'a>> = blaze_own::Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        slots.push(None);
    }
    for handle in handles {
        match handle.join(cx) {
            Ok(part) => {
                for (idx, item) in part {
                    if let Some(slot) = slots.get_mut(idx) {
                        *slot = Some(item);
                    }
                }
            }
            Err(error) => {
                pool_err.get_or_insert(CliError::Pool(error));
            }
        }
    }
    if let Err(error) = scope.join(cx) {
        pool_err.get_or_insert(CliError::Pool(error));
    }
    if let Some(error) = pool_err {
        return Err(error);
    }
    for slot in slots {
        if let Some(item) = slot {
            out.push(item);
        }
    }
    Ok(out)
}

/// Maximum changed causes in an incremental `save()` that reuse `request_alloc` directly instead
/// of creating and dropping a fresh 64 GiB `PHASE_RESERVE` `blaze_mem::Root`.
const SAVE_SCOPED_ARENA_CAUSES: usize = 64;

/// [`run_jobs_par`] giving each worker its own reusable bump [`Frame`] (reset between jobs and
/// trimmed above [`PAGE_SCRATCH_TRIM`]) so `BEP6` page dictionary & merge scratch never contends
/// on `save_mem` or allocates on `Global`.
///
/// When `reuse_frame` is `Some` and the jobs run inline (`jobs.len() < min_par || active_workers
/// <= 1`), uses the caller's frame directly so small 1-line incremental edits skip reserving and
/// unmapping a 1 GiB `PAGE_SCRATCH_FRAME`.
#[expect(clippy::too_many_arguments, reason = "parallel framed job runner: each argument one scheduling/memory handle")]
fn run_jobs_par_framed<'a, J, R, F>(
    cx: &Cx,
    workers: core::num::NonZeroU32,
    min_par: usize,
    span: &'static str,
    jobs: &[J],
    alloc: SharedAlloc<'a>,
    reuse_frame: Option<&Frame>,
    run: F,
) -> Result<blaze_own::Vec<R, SharedAlloc<'a>>, CliError>
where
    J: Sync,
    R: Send,
    F: Fn(&J, &Frame) -> R + Sync,
{
    let mut out = blaze_own::Vec::with_capacity_in(jobs.len(), alloc);
    if jobs.is_empty() {
        return Ok(out);
    }
    let active_workers = workers.get().min(cx.workers());
    if jobs.len() < min_par || active_workers <= 1 {
        if let Some(scratch) = reuse_frame {
            for job in jobs {
                out.push(run(job, scratch));
            }
            return Ok(out);
        }
        let mut scratch = Frame::with_reserve(PAGE_SCRATCH_FRAME, blaze_mem::Tag::Query)
            .map_err(|error| crate::engine_phase_mem::PhaseMemError::Request { error })?;
        for job in jobs {
            scratch.reset();
            if scratch.committed() > PAGE_SCRATCH_TRIM {
                scratch.trim(PAGE_SCRATCH_KEEP);
            }
            out.push(run(job, &scratch));
        }
        return Ok(out);
    }
    let n = jobs.len();
    let worker_count = usize::try_from(active_workers).unwrap_or(1).min(n);
    let batch = if n >= worker_count.saturating_mul(16) { 8 } else { 1 };
    let next = core::sync::atomic::AtomicUsize::new(0);
    let next_ref = &next;
    let run_ref = &run;
    let scope = cx.scope();
    scope.narrow_workers(workers, bstd::task::Class::Foreground);
    let mut handles = blaze_own::Vec::with_capacity_in(worker_count, alloc);
    let est_per_worker = (n / worker_count).saturating_add(batch);
    for _ in 0..worker_count {
        handles.push(scope.may_overlap(span, SAVE_TASK_FRAME, move |_cx: &Cx| -> Result<_, core::convert::Infallible> {
            let mut scratch = match Frame::with_reserve(PAGE_SCRATCH_FRAME, blaze_mem::Tag::Query) {
                Ok(frame) => frame,
                Err(error) => return Ok(Err(error)),
            };
            let mut local = blaze_own::Vec::with_capacity_in(est_per_worker, alloc);
            loop {
                let start = next_ref.fetch_add(batch, core::sync::atomic::Ordering::Relaxed);
                if start >= n {
                    break;
                }
                let end = start.saturating_add(batch).min(n);
                if let Some(slice) = jobs.get(start..end) {
                    for (offset, job) in slice.iter().enumerate() {
                        scratch.reset();
                        if scratch.committed() > PAGE_SCRATCH_TRIM {
                            scratch.trim(PAGE_SCRATCH_KEEP);
                        }
                        local.push((start + offset, run_ref(job, &scratch)));
                    }
                }
            }
            Ok(Ok(local))
        }));
    }
    let mut pool_err: Option<CliError> = None;
    let mut slots: blaze_own::Vec<Option<R>, SharedAlloc<'a>> = blaze_own::Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        slots.push(None);
    }
    for handle in handles {
        match handle.join(cx) {
            Ok(Ok(part)) => {
                for (idx, item) in part {
                    if let Some(slot) = slots.get_mut(idx) {
                        *slot = Some(item);
                    }
                }
            }
            Ok(Err(error)) => {
                pool_err.get_or_insert_with(|| CliError::from(crate::engine_phase_mem::PhaseMemError::Request { error }));
            }
            Err(error) => {
                pool_err.get_or_insert(CliError::Pool(error));
            }
        }
    }
    if let Err(error) = scope.join(cx) {
        pool_err.get_or_insert(CliError::Pool(error));
    }
    if let Some(error) = pool_err {
        return Err(error);
    }
    for slot in slots {
        if let Some(item) = slot {
            out.push(item);
        }
    }
    Ok(out)
}

#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
fn save<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Stored<'_>>,
    writer: Writer,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    request_alloc: SharedAlloc<'_>,
    frame: &Frame,
    row_keys: &[Key],
    (last_root, rekey): (Option<Key>, Rekey),
    workers: core::num::NonZeroU32,
    save_base: SaveBase,
) -> Result<Saved, CliError> {
    // Incremental edits (`last.is_some()` with few changed causes) reuse `request_alloc` and the
    // caller's scratch frame directly, avoiding the cost of reserving and unmapping a brand-new
    // 64 GiB `PHASE_RESERVE` `blaze_mem::Root` + 1 GiB `PAGE_SCRATCH_FRAME` on every edit.
    // Cold or large rebuilds keep a scoped `blaze_mem::Root` so 100% of their serialization
    // buffers return to the OS before `after_answer` commit.
    let small_edit = last.is_some() && kept.causes.len() <= SAVE_SCOPED_ARENA_CAUSES;
    let save_mem = if small_edit {
        None
    } else {
        Some(
            blaze_mem::Root::new(blaze_mem::RootOpts {
                reserve: crate::engine_phase_mem::PHASE_RESERVE,
                tag: blaze_mem::Tag::Query,
                live_at_drop: blaze_mem::LiveAtDrop::Refused,
            })
            .map_err(|error| crate::engine_phase_mem::PhaseMemError::Request { error })?,
        )
    };
    let alloc = match save_mem.as_ref() {
        Some(mem) => mem.shared(),
        None => request_alloc,
    };
    let inline_scratch = small_edit.then_some(frame);
    let result = (|| -> Result<Saved, CliError> {
    let mut encoded = match last {
        Some(stored) => {
            let mut pool_err: Option<CliError> = None;
            let mut snap_err: Option<CliError> = None;
            let encoded = pages::encode_changes_radix(
                kept,
                stored.root,
                alloc,
                |key| match stored.get_radix_record(key) {
                    Ok(got) => got,
                    Err(err) => {
                        snap_err.get_or_insert_with(|| CliError::from(crate::store::PlaceError::Snapshot(err)));
                        None
                    }
                },
                |jobs| match run_jobs_par_framed(cx, workers, PAGE_PAR_MIN, "answer pages", jobs, alloc, inline_scratch, |job, scratch| job.run_in(alloc, scratch)) {
                    Ok(built) => built,
                    Err(error) => {
                        pool_err = Some(error);
                        blaze_own::Vec::new_in(alloc)
                    }
                },
            );
            if let Some(error) = snap_err {
                return Err(error);
            }
            if let Some(error) = pool_err {
                return Err(error);
            }
            encoded.map_err(page_error)?
        }
        None => {
            let mut pool_err: Option<CliError> = None;
            let encoded = pages::encode_par(kept, None, alloc, |jobs| {
                match run_jobs_par_framed(cx, workers, PAGE_PAR_MIN, "answer pages", jobs, alloc, inline_scratch, |job, scratch| job.run_in(alloc, scratch)) {
                    Ok(built) => built,
                    Err(error) => {
                        pool_err = Some(error);
                        blaze_own::Vec::new_in(alloc)
                    }
                }
            });
            if let Some(error) = pool_err {
                return Err(error);
            }
            encoded
        }
    };
    // The encoder's puts are task pages first (`..task_puts`), then radix/directory sub-records,
    // then the top root last (durable order: children before parent).
    let task_puts = usize::try_from(encoded.task_puts).unwrap_or(0);
    let mut saved = Saved {
        bytes: encoded.bytes,
        new_pages: encoded.task_puts,
        root: encoded.root,
        pages_digest: None,
        changed: (encoded.changed_entries, encoded.changed_pages),
        ..Saved::default()
    };
    // Drain `encoded.puts` as each record is written into `store` so encoded page buffers return
    // to `save_mem` immediately rather than remaining live until after the whole loop.
    let puts = core::mem::replace(&mut encoded.puts, blaze_own::Vec::new_in(alloc));
    for (at, (key, bytes)) in puts.into_iter().enumerate() {
        store.put_local(cx, key, &bytes)?;
        saved.puts += 1;
        if at < task_puts {
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            saved.new_page_bytes += len;
            saved.largest_new_page = saved.largest_new_page.max(len);
        }
    }
    #[cfg(test)]
    {
        saved.pages_digest = Some(test_pages_digest(lent, &encoded, alloc, frame)?);
    }
    let mut header_pool_err: Option<CliError> = None;
    let (headers, writes) = Headers::after_with(last.map(|base| base.headers), kept, writer, kept.answer.serial, HeadersPlant::None, |index| match last {
        Some(base) => base.headers_at(index),
        None => Ok(None),
    }, alloc, |jobs| match run_jobs_par(cx, workers, HEADER_PAR_MIN, "answer headers", jobs, alloc, |job| job.run(alloc)) {
        Ok(built) => built,
        Err(error) => {
            header_pool_err = Some(error);
            blaze_own::Vec::new_in(alloc)
        }
    }).map_err(page_error)?;
    if let Some(error) = header_pool_err {
        return Err(error);
    }
    saved.new_headers = writes.len() as u32;
    for (key, bytes) in &writes {
        store.put_local(cx, *key, bytes)?;
        saved.puts += 1;
    }
    drop(writes);
    // The printed lines, under the key of their bytes: equal lines are one record.
    // Empty bytes signal that `ViewSlot::render` already proved `lines.0` is held in the store
    // and skipped re-encoding it; `direct` below still pins `lines.0` under `{name}.answer`.
    if !lines.1.is_empty() {
        store.put_local(cx, lines.0, lines.1)?;
        saved.puts += 1;
    }
    let leaves = Leaves::bytes_in(rows, frame)?;
    let mut pointer: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    pointer.extend_from_slice(POINTER_MAGIC);
    pointer.extend_from_slice(&encoded.root.0.0);
    pointer.extend_from_slice(&lines.0.0.0);
    let facts = Facts { files: encoded.files, printed, findings: encoded.findings, owed: encoded.owed };
    pointer.extend_from_slice(&facts.files.to_le_bytes());
    pointer.push(facts.printed.byte());
    pointer.extend_from_slice(&facts.findings.to_le_bytes());
    pointer.extend_from_slice(&facts.owed.to_le_bytes());
    pointer.extend_from_slice(&tools_digest(tools).0);
    match &headers {
        Headers::Current(keys) => {
            pointer.push(HEADERS_CURRENT);
            pointer.extend_from_slice(&0u64.to_le_bytes());
            for key in &keys.keys {
                pointer.extend_from_slice(&key.0.0);
            }
        }
        Headers::Absent { since } => {
            pointer.push(HEADERS_ABSENT);
            pointer.extend_from_slice(&since.0.to_le_bytes());
            // No shard key: the zero key in each of the index's slots.
            pointer.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        }
    }
    journal.encode(&mut pointer);
    let sealed = seal::seal(&pointer, &leaves, frame)?;
    let key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put_local(cx, key, &sealed)?;
    saved.puts += 1;
    saved.pointer_key = key;
    saved.lines_key = lines.0;
    saved.facts = facts;
    // A page and the payloads of *every* row on it share one immutable reach index.
    // StoreSink also includes each names payload's expansion index and its children.
    // The store follows exactly one index level from an indexed root.
    // Prepare, sort, and deduplicate per-page CAS key lists in parallel across workers; on the
    // main thread, publish non-scope pages directly into `lent.index_beside` with zero re-copy or
    // serial re-sort, and delegate the few `CrateScope` pages to `StoreSink::index_rows_into`.
    use crate::engine_answer_rows::{PublishError, StoreSink};
    struct PreparedReach<'a> {
        page: Key,
        keys: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'a>>,
        scopes: blaze_own::Vec<(Name, Key), SharedAlloc<'a>>,
    }
    // Fast-path pages with 0 `RECORDS` (including all upper-half `READERS`-only pages) directly to
    // a minimal 32-byte `index_beside(page, &[page])` with zero alloc/sort/dedup and zero worker
    // fan-out, preserving 100% `blaze-cas` `IndexedRoot` GC reachability while cutting `Index`
    // beside-record writes on reader-edge edits from megabytes to ~32 B/page.
    let mut nonempty_recs: blaze_own::Vec<(Key, &[(Name, Key)]), SharedAlloc<'_>> =
        blaze_own::Vec::new_in(alloc);
    let mut empty_pages: blaze_own::Vec<Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
    for (page, rows) in encoded.page_records() {
        if rows.is_empty() {
            empty_pages.push(page);
        } else {
            nonempty_recs.push((page, rows));
        }
    }
    let prepared = run_jobs_par(cx, workers, PAGE_PAR_MIN, "answer reach", &nonempty_recs, alloc, |&(page, rows)| {
        let mut keys: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> =
            blaze_own::Vec::with_capacity_in(rows.len().saturating_add(1), alloc);
        let mut scopes: blaze_own::Vec<(Name, Key), SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
        keys.push(blaze_cas::Key(page.0));
        for &(name, record) in rows {
            keys.push(blaze_cas::Key(record.0));
            if name.kind == blaze_drive::Kind::CrateScope {
                scopes.push((name, record));
            }
        }
        if scopes.is_empty() {
            keys.sort_unstable();
            keys.dedup();
        }
        PreparedReach { page, keys, scopes }
    })?;
    let mut sink = StoreSink { cx, store, lent, toolchain };
    let mut records = blaze_own::Vec::new_in(alloc);
    let mut toolchain_records = blaze_own::Vec::new_in(alloc);
    let mut cas_buf: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
    let mut indexed_beside: BesideCache<'_> = blaze_hash::Map::new(blaze_hash::MapOpts {
        hasher: blaze_hash::BuildKeyHasher::default(),
        alloc,
        capacity: Some(task_puts),
    });
    for page in empty_pages {
        let cas_page = blaze_cas::Key(page.0);
        let beside = sink
            .lent
            .index_beside(cas_page, &[cas_page])
            .map_err(|error| reach_error(PublishError::Cas(error)))?;
        indexed_beside.insert(page.0.0, beside);
        saved.puts += 1;
    }
    for (item, &(_, rows)) in prepared.into_iter().zip(nonempty_recs.iter()) {
        for &(name, record) in rows {
            if sink.store.is_toolchain(name) {
                toolchain_records.push(record);
            }
        }
        let beside = if item.scopes.is_empty() {
            if item.keys.len() > blaze_cas::plain::INDEX_KEYS_MOST {
                return Err(reach_error(PublishError::RowsReach {
                    segment: item.page,
                    cap: blaze_cas::plain::INDEX_KEYS_MOST,
                }));
            }
            sink.lent
                .index_beside(blaze_cas::Key(item.page.0), &item.keys)
                .map_err(|error| reach_error(PublishError::Cas(error)))?
        } else {
            records.clear();
            records.reserve(rows.len().saturating_add(1));
            records.push(item.page);
            for &(_, record) in rows {
                records.push(record);
            }
            sink.index_rows_into(item.page, &records, &item.scopes, &mut cas_buf).map_err(reach_error)?
        };
        indexed_beside.insert(item.page.0.0, beside);
        saved.puts += 1;
    }
    let reach_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}");
    let answer_name = bstd::mem::format_in!(frame, "{name}.{ANSWER_ROOT}");
    let last_info = match (last, last_root) {
        (Some(base), Some(key)) => Some((base.root.page_count(), key)),
        _ => None,
    };
    let mut unsharded_indexes: Option<blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>>> = None;
    let (pages_live, rekeyed) = if !encoded.is_radix() {
        let last_pages = match (last, last_root) {
            (Some(base), Some(key)) if !base.root.is_radix() => Some((&base.root.pages[..], key)),
            _ => None,
        };
        let (indexes, rekeyed) = page_indexes(lent, (reach_name.as_str(), answer_name.as_str()), last_pages, &encoded.pages, rekey, Some(&indexed_beside), alloc)?;
        let count = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
        if !(rekeyed.whole.is_none()
            && rekeyed.pages == 0
            && lent.renew_root(reach_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live())
        {
            root_through_reach(lent, reach_name.as_str(), &indexes, Some(&indexed_beside), last_root)?;
        }
        // Retire any stale sharded reach roots left over if this answer previously exceeded the
        // radix threshold and shrank back to unsharded, so certified no-ops never renew them.
        unroot_reach_shards(lent, name, 0, true, frame);
        unsharded_indexes = Some(indexes);
        (count, rekeyed)
    } else {
        if lent.has_root(reach_name.as_str()) {
            lent.unroot(reach_name.as_str());
        }
        publish_reach_shards_from(
            lent,
            name,
            answer_name.as_str(),
            ReachSource::Radix { last: last_info, encoded: &encoded },
            rekey,
            Some(&indexed_beside),
            alloc,
            frame,
        )?
    };
    saved.pages = pages_live;
    saved.rekeyed = rekeyed;
    let mut base_direct = blaze_own::Vec::with_capacity_in(2 + encoded.tree_keys().len() + HeaderPages::COUNT, alloc);
    base_direct.push(blaze_cas::Key(key.0));
    base_direct.push(blaze_cas::Key(encoded.root.0));
    base_direct.extend(encoded.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
    base_direct.push(blaze_cas::Key(lines.0.0));
    if let Some(keys) = headers.current() {
        base_direct.extend(keys.keys.iter().filter(|k| **k != Key::default()).map(|k| blaze_cas::Key(k.0)));
    }
    // Pure function of `key` (every entry is embedded in or content-addressed by the sealed
    // pointer): pinned under `BASES_KEPT_ROOT` and reused by cross-worktree `adopt_certified`.
    let base_direct_index = lent.index_beside(blaze_cas::Key(key.0), &base_direct).map_err(crate::store::PlaceError::from)?;
    // Two roots, one commit: the pages' reach roots (one `{name}.reach` up to 256 pages, or
    // 256-page shards `{name}.reach.00`..`{name}.reach.ff` above that, where unchanged shards
    // renew in O(1)), and the answer's own index, which names this answer's root key: the next
    // commit's proof that the reach roots are this answer's ([`prove_last_reach`]).
    // When no extra member listing `row_keys` are attached (the common case on body-only `Ready`
    // edits), `base_direct_index` already holds `encoded.root`, every tree key, `lines`, and
    // every header shard, so `{name}.answer` reuses it directly without writing a second CAS index.
    let answer_index = if row_keys.is_empty() {
        base_direct_index
    } else {
        let mut direct = blaze_own::Vec::with_capacity_in(base_direct.len() + row_keys.len(), alloc);
        if let Some(rest) = base_direct.get(1..) {
            direct.extend_from_slice(rest);
        }
        // The member listing rows this request read or put (engine_bodies.rs `member_row_keys`,
        // engine_member_rows.rs): named here so they live while the answer does, and the next
        // command reads them by key instead of opening and listing each member again.
        direct.extend(row_keys.iter().map(|key| blaze_cas::Key(key.0)));
        lent.index(&direct).map_err(crate::store::PlaceError::from)?
    };
    lent.root_through(answer_name.as_str(), &[answer_index], blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
    // This root accumulates indexes across commands. Unchanged pages' toolchain
    // payloads were admitted with the page and this renewal extends their lifetime.
    store.keep_toolchain(toolchain, &toolchain_records)?;
    // Refresh the toolchain-only fallback base (`tool-base.<PINNED>.<TRIPLE>.<tools>`) whenever
    // this build holds non-failed closed toolchain rows, so any fresh project or git worktree
    // automatically reuses pre-warmed `core`/`alloc`/`std`/`bstd`.
    keep_toolchain_base(cx, store, lent, toolchain, tools, kept, workers, alloc, frame)?;
    store.pin(name, &[key])?;
    // Reset `<name>.journal` to `[key]` (reusing the exact same 32-byte root record as `name`)
    // so subsequent runs do not probe older segments for a stale checkpoint of an earlier answer.
    store.pin(crate::journal_checkpoint::name(name, frame).as_str(), &[key])?;
    // This answer as its workspace's newest base: a checkout of it with none of its own starts
    // from it ([`BASES_ROOT`]). Scoped checks skip this so a partial answer never replaces a
    // workspace base. The control keeps the one home-wide pointer instead.
    let base_aux = match save_base {
        SaveBase::Skip => {
            saved.base = BaseKept::NotAsked;
            return Ok(saved);
        }
        SaveBase::Keep => BaseAux::default(),
        SaveBase::KeepWith(aux) => aux,
    };
    #[cfg(test)]
    if base_plant() == BasePlant::OnePointer {
        store.pin(LATEST_ROOT, &[key])?;
        return Ok(saved);
    }
    saved.base = keep_base(
        cx,
        store,
        lent,
        rows,
        key,
        base_direct_index,
        base_aux,
        &encoded,
        &indexed_beside,
        unsharded_indexes.as_deref(),
        last_info,
        rekey,
        alloc,
        frame,
    )?;
    Ok(saved)
    })();
    drop(save_mem);
    result
}

/// Prefix for a kept workspace base's own reach and answer roots:
/// `engine.bases.slot.<16 hex of workspace>.<16 hex of layout>`.
/// Each entry in [`BASES_ROOT`] has a unique `(workspace, layout)` pair (bounded by
/// [`BASES_MOST`] = 32), so incremental edits in the same layout reuse the sharded reach fast
/// path in O(1) while older kept layouts retain all of their pages across GC and `blaze clean`.
fn base_slot_prefix<'f>(workspace: ContentId, layout: ContentId, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("engine.bases.slot.");
    for byte in workspace.0.iter().take(8) {
        let _ = write!(name, "{byte:02x}");
    }
    name.push('.');
    for byte in layout.0.iter().take(8) {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// Unroots every GC reach root owned by an evicted or pruned workspace base slot.
fn unroot_base_slot<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    workspace: ContentId,
    layout: ContentId,
    frame: &Frame,
) {
    let prefix = base_slot_prefix(workspace, layout, frame);
    let answer_name = bstd::mem::format_in!(frame, "{}.{ANSWER_ROOT}", prefix.as_str());
    let reach_name = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}", prefix.as_str());
    let members_name = bstd::mem::format_in!(frame, "{}.members", prefix.as_str());
    let grains_name = bstd::mem::format_in!(frame, "{}.grains", prefix.as_str());
    if lent.has_root(answer_name.as_str()) {
        lent.unroot(answer_name.as_str());
    }
    if lent.has_root(reach_name.as_str()) {
        lent.unroot(reach_name.as_str());
    }
    if lent.has_root(members_name.as_str()) {
        lent.unroot(members_name.as_str());
    }
    if lent.has_root(grains_name.as_str()) {
        lent.unroot(grains_name.as_str());
    }
    unroot_reach_shards(lent, prefix.as_str(), 0, true, frame);
}

/// Renews an already-kept workspace base slot's answer, auxiliary (`.members`/`.grains`), and
/// reach roots in O(1) so surviving entries in [`BASES_ROOT`] do not expire while another checkout
/// keeps updating the list.
fn renew_base_slot<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    workspace: ContentId,
    layout: ContentId,
    frame: &Frame,
) {
    let prefix = base_slot_prefix(workspace, layout, frame);
    let answer_name = bstd::mem::format_in!(frame, "{}.{ANSWER_ROOT}", prefix.as_str());
    let _ = lent.renew_root(answer_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS);
    let members_name = bstd::mem::format_in!(frame, "{}.members", prefix.as_str());
    let _ = lent.renew_root(members_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS);
    let grains_name = bstd::mem::format_in!(frame, "{}.grains", prefix.as_str());
    let _ = lent.renew_root(grains_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS);
    let reach_name = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}", prefix.as_str());
    if lent.renew_root(reach_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live() {
        return;
    }
    let first = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}.00", prefix.as_str());
    if !lent.renew_root(first.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live() {
        return;
    }
    let mut shard_name = blaze_own::String::new_in(frame);
    for shard in 1..REACH_SHARDS_MOST {
        shard_name.clear();
        let _ = write!(shard_name, "{}.{REACH_ROOT}.{shard:02x}", prefix.as_str());
        if !lent.renew_root(shard_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live() {
            break;
        }
    }
    let delta_name = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}", prefix.as_str());
    let _ = lent.renew_root(delta_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS);
}

/// Resolves the `Index` record to pin under [`BASES_KEPT_ROOT`] for `pointer`: uses the
/// deterministic `beside_key(pointer)` index holding `[pointer, root, tree_keys..., lines,
/// headers...]` when present in `lent`, else falls back to a single-key index `[pointer]`.
fn kept_index_for_base<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    pointer: Key,
) -> Result<blaze_cas::Key, CliError> {
    let cas_ptr = blaze_cas::Key(pointer.0);
    let beside = blaze_cas::plain::beside_key(cas_ptr);
    if let Some(held) = lent.get(&beside).map_err(crate::store::PlaceError::from)?
        && blaze_keyed::kind::ReservedTag::Index.is(held.tag)
        && held.bytes.len() % 32 == 0
    {
        return Ok(beside);
    }
    Ok(lent.index(&[cas_ptr]).map_err(crate::store::PlaceError::from)?)
}

/// The bases with `pointer` as its workspace's newest ([`with_base`]), rewritten and pinned
/// alongside each base's answer root, lines, header shards, and page reach indexes so GC or
/// `blaze clean` in another worktree cannot leave any base in [`BASES_ROOT`] dangling.
#[expect(clippy::too_many_arguments, reason = "base publish edge: each argument one handle or reach input")]
fn keep_base<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    rows: &Rows<'_>,
    pointer: Key,
    base_direct_index: blaze_cas::Key,
    base_aux: BaseAux,
    encoded: &pages::Encoded<'_>,
    indexed_beside: &BesideCache<'_>,
    unsharded_indexes: Option<&[blaze_cas::Key]>,
    last_info: Option<(usize, Key)>,
    rekey: Rekey,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<BaseKept, CliError> {
    let Some(layout) = layout_of(rows, frame) else { return Ok(BaseKept::NoWorkspace) };
    let lock = rows.lock_id().unwrap_or(NO_LOCK);
    let (old_bases, replaced) = match read_bases(cx, store, frame)? {
        Some(Some(bases)) => (bases, false),
        Some(None) => (blaze_own::Vec::new_in(frame), true),
        None => (blaze_own::Vec::new_in(frame), false),
    };
    let Some(workspace) = workspace_of_rows(&old_bases, rows.workspace, layout, lock) else { return Ok(BaseKept::Unknown) };
    let now = Base { workspace: workspace.id, lock, layout, root: root_of(rows), pointer, seq: 0 };
    let bases = with_base(&old_bases, now, frame);
    let bytes = encode_bases(&bases, frame);
    let record = Key(ContentId::of(BASES_DOMAIN, &bytes));
    store.put_local(cx, record, &bytes)?;
    store.pin(BASES_ROOT, &[record])?;

    // Pin this base's page reach roots under its `(workspace, layout)` slot so pages remain live
    // even if this checkout later moves to another layout or expires.
    let prefix = base_slot_prefix(now.workspace, now.layout, frame);
    let base_answer = bstd::mem::format_in!(frame, "{}.{ANSWER_ROOT}", prefix.as_str());
    let base_reach = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}", prefix.as_str());
    if let Some(indexes) = unsharded_indexes {
        lent.root_through(base_reach.as_str(), indexes, blaze_cas::plain::ROOT_LIFE_NS)
            .map_err(crate::store::PlaceError::from)?;
        unroot_reach_shards(lent, prefix.as_str(), 0, true, frame);
    } else {
        if lent.has_root(base_reach.as_str()) {
            lent.unroot(base_reach.as_str());
        }
        let _ = publish_reach_shards_from(
            lent,
            prefix.as_str(),
            base_answer.as_str(),
            ReachSource::Radix { last: last_info, encoded },
            rekey,
            Some(indexed_beside),
            alloc,
            frame,
        )?;
    }
    lent.root_through(base_answer.as_str(), &[base_direct_index], blaze_cas::plain::ROOT_LIFE_NS)
        .map_err(crate::store::PlaceError::from)?;
    // Propagate standing `.members` and `.grains` keys under this `(workspace, layout)` base slot
    // (without changing `BEB2` layout) so cross-worktree `adopt_certified` and `OtherCheckout`
    // starts inherit member reach and `ReadGrains` directly.
    let base_members = bstd::mem::format_in!(frame, "{}.members", prefix.as_str());
    match base_aux.members {
        Some(key) => {
            if read_single_key(store, base_members.as_str())? != Some(key)
                || !lent.renew_root(base_members.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live()
            {
                store.pin(base_members.as_str(), &[key])?;
            }
        }
        None => {
            if lent.has_root(base_members.as_str()) {
                lent.unroot(base_members.as_str());
            }
        }
    }
    let base_grains = bstd::mem::format_in!(frame, "{}.grains", prefix.as_str());
    match base_aux.grains {
        Some(key) => {
            if read_single_key(store, base_grains.as_str())? != Some(key)
                || !lent.renew_root(base_grains.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live()
            {
                store.pin(base_grains.as_str(), &[key])?;
            }
        }
        None => {
            if lent.has_root(base_grains.as_str()) {
                lent.unroot(base_grains.as_str());
            }
        }
    }

    // Unroot evicted base slots and renew surviving base slots in O(1).
    for old in old_bases.iter() {
        if !bases.iter().any(|it| it.workspace == old.workspace && it.layout == old.layout) {
            unroot_base_slot(lent, old.workspace, old.layout, frame);
        }
    }
    let mut kept_indexes: blaze_own::Vec<blaze_cas::Key, &Frame> = blaze_own::Vec::with_capacity_in(bases.len(), frame);
    for base in bases.iter() {
        if base.workspace == now.workspace && base.layout == now.layout {
            kept_indexes.push(base_direct_index);
        } else {
            renew_base_slot(lent, base.workspace, base.layout, frame);
            kept_indexes.push(kept_index_for_base(lent, base.pointer)?);
        }
    }
    lent.root_through(BASES_KEPT_ROOT, &kept_indexes, blaze_cas::plain::ROOT_LIFE_NS)
        .map_err(crate::store::PlaceError::from)?;
    Ok(match replaced {
        true => BaseKept::Replaced,
        false => BaseKept::Kept { entries: u32::try_from(bases.len()).unwrap_or(u32::MAX) },
    })
}

/// Reads a single 32-byte key from a named store root, or `None` if absent or another length.
fn read_single_key<'m, D: Driver, S: Sharing>(
    store: &mut CommandStore<'m, '_, D, S>,
    root_name: &str,
) -> Result<Option<Key>, CliError> {
    Ok(store.named(root_name)?.filter(|keys| keys.len() == 32).map(|keys| key_at(keys, 0)))
}

/// Reads the optional `<prefix>.members` and `<prefix>.grains` auxiliary keys for a base or root.
fn read_base_aux<'m, D: Driver, S: Sharing>(
    store: &mut CommandStore<'m, '_, D, S>,
    prefix: &str,
    frame: &Frame,
) -> Result<BaseAux, CliError> {
    let members_root = bstd::mem::format_in!(frame, "{prefix}.members");
    let grains_root = bstd::mem::format_in!(frame, "{prefix}.grains");
    Ok(BaseAux {
        members: read_single_key(store, members_root.as_str())?,
        grains: read_single_key(store, grains_root.as_str())?,
    })
}

/// Adopts an identical stored answer from another checkout (`StoredBase::OtherCheckout`, zero
/// moved leaves, matching tools, replayable, 0 owed) under this checkout's own root `name`
/// without re-running the `Phases` driver.
///
/// Writes a fresh sealed pointer carrying this checkout's leaf stamps (`rows`) and journal cursor
/// (`journal`), and pins `{name}`, `{name}.journal`, `{name}.answer`, `{name}.reach` (or
/// sharded reach roots), and any standing `{name}.members` / `{name}.grains` auxiliary keys from
/// `aux` so this checkout's answer is completely independent of the source checkout under GC and
/// `blaze clean` and takes `Ready` on its very first edit with zero `ReadyRefused::Members` and
/// zero `ReadGrains` rebuild.
///
/// Returns `Ok(Some(new_pointer_key))` when adopted, or `Ok(None)` if a legacy radix base lacks
/// its `beside_key(pointer)` root-tree index (caller falls through to `Phases`).
#[expect(clippy::too_many_arguments, reason = "cross-worktree adoption edge: each argument one handle")]
pub(crate) fn adopt_certified<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    pointer: &Pointer<'_>,
    root_dir: &Root<'_>,
    aux: BaseAux,
    rows: &Rows<'_>,
    journal: &crate::journal_sweep::Taken<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<Option<Key>, CliError> {
    let old_cas_ptr = blaze_cas::Key(pointer.key.0);
    let old_beside = blaze_cas::plain::beside_key(old_cas_ptr);
    let mut direct: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
    if let Some(held) = lent.get(&old_beside).map_err(crate::store::PlaceError::from)?
        && blaze_keyed::kind::ReservedTag::Index.is(held.tag)
        && held.bytes.len() % 32 == 0
    {
        let count = held.bytes.len() / 32;
        direct.reserve(count);
        for i in 0..count {
            let k = cas_key_at(held.bytes, i);
            if k != old_cas_ptr {
                direct.push(k);
            }
        }
    } else if !root_dir.is_radix() {
        direct.push(blaze_cas::Key(pointer.root.0));
        direct.push(blaze_cas::Key(pointer.lines.0));
        if let Some(keys) = pointer.headers.current() {
            direct.extend(keys.keys.iter().filter(|k| **k != Key::default()).map(|k| blaze_cas::Key(k.0)));
        }
    } else {
        return Ok(None);
    }

    let leaves = Leaves::bytes_in(rows, frame)?;
    let mut ptr_bytes: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    ptr_bytes.extend_from_slice(POINTER_MAGIC);
    ptr_bytes.extend_from_slice(&pointer.root.0.0);
    ptr_bytes.extend_from_slice(&pointer.lines.0.0);
    ptr_bytes.extend_from_slice(&pointer.facts.files.to_le_bytes());
    ptr_bytes.push(pointer.facts.printed.byte());
    ptr_bytes.extend_from_slice(&pointer.facts.findings.to_le_bytes());
    ptr_bytes.extend_from_slice(&pointer.facts.owed.to_le_bytes());
    ptr_bytes.extend_from_slice(&pointer.tools.0);
    match &pointer.headers {
        Headers::Current(keys) => {
            ptr_bytes.push(HEADERS_CURRENT);
            ptr_bytes.extend_from_slice(&0u64.to_le_bytes());
            for key in &keys.keys {
                ptr_bytes.extend_from_slice(&key.0.0);
            }
        }
        Headers::Absent { since } => {
            ptr_bytes.push(HEADERS_ABSENT);
            ptr_bytes.extend_from_slice(&since.0.to_le_bytes());
            ptr_bytes.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        }
    }
    journal.encode(&mut ptr_bytes);
    let sealed = seal::seal(&ptr_bytes, &leaves, frame)?;
    let new_key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put_local(cx, new_key, &sealed)?;

    let reach_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}");
    let answer_name = bstd::mem::format_in!(frame, "{name}.{ANSWER_ROOT}");
    if !root_dir.is_radix() {
        let mut indexes: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
        indexes.extend(
            root_dir
                .pages
                .iter()
                .filter(|p| **p != Key::default())
                .map(|p| blaze_cas::plain::beside_key(blaze_cas::Key(p.0))),
        );
        indexes.sort_unstable();
        root_through_reach(lent, reach_name.as_str(), &indexes, None, Some(pointer.root))?;
        unroot_reach_shards(lent, name, 0, true, frame);
    } else {
        if lent.has_root(reach_name.as_str()) {
            lent.unroot(reach_name.as_str());
        }
        let num_shards = root_dir.page_count().div_ceil(REACH_SHARD_PAGES);
        let leaves_per_reach = REACH_SHARD_PAGES / pages::RADIX_LEAF_PAGES;
        let mut shard_name = blaze_own::String::new_in(frame);
        let mut scratch: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::with_capacity_in(REACH_SHARD_PAGES, alloc);
        for shard in 0..num_shards {
            shard_name.clear();
            let _ = write!(shard_name, "{name}.{REACH_ROOT}.{shard:02x}");
            scratch.clear();
            let from_leaf = shard * leaves_per_reach;
            let to_leaf = (from_leaf + leaves_per_reach).min(root_dir.page_shards.len());
            for leaf_idx in from_leaf..to_leaf {
                let shard_key = root_dir.page_shards.get(leaf_idx).copied().unwrap_or_default();
                if shard_key == Key::default() {
                    continue;
                }
                let leaf = decode_lent_radix_shard(lent, shard_key, leaf_idx, root_dir.bits, alloc)?;
                scratch.extend(
                    leaf.keys
                        .iter()
                        .filter(|p| **p != Key::default())
                        .map(|p| blaze_cas::plain::beside_key(blaze_cas::Key(p.0))),
                );
            }
            scratch.sort_unstable();
            root_through_reach(lent, shard_name.as_str(), &scratch, None, Some(pointer.root))?;
        }
        let delta_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}");
        lent.root_through(delta_name.as_str(), &[], blaze_cas::plain::ROOT_LIFE_NS)
            .map_err(crate::store::PlaceError::from)?;
        unroot_reach_shards(lent, name, num_shards, false, frame);
    }
    let answer_index = lent.index(&direct).map_err(crate::store::PlaceError::from)?;
    lent.root_through(answer_name.as_str(), &[answer_index], blaze_cas::plain::ROOT_LIFE_NS)
        .map_err(crate::store::PlaceError::from)?;
    let mut base_direct: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::with_capacity_in(direct.len() + 1, alloc);
    base_direct.push(blaze_cas::Key(new_key.0));
    base_direct.extend_from_slice(&direct);
    let _ = lent.index_beside(blaze_cas::Key(new_key.0), &base_direct).map_err(crate::store::PlaceError::from)?;
    store.pin(name, &[new_key])?;
    store.pin(crate::journal_checkpoint::name(name, frame).as_str(), &[new_key])?;
    let members_name = bstd::mem::format_in!(frame, "{name}.members");
    match aux.members {
        Some(key) => store.pin(members_name.as_str(), &[key])?,
        None if lent.has_root(members_name.as_str()) => lent.unroot(members_name.as_str()),
        None => {}
    }
    let grains_name = bstd::mem::format_in!(frame, "{name}.grains");
    match aux.grains {
        Some(key) => store.pin(grains_name.as_str(), &[key])?,
        None if lent.has_root(grains_name.as_str()) => lent.unroot(grains_name.as_str()),
        None => {}
    }
    Ok(Some(new_key))
}

/// The bases the home keeps: `None` with no [`BASES_ROOT`]; `Some(None)` for a root or record
/// not of this form (written anew by the next save).
fn read_bases<'m, 'f, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, frame: &'f Frame) -> Result<Option<Option<blaze_own::Vec<Base, &'f Frame>>>, CliError> {
    let Some(keys) = store.named(BASES_ROOT)? else { return Ok(None) };
    // The record alone ([`BASES_ROOT`]); a root of more keys is the one-root form before
    // 2026-11-21, not this one.
    if keys.len() != 32 {
        return Ok(Some(None));
    }
    let Some(held) = store.get(cx, key_at(keys, 0))? else { return Ok(Some(None)) };
    Ok(Some(decode_bases(held.bytes, frame)))
}

/// A fully validated fallback base answer: its sealed pointer, printed lines record, decoded
/// root directory, and optional standing auxiliary keys (`.members`/`.grains`) all opened cleanly
/// against `store`.
pub(crate) struct LoadedBase<'m, 'a> {
    pub(crate) pointer: Pointer<'m>,
    pub(crate) lines: crate::engine_lines::Last<'m, 'a>,
    pub(crate) root: Root<'a>,
    pub(crate) aux: BaseAux,
}

/// How [`open_named_base`] checks that a stored answer's CAS reach roots are still live in the
/// store manifest before reusing its pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReachCheck<'a> {
    /// Standard answer root (`<prefix>.answer` + either `<prefix>.reach` or sharded
    /// `<prefix>.reach.00..` + `<prefix>.reach.delta`).
    Sharded(&'a str),
    /// Single-root answer (`tool-base.*`: always `<prefix>.answer` + `<prefix>.reach`).
    Single(&'a str),
    /// Test-only `BasePlant::OnePointer` (`LATEST_ROOT` carries no separate reach root).
    #[cfg(test)]
    None,
}

/// Zero-I/O manifest check for the certificate fast path (before `open_root`): returns `true`
/// iff `<prefix>.answer` and either `<prefix>.reach` or a complete, gapless power-of-two set of
/// sharded reach roots `<prefix>.reach.00..` plus `<prefix>.reach.delta` are all unexpired in
/// `lent`. Prevents falsely certifying an answer whose reach shards were partially LRU-evicted
/// or unrooted after a crash.
pub(crate) fn reach_roots_live_without_root<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    prefix: &str,
    frame: &Frame,
) -> bool {
    let answer_name = bstd::mem::format_in!(frame, "{prefix}.{ANSWER_ROOT}");
    if lent.root_named(answer_name.as_str()).is_none() {
        return false;
    }
    let reach_name = bstd::mem::format_in!(frame, "{prefix}.{REACH_ROOT}");
    if lent.root_named(reach_name.as_str()).is_some() {
        return true;
    }
    let delta_name = bstd::mem::format_in!(frame, "{prefix}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}");
    if lent.root_named(delta_name.as_str()).is_none() {
        return false;
    }
    let mut shard_name = blaze_own::String::new_in(frame);
    let mut live_prefix = 0usize;
    let mut seen_missing = false;
    for shard in 0..REACH_SHARDS_MOST {
        shard_name.clear();
        let _ = write!(shard_name, "{prefix}.{REACH_ROOT}.{shard:02x}");
        if lent.root_named(shard_name.as_str()).is_some() {
            if seen_missing {
                return false;
            }
            live_prefix += 1;
        } else {
            seen_missing = true;
        }
    }
    live_prefix >= 2 && live_prefix.is_power_of_two()
}

/// Verifies that every CAS reach root pinning `root`'s pages under `prefix` is still live
/// (unexpired) in `lent`'s manifest.
///
/// On the happy path this performs zero disk I/O (manifest root checks only). When a reach root
/// is missing or expired, it inspects the uncovered pages to surface the exact missing page or
/// `beside_key` (`Unreadable::Pages(PagesError::Missing { .. })`) if GC already swept it, else
/// `Unreadable::MissingReach { what }` naming the missing reach root.
pub(crate) fn verify_reach_roots<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    prefix: &str,
    root: &Root<'_>,
    sharded: bool,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<Result<(), Unreadable>, CliError> {
    let answer_name = bstd::mem::format_in!(frame, "{prefix}.{ANSWER_ROOT}");
    if lent.root_named(answer_name.as_str()).is_none() {
        return Ok(Err(Unreadable::MissingReach { what: MissingReachRoot::Answer }));
    }
    if !sharded || !root.is_radix() {
        let reach_name = bstd::mem::format_in!(frame, "{prefix}.{REACH_ROOT}");
        if lent.root_named(reach_name.as_str()).is_some() {
            return Ok(Ok(()));
        }
        if !root.is_radix() {
            for (idx, &page_key) in root.pages.iter().enumerate() {
                if page_key == Key::default() {
                    continue;
                }
                let page = u32::try_from(idx).unwrap_or(u32::MAX);
                let cas_page = blaze_cas::Key(page_key.0);
                if lent.get(&cas_page).map_err(crate::store::PlaceError::from)?.is_none() {
                    return Ok(Err(Unreadable::Pages(PagesError::Missing { page, key: page_key })));
                }
                let beside = blaze_cas::plain::beside_key(cas_page);
                if lent.get(&beside).map_err(crate::store::PlaceError::from)?.is_none() {
                    return Ok(Err(Unreadable::Pages(PagesError::Missing { page, key: Key(beside.0) })));
                }
            }
        }
        return Ok(Err(Unreadable::MissingReach { what: MissingReachRoot::Reach }));
    }
    let num_shards = root.page_count().div_ceil(REACH_SHARD_PAGES);
    let leaves_per_reach = REACH_SHARD_PAGES / pages::RADIX_LEAF_PAGES;
    let mut shard_name = blaze_own::String::new_in(frame);
    let mut first_missing_shard: Option<u16> = None;
    for shard in 0..num_shards {
        shard_name.clear();
        let _ = write!(shard_name, "{prefix}.{REACH_ROOT}.{shard:02x}");
        if lent.root_named(shard_name.as_str()).is_some() {
            continue;
        }
        first_missing_shard.get_or_insert(u16::try_from(shard).unwrap_or(u16::MAX));
        let from_leaf = shard * leaves_per_reach;
        let to_leaf = (from_leaf + leaves_per_reach).min(root.page_shards.len());
        for leaf_idx in from_leaf..to_leaf {
            let shard_key = root.page_shards.get(leaf_idx).copied().unwrap_or_default();
            if shard_key == Key::default() {
                continue;
            }
            let leaf = match decode_lent_radix_shard(lent, shard_key, leaf_idx, root.bits, alloc) {
                Ok(leaf) => leaf,
                Err(_) => {
                    return Ok(Err(Unreadable::Pages(PagesError::Missing {
                        page: pages::ROOT_PAGE,
                        key: shard_key,
                    })));
                }
            };
            let base = leaf_idx << pages::RADIX_LEAF_BITS;
            for (slot, &page_key) in leaf.keys.iter().enumerate() {
                if page_key == Key::default() {
                    continue;
                }
                let page = u32::try_from(base + slot).unwrap_or(u32::MAX);
                let cas_page = blaze_cas::Key(page_key.0);
                if lent.get(&cas_page).map_err(crate::store::PlaceError::from)?.is_none() {
                    return Ok(Err(Unreadable::Pages(PagesError::Missing { page, key: page_key })));
                }
                let beside = blaze_cas::plain::beside_key(cas_page);
                if lent.get(&beside).map_err(crate::store::PlaceError::from)?.is_none() {
                    return Ok(Err(Unreadable::Pages(PagesError::Missing { page, key: Key(beside.0) })));
                }
            }
        }
    }
    if let Some(shard) = first_missing_shard {
        return Ok(Err(Unreadable::MissingReach { what: MissingReachRoot::Shard(shard) }));
    }
    let delta_name = bstd::mem::format_in!(frame, "{prefix}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}");
    if lent.root_named(delta_name.as_str()).is_none() {
        return Ok(Err(Unreadable::MissingReach { what: MissingReachRoot::Delta }));
    }
    Ok(Ok(()))
}

/// Opens a named root's pointer, lines record, root directory, and optional `.members`/`.grains`
/// auxiliary keys, verifying that its CAS reach roots (`reach`) are still live. Returns `Ok(None)`
/// if the root is absent, unreadable, missing reach roots, excluded, or (when
/// `require_current_tools`) written under other tools.
#[expect(clippy::too_many_arguments, reason = "base open helper: each argument one handle or constraint")]
pub(crate) fn open_named_base<'m, 'f: 'm, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    reach: ReachCheck<'_>,
    tools: &Tools,
    exclude: Option<Key>,
    require_current_tools: bool,
    alloc: SharedAlloc<'a>,
    frame: &'f Frame,
) -> Result<Option<LoadedBase<'m, 'a>>, CliError> {
    let Some(Ok(pointer)) = load_pointer(cx, store, name)? else { return Ok(None) };
    if exclude == Some(pointer.key) {
        return Ok(None);
    }
    if require_current_tools && !pointer.certifies_under(tools) {
        return Ok(None);
    }
    let Ok(lines) = load_lines_in(cx, store, pointer.lines, frame, alloc)? else { return Ok(None) };
    let Ok(root) = open_root(cx, store, pointer.root, alloc)? else { return Ok(None) };
    match reach {
        ReachCheck::Sharded(prefix) => {
            if verify_reach_roots(lent, prefix, &root, true, alloc, frame)?.is_err() {
                return Ok(None);
            }
        }
        ReachCheck::Single(prefix) => {
            if verify_reach_roots(lent, prefix, &root, false, alloc, frame)?.is_err() {
                return Ok(None);
            }
        }
        #[cfg(test)]
        ReachCheck::None => {}
    }
    let aux = read_base_aux(store, name, frame)?;
    Ok(Some(LoadedBase { pointer, lines, root, aux }))
}

/// Opens the best readable workspace base from [`BASES_ROOT`] for the checkout at `dir_root`:
/// iterates [`candidate_bases`] in priority order (same workspace + lock, then same workspace,
/// else same root manifest; newest first), skipping `exclude` and any unreadable or
/// partially-evicted base, and preferring a base written under `tools` (`certifies_under(tools)`)
/// before falling back to any readable base when `!require_current_tools`.
#[expect(clippy::too_many_arguments, reason = "workspace base open helper: each argument one handle or constraint")]
pub(crate) fn open_base_for_tools<'m, 'f: 'm, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    dir_root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    tools: &Tools,
    exclude: Option<Key>,
    require_current_tools: bool,
    alloc: SharedAlloc<'a>,
    frame: &'f Frame,
) -> Result<Option<LoadedBase<'m, 'a>>, CliError> {
    #[cfg(test)]
    if base_plant() == BasePlant::OnePointer {
        return open_named_base(cx, store, lent, LATEST_ROOT, ReachCheck::None, tools, exclude, require_current_tools, alloc, frame);
    }
    let Some(Some(bases)) = read_bases(cx, store, frame)? else { return Ok(None) };
    let read = |name: &str, text: &mut bstd::mem::Bytes<'_>| -> Result<bool, CliError> {
        match dir_root.read_all(cx, &bstd::path::Path::new(name)?, text) {
            Ok(_) => Ok(true),
            Err(error) if matches!(error.kind, bstd::fs::FsKind::NotFound | bstd::fs::FsKind::IsADirectory) => Ok(false),
            Err(error) => Err(error.into()),
        }
    };
    let mut lock_text = bstd::mem::Bytes::new_in(frame, "the checkout's lock, for its workspace's base");
    let (workspace, lock_id) = match read(LOCK_FILE, &mut lock_text)? {
        true => (Workspace::of_lock(lock_text.as_slice(), frame), ContentId::of(crate::sweep::FILE_DOMAIN, lock_text.as_slice())),
        false => (None, NO_LOCK),
    };
    let mut candidates = candidate_bases(&bases, workspace, lock_id, NO_ROOT, frame);
    if candidates.is_empty() {
        let mut manifest = bstd::mem::Bytes::new_in(frame, "the checkout's root manifest, for its workspace's base");
        if read(ROOT_MANIFEST, &mut manifest)? {
            candidates = candidate_bases(&bases, None, NO_LOCK, ContentId::of(crate::sweep::FILE_DOMAIN, manifest.as_slice()), frame);
        }
    }
    let mut fallback: Option<LoadedBase<'m, 'a>> = None;
    for base in candidates.iter() {
        if exclude == Some(base.pointer) {
            continue;
        }
        let Ok(pointer) = pointer_at(cx, store, base.pointer)? else { continue };
        let current = pointer.certifies_under(tools);
        if require_current_tools && !current {
            continue;
        }
        if !current && fallback.is_some() {
            continue;
        }
        let Ok(lines) = load_lines_in(cx, store, pointer.lines, frame, alloc)? else { continue };
        let Ok(root) = open_root(cx, store, pointer.root, alloc)? else { continue };
        let prefix = base_slot_prefix(base.workspace, base.layout, frame);
        if verify_reach_roots(lent, prefix.as_str(), &root, true, alloc, frame)?.is_err() {
            continue;
        }
        let aux = read_base_aux(store, prefix.as_str(), frame)?;
        let loaded = LoadedBase { pointer, lines, root, aux };
        if current {
            return Ok(Some(loaded));
        }
        fallback = Some(loaded);
    }
    Ok(fallback)
}

/// Opens the pre-warmed toolchain-only fallback base (`tool-base.<PINNED>.<TRIPLE>.<tools>`),
/// verifying its pointer, lines, root directory, reach roots, and non-empty `root.files` (so a
/// pre-fix `tool-base` written without `by_file` is never loaded to leak stale `CrateScope` rows).
pub(crate) fn open_toolchain_base<'m, 'f: 'm, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    alloc: SharedAlloc<'a>,
    frame: &'f Frame,
) -> Result<Option<LoadedBase<'m, 'a>>, CliError> {
    let name = toolchain_base_root(toolchain, tools, frame);
    let Some(loaded) = open_named_base(
        cx,
        store,
        lent,
        name.as_str(),
        ReachCheck::Single(name.as_str()),
        tools,
        None,
        false,
        alloc,
        frame,
    )? else {
        return Ok(None);
    };
    if loaded.root.live_pages > 0 && loaded.root.files.is_empty() {
        return Ok(None);
    }
    Ok(Some(loaded))
}

/// The home-wide toolchain-only answer root: `tool-base.<PINNED>.<TRIPLE>.<tools>`.
/// Holds only closed toolchain crate rows (`CrateScope` and `ImplBucket`) with empty checkout
/// leaves so any fresh project or git worktree on the same `(toolchain, target, tools)` starts
/// with `core`/`alloc`/`std`/`bstd` already standing via [`StoredBase::OtherCheckout`].
pub(crate) fn toolchain_base_root<'f>(
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    frame: &'f Frame,
) -> blaze_own::String<&'f Frame> {
    let tc_hex = toolchain.hex();
    let tools_id = tools_digest(tools);
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("tool-base.");
    name.push_str(&tc_hex.as_str()[..16]);
    name.push('.');
    name.push_str(crate::graph::TRIPLE);
    name.push('.');
    for byte in &tools_id.0[..8] {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// Fallback lookup for a fresh checkout that has neither its own answer nor a matching workspace
/// base: loads the pre-warmed toolchain-only `Answer` pointer under
/// `tool-base.<PINNED>.<TRIPLE>.<tools>`.
#[cfg(test)]
pub(crate) fn load_toolchain_base<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    frame: &Frame,
) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let name = toolchain_base_root(toolchain, tools, frame);
    load_pointer(cx, store, name.as_str())
}

/// Copies a single engine [`blaze_engine::Row`] into `alloc` without cloning via `Clone`.
fn copy_row<'a>(row: &blaze_engine::Row<'_>, alloc: SharedAlloc<'a>) -> blaze_engine::Row<'a> {
    blaze_engine::Row {
        key: row.key,
        phase: row.phase,
        file: row.file,
        source: row.source,
        out: row.out,
        serial: row.serial,
        defines: blaze_engine::SVec::from_slice_in(&row.defines, alloc),
        reads: blaze_engine::SVec::from_slice_in(&row.reads, alloc),
        by_cell: blaze_engine::SVec::from_slice_in(&row.by_cell, alloc),
        findings: row.findings,
        inline: row.inline,
        failed: row.failed,
    }
}

/// Writes or merges the toolchain-only fallback base (`tool-base.<PINNED>.<TRIPLE>.<tools>`)
/// whenever `kept` contains non-failed toolchain rows (`store.is_toolchain`). Unchanged bases
/// perform no writes; new toolchain sets (e.g. `bstd` added after `std`) are merged cleanly.
#[expect(clippy::too_many_arguments, reason = "toolchain base save edge: each argument one handle")]
fn keep_toolchain_base<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    lent: &'m blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    toolchain: blaze_toolchain::ToolchainId,
    tools: &Tools,
    kept: &Kept<'_>,
    workers: core::num::NonZeroU32,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<(), CliError> {
    let row_of = |name: &Name| -> Option<&blaze_engine::Row<'_>> {
        if let Some(work) = kept.answer.work()
            && let Some(slot) = work.rows.get(name)
        {
            return slot.as_ref();
        }
        kept.answer.rows.get(name)
    };
    let mut fresh_names: blaze_own::Vec<Name, &Frame> = blaze_own::Vec::new_in(frame);
    if let Some(work) = kept.answer.work() {
        for (&name, slot) in work.rows.iter() {
            if let Some(row) = slot
                && !row.failed
                && store.is_toolchain(name)
                && kept.records.contains_key(&name)
            {
                fresh_names.push(name);
            }
        }
    } else {
        for (&name, row) in kept.answer.rows.iter() {
            if !row.failed && store.is_toolchain(name) && kept.records.contains_key(&name) {
                fresh_names.push(name);
            }
        }
    }
    fresh_names.sort_unstable();
    fresh_names.dedup();
    if fresh_names.is_empty() {
        return Ok(());
    }
    let tc_root = toolchain_base_root(toolchain, tools, frame);
    let existing_ptr = match load_pointer(cx, store, tc_root.as_str())? {
        Some(Ok(ptr)) => Some((ptr.root, ptr.headers)),
        _ => None,
    };
    // Fast unchanged check: inspect ONLY the pages holding `fresh_names` in place via `Stored`
    // without decoding the entire `tool-base` answer. Only fall through to `pages::decode` +
    // merge when at least one toolchain row is new or changed, reach roots are missing, or a
    // pre-fix `tool-base` lacks `root.files` ownership for its rows.
    if let Some((ptr_root, ref ptr_headers)) = existing_ptr
        && let Ok(root) = open_root(cx, store, ptr_root, alloc)?
        && verify_reach_roots(lent, tc_root.as_str(), &root, false, alloc, frame)?.is_ok()
        && !root.files.is_empty()
    {
        let snapshot = store.snapshot_in(alloc)?;
        let existing = Stored::new(&root, &snapshot, ptr_headers, pages::Check::Store, alloc);
        let unchanged = fresh_names.iter().all(|name| {
            let Some(new_row) = row_of(name) else { return false };
            if !new_row.inline && !root.files.contains_key(&new_row.file) {
                return false;
            }
            let Ok(Some(page)) = existing.page(*name) else { return false };
            let Some(old_row) = page.row(*name) else { return false };
            !old_row.facts.failed
                && old_row.facts.key == new_row.key
                && old_row.facts.out == new_row.out
                && page.record(*name) == kept.records.get(name).copied()
                && new_row.defines.iter().all(|&(cell, digest)| {
                    page.cell(&blaze_engine::CellId { of: *name, cell })
                        .is_some_and(|(stored_digest, _)| stored_digest == digest)
                })
        });
        if unchanged {
            return Ok(());
        }
    }
    let prev = match existing_ptr {
        Some((ptr_root, _)) => {
            let mut store_err: Option<CliError> = None;
            let decoded = pages::decode(
                ptr_root,
                |k| {
                    if store_err.is_some() {
                        return None;
                    }
                    match store.get(cx, k) {
                        Ok(Some(held)) => Some(Ok(Sealed::by_store(k, held.bytes, held.proof))),
                        Ok(None) => None,
                        Err(error) => {
                            store_err = Some(error);
                            None
                        }
                    }
                },
                alloc,
            );
            if let Some(error) = store_err {
                return Err(error);
            }
            decoded.ok()
        }
        None => None,
    };
    let mut tc_kept = match prev {
        Some(mut existing) => {
            existing.places.clear();
            existing
        }
        None => Kept::new_in(alloc),
    };
    if tc_kept.answer.serial.0 < kept.answer.serial.0 {
        tc_kept.answer.serial = kept.answer.serial;
    }
    for &name in fresh_names.iter() {
        if let Some(row) = row_of(&name) {
            tc_kept.answer.rows.insert(name, copy_row(row, alloc));
        }
        if let Some(&record) = kept.records.get(&name) {
            tc_kept.records.insert(name, record);
        }
        if let Some(&cause) = kept.causes.get(&name) {
            tc_kept.causes.insert(name, cause);
        }
    }
    // Rebuild `by_file`, `cells`, and `readers` from all standing toolchain rows so:
    // - every non-inline toolchain row is owned by its `row.file` in `Root.files` (and thus
    //   cleanly retired by `Edit.files` when another project's world does not plan that crate,
    //   never lingering as an unowned ghost row whose `READERS` edge wakes `MissingEntry`),
    // - downstream crates (`alloc` reading `core`, `std` reading `alloc`, `ImplBucket` reading
    //   `CrateScope`, etc.) see every toolchain cell's digest instead of `ABSENT`.
    tc_kept.answer.by_file = blaze_engine::Answer::new_in(alloc).by_file;
    tc_kept.answer.cells = blaze_engine::Answer::new_in(alloc).cells;
    for (&name, row) in tc_kept.answer.rows.iter() {
        if !row.inline {
            tc_kept.answer.by_file.entry(row.file).or_insert_with(|| blaze_own::Vec::new_in(alloc)).push(name);
        }
        for &(cell, digest) in row.defines.iter() {
            tc_kept.answer.cells.insert(blaze_engine::CellId { of: name, cell }, (digest, row.serial));
        }
    }
    for names in tc_kept.answer.by_file.values_mut() {
        names.sort_unstable();
        names.dedup();
    }
    tc_kept.answer.readers = blaze_engine::engine::Readers::from_rows(&tc_kept.answer.rows, alloc);
    let mut tc_page_err: Option<CliError> = None;
    let encoded = pages::encode_par(&tc_kept, None, alloc, |jobs| {
        match run_jobs_par_framed(cx, workers, PAGE_PAR_MIN, "toolchain base pages", jobs, alloc, None, |job, scratch| job.run_in(alloc, scratch)) {
            Ok(built) => built,
            Err(error) => {
                tc_page_err = Some(error);
                blaze_own::Vec::new_in(alloc)
            }
        }
    });
    if let Some(error) = tc_page_err {
        return Err(error);
    }
    for (key, bytes) in encoded.puts.iter() {
        store.put_local(cx, *key, bytes)?;
    }
    let mut tc_hdr_err: Option<CliError> = None;
    let (headers, writes) = Headers::after_with(None, &tc_kept, Writer::Phases, tc_kept.answer.serial, HeadersPlant::None, |_| Ok(None), alloc, |jobs| {
        match run_jobs_par(cx, workers, HEADER_PAR_MIN, "toolchain base headers", jobs, alloc, |job| job.run(alloc)) {
            Ok(built) => built,
            Err(error) => {
                tc_hdr_err = Some(error);
                blaze_own::Vec::new_in(alloc)
            }
        }
    }).map_err(page_error)?;
    if let Some(error) = tc_hdr_err {
        return Err(error);
    }
    for (key, bytes) in &writes {
        store.put_local(cx, *key, bytes)?;
    }
    let (empty_lines_key, empty_lines_bytes) = crate::engine_lines::Lines::new_in(alloc).record();
    store.put_local(cx, empty_lines_key, &empty_lines_bytes)?;
    let empty_rows = Rows::new_in(frame);
    let leaves = Leaves::bytes_in(&empty_rows, frame)?;
    let mut pointer: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    pointer.extend_from_slice(POINTER_MAGIC);
    pointer.extend_from_slice(&encoded.root.0.0);
    pointer.extend_from_slice(&empty_lines_key.0.0);
    let printed = Printed { names: true, bodies_whole: false, replayable: false };
    let facts = Facts { files: encoded.files, printed, findings: encoded.findings, owed: encoded.owed };
    pointer.extend_from_slice(&facts.files.to_le_bytes());
    pointer.push(facts.printed.byte());
    pointer.extend_from_slice(&facts.findings.to_le_bytes());
    pointer.extend_from_slice(&facts.owed.to_le_bytes());
    pointer.extend_from_slice(&tools_digest(tools).0);
    match &headers {
        Headers::Current(keys) => {
            pointer.push(HEADERS_CURRENT);
            pointer.extend_from_slice(&0u64.to_le_bytes());
            for key in &keys.keys {
                pointer.extend_from_slice(&key.0.0);
            }
        }
        Headers::Absent { since } => {
            pointer.push(HEADERS_ABSENT);
            pointer.extend_from_slice(&since.0.to_le_bytes());
            pointer.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        }
    }
    crate::journal_sweep::Stored::encode_units(core::iter::empty(), None, &mut pointer);
    let sealed = seal::seal(&pointer, &leaves, frame)?;
    let ptr_key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put_local(cx, ptr_key, &sealed)?;
    use crate::engine_answer_rows::StoreSink;
    let mut sink = StoreSink { cx, store, lent, toolchain };
    let mut records = blaze_own::Vec::new_in(alloc);
    let mut scopes = blaze_own::Vec::new_in(alloc);
    let mut cas_buf: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
    let mut indexed_beside: BesideCache<'_> = blaze_hash::Map::new(blaze_hash::MapOpts {
        hasher: blaze_hash::BuildKeyHasher::default(),
        alloc,
        capacity: Some(usize::try_from(encoded.task_puts).unwrap_or(0)),
    });
    for (page, page_rows) in encoded.page_records() {
        let beside = if page_rows.is_empty() {
            let cas_page = blaze_cas::Key(page.0);
            sink.lent
                .index_beside(cas_page, &[cas_page])
                .map_err(|error| reach_error(crate::engine_answer_rows::PublishError::Cas(error)))?
        } else {
            records.clear();
            scopes.clear();
            records.push(page);
            for &(name, record) in page_rows {
                records.push(record);
                if name.kind == blaze_drive::Kind::CrateScope {
                    scopes.push((name, record));
                }
            }
            sink.index_rows_into(page, &records, &scopes, &mut cas_buf).map_err(reach_error)?
        };
        indexed_beside.insert(page.0.0, beside);
    }
    let index_of = |p: Key| {
        indexed_beside
            .get(&p.0.0)
            .copied()
            .unwrap_or_else(|| blaze_cas::plain::beside_key(blaze_cas::Key(p.0)))
    };
    let mut page_idx = blaze_own::Vec::new_in(alloc);
    if !encoded.is_radix() {
        page_idx.extend(encoded.pages.iter().filter(|p| **p != Key::default()).map(|p| index_of(*p)));
    } else {
        for (shard_idx, &shard_key) in encoded.page_shards.iter().enumerate() {
            if shard_key == Key::default() {
                continue;
            }
            let shard = decode_lent_radix_shard(lent, shard_key, shard_idx, encoded.bits, alloc)?;
            page_idx.extend(shard.keys.iter().filter(|p| **p != Key::default()).map(|p| index_of(*p)));
        }
    }
    page_idx.sort_unstable();
    let mut direct = blaze_own::Vec::new_in(alloc);
    direct.push(blaze_cas::Key(encoded.root.0));
    direct.extend(encoded.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
    direct.push(blaze_cas::Key(empty_lines_key.0));
    if let Some(keys) = headers.current() {
        direct.extend(keys.keys.iter().filter(|k| **k != Key::default()).map(|k| blaze_cas::Key(k.0)));
    }
    let answer_index = lent.index(&direct).map_err(crate::store::PlaceError::from)?;
    let reach_name = bstd::mem::format_in!(frame, "{}.{REACH_ROOT}", tc_root.as_str());
    let answer_name = bstd::mem::format_in!(frame, "{}.{ANSWER_ROOT}", tc_root.as_str());
    let life = crate::store::roots::TOOLCHAIN_LIFE_NS;
    lent.root_through(reach_name.as_str(), &page_idx, life).map_err(crate::store::PlaceError::from)?;
    lent.root_through(answer_name.as_str(), &[answer_index], life).map_err(crate::store::PlaceError::from)?;
    lent.root_for(tc_root.as_str(), &[blaze_cas::Key(ptr_key.0)], life).map_err(crate::store::PlaceError::from)?;
    Ok(())
}

/// Reads the raw sorted key bytes of a named root record (`None` when absent/expired).
fn held_root_bytes<'l, D: Driver>(lent: &'l blaze_cas::plain::Lent<D, crate::store::StoreAlloc>, name: &str) -> Result<Option<&'l [u8]>, CliError> {
    let Some(key) = lent.root_named(name) else { return Ok(None) };
    let held = lent.get(&key).map_err(crate::store::PlaceError::from)?;
    let held = held.ok_or_else(|| page_error(PagesError::Reach { owner: Key(key.0), index: Key(key.0), what: "a named root's record is not held" }))?;
    if held.bytes.len() % 32 != 0 {
        return Err(page_error(PagesError::Reach { owner: Key(key.0), index: Key(key.0), what: "a root's record is not whole keys" }));
    }
    Ok(Some(held.bytes))
}

#[inline]
fn cas_key_at(bytes: &[u8], at: usize) -> blaze_cas::Key {
    blaze_cas::Key(ContentId(bytes[at * 32..at * 32 + 32].try_into().expect("a whole key")))
}

/// Cache of newly indexed pages' `beside_key` digests so reach-root construction avoids
/// recomputing BLAKE3 over pages just indexed in the same `save()` call.
type BesideCache<'a> = blaze_hash::Map<[u8; 32], blaze_cas::Key, blaze_hash::BuildKeyHasher, SharedAlloc<'a>>;

/// Decodes one radix leaf shard already stored (in arena or committed segment) in `lent` without
/// lending its bytes (never pins an uncommitted shared arena) or re-hashing store-proved bytes.
fn decode_lent_radix_shard<'a, D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    shard_key: Key,
    shard_idx: usize,
    bits: u8,
    alloc: SharedAlloc<'a>,
) -> Result<pages::RadixShard<'a>, CliError> {
    let shard_u32 = u32::try_from(shard_idx).unwrap_or(u32::MAX);
    crate::store::plain_place::lent_with_checked(lent, shard_key, |bytes, proof| {
        pages::decode_radix_shard(Sealed::by_store(shard_key, bytes, proof), shard_u32, bits, alloc)
    })?
    .ok_or_else(|| page_error(PagesError::Missing { page: pages::ROOT_PAGE, key: shard_key }))?
    .map_err(page_error)
}

#[cfg(test)]
fn test_pages_digest<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    encoded: &pages::Encoded<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<ContentId, CliError> {
    if !encoded.is_radix() {
        let mut page_keys: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(encoded.pages.len() * 32, frame);
        for page in encoded.pages.iter() {
            page_keys.extend_from_slice(&page.0.0);
        }
        return Ok(ContentId::of(PAGES_DOMAIN, &page_keys));
    }
    let mut h = blaze_ids::Hasher::new(PAGES_DOMAIN);
    let zeros = [0u8; 32];
    for (shard_idx, &shard_key) in encoded.page_shards.iter().enumerate() {
        if shard_key == Key::default() {
            for _ in 0..pages::RADIX_LEAF_PAGES {
                h.update(&zeros);
            }
        } else {
            let shard = decode_lent_radix_shard(lent, shard_key, shard_idx, encoded.bits, alloc)?;
            for key in shard.keys.iter() {
                h.update(&key.0.0);
            }
        }
    }
    Ok(h.finish())
}

/// Proves that the previous commit's `{name}.answer` root names `last_root` and its paging matches
/// `now_len`, returning `Ok(last_root)` or the exact [`WhyWhole`] reason to build the reach roots
/// whole.
fn prove_last_reach_root<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    answer_name: &str,
    last: Option<(usize, Key)>,
    now_len: usize,
    rekey: Rekey,
) -> Result<Result<Key, WhyWhole>, CliError> {
    #[cfg(test)]
    if rekey == Rekey::All {
        return Ok(Err(WhyWhole::Planted));
    }
    #[cfg(not(test))]
    let Rekey::Changed = rekey;
    let Some((old_len, last_root)) = last else { return Ok(Err(WhyWhole::NoLast)) };
    if old_len != now_len {
        return Ok(Err(WhyWhole::Paging));
    }
    let Some(answer) = held_root_bytes(lent, answer_name)? else { return Ok(Err(WhyWhole::NoRoot)) };
    if answer.len() != 32 {
        return Ok(Err(WhyWhole::NotLast));
    }
    let direct = lent.get(&cas_key_at(answer, 0)).map_err(crate::store::PlaceError::from)?;
    let Some(direct) = direct.filter(|got| blaze_keyed::kind::ReservedTag::Index.is(got.tag) && got.bytes.len() % 32 == 0) else {
        return Ok(Err(WhyWhole::NotLast));
    };
    let wanted = blaze_cas::Key(last_root.0);
    let (mut low, mut high) = (0usize, direct.bytes.len() / 32);
    let mut names_last = false;
    while low < high {
        let mid = low + (high - low) / 2;
        match cas_key_at(direct.bytes, mid).cmp(&wanted) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => {
                names_last = true;
                break;
            }
        }
    }
    if !names_last {
        return Ok(Err(WhyWhole::NotLast));
    }
    Ok(Ok(last_root))
}

/// Proves that the previous commit's `{name}.answer` root names `last_root` and its paging matches
/// `now_len`, returning either `Ok((old_pages, last_root))` or the exact [`WhyWhole`] reason to
/// build the reach roots whole.
fn prove_last_reach<'a, D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    answer_name: &str,
    last: Option<(&'a [Key], Key)>,
    now_len: usize,
    rekey: Rekey,
) -> Result<Result<(&'a [Key], Key), WhyWhole>, CliError> {
    let info = last.map(|(old, key)| (old.len(), key));
    match prove_last_reach_root(lent, answer_name, info, now_len, rekey)? {
        Ok(last_root) => {
            let Some((old, _)) = last else { return Ok(Err(WhyWhole::NoLast)) };
            Ok(Ok((old, last_root)))
        }
        Err(why) => Ok(Err(why)),
    }
}

/// Merges the changed pages between `old` and `now` into `carried` (the previous sorted root
/// bytes), returning the updated sorted page-index list and how many pages moved.
fn merge_carried_indexes<'h>(
    carried: &[u8],
    old: &[Key],
    now: &[Key],
    last_root: Key,
    known_beside: Option<&BesideCache<'_>>,
    alloc: SharedAlloc<'h>,
) -> Result<(blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>>, u32), CliError> {
    let index_of = |page: Key| {
        known_beside
            .and_then(|map| map.get(&page.0.0).copied())
            .unwrap_or_else(|| blaze_cas::plain::beside_key(blaze_cas::Key(page.0)))
    };
    let mut out: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::new_in(alloc);
    let mut into: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::new_in(alloc);
    let mut pages = 0u32;
    for (was, is) in old.iter().zip(now.iter()).filter(|(was, is)| was != is) {
        pages = pages.saturating_add(1);
        if *was != Key::default() {
            out.push(index_of(*was));
        }
        if *is != Key::default() {
            into.push(index_of(*is));
        }
    }
    out.sort_unstable();
    into.sort_unstable();
    let count = carried.len() / 32;
    let mut indexes: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> = blaze_own::Vec::with_capacity_in((count + into.len()).saturating_sub(out.len()), alloc);
    let (mut gone, mut added) = (0usize, 0usize);
    for at in 0..count {
        let key = cas_key_at(carried, at);
        while added < into.len() && into[added] < key {
            indexes.push(into[added]);
            added += 1;
        }
        if gone < out.len() && out[gone] == key {
            gone += 1;
            continue;
        }
        if gone < out.len() && out[gone] < key {
            return Err(page_error(PagesError::Reach { owner: Key(last_root.0), index: Key(out[gone].0), what: "the last answer's reach root lacks a changed page's old index" }));
        }
        indexes.push(key);
    }
    if gone < out.len() {
        return Err(page_error(PagesError::Reach { owner: Key(last_root.0), index: Key(out[gone].0), what: "the last answer's reach root lacks a changed page's old index" }));
    }
    indexes.extend_from_slice(&into[added..]);
    Ok((indexes, pages))
}

/// The reach root's page indexes for the pages `now` (one a page with anything on it, its
/// blaze-cas `beside_key`), sorted, and what was keyed for them. Used for answers with at most
/// [`REACH_SHARD_PAGES`] pages (single `{name}.reach` root); larger answers use
/// [`publish_reach_shards`].
fn page_indexes<'h, D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    names: (&str, &str),
    last: Option<(&[Key], Key)>,
    now: &[Key],
    rekey: Rekey,
    known_beside: Option<&BesideCache<'_>>,
    alloc: SharedAlloc<'h>,
) -> Result<(blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>>, Rekeyed), CliError> {
    let index_of = |page: Key| {
        known_beside
            .and_then(|map| map.get(&page.0.0).copied())
            .unwrap_or_else(|| blaze_cas::plain::beside_key(blaze_cas::Key(page.0)))
    };
    let whole = |why: WhyWhole| {
        let mut indexes = blaze_own::Vec::new_in(alloc);
        indexes.extend(now.iter().filter(|page| **page != Key::default()).map(|page| index_of(*page)));
        indexes.sort_unstable();
        let pages = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
        Ok((indexes, Rekeyed { pages, whole: Some(why) }))
    };
    let (old, last_root) = match prove_last_reach(lent, names.1, last, now.len(), rekey)? {
        Ok(proven) => proven,
        Err(why) => return whole(why),
    };
    let Some(carried) = held_root_bytes(lent, names.0)? else { return whole(WhyWhole::NoRoot) };
    let (indexes, pages) = merge_carried_indexes(carried, old, now, last_root, known_beside, alloc)?;
    Ok((indexes, Rekeyed { pages, whole: None }))
}

/// Whether `keys` (sorted) has the exact same 32-byte key sequence as `bytes`.
fn keys_equal_bytes(keys: &[blaze_cas::Key], bytes: &[u8]) -> bool {
    keys.len().saturating_mul(32) == bytes.len()
        && keys.iter().zip(bytes.chunks_exact(32)).all(|(key, chunk)| key.0.0 == chunk)
}

/// Merges sorted 32-byte keys from `carried` with sorted deduplicated `added` into one sorted
/// deduplicated vector.
fn merge_sorted_keys<'h>(carried: &[u8], added: &[blaze_cas::Key], alloc: SharedAlloc<'h>) -> blaze_own::Vec<blaze_cas::Key, SharedAlloc<'h>> {
    let count = carried.len() / 32;
    let mut out = blaze_own::Vec::with_capacity_in(count + added.len(), alloc);
    let mut i = 0usize;
    let mut added_iter = added.iter().copied().peekable();
    while i < count {
        let a = cas_key_at(carried, i);
        let Some(&b) = added_iter.peek() else {
            break;
        };
        match a.cmp(&b) {
            core::cmp::Ordering::Less => {
                out.push(a);
                i += 1;
            }
            core::cmp::Ordering::Greater => {
                out.push(b);
                let _ = added_iter.next();
            }
            core::cmp::Ordering::Equal => {
                out.push(a);
                i += 1;
                let _ = added_iter.next();
            }
        }
    }
    while i < count {
        out.push(cas_key_at(carried, i));
        i += 1;
    }
    out.extend(added_iter);
    out
}

/// Publishes an indexed reach root `root_name` over `indexes`. When `lent.root_through` reports
/// `StoreError::RootNamesUnheld` for a key NOT indexed by this command (`known_beside`), it came
/// from an unchanged page carried from `last_owner` whose `beside_key` was evicted or expired:
/// surface a typed [`PagesError::Reach`] instead of misreporting a writer bug.
fn root_through_reach<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    root_name: &str,
    indexes: &[blaze_cas::Key],
    known_beside: Option<&BesideCache<'_>>,
    last_owner: Option<Key>,
) -> Result<(), CliError> {
    match lent.root_through(root_name, indexes, blaze_cas::plain::ROOT_LIFE_NS) {
        Ok(_) => Ok(()),
        Err(blaze_cas::plain::LentError::Store {
            why: blaze_keyed::store::StoreError::RootNamesUnheld { key, .. },
            ..
        }) if let Some(owner) = last_owner
            && !known_beside.is_some_and(|map| map.values().any(|k| k.0 == key)) =>
        {
            Err(page_error(PagesError::Reach {
                owner,
                index: Key(key),
                what: "a carried page's reach index is not in the store (its prior reach root was evicted or expired)",
            }))
        }
        Err(error) => Err(crate::store::PlaceError::from(error).into()),
    }
}

/// Source of page keys for [`publish_reach_shards_from`]: either a flat slice (unit tests) or a
/// 2-level radix [`pages::Encoded`] whose leaf shards live in `lent` and whose incremental deltas
/// avoid scanning all `1 << bits` slots.
enum ReachSource<'a, 'e> {
    #[cfg(test)]
    Flat { last: Option<(&'a [Key], Key)>, now: &'a [Key] },
    Radix { last: Option<(usize, Key)>, encoded: &'a pages::Encoded<'e> },
}

/// Sharded reach roots (`{name}.reach.00`..`{name}.reach.7f`, [`REACH_SHARD_PAGES`] page slots per
/// shard) plus a bounded delta root (`{name}.reach.delta`, capped at [`REACH_DELTA_CAP`]) for
/// answers spanning more than [`REACH_SHARD_PAGES`] pages.
///
/// On incremental edits whose accumulated delta fits within [`REACH_DELTA_CAP`], every base shard
/// renews in O(1) via [`blaze_cas::plain::Lent::renew_root`] (0 CAS probes, 0 writes) and only
/// `{name}.reach.delta` is published (probing only the delta keys, not `touched_shards * 512`).
/// When the delta overflows [`REACH_DELTA_CAP`] (or a root is absent/expired), changed base shards
/// fold in place (unchanged shards still renew with 0 probes) and `{name}.reach.delta` resets to
/// empty, reclaiming superseded pages.
#[expect(clippy::too_many_arguments, reason = "reach shard publisher: each argument one handle")]
fn publish_reach_shards_from<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    answer_name: &str,
    source: ReachSource<'_, '_>,
    rekey: Rekey,
    known_beside: Option<&BesideCache<'_>>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<(u32, Rekeyed), CliError> {
    let index_of = |page: Key| {
        known_beside
            .and_then(|map| map.get(&page.0.0).copied())
            .unwrap_or_else(|| blaze_cas::plain::beside_key(blaze_cas::Key(page.0)))
    };
    let (last_info, now_len, total_live) = match source {
        #[cfg(test)]
        ReachSource::Flat { last, now } => (
            last.map(|(old, key)| (old.len(), key)),
            now.len(),
            u32::try_from(now.iter().filter(|page| **page != Key::default()).count()).unwrap_or(u32::MAX),
        ),
        ReachSource::Radix { last, encoded } => (last, encoded.page_count(), encoded.live_pages),
    };
    let proof = prove_last_reach_root(lent, answer_name, last_info, now_len, rekey)?;
    let num_shards = now_len.div_ceil(REACH_SHARD_PAGES);
    let mut shard_name = blaze_own::String::new_in(frame);
    let delta_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}");
    let mut missing_root = false;

    // Incremental delta fast path: renew all base shards in O(1) and publish only changed page
    // indexes in `{name}.reach.delta` while no page was cleared to empty and the accumulated delta
    // fits within REACH_DELTA_CAP.
    if proof.is_ok() {
        let mut added: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut replaced: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::new_in(alloc);
        let mut moved = 0u32;
        let mut cleared = false;
        let has_deltas = match source {
            #[cfg(test)]
            ReachSource::Flat { last: Some((old, _)), now } => {
                for (was, is) in old.iter().zip(now.iter()).filter(|(was, is)| was != is) {
                    moved = moved.saturating_add(1);
                    if *was != Key::default() {
                        replaced.push(index_of(*was));
                    }
                    if *is != Key::default() {
                        added.push(index_of(*is));
                    } else {
                        cleared = true;
                    }
                }
                true
            }
            #[cfg(test)]
            ReachSource::Flat { last: None, .. } => false,
            ReachSource::Radix { encoded, .. } => match &encoded.page_deltas {
                Some(deltas) => {
                    for delta in deltas.iter() {
                        moved = moved.saturating_add(1);
                        if delta.old != Key::default() {
                            replaced.push(index_of(delta.old));
                        }
                        if delta.new != Key::default() {
                            added.push(index_of(delta.new));
                        } else {
                            cleared = true;
                        }
                    }
                    true
                }
                None => false,
            },
        };
        if has_deltas && !cleared {
            if let Some(carried_delta) = held_root_bytes(lent, delta_name.as_str())? {
                added.sort_unstable();
                added.dedup();
                let next_delta = merge_sorted_keys(carried_delta, &added, alloc);
                if next_delta.len() <= REACH_DELTA_CAP {
                    let mut all_shards_live = true;
                    for shard in 0..num_shards {
                        shard_name.clear();
                        let _ = write!(shard_name, "{name}.{REACH_ROOT}.{shard:02x}");
                        if !lent.renew_root(shard_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live() {
                            all_shards_live = false;
                            missing_root = true;
                            break;
                        }
                    }
                    if all_shards_live {
                        let delta_unchanged = keys_equal_bytes(&next_delta, carried_delta);
                        if delta_unchanged && lent.renew_root(delta_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live() {
                            return Ok((total_live, Rekeyed { pages: moved, whole: None }));
                        }
                        if !replaced.is_empty() {
                            lent.hint_replaced_indexes(&replaced);
                        }
                        lent.root_through(delta_name.as_str(), &next_delta, blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
                        return Ok((total_live, Rekeyed { pages: moved, whole: None }));
                    }
                }
            } else {
                missing_root = true;
            }
        }
    }

    // Fold / cold path: write each base shard whose sorted indexes changed (renewing unchanged
    // shards in O(1) with 0 probes when `proof` holds) and reset `{name}.reach.delta` to empty.
    let mut scratch: blaze_own::Vec<blaze_cas::Key, SharedAlloc<'_>> = blaze_own::Vec::with_capacity_in(REACH_SHARD_PAGES, alloc);
    let mut rekeyed_pages = 0u32;
    let leaves_per_reach = REACH_SHARD_PAGES / pages::RADIX_LEAF_PAGES;
    for shard in 0..num_shards {
        shard_name.clear();
        let _ = write!(shard_name, "{name}.{REACH_ROOT}.{shard:02x}");
        scratch.clear();
        match source {
            #[cfg(test)]
            ReachSource::Flat { now, .. } => {
                let from = shard * REACH_SHARD_PAGES;
                let to = (from + REACH_SHARD_PAGES).min(now.len());
                if let Some(slice) = now.get(from..to) {
                    scratch.extend(slice.iter().filter(|page| **page != Key::default()).map(|page| index_of(*page)));
                }
            }
            ReachSource::Radix { encoded, .. } => {
                let from_leaf = shard * leaves_per_reach;
                let to_leaf = (from_leaf + leaves_per_reach).min(encoded.page_shards.len());
                for leaf_idx in from_leaf..to_leaf {
                    let shard_key = encoded.page_shards.get(leaf_idx).copied().unwrap_or_default();
                    if shard_key == Key::default() {
                        continue;
                    }
                    let leaf = decode_lent_radix_shard(lent, shard_key, leaf_idx, encoded.bits, alloc)?;
                    scratch.extend(leaf.keys.iter().filter(|page| **page != Key::default()).map(|page| index_of(*page)));
                }
            }
        }
        scratch.sort_unstable();
        if proof.is_ok() {
            if let Some(existing) = held_root_bytes(lent, shard_name.as_str())? {
                if keys_equal_bytes(&scratch, existing)
                    && lent.renew_root(shard_name.as_str(), blaze_cas::plain::ROOT_LIFE_NS).is_live()
                {
                    continue;
                }
            } else {
                missing_root = true;
            }
        }
        rekeyed_pages = rekeyed_pages.saturating_add(u32::try_from(scratch.len()).unwrap_or(u32::MAX));
        root_through_reach(lent, shard_name.as_str(), &scratch, known_beside, last_info.map(|(_, key)| key))?;
    }
    lent.root_through(delta_name.as_str(), &[], blaze_cas::plain::ROOT_LIFE_NS).map_err(crate::store::PlaceError::from)?;
    // Unroot any higher-numbered stale shards if the answer shrank to fewer shards.
    unroot_reach_shards(lent, name, num_shards, false, frame);
    let whole = match proof {
        Err(why) => Some(why),
        Ok(_) if missing_root => Some(WhyWhole::NoRoot),
        Ok(_) => None,
    };
    Ok((total_live, Rekeyed { pages: rekeyed_pages, whole }))
}

/// Unroots live reach shard roots `from_shard..REACH_SHARDS_MOST` (and `{name}.reach.delta` when
/// `include_delta` is true) so shrinking an answer never leaves stale shard roots behind.
fn unroot_reach_shards<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    from_shard: usize,
    include_delta: bool,
    frame: &Frame,
) {
    let mut shard_name = blaze_own::String::new_in(frame);
    for shard in from_shard..REACH_SHARDS_MOST {
        shard_name.clear();
        let _ = write!(shard_name, "{name}.{REACH_ROOT}.{shard:02x}");
        if lent.has_root(shard_name.as_str()) {
            lent.unroot(shard_name.as_str());
        }
    }
    if include_delta {
        let delta_name = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}");
        if lent.has_root(delta_name.as_str()) {
            lent.unroot(delta_name.as_str());
        }
    }
}

#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "test helper for flat reach shard publishing")]
fn publish_reach_shards<D: Driver>(
    lent: &blaze_cas::plain::Lent<D, crate::store::StoreAlloc>,
    name: &str,
    answer_name: &str,
    last: Option<(&[Key], Key)>,
    now: &[Key],
    rekey: Rekey,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<(u32, Rekeyed), CliError> {
    publish_reach_shards_from(lent, name, answer_name, ReachSource::Flat { last, now }, rekey, None, alloc, frame)
}

/// Appends any live sharded reach root names (`{name}.reach.00`..`{name}.reach.7f` and
/// `{name}.reach.delta`) to `out` so a certified no-op check renews every live reach root in O(1)
/// alongside the single `{name}.reach` root. Small answers (`<= 512` pages) check shard `00` once
/// (a manifest miss, 0 reads) and add nothing.
pub(crate) fn push_reach_shards<'f, D: Driver, S: Sharing>(
    store: &mut CommandStore<'_, '_, D, S>,
    name: &str,
    frame: &'f Frame,
    out: &mut blaze_own::Vec<blaze_own::String<&'f Frame>, &'f Frame>,
) -> Result<(), CliError> {
    let first = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.00");
    if store.named(first.as_str())?.is_none() {
        return Ok(());
    }
    out.push(first);
    for shard in 1..REACH_SHARDS_MOST {
        out.push(bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{shard:02x}"));
    }
    out.push(bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}"));
    Ok(())
}

/// [`push_reach_shards`] directly through `PlainPlace` for the resident daemon's certified fast path.
pub(crate) fn push_reach_shards_plain<'f, D: Driver>(
    place: &crate::store::PlainPlace<'_, D>,
    name: &str,
    frame: &'f Frame,
    out: &mut blaze_own::Vec<blaze_own::String<&'f Frame>, &'f Frame>,
) {
    let first = bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.00");
    if place.root(first.as_str()).ok().flatten().is_none() {
        return;
    }
    out.push(first);
    for shard in 1..REACH_SHARDS_MOST {
        out.push(bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{shard:02x}"));
    }
    out.push(bstd::mem::format_in!(frame, "{name}.{REACH_ROOT}.{REACH_DELTA_SUFFIX}"));
}

fn reach_error(error: crate::engine_answer_rows::PublishError) -> CliError {
    use crate::engine_answer_rows::PublishError;
    match error {
        PublishError::Command(error) => error,
        PublishError::Cas(error) => crate::store::PlaceError::from(error).into(),
        PublishError::ExpansionIndex { record, index, .. } => page_error(PagesError::Reach { owner: record, index, what: "invalid expansion index" }),
        PublishError::RowsIndex { segment, index } => page_error(PagesError::Reach { owner: segment, index, what: "missing payload index" }),
        PublishError::RowsReach { segment, .. } => page_error(PagesError::Reach {
            owner: segment, index: Key(blaze_cas::plain::beside_key(blaze_cas::Key(segment.0)).0), what: "page payload reach exceeds the CAS index limit",
        }),
    }
}

#[cfg(test)]
#[path = "tests/engine_answer_page_edit.rs"]
mod page_edit;

#[cfg(test)]
#[path = "tests/engine_bases_tests.rs"]
mod bases_tests;

#[cfg(test)]
mod reach_shard_tests {
    use super::*;
    use blaze_cas::plain::Settings;
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    /// Answers spanning `> REACH_SHARD_PAGES` pages shard reach roots into `{name}.reach.00`..
    /// so an incremental edit touching 1 page renews unchanged shards in O(1) (0 CAS probes) and
    /// probes at most `REACH_SHARD_PAGES` keys in the changed shard alone.
    #[test]
    fn sharded_reach_roots_renew_unchanged_shards_in_o1_and_keep_every_page_reachable() {
        let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let home = crate::store::Home::of(&cx, &io.need_fs().expect("filesystem"), Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("stderr");
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 41 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");

        // 4 shards of REACH_SHARD_PAGES (2,048 pages total).
        let total_pages = REACH_SHARD_PAGES * 4;
        let mut pages = blaze_own::Vec::with_capacity_in(total_pages, alloc);
        for i in 0..total_pages {
            let bytes = (i as u32).to_le_bytes();
            let page_key = Key(ContentId::of("reach shard test page", &bytes));
            store.put(&cx, page_key, &bytes).expect("put page");
            let cas = blaze_cas::Key(page_key.0);
            lent.index_beside(cas, &[cas]).expect("index beside");
            pages.push(page_key);
        }
        let first_root = Key(ContentId::of("reach shard test root", b"v1"));
        store.put(&cx, first_root, b"v1").expect("put first root");
        let answer_index = lent.index(&[blaze_cas::Key(first_root.0)]).expect("answer index");
        let (live, rekeyed) = publish_reach_shards(&lent, "big", "big.answer", None, &pages, Rekey::Changed, alloc, frame).expect("cold publish shards");
        lent.root_through("big.answer", &[answer_index], blaze_cas::plain::ROOT_LIFE_NS).expect("pin big.answer");
        assert_eq!(live as usize, total_pages);
        assert_eq!(rekeyed.whole, Some(WhyWhole::NoLast));
        lent.commit().expect("commit cold shards");

        // `push_reach_shards` detects sharded answers (shards + delta) and skips unsharded names.
        let mut shards_found = blaze_own::Vec::new_in(frame);
        push_reach_shards(&mut store, "small-absent", frame, &mut shards_found).expect("unsharded check");
        assert!(shards_found.is_empty(), "absent/unsharded root adds 0 shard names");
        push_reach_shards(&mut store, "big", frame, &mut shards_found).expect("sharded check");
        assert_eq!(shards_found.len(), REACH_SHARDS_MOST + 1);
        assert_eq!(shards_found[0].as_str(), "big.reach.00");
        assert_eq!(shards_found[REACH_SHARDS_MOST].as_str(), "big.reach.delta");

        // Mutate ONLY 1 page in shard 0: all 4 base shards renew in O(1) (0 probes), and only
        // `{name}.reach.delta` is published (1 probe, not 512!).
        let mut next_pages = blaze_own::Vec::with_capacity_in(total_pages, alloc);
        next_pages.extend_from_slice(&pages);
        let replaced_old = next_pages[7];
        let replacement = Key(ContentId::of("reach shard test page", b"edited-7"));
        store.put(&cx, replacement, b"edited-7").expect("put replacement page");
        let rep_cas = blaze_cas::Key(replacement.0);
        lent.index_beside(rep_cas, &[rep_cas]).expect("index beside replacement");
        next_pages[7] = replacement;

        let probes_before = lent.put_tally().disk.root_probes;
        let (live2, rekeyed2) = publish_reach_shards(&lent, "big", "big.answer", Some((&pages, first_root)), &next_pages, Rekey::Changed, alloc, frame).expect("edit publish shards");
        let probes_delta = lent.put_tally().disk.root_probes - probes_before;
        assert_eq!(live2 as usize, total_pages);
        assert_eq!(rekeyed2, Rekeyed { pages: 1, whole: None }, "only the 1 edited page re-keyed");
        assert_eq!(probes_delta, 1, "1-page edit probes only the 1 delta key, not all 512 keys of shard 0 (got {probes_delta})");
        lent.commit().expect("commit edited shards");

        // Verify replacement is reachable via `big.reach.delta`, and all 4 base shards remain live.
        let old_beside = blaze_cas::plain::beside_key(blaze_cas::Key(replaced_old.0)).0.0;
        let new_beside = blaze_cas::plain::beside_key(rep_cas).0.0;
        let delta = store.named("big.reach.delta").expect("read delta").expect("delta live");
        assert_eq!(delta.len(), 32, "delta holds the 1 replacement page index");
        assert_eq!(&delta[..32], &new_beside, "delta holds replacement page index");
        for shard in 0..4usize {
            let name = bstd::mem::format_in!(frame, "big.reach.{shard:02x}");
            let bytes = store.named(name.as_str()).expect("read shard").expect("shard live");
            assert_eq!(bytes.len(), REACH_SHARD_PAGES * 32, "base shard {shard} preserved all keys");
        }
        let shard0 = store.named("big.reach.00").expect("read shard 0").expect("shard 0 live");
        assert!(shard0.chunks_exact(32).any(|k| k == &old_beside), "base shard 0 stays untouched until fold");
    }

    /// Multi-page edit (82 changed pages spread across all 16 shards) probes only the 82 delta
    /// keys (not `16 * 512 = 8,192` or `63,531`!), keeps 100% of live pages reachable, and folds
    /// cleanly once accumulated edits exceed `REACH_DELTA_CAP`, reclaiming superseded pages.
    #[test]
    fn multi_page_edit_across_shards_probes_only_changed_pages_and_folds_cleanly() {
        let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let home = crate::store::Home::of(&cx, &io.need_fs().expect("filesystem"), Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("stderr");
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 43 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");

        let shard_count = 16usize;
        let total_pages = REACH_SHARD_PAGES * shard_count;
        let mut pages = blaze_own::Vec::with_capacity_in(total_pages, alloc);
        for i in 0..total_pages {
            let bytes = (i as u32).to_le_bytes();
            let page_key = Key(ContentId::of("reach shard test page", &bytes));
            store.put(&cx, page_key, &bytes).expect("put page");
            let cas = blaze_cas::Key(page_key.0);
            lent.index_beside(cas, &[cas]).expect("index beside");
            pages.push(page_key);
        }
        let root_v1 = Key(ContentId::of("reach shard test root", b"multi-v1"));
        store.put(&cx, root_v1, b"multi-v1").expect("put root v1");
        let answer_idx1 = lent.index(&[blaze_cas::Key(root_v1.0)]).expect("answer idx 1");
        let (live1, rekeyed1) = publish_reach_shards(&lent, "multi", "multi.answer", None, &pages, Rekey::Changed, alloc, frame).expect("cold publish");
        lent.root_through("multi.answer", &[answer_idx1], blaze_cas::plain::ROOT_LIFE_NS).expect("pin answer 1");
        assert_eq!(live1 as usize, total_pages);
        assert_eq!(rekeyed1.whole, Some(WhyWhole::NoLast));
        lent.commit().expect("commit cold");

        // Edit 82 pages spread across ALL 16 shards (without delta, 16 * 512 = 8,192 probes).
        let mut pages_v2 = blaze_own::Vec::with_capacity_in(total_pages, alloc);
        pages_v2.extend_from_slice(&pages);
        let mut old_7_beside = [0u8; 32];
        for n in 0..82usize {
            let slot = (n % shard_count) * REACH_SHARD_PAGES + (n / shard_count) * 7;
            if n == 0 {
                old_7_beside = blaze_cas::plain::beside_key(blaze_cas::Key(pages_v2[slot].0)).0.0;
            }
            let payload = format!("v2-edit-{n}");
            let key = Key(ContentId::of("reach shard test page", payload.as_bytes()));
            store.put(&cx, key, payload.as_bytes()).expect("put v2 page");
            let cas = blaze_cas::Key(key.0);
            lent.index_beside(cas, &[cas]).expect("index beside v2");
            pages_v2[slot] = key;
        }
        let root_v2 = Key(ContentId::of("reach shard test root", b"multi-v2"));
        store.put(&cx, root_v2, b"multi-v2").expect("put root v2");
        let answer_idx2 = lent.index(&[blaze_cas::Key(root_v2.0)]).expect("answer idx 2");

        let probes_before = lent.put_tally().disk.root_probes;
        let (live2, rekeyed2) = publish_reach_shards(&lent, "multi", "multi.answer", Some((&pages, root_v1)), &pages_v2, Rekey::Changed, alloc, frame).expect("v2 publish");
        let probes_v2 = lent.put_tally().disk.root_probes - probes_before;
        lent.root_through("multi.answer", &[answer_idx2], blaze_cas::plain::ROOT_LIFE_NS).expect("pin answer 2");
        assert_eq!(live2 as usize, total_pages);
        assert_eq!(rekeyed2, Rekeyed { pages: 82, whole: None });
        assert_eq!(probes_v2, 82, "82 changed pages across 16 shards must probe only 82 delta keys (got {probes_v2})");
        lent.commit().expect("commit v2");

        // Verify 100% of live pages in `pages_v2` are reachable across shards U delta.
        let mut reachable = std::collections::BTreeSet::new();
        for shard in 0..shard_count {
            let name = bstd::mem::format_in!(frame, "multi.reach.{shard:02x}");
            let bytes = store.named(name.as_str()).expect("read shard").expect("shard live");
            for chunk in bytes.chunks_exact(32) {
                let mut k = [0u8; 32];
                k.copy_from_slice(chunk);
                reachable.insert(k);
            }
        }
        let delta_v2 = store.named("multi.reach.delta").expect("read delta").expect("delta live");
        assert_eq!(delta_v2.len(), 82 * 32, "delta holds all 82 new page indexes");
        for chunk in delta_v2.chunks_exact(32) {
            let mut k = [0u8; 32];
            k.copy_from_slice(chunk);
            reachable.insert(k);
        }
        for page in pages_v2.iter() {
            let beside = blaze_cas::plain::beside_key(blaze_cas::Key(page.0)).0.0;
            assert!(reachable.contains(&beside), "every live page must be reachable after delta edit");
        }

        // Now push delta past REACH_DELTA_CAP by editing 450 pages in shard 0 & shard 1 alone:
        // triggers a clean fold (only shards 0..16 that changed since cold rewrite; delta resets to 0).
        let mut pages_v3 = blaze_own::Vec::with_capacity_in(total_pages, alloc);
        pages_v3.extend_from_slice(&pages_v2);
        for n in 0..450usize {
            let slot = n;
            let payload = format!("v3-fold-{n}");
            let key = Key(ContentId::of("reach shard test page", payload.as_bytes()));
            store.put(&cx, key, payload.as_bytes()).expect("put v3 page");
            let cas = blaze_cas::Key(key.0);
            lent.index_beside(cas, &[cas]).expect("index beside v3");
            pages_v3[slot] = key;
        }
        let (live3, rekeyed3) = publish_reach_shards(&lent, "multi", "multi.answer", Some((&pages_v2, root_v2)), &pages_v3, Rekey::Changed, alloc, frame).expect("v3 fold publish");
        assert_eq!(live3 as usize, total_pages);
        assert_eq!(rekeyed3.whole, None, "bounded delta fold is incremental, not WhyWhole");
        lent.commit().expect("commit v3");

        let delta_v3 = store.named("multi.reach.delta").expect("read delta after fold").expect("delta root stays live");
        assert_eq!(delta_v3.len(), 0, "delta is reset to empty after folding");
        let shard0_v3 = store.named("multi.reach.00").expect("read shard 0").expect("shard 0 live");
        assert!(!shard0_v3.chunks_exact(32).any(|k| k == &old_7_beside), "superseded old page index was dropped on fold");
        for (shard, slice) in pages_v3.chunks(REACH_SHARD_PAGES).enumerate() {
            let name = bstd::mem::format_in!(frame, "multi.reach.{shard:02x}");
            let bytes = store.named(name.as_str()).expect("read shard").expect("shard live");
            assert_eq!(bytes.len(), REACH_SHARD_PAGES * 32, "folded shard {shard} has exact live page count");
            let mut set = std::collections::BTreeSet::new();
            for chunk in bytes.chunks_exact(32) {
                let mut k = [0u8; 32];
                k.copy_from_slice(chunk);
                set.insert(k);
            }
            for page in slice {
                let beside = blaze_cas::plain::beside_key(blaze_cas::Key(page.0)).0.0;
                assert!(set.contains(&beside), "folded shard {shard} contains every live page");
            }
        }
    }

    /// End-to-end 65,536-page (`bits = 16`) radix answer in `engine_answer`:
    /// - lazy `Stored` reads 0 leaf shards / 0 task pages at open, and only 1 leaf shard on lookup
    /// - 4-page incremental edit writes `<= 45 KiB` of root+leaf+dir metadata and probes only 4 delta keys
    /// - full `blaze gc` (`Liveness::Reached`) sweeps unpinned garbage while preserving 100% of
    ///   radix leaf shards, directory sub-records, top root, and task pages reachable via `{name}.answer`!
    #[test]
    fn radix_65536_page_answer_lazy_read_four_page_edit_and_gc_reachability() {
        use blaze_drive::{Kind, Tool};
        use blaze_engine::{Cause, Gen, Phase, Row, Stage};

        let io = Io::simulated(SimWorld::new(47, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let fs = io.need_fs().expect("fs");
        let host = io.need_host().expect("host");
        let clock = io.need_clock().expect("clock");
        let mut rng = io.need_rng().expect("rng");
        let out = io.need_out().expect("out");
        let err = io.need_err().expect("err");
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);

        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 47 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");
        let tools = Tools([Tool(ContentId([9; 32])); blaze_drive::key::KINDS]);
        let phase = Phase::new(Stage::Bodies, 0);

        let payload_v1 = blaze_drive::key::content_key(b"radix-v1-payload");
        store.put(&cx, payload_v1, b"radix-v1-payload").expect("put payload v1");
        let mut cold = Kept::new_in(alloc);
        cold.answer.serial = Gen(1);
        let mut task_names: std::vec::Vec<Name> = std::vec::Vec::new();
        for i in 0..200u32 {
            let file = Name::new(Kind::FileItems, ContentId::of("radix e2e file", &(i % 12).to_le_bytes()));
            let name = Name::new(Kind::BodyCheck, ContentId::of("radix e2e task", &i.to_le_bytes()));
            let source = ContentId::of("radix e2e src", &i.to_le_bytes());
            cold.answer.by_file.entry(file).or_insert_with(|| blaze_own::Vec::new_in(alloc)).push(name);
            cold.places.insert(file, pages::FilePlace { unit: file, edition: 3, flags: 1 });
            cold.answer.rows.insert(name, Row {
                key: tools.key(name, &source), phase, file, source, out: payload_v1.0, serial: Gen(1),
                defines: blaze_own::Vec::new_in(alloc), reads: blaze_own::Vec::new_in(alloc), by_cell: blaze_own::Vec::new_in(alloc),
                findings: 0, inline: false, failed: false,
            });
            cold.causes.insert(name, (Gen(1), Cause::New));
            cold.records.insert(name, payload_v1);
            task_names.push(name);
        }
        for names in cold.answer.by_file.values_mut() {
            names.sort_unstable();
        }

        // Encode at `bits = 16` (65,536 pages -> 256 leaf shards) and publish with reach shards + `big.answer` pinning `tree_keys`.
        let enc_v1 = pages::encode_at_bits(&cold, 16, alloc);
        assert!(enc_v1.is_radix());
        for (k, b) in enc_v1.puts.iter() {
            store.put(&cx, *k, b).expect("put v1");
        }
        for (page, rows) in enc_v1.page_records() {
            let mut recs = blaze_own::Vec::new_in(alloc);
            recs.push(blaze_cas::Key(page.0));
            for &(_, rec) in rows {
                recs.push(blaze_cas::Key(rec.0));
            }
            lent.index_beside(blaze_cas::Key(page.0), &recs).expect("index beside v1");
        }
        let (live1, rekey1) = publish_reach_shards_from(
            &lent, "big", "big.answer",
            ReachSource::Radix { last: None, encoded: &enc_v1 },
            Rekey::Changed, None, alloc, frame,
        ).expect("publish cold radix reach shards");
        assert_eq!(live1, enc_v1.live_pages);
        assert_eq!(rekey1.whole, Some(WhyWhole::NoLast));
        let mut direct1 = blaze_own::Vec::new_in(alloc);
        direct1.push(blaze_cas::Key(enc_v1.root.0));
        direct1.extend(enc_v1.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
        let idx1 = lent.index(&direct1).expect("direct1");
        lent.root_through("big.answer", &[idx1], blaze_cas::plain::ROOT_LIFE_NS).expect("pin big.answer v1");
        // Also put an unpinned garbage record so we prove GC actually sweeps.
        let garbage_key = Key(ContentId::of("radix e2e garbage", b"dead"));
        store.put(&cx, garbage_key, &[0xAB; 8192]).expect("put garbage");
        lent.release_lease();
        lent.commit().expect("commit v1");

        // Open root lazily: 0 task pages read, 0 radix leaf shards loaded at open.
        let dir_v1 = open_root(&cx, &mut store, enc_v1.root, alloc).expect("store").expect("valid radix root");
        assert!(dir_v1.is_radix());
        let snap_v1 = store.snapshot_in(alloc).expect("snapshot v1");
        let headers = Headers::Absent { since: Gen(1) };
        let stored_v1 = Stored::new(&dir_v1, &snap_v1, &headers, pages::Check::Store, alloc);
        assert_eq!(stored_v1.pages_read(), 0, "open_root + Stored::new reads 0 task pages");

        // Pick 3 tasks in file 0 across 3 distinct leaf shards (distinct from file 0's own page)
        // so editing those 3 tasks touches EXACTLY 4 task pages (3 task pages + 1 file page) across
        // 4 distinct leaf shards!
        let edited_file = Name::new(Kind::FileItems, ContentId::of("radix e2e file", &0u32.to_le_bytes()));
        let file_page = pages::content_page_of(edited_file, 16) as usize;
        let mut picked: std::vec::Vec<Name> = std::vec::Vec::new();
        let mut used_shards = std::collections::BTreeSet::new();
        used_shards.insert(Root::shard_of(file_page).0);
        for &n in cold.answer.by_file[&edited_file].iter() {
            let p = pages::content_page_of(n, 16) as usize;
            let (s, _) = Root::shard_of(p);
            if used_shards.insert(s) {
                picked.push(n);
                if picked.len() == 3 { break; }
            }
        }
        assert_eq!(picked.len(), 3);
        // Reading one task lazily resolves its leaf shard and task page.
        let first_page = stored_v1.page(picked[0]).expect("read page").expect("page exists");
        assert!(first_page.row(picked[0]).is_some());
        assert_eq!(stored_v1.pages_read(), 1);

        let payload_v2 = blaze_drive::key::content_key(b"radix-v2-payload");
        store.put(&cx, payload_v2, b"radix-v2-payload").expect("put payload v2");
        let new_src = ContentId::of("radix e2e edited src", b"v2");
        let mut members: std::vec::Vec<blaze_engine::Planned> = std::vec::Vec::new();
        for &n in cold.answer.by_file[&edited_file].iter() {
            let src = if picked.contains(&n) { new_src } else { cold.answer.rows[&n].source };
            members.push(blaze_engine::Planned { name: n, phase, file: edited_file, source: src });
        }
        members.sort_unstable_by_key(|p| p.name);
        let edit = blaze_engine::Edit { files: &[(edited_file, &members)] };
        let mut eng = blaze_engine::Engine::new_in(tools, stored_v1.answer(), &edit, blaze_engine::Everything, blaze_engine::Plant::None, alloc).expect("engine");
        assert_eq!(eng.next_phase().expect("phase"), Some(phase));
        assert_eq!(eng.tasks().len(), 3);
        let mut batch = blaze_engine::Buffer::new_in(alloc);
        let mut records = blaze_own::BTreeMap::new_in(alloc);
        for task in eng.tasks() {
            batch.done.push(blaze_engine::Done {
                name: task.planned.name, key: task.key, out: payload_v2.0,
                defines: blaze_own::Vec::new_in(alloc), reads: blaze_own::Vec::new_in(alloc),
                findings: 0, failed: false,
            });
            records.insert(task.planned.name, payload_v2);
        }
        eng.merge(core::slice::from_mut(&mut batch)).expect("merge");
        assert_eq!(eng.next_phase().expect("settled"), None);
        let blaze_engine::Finished { answer: edit_answer, causes: edit_causes, .. } = eng.finish().expect("finish");
        let edit_kept = Kept::after(blaze_own::BTreeMap::new_in(alloc), edit_answer, &edit_causes, records);

        let enc_v2 = pages::encode_changes_radix(
            &edit_kept,
            stored_v1.root,
            alloc,
            |k| stored_v1.get_radix_record(k).expect("radix record"),
            |jobs| {
                let mut out = blaze_own::Vec::with_capacity_in(jobs.len(), alloc);
                for job in jobs { out.push(job.run(alloc)); }
                out
            },
        ).expect("encode_changes_radix v2");
        let task_puts = enc_v2.task_puts as usize;
        assert_eq!(task_puts, 4);
        let meta_bytes: usize = enc_v2.puts[task_puts..].iter().map(|(_, b)| b.len()).sum();
        assert!(meta_bytes <= 45 * 1024, "4-page radix edit metadata must be <= 45 KiB (got {meta_bytes} bytes)");

        for (k, b) in enc_v2.puts.iter() {
            store.put(&cx, *k, b).expect("put v2");
        }
        for (page, rows) in enc_v2.page_records() {
            let mut recs = blaze_own::Vec::new_in(alloc);
            recs.push(blaze_cas::Key(page.0));
            for &(_, rec) in rows { recs.push(blaze_cas::Key(rec.0)); }
            lent.index_beside(blaze_cas::Key(page.0), &recs).expect("index beside v2");
        }
        let probes_before = lent.put_tally().disk.root_probes;
        let (live2, rekey2) = publish_reach_shards_from(
            &lent, "big", "big.answer",
            ReachSource::Radix { last: Some((dir_v1.page_count(), enc_v1.root)), encoded: &enc_v2 },
            Rekey::Changed, None, alloc, frame,
        ).expect("publish incremental radix reach shards");
        let probes_delta = lent.put_tally().disk.root_probes - probes_before;
        assert_eq!(live2, enc_v2.live_pages);
        assert_eq!(rekey2, Rekeyed { pages: 4, whole: None });
        assert_eq!(probes_delta, 4, "4-page radix edit probes only 4 delta keys (got {probes_delta})");

        let mut direct2 = blaze_own::Vec::new_in(alloc);
        direct2.push(blaze_cas::Key(enc_v2.root.0));
        direct2.extend(enc_v2.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
        let idx2 = lent.index(&direct2).expect("direct2");
        lent.root_through("big.answer", &[idx2], blaze_cas::plain::ROOT_LIFE_NS).expect("pin big.answer v2");
        lent.release_lease();
        lent.commit().expect("commit v2");
        drop(edit_kept);
        drop(edit_causes);
        drop(stored_v1);
        drop(snap_v1);
        drop(store);
        drop(lent);

        // Run full `blaze gc` under `Liveness::Reached` (budget 16 MiB > live store size, so live
        // roots are NOT LRU-evicted while unpinned records are swept), and verify ALL radix shards,
        // directory sub-records, top root, and task pages survive!
        let gc = crate::gc::run_with(
            &cx, &fs, &host, &clock, &mut rng, Some("/h"),
            crate::gc::GcOpts { max_store_bytes: Some(16 << 20), dry_run: false, all_formats: false, background: false },
            &out, &err, heap,
        ).expect("run gc");
        assert!(!gc.skipped_locked);

        // Reopen store after GC and verify every single row & radix shard of v2 decodes cleanly!
        let lent2 = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 48 }, heap.alloc()).expect("reopen after gc");
        let mut store2 = CommandStore::new(&cx, &lent2, false, 0, None, clock, &err, erased.handle()).expect("store2");
        let (decoded_v2, root_v2) = load_answer(&cx, &mut store2, enc_v2.root, alloc).expect("store read").expect("radix answer survives gc");
        assert!(root_v2.is_radix());
        assert_eq!(decoded_v2.answer.rows.len(), 200, "all 200 rows survived GC");
        for &n in &picked {
            assert_eq!(decoded_v2.answer.rows[&n].out, payload_v2.0, "edited row {n:?} has v2 payload after GC");
        }
        // Verify every single tree key (leaf shard, file shard, places, summary) is explicitly
        // listed in `big.answer`'s direct index so GC can never sweep any of them.
        let ans_root = store2.named("big.answer").expect("big.answer").expect("big.answer live");
        let ans_idx = lent2.get(&cas_key_at(ans_root, 0)).expect("get idx").expect("direct index live");
        for key in enc_v2.tree_keys() {
            assert!(
                ans_idx.bytes.chunks_exact(32).any(|chunk| chunk == key.0.0),
                "tree key {key:?} must be pinned in big.answer direct index for GC reachability"
            );
        }
        let _ = garbage_key;
    }

    /// Rule 26 round-trip: saving a workspace base with `SaveBase::KeepWith(BaseAux { members,
    /// grains })` persists `.members`/`.grains` under the `(workspace, layout)` base slot without
    /// changing `BEB2`; another worktree opening that base via `open_base_for_tools` reads back
    /// the exact keys and `adopt_certified` pins them under Worktree B's own `<name>.members` and
    /// `<name>.grains` roots; evicting Worktree A cleans up the base slot's auxiliary roots while
    /// Worktree B's adopted roots remain live.
    #[test]
    fn base_aux_propagates_across_keep_base_and_adopt_certified() {
        use blaze_drive::{Kind, Tool};
        use blaze_engine::{Cause, Gen, Phase, Row, Stage};

        let io = Io::simulated(SimWorld::new(53, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let wt_a_dir = fs.open_dir(&cx, &bstd::path::Path::new("wt_a").expect("wt_a")).expect("open wt_a");
        let wt_b_dir = fs.open_dir(&cx, &bstd::path::Path::new("wt_b").expect("wt_b")).expect("open wt_b");
        let manifest_bytes = b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n";
        let lock_bytes = b"version = 4\n\n[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\n";
        for dir in [&wt_a_dir, &wt_b_dir] {
            dir.publish(&cx, &bstd::path::Path::new("Cargo.toml").expect("manifest"), bstd::fs::Content::bytes(manifest_bytes), bstd::fs::Durable::No).expect("write Cargo.toml");
            dir.publish(&cx, &bstd::path::Path::new("Cargo.lock").expect("lock"), bstd::fs::Content::bytes(lock_bytes), bstd::fs::Durable::No).expect("write Cargo.lock");
        }
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 53 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");
        let tools = Tools([Tool(ContentId([7; 32])); blaze_drive::key::KINDS]);
        let toolchain = blaze_toolchain::ToolchainId([2; 32]);
        let journal = crate::journal_sweep::take(&cx, &fs, &wt_a_dir, home.dir(), &clock, crate::journal_sweep::Stored::NONE, frame).expect("journal");
        let lines = crate::engine_lines::Lines::new_in(alloc).record();

        let mut rows = Rows::new_in(frame);
        rows.workspace = Workspace::of_lock(lock_bytes, frame);
        let manifest_path = u32::try_from(rows.names.push("Cargo.toml")).expect("path idx");
        let lock_path = u32::try_from(rows.names.push("Cargo.lock")).expect("path idx");
        rows.rows.push(crate::sweep::Leaf {
            kind: crate::sweep::LeafKind::Manifest,
            flags: 0,
            path: manifest_path,
            stamp: None,
            id: ContentId::of(crate::sweep::FILE_DOMAIN, manifest_bytes),
        });
        rows.rows.push(crate::sweep::Leaf {
            kind: crate::sweep::LeafKind::Manifest,
            flags: 0,
            path: lock_path,
            stamp: None,
            id: ContentId::of(crate::sweep::FILE_DOMAIN, lock_bytes),
        });

        let payload = blaze_drive::key::content_key(b"row-payload");
        store.put(&cx, payload, b"row-payload").expect("put payload");
        let members_key = blaze_drive::key::content_key(b"members-record-bytes");
        store.put(&cx, members_key, b"members-record-bytes").expect("put members");
        let grains_key = blaze_drive::key::content_key(b"grains-record-bytes");
        store.put(&cx, grains_key, b"grains-record-bytes").expect("put grains");
        let aux = BaseAux { members: Some(members_key), grains: Some(grains_key) };

        let mut cold = Kept::new_in(alloc);
        cold.answer.serial = Gen(1);
        let file = Name::new(Kind::FileItems, ContentId::of("base aux test file", b"lib.rs"));
        let task = Name::new(Kind::BodyCheck, ContentId::of("base aux test body", b"f"));
        let src = ContentId::of("base aux test src", b"v1");
        cold.answer.by_file.entry(file).or_insert_with(|| blaze_own::Vec::new_in(alloc)).push(task);
        cold.answer.rows.insert(task, Row {
            key: tools.key(task, &src),
            phase: Phase::new(Stage::Bodies, 0),
            file,
            source: src,
            out: payload.0,
            serial: Gen(1),
            defines: blaze_own::Vec::new_in(alloc),
            reads: blaze_own::Vec::new_in(alloc),
            by_cell: blaze_own::Vec::new_in(alloc),
            findings: 0,
            inline: false,
            failed: false,
        });
        cold.causes.insert(task, (Gen(1), Cause::New));
        cold.records.insert(task, payload);

        let wt_a_root = root_name("/wt_a", frame);
        let wt_b_root = root_name("/wt_b", frame);
        let saved = save_edit(
            &cx, &mut store, &lent, wt_a_root.as_str(), toolchain, &tools, &cold,
            Printed { names: true, bodies_whole: true, replayable: true },
            (lines.0, &lines.1), None, &rows, &journal, alloc, frame, &[],
            (None, core::num::NonZeroU32::MIN), SaveBase::KeepWith(aux),
        ).expect("save wt_a");
        assert!(matches!(saved.base, BaseKept::Kept { entries: 1 }), "saved base: {:?}", saved.base);
        lent.commit().expect("commit wt_a");
        // Empty `row_keys` must reuse `beside_key(pointer_key)` for `{name}.answer` (zero redundant Index put).
        let wt_a_answer = bstd::mem::format_in!(frame, "{}.{ANSWER_ROOT}", wt_a_root.as_str());
        let expected_index = blaze_cas::plain::beside_key(blaze_cas::Key(saved.pointer_key.0));
        let answer_bytes = held_root_bytes(&lent, wt_a_answer.as_str()).expect("read answer root").expect("answer root");
        assert_eq!(answer_bytes, expected_index.0.0.as_slice(), "empty row_keys reuses base_direct_index");

        // Worktree B opens the workspace base from `BASES_ROOT` and recovers `BaseAux`.
        let loaded = open_base_for_tools(&cx, &mut store, &lent, &wt_b_dir, &tools, None, true, alloc, frame)
            .expect("open base")
            .expect("found base for wt_b");
        assert_eq!(loaded.aux, aux, "open_base_for_tools must return standing members/grains keys");

        // Worktree B adopts the certified answer: its own `<wt_b>.members` and `<wt_b>.grains` are pinned.
        let adopted = adopt_certified(
            &cx, &mut store, &lent, wt_b_root.as_str(), &loaded.pointer, &loaded.root,
            loaded.aux, &rows, &journal, alloc, frame,
        ).expect("adopt_certified");
        assert!(adopted.is_some(), "adopt_certified succeeded");
        lent.commit().expect("commit wt_b");

        let wt_b_aux = read_base_aux(&mut store, wt_b_root.as_str(), frame).expect("read wt_b aux");
        assert_eq!(wt_b_aux, aux, "adopted checkout owns its own .members and .grains roots");

        // Unrooting Worktree A removes the base slot's `.members`/`.grains` while Worktree B's stay live.
        let ws = rows.workspace.expect("workspace");
        let layout = layout_of(&rows, frame).expect("layout");
        let slot_prefix = base_slot_prefix(ws.id, layout, frame);
        drop(loaded);
        drop(store);
        unroot_checkout(&lent, "/wt_a", frame).expect("unroot wt_a");
        let mut store2 = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle()).expect("store2");
        assert_eq!(read_base_aux(&mut store2, slot_prefix.as_str(), frame).expect("slot aux"), BaseAux::default());
        assert_eq!(read_base_aux(&mut store2, wt_b_root.as_str(), frame).expect("wt_b aux after clean"), aux);
    }

    /// 128-core false-sharing elimination in `Stored`: `Stored` stays `<= 1 KiB` by value,
    /// `AskedShard` is 128-byte cache-line padded (`CachePadded`), `ask_shard_index` distributes
    /// across shards directly from `Name` bytes without a shared global `asks` atomic, and
    /// `note_ask` / `asks` / `asked` accurately count repeats, distinct names, purposes, and
    /// per-shard cap overflow (`untold`).
    #[test]
    fn stored_sharded_asks_record_accurately_and_stay_cache_padded() {
        use blaze_drive::Kind;
        use blaze_engine::{Ask, Gen};

        assert!(core::mem::size_of::<Stored<'static>>() <= 1 << 10, "Stored must stay <= 1 KiB by value");
        assert!(
            core::mem::align_of::<bstd::sync::CachePadded<AskedShard<'static>>>() >= 64,
            "AskedShard must be cache-line aligned"
        );

        let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 71 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle()).expect("command store");
        let empty = Kept::new_in(alloc);
        let enc = pages::encode_at_bits(&empty, 1, alloc);
        for (k, b) in enc.puts.iter() {
            store.put(&cx, *k, b).expect("put empty root");
        }
        lent.commit().expect("commit empty root");
        let root_dir = open_root(&cx, &mut store, enc.root, alloc).expect("open").expect("valid root");
        let snap = store.snapshot_in(alloc).expect("snapshot");
        let headers = Headers::Absent { since: Gen(1) };
        let stored = Stored::new(&root_dir, &snap, &headers, pages::Check::Store, alloc);

        // Spread 256 distinct BodyCheck names + repeated asks across shards.
        let mut shards_hit = [false; ASKED_SHARDS];
        for i in 0u32..256 {
            let name = Name::new(Kind::BodyCheck, ContentId::of("stored shard test", &i.to_le_bytes()));
            shards_hit[Stored::ask_shard_index(name)] = true;
            stored.note_ask(name, Ask::Header);
            if i % 4 == 0 {
                stored.note_ask(name, Ask::Header);
            }
        }
        let distinct_shards = shards_hit.iter().filter(|&&hit| hit).count();
        assert!(distinct_shards >= ASKED_SHARDS / 2, "hash-based shard selection must spread across shards, hit {distinct_shards}/{ASKED_SHARDS}");
        assert_eq!(stored.asks(), 256 + 64, "total sharded ask count matches exact calls");
        let by = stored.asked();
        assert_eq!(by.asks[Kind::BodyCheck.index()], 320);
        assert_eq!(by.distinct[Kind::BodyCheck.index()], 256);
        assert_eq!(by.by_purpose[Ask::Header.purpose()], 320);
        assert_eq!(by.purpose_distinct[Ask::Header.purpose()], 256);
        assert_eq!(by.untold, 0, "no asks past per-shard cap");

        // Exceed MOST_ASKS_PER_SHARD on one hot name and verify untold counts exact overflow.
        let hot = Name::new(Kind::FileItems, ContentId::of("stored shard hot", b"hot"));
        let overflow = MOST_ASKS_PER_SHARD + 17;
        for _ in 0..overflow {
            stored.note_ask(hot, Ask::Header);
        }
        assert_eq!(stored.asks(), 320 + overflow);
        let after = stored.asked();
        assert!(after.untold >= 17, "asks past per-shard cap must be counted in untold, got {}", after.untold);
    }

    /// Reproduces the `reach.4a` partial-eviction failure:
    /// 1. A 65,536-page (`bits = 16`) sharded radix answer commits `{root}`, `{root}.answer`,
    ///    `{root}.reach.00..7f`, and `{root}.reach.delta`.
    /// 2. Unrooting `{root}.reach.4a` alone (before compaction) is caught immediately by both
    ///    `reach_roots_live_without_root` (zero-I/O certificate guard) and `verify_reach_roots`
    ///    (`Unreadable::MissingReach { what: MissingReachRoot::Shard(0x4a) }`).
    /// 3. Unrooting `{root}.reach.4a..7f` and `{root}.reach.delta` (exact LRU suffix eviction
    ///    order) followed by `f6s3` compaction sweeps the pages and `beside_key`s in shards
    ///    `4a..7f` while `{root}` and `{root}.answer` (including all 256 radix leaf shards) remain
    ///    live. `verify_reach_roots` detects the swept page/beside_key at load time
    ///    (`Unreadable::Pages(PagesError::Missing { .. })`), `open_named_base` refuses to reuse
    ///    the holed answer, and `publish_reach_shards_from` surfaces `PagesError::Reach` instead
    ///    of misreporting `StoreError::RootNamesUnheld` as a writer bug.
    #[test]
    fn partially_evicted_radix_reach_shard_is_rejected_at_load_and_surfaces_reach_error_on_save() {
        use blaze_cas::plain::LEASE_NS;
        use blaze_drive::{Kind, Tool};
        use blaze_engine::{Cause, Gen, Phase, Row, Stage};

        let io = Io::simulated(SimWorld::new(89, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let tools = Tools([Tool(ContentId([11; 32])); blaze_drive::key::KINDS]);
        let phase = Phase::new(Stage::Bodies, 0);

        // Build a `bits = 16` radix answer with a page guaranteed to land in reach shard `0x4a`
        // (page indices `0x4a * 512 .. 0x4b * 512`, i.e. content pages with high byte in `0x94..0x96`,
        // wait: content pages are `0..32768`, so reach shards `0x00..0x3f` hold content pages and
        // `0x40..0x7f` hold readers pages!). Add readers edges so shard `0x4a` has a live page.
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 89 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");
        let payload = blaze_drive::key::content_key(b"radix-evict-payload");
        store.put(&cx, payload, b"radix-evict-payload").expect("put payload");
        let mut cold = Kept::new_in(alloc);
        cold.answer.serial = Gen(1);
        let mut found_4a = false;
        for i in 0..2048u32 {
            let file = Name::new(Kind::FileItems, ContentId::of("evict radix file", &(i % 16).to_le_bytes()));
            let name = Name::new(Kind::BodyCheck, ContentId::of("evict radix task", &i.to_le_bytes()));
            let src = ContentId::of("evict radix src", &i.to_le_bytes());
            let cell = blaze_engine::CellId { of: name, cell: crate::engine_tasks::WHOLE };
            let rpage = pages::readers_page_of(&cell, 16) as usize;
            if rpage / REACH_SHARD_PAGES == 0x4a {
                found_4a = true;
            }
            cold.answer.by_file.entry(file).or_insert_with(|| blaze_own::Vec::new_in(alloc)).push(name);
            cold.answer.readers.insert(cell, name);
            let mut reads = blaze_own::Vec::new_in(alloc);
            reads.push((cell, payload.0));
            let mut by_cell = blaze_own::Vec::new_in(alloc);
            by_cell.push(0);
            cold.answer.rows.insert(name, Row {
                key: tools.key(name, &src), phase, file, source: src, out: payload.0, serial: Gen(1),
                defines: blaze_own::Vec::new_in(alloc), reads, by_cell,
                findings: 0, inline: false, failed: false,
            });
            cold.causes.insert(name, (Gen(1), Cause::New));
            cold.records.insert(name, payload);
        }
        assert!(found_4a, "test must place at least one live page in reach shard 0x4a");
        for names in cold.answer.by_file.values_mut() {
            names.sort_unstable();
        }

        let enc = pages::encode_at_bits(&cold, 16, alloc);
        assert!(enc.is_radix());
        for (k, b) in enc.puts.iter() {
            store.put(&cx, *k, b).expect("put radix record");
        }
        let mut indexed_beside: BesideCache<'_> = blaze_hash::Map::new(blaze_hash::MapOpts {
            hasher: blaze_hash::BuildKeyHasher::default(),
            alloc,
            capacity: Some(usize::try_from(enc.task_puts).unwrap_or(0)),
        });
        for (page, rows) in enc.page_records() {
            let mut recs = blaze_own::Vec::with_capacity_in(rows.len() + 1, alloc);
            recs.push(blaze_cas::Key(page.0));
            for &(_, rec) in rows {
                recs.push(blaze_cas::Key(rec.0));
            }
            recs.sort_unstable();
            recs.dedup();
            let beside = lent.index_beside(blaze_cas::Key(page.0), &recs).expect("index beside");
            indexed_beside.insert(page.0.0, beside);
        }
        let (lines_key, lines_bytes) = crate::engine_lines::Lines::new_in(alloc).record();
        store.put(&cx, lines_key, &lines_bytes).expect("put lines");
        let empty_rows = Rows::new_in(frame);
        let leaves = Leaves::bytes_in(&empty_rows, frame).expect("leaves");
        let mut ptr_bytes: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
        ptr_bytes.extend_from_slice(POINTER_MAGIC);
        ptr_bytes.extend_from_slice(&enc.root.0.0);
        ptr_bytes.extend_from_slice(&lines_key.0.0);
        ptr_bytes.extend_from_slice(&enc.files.to_le_bytes());
        ptr_bytes.push(Printed { names: true, bodies_whole: true, replayable: true }.byte());
        ptr_bytes.extend_from_slice(&enc.findings.to_le_bytes());
        ptr_bytes.extend_from_slice(&enc.owed.to_le_bytes());
        ptr_bytes.extend_from_slice(&tools_digest(&tools).0);
        ptr_bytes.push(HEADERS_ABSENT);
        ptr_bytes.extend_from_slice(&1u64.to_le_bytes());
        ptr_bytes.extend_from_slice(&[0u8; 32 * HeaderPages::COUNT]);
        crate::journal_sweep::Stored::encode_units(core::iter::empty(), None, &mut ptr_bytes);
        let sealed = seal::seal(&ptr_bytes, &leaves, frame).expect("seal");
        let ptr_key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
        store.put(&cx, ptr_key, &sealed).expect("put pointer");

        let _ = publish_reach_shards_from(
            &lent, "big", "big.answer", ReachSource::Radix { last: None, encoded: &enc },
            Rekey::Changed, Some(&indexed_beside), alloc, frame,
        ).expect("publish cold radix shards");
        let mut direct = blaze_own::Vec::new_in(alloc);
        direct.push(blaze_cas::Key(ptr_key.0));
        direct.push(blaze_cas::Key(enc.root.0));
        direct.extend(enc.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
        direct.push(blaze_cas::Key(lines_key.0));
        let answer_idx = lent.index_beside(blaze_cas::Key(ptr_key.0), &direct).expect("answer idx");
        lent.root_through("big.answer", &[answer_idx], blaze_cas::plain::ROOT_LIFE_NS).expect("pin big.answer");
        store.pin("big", &[ptr_key]).expect("pin big");
        // Write unpinned dead bytes into the same segment so compaction under `Liveness::Reached`
        // sees `< 50%` live share and rewrites the segment, sweeping unpinned shards `4a..7f`.
        let mut junk = blaze_own::Vec::new_in(alloc);
        junk.resize(8 << 20, 0xA5);
        let junk_key = Key(ContentId::of("evict radix junk", b"dead-segment-padding"));
        store.put(&cx, junk_key, &junk).expect("put junk");
        lent.commit().expect("commit intact radix answer");

        // Intact: both fast certificate check and full root verification succeed.
        let dir_intact = open_root(&cx, &mut store, enc.root, alloc).expect("store").expect("root");
        assert!(reach_roots_live_without_root(&lent, "big", frame), "intact radix answer passes zero-I/O check");
        assert!(verify_reach_roots(&lent, "big", &dir_intact, true, alloc, frame).expect("store").is_ok());
        assert!(
            open_named_base(&cx, &mut store, &lent, "big", ReachCheck::Sharded("big"), &tools, None, true, alloc, frame)
                .expect("open_named_base")
                .is_some(),
            "intact radix base opens cleanly"
        );

        // Case 1: unroot ONLY shard 0x4a (before compaction sweeps any bytes).
        lent.unroot("big.reach.4a");
        lent.commit().expect("commit unroot 4a");
        assert!(!reach_roots_live_without_root(&lent, "big", frame), "gap at shard 0x4a must fail zero-I/O check");
        let pre_compact = verify_reach_roots(&lent, "big", &dir_intact, true, alloc, frame).expect("store");
        assert!(
            matches!(pre_compact, Err(Unreadable::MissingReach { what: MissingReachRoot::Shard(0x4a) })),
            "missing shard 0x4a before compaction must report MissingReach(Shard(0x4a)), got {pre_compact:?}"
        );

        // Case 2: exact LRU suffix eviction (`reach.4a..7f` and `reach.delta`) + compaction sweep.
        for shard in 0x4a..REACH_SHARDS_MOST {
            let sname = bstd::mem::format_in!(frame, "big.reach.{shard:02x}");
            if lent.has_root(sname.as_str()) {
                lent.unroot(sname.as_str());
            }
        }
        lent.unroot("big.reach.delta");
        lent.release_lease();
        lent.commit().expect("commit suffix eviction");
        drop(store);
        drop(lent);

        io.with_sim(|sim| sim.advance(core::time::Duration::from_nanos(LEASE_NS + 1_000_000))).expect("advance past lease");
        let cx2 = io.cx(BUDGET, TASK_FRAME).expect("cx2");
        let frame2 = cx2.frame();
        let erased2 = blaze_own::Erased::new(frame2);
        let lent2 = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 90 }, heap.alloc()).expect("reopen");
        let _ = lent2.compact_foreground(32).expect("compact swept 4a..7f");
        lent2.commit().expect("commit compact");
        let mut store2 = CommandStore::new(&cx2, &lent2, false, 0, None, clock, &err, erased2.handle()).expect("store2");

        // `big` pointer and `big.answer` (with all 256 radix leaf shards) survived compaction,
        // while shard 0x4a's pages and `beside_key`s were swept!
        let dir_after = open_root(&cx2, &mut store2, enc.root, alloc).expect("store").expect("root still opens via big.answer");
        assert!(!reach_roots_live_without_root(&lent2, "big", frame2), "evicted suffix must fail zero-I/O check");
        let post_compact = verify_reach_roots(&lent2, "big", &dir_after, true, alloc, frame2).expect("store");
        assert!(
            matches!(post_compact, Err(Unreadable::Pages(PagesError::Missing { .. }))),
            "swept shard 0x4a must report PagesError::Missing at load time, got {post_compact:?}"
        );
        assert!(
            open_named_base(&cx2, &mut store2, &lent2, "big", ReachCheck::Sharded("big"), &tools, None, false, alloc, frame2)
                .expect("open_named_base")
                .is_none(),
            "partially evicted base must be rejected by open_named_base"
        );

        // Save-time guard: publishing a scoped root over carried `enc` shards surfaces
        // `PagesError::Reach` instead of misreporting `StoreError::RootNamesUnheld` as a writer bug.
        let carried_err = publish_reach_shards_from(
            &lent2, "big.s.scoped", "big.s.scoped.answer",
            ReachSource::Radix { last: Some((enc.page_count(), enc.root)), encoded: &enc },
            Rekey::Changed, None, alloc, frame2,
        ).expect_err("carried swept beside_key must be refused");
        assert!(
            matches!(
                carried_err,
                CliError::EngineCheck(crate::engine_check::EngineCheckError::Engine(
                    blaze_engine::EngineError::Last { error: blaze_engine::last::LastError::Pages(PagesError::Reach { .. }) }
                ))
            ),
            "carried swept beside_key must surface PagesError::Reach, got: {carried_err}"
        );

        // Control: on a cold write (`last: None`), an unput beside_key STILL surfaces `RootNamesUnheld`.
        let cold_err = publish_reach_shards_from(
            &lent2, "big.s.cold", "big.s.cold.answer",
            ReachSource::Radix { last: None, encoded: &enc },
            Rekey::Changed, None, alloc, frame2,
        ).expect_err("cold unput beside_key must still be refused");
        assert!(
            cold_err.to_string().contains("which was never put"),
            "cold unput beside_key must remain a writer's bug error, got: {cold_err}"
        );
    }
}
