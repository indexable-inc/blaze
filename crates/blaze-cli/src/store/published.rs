//! The published set on the store's side (docs/v2/plan/design/published.md; blaze-remote
//! published.rs and published/set.rs hold the layout, the set's name and the reader's check):
//!
//! - THE WALK ([`walk_toolchain`]): the root `tool.<id>` (store/roots.rs) through its indexes to
//!   each record it keeps, with the record's own tag and namespace, for blaze-remote `lay_out_bound`
//!   (whose predicate keeps the public ones). A key a root names that the store does not hold, or
//!   a record of the wrong tag where an index belongs, stops the walk by name: nothing is skipped.
//! - THE FLOOR ([`seen`], [`floor`], [`keep_floor`]): the highest generation of a set this home
//!   has taken, and when (the wall clock) its head was last asked, one record under the root
//!   `pubf.<set id>` (store/roots.rs `RootName::published_floor`), kept as long as the toolchain's
//!   records. A reader's [`SetCheck`](blaze_remote::published::set::SetCheck) refuses a head below
//!   the generation: an older set served again is a rollback, refused by name. The time is the
//!   freshness window's (store/take.rs `FRESH_NS`).
//! - THE FOLDER WRITE ([`folder`], in a trusted builder's blaze: blaze-cli's `publish` feature,
//!   which turns on blaze-remote's `sign`; D136): the set laid out at the generation after its
//!   folder's head, every object named by a digest written first, then `prefix`, then `head`,
//!   each by a rename.

use core::fmt;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace, Tag};
use blaze_keyed::kind::{ReservedTag, TagReach, reach_of};
use blaze_core::Driver;
use blaze_ids::ContentId;
use blaze_toolchain::ToolchainId;

use super::plain_place::{PlaceError, PlainPlace, StoreAlloc};
use super::roots::{RootName, TOOLCHAIN_LIFE_NS};

// A floor record is filed under [`ReservedTag::Floor`] (blaze-keyed's registry).
const FLOOR_DOMAIN: &str = "blaze-cli published floor 1";
const FLOOR_MAGIC: [u8; 4] = *b"BLPF";
/// A floor record: the magic, the set's id, the generation, the wall time its head was last
/// asked (ns since the Unix epoch; both little-endian).
const FLOOR_LEN: usize = 4 + 32 + 8 + 8;

/// What this home holds of a published set: the highest generation it has taken (0: none), and
/// when its head was last asked, by the command's wall clock (0: never).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Seen {
    pub generation: u64,
    pub asked_wall_ns: u64,
}

/// Why the published set's walk, floor or write stopped: each names the root or the object.
#[derive(Debug)]
#[cfg_attr(not(any(test, feature = "publish")), allow(dead_code, reason = "the folder write's refusals: made by `blaze store publish` in a trusted builder's blaze (the `publish` feature)"))]
pub enum PublishedError {
    /// The store refused a read or the root under `root`.
    Store { root: RootName, why: LentError },
    /// The store refused the floor record's put.
    Place(PlaceError),
    /// The root names `key`, which the store does not hold.
    Gone { root: RootName, key: CasKey },
    /// Where an index (or an indexed root) belongs, `key` is a record of another tag.
    NotIndexed { root: RootName, key: CasKey },
    /// The floor record under `root` is not one: its length, or its set.
    Floor { root: RootName, len: usize },
    /// A set's part or an object's name did not make a path (cannot be: both are checked names).
    Path(bstd::path::PathError),
    /// A folder origin's file call: what was being read or written, and bstd's error (which
    /// names the file and never holds its bytes).
    Folder { what: &'static str, error: bstd::fs::FsError },
    /// The folder's own head was refused (another set's, another key's, or damaged).
    Head(blaze_remote::read::Refused),
    /// Only a folder origin is written; an HTTP origin is written by CI and the remote workers.
    HttpOrigin,
    /// The signer's key file does not hold 64 hex digits (its bytes are never shown).
    SignerKey,
    /// `blaze store publish` in a blaze built without the `publish` feature: only a trusted
    /// builder's (CI's) signs a set (D136).
    NotPublisher,
    /// The set's name (this blaze's toolchain and target) is not one a set may have.
    Set(blaze_remote::published::set::SetNameError),
    #[cfg(any(test, feature = "publish"))]
    Lay(blaze_remote::published::LayOutError),
}

impl fmt::Display for PublishedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishedError::Store { root, why } => write!(f, "the published set's root `{}`: {why}", root.as_str()),
            PublishedError::Place(error) => write!(f, "the published set's floor: {error}"),
            PublishedError::Gone { root, key } => write!(f, "the root `{}` names {:?}, which the store does not hold: nothing is published from a root with a hole", root.as_str(), key.0),
            PublishedError::NotIndexed { root, key } => write!(f, "the root `{}` names {:?} where an index belongs (blaze-keyed `ReservedTag::Index`, `ReservedTag::IndexedRoot`)", root.as_str(), key.0),
            PublishedError::Floor { root, len } => write!(f, "the floor root `{}` names a record of {len} bytes that is not this set's floor ({FLOOR_LEN} bytes, `BLPF`)", root.as_str()),
            PublishedError::Path(error) => write!(f, "a published set's path: {error}"),
            PublishedError::Folder { what, error } => write!(f, "the folder origin, {what}: {error}"),
            PublishedError::Head(why) => write!(f, "the folder origin's head is refused, so nothing is laid over it: {why}"),
            PublishedError::HttpOrigin => f.write_str("an http:// or https:// origin is not written from here: only CI and the remote workers publish to a shared origin; name a folder"),
            PublishedError::SignerKey => f.write_str("the signer's key file does not hold an Ed25519 seed as 64 hex digits (its contents are not shown)"),
            PublishedError::NotPublisher => f.write_str("this blaze was built without publishing: only a trusted builder's is (blaze-cli's `publish` feature, D136)"),
            PublishedError::Set(error) => write!(f, "the published set's name: {error}"),
            #[cfg(any(test, feature = "publish"))]
            PublishedError::Lay(error) => write!(f, "the published set's layout: {error}"),
        }
    }
}

impl core::error::Error for PublishedError {}

impl From<PublishedError> for crate::CliError {
    fn from(error: PublishedError) -> crate::CliError {
        crate::CliError::Store(super::StoreFailure::Published(error))
    }
}

/// One record a walk found: its key, its own tag and namespace, its bytes in place.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(any(test, feature = "publish")), allow(dead_code, reason = "walked by `blaze store publish`, in a trusted builder's blaze (the `publish` feature)"))]
pub(crate) struct Walked<'l> {
    pub key: ContentId,
    pub tag: Tag,
    pub namespace: Namespace,
    pub bytes: &'l [u8],
}

fn key_of(bytes: &[u8]) -> CasKey {
    CasKey(ContentId(bytes.try_into().expect("a 32-byte chunk")))
}

/// Every record the root `tool.<toolchain>` keeps, added to `out` in the root's order (the
/// layout sorts): through each index it names to each key the index holds. No root: nothing.
#[cfg_attr(not(any(test, feature = "publish")), allow(dead_code, reason = "called by `blaze store publish`, in a trusted builder's blaze (the `publish` feature)"))]
pub(crate) fn walk_toolchain<'l, D: Driver>(lent: &'l Lent<D, StoreAlloc>, toolchain: ToolchainId, out: &mut std::vec::Vec<Walked<'l>, StoreAlloc>) -> Result<(), PublishedError> {
    let root = RootName::toolchain(toolchain);
    let store = |why| PublishedError::Store { root, why };
    let Some(at) = lent.root_named(root.as_str()) else { return Ok(()) };
    let Some(indexes) = lent.get(&at).map_err(store)? else { return Err(PublishedError::Gone { root, key: at }) };
    if reach_of(indexes.tag) != TagReach::Indexes {
        return Err(PublishedError::NotIndexed { root, key: at });
    }
    for index in indexes.bytes.chunks_exact(32).map(key_of) {
        let Some(keys) = lent.get(&index).map_err(store)? else { return Err(PublishedError::Gone { root, key: index }) };
        if reach_of(keys.tag) != TagReach::IndexKeys {
            return Err(PublishedError::NotIndexed { root, key: index });
        }
        for key in keys.bytes.chunks_exact(32).map(key_of) {
            let Some(record) = lent.get(&key).map_err(store)? else { return Err(PublishedError::Gone { root, key }) };
            out.push(Walked { key: key.0, tag: record.tag, namespace: record.namespace, bytes: record.bytes });
        }
    }
    Ok(())
}

/// The origin named for a publish: a folder; an `http://` or `https://` one refused by name for
/// every blaze (a shared origin is written by CI and the remote workers alone).
pub(crate) fn folder_only(origin: &str) -> Result<&str, PublishedError> {
    if origin.starts_with("http://") || origin.starts_with("https://") {
        return Err(PublishedError::HttpOrigin);
    }
    Ok(origin)
}

/// The highest generation of `set` this home has taken; 0 when it has taken none.
pub(crate) fn floor<D: Driver>(lent: &Lent<D, StoreAlloc>, set: &ContentId) -> Result<u64, PublishedError> {
    Ok(seen(lent, set)?.generation)
}

/// What this home holds of `set`: its floor and when its head was last asked; zeros when it
/// has taken none.
pub(crate) fn seen<D: Driver>(lent: &Lent<D, StoreAlloc>, set: &ContentId) -> Result<Seen, PublishedError> {
    let root = RootName::published_floor(set);
    let store = |why| PublishedError::Store { root, why };
    let Some(at) = lent.root_named(root.as_str()) else { return Ok(Seen { generation: 0, asked_wall_ns: 0 }) };
    // A root's record is the keys it names: one, the floor record's.
    let Some(named) = lent.get(&at).map_err(store)? else { return Err(PublishedError::Gone { root, key: at }) };
    if named.bytes.len() != 32 {
        return Err(PublishedError::Floor { root, len: named.bytes.len() });
    }
    let key = key_of(named.bytes);
    let Some(record) = lent.get(&key).map_err(store)? else { return Err(PublishedError::Gone { root, key }) };
    let bytes = record.bytes;
    if !ReservedTag::Floor.is(record.tag) ||bytes.len() != FLOOR_LEN || bytes[..4] != FLOOR_MAGIC || bytes[4..36] != set.0 {
        return Err(PublishedError::Floor { root, len: bytes.len() });
    }
    let word = |at: usize| {
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(word)
    };
    Ok(Seen { generation: word(36), asked_wall_ns: word(44) })
}

/// Keeps `generation` as the floor of `set` from the next commit on, its head asked at
/// `asked_wall_ns` (the command's wall clock): a `LOCAL` record (this home's fact, never
/// published) under the root `pubf.<set>`.
pub(crate) fn keep_floor<D: Driver>(place: &mut PlainPlace<'_, D>, set: &ContentId, generation: u64, asked_wall_ns: u64) -> Result<(), PublishedError> {
    let mut bytes = [0u8; FLOOR_LEN];
    bytes[..4].copy_from_slice(&FLOOR_MAGIC);
    bytes[4..36].copy_from_slice(&set.0);
    bytes[36..44].copy_from_slice(&generation.to_le_bytes());
    bytes[44..].copy_from_slice(&asked_wall_ns.to_le_bytes());
    let key = ContentId::of(FLOOR_DOMAIN, &bytes);
    place.put_record(blaze_drive::Key(key), false, ReservedTag::Floor.tag(), &bytes).map_err(PublishedError::Place)?;
    let root = RootName::published_floor(set);
    place.lent().root_for(root.as_str(), &[CasKey(key)], TOOLCHAIN_LIFE_NS).map_err(|why| PublishedError::Store { root, why })?;
    Ok(())
}

/// The write of a set to a folder origin: in a trusted builder's blaze (blaze-cli's `publish`
/// feature turns on blaze-remote's `sign`, D136), and under test.
#[cfg(any(test, feature = "publish"))]
pub(crate) mod folder {
    use blaze_own::{Alloc, Vec};
    use blaze_remote::PublicKey;
    use blaze_remote::published::set::{SetCheck, SetName, is_object_name, write_rank};
    use blaze_remote::published::{Candidate, Class, Layout, lay_out_bound};
    use blaze_remote::read::form::HEAD_LEN;
    use blaze_remote::read::{Head, Namespace, Object, Sealed};
    use blaze_remote::trust::Signer;
    use bstd::cx::Cx;
    use bstd::fs::{Content, Dir, FsKind, ReadWrite};
    use bstd::path::Path;

    use super::{PublishedError, Walked};

    /// Payload bytes a segment holds at most, and index entries a page.
    pub(crate) const SEGMENT_BYTES: u64 = 32 << 20;
    pub(crate) const PAGE_ENTRIES: u32 = 256;

    /// The order objects are written in: the head last, or (the planted bug) first.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Order {
        HeadLast,
        #[cfg_attr(not(test), allow(dead_code, reason = "the planted order: the publish DST's control"))]
        HeadFirst,
    }

    /// A set laid out for its folder: at `generation`; `unchanged` when the folder's head is
    /// already these bytes (nothing is written).
    pub(crate) struct Plan<'a> {
        pub objects: Vec<Sealed<'a>, Alloc<'a>>,
        pub generation: u64,
        pub unchanged: bool,
        pub published: u32,
        pub left_out: u32,
    }

    /// The signer whose seed is the file at `path` (64 hex digits): read through the granted
    /// folder, at most 66 bytes, never printed, recorded or put in an error.
    pub(crate) fn signer_from_file(cx: &Cx, dir: &Dir<ReadWrite>, path: &Path) -> Result<Signer, PublishedError> {
        let failed = |error| PublishedError::Folder { what: "the signer's key file", error };
        let file = dir.open_read(cx, path).map_err(failed)?;
        let mut text = [0u8; 66];
        let n = file.read_into(&mut text, 0).wait(cx).map_err(failed)?;
        let seed = core::str::from_utf8(&text[..n]).ok().and_then(PublicKey::from_hex).ok_or(PublishedError::SignerKey)?;
        Ok(Signer::from_seed(&seed.0))
    }

    fn made(cx: &Cx, dir: &Dir<ReadWrite>, part: &str) -> Result<Dir<ReadWrite>, PublishedError> {
        let path = Path::new(part).map_err(PublishedError::Path)?;
        match dir.make_dir_synced(cx, &path) {
            Ok(made) => Ok(made),
            Err(error) if error.kind == FsKind::Exists => Ok(dir.subdir(&path)),
            Err(error) => Err(PublishedError::Folder { what: "a set's folder", error }),
        }
    }

    /// The set's folder beneath `origin`, made where it is not: `public/<toolchain>/<target>`.
    pub(crate) fn set_dir(cx: &Cx, origin: &Dir<ReadWrite>, set: &SetName<'_>) -> Result<Dir<ReadWrite>, PublishedError> {
        let [public, toolchain, target] = set.dir();
        let public = made(cx, origin, public)?;
        let toolchain = made(cx, &public, toolchain)?;
        made(cx, &toolchain, target)
    }

    /// The folder's head, checked as a reader checks it (bound to `set`, signed by `signer`):
    /// `None` when there is none. At most one byte past a head is read.
    fn current_head(cx: &Cx, dir: &Dir<ReadWrite>, set: &SetName<'_>, signer: &Signer) -> Result<Option<([u8; HEAD_LEN], Head)>, PublishedError> {
        let failed = |error| PublishedError::Folder { what: "the set's head", error };
        let path = Path::new("head").map_err(PublishedError::Path)?;
        let file = match dir.open_read(cx, &path) {
            Ok(file) => file,
            Err(error) if error.kind == FsKind::NotFound => return Ok(None),
            Err(error) => return Err(failed(error)),
        };
        let mut bytes = [0u8; HEAD_LEN + 1];
        let n = file.read_into(&mut bytes, 0).wait(cx).map_err(failed)?;
        let keys = [signer.public()];
        let head = Head::decode(&bytes[..n], Namespace::PUBLIC, &SetCheck { set: set.id(), keys: &keys, seen: 0 }).map_err(PublishedError::Head)?;
        let mut held = [0u8; HEAD_LEN];
        held.copy_from_slice(&bytes[..HEAD_LEN]);
        Ok(Some((held, head)))
    }

    /// `records` laid out for the folder `dir` of `set`: at its head's generation when that lays
    /// out the same head (unchanged), else at the next one (1 for an empty folder).
    pub(crate) fn plan<'a>(cx: &Cx, dir: &Dir<ReadWrite>, set: &SetName<'_>, records: &[Walked<'_>], signer: &Signer, alloc: Alloc<'a>) -> Result<Plan<'a>, PublishedError> {
        plan_sized(cx, dir, set, records, signer, SEGMENT_BYTES, alloc)
    }

    /// [`plan`] with segments cut at `segment_bytes` of payloads (a test cuts many).
    pub(crate) fn plan_sized<'a>(cx: &Cx, dir: &Dir<ReadWrite>, set: &SetName<'_>, records: &[Walked<'_>], signer: &Signer, segment_bytes: u64, alloc: Alloc<'a>) -> Result<Plan<'a>, PublishedError> {
        let id = set.id();
        let mut candidates: Vec<Candidate<'_>, Alloc<'_>> = Vec::with_capacity_in(records.len(), alloc);
        for record in records {
            candidates.push(Candidate { key: record.key, tag: record.tag, namespace: record.namespace, class: Class::Toolchain, bytes: record.bytes });
        }
        let lay = |generation| {
            let layout = Layout { segment_bytes, page_entries: PAGE_ENTRIES, generation };
            lay_out_bound(&candidates, layout, Some(&id), signer, alloc).map_err(PublishedError::Lay)
        };
        let current = current_head(cx, dir, set, signer)?;
        let generation = current.as_ref().map_or(1, |(_, head)| head.generation);
        let laid = lay(generation)?;
        let same = current.as_ref().is_some_and(|(bytes, _)| laid.objects.iter().any(|sealed| sealed.object == Object::Head && sealed.bytes.as_slice() == bytes.as_slice()));
        if same || current.is_none() {
            return Ok(Plan { objects: laid.objects, generation, unchanged: same, published: laid.published, left_out: laid.left_out });
        }
        let generation = generation + 1;
        let laid = lay(generation)?;
        Ok(Plan { objects: laid.objects, generation, unchanged: false, published: laid.published, left_out: laid.left_out })
    }

    /// The plan's objects written into `dir` in `order`, each whole by a rename with its bytes
    /// flushed and the folder's names synced, at most `most` of them (a test stops it there, as
    /// a crash would): how many were written.
    ///
    /// Full `Order::HeadLast` publishes batch all rank-0/1 objects into one linear `Dir::txn()`
    /// (`commit_barrier`, syncing the directory once) before publishing the rank-2 head synced,
    /// preserving the `HeadLast` crash invariant while cutting `O(N)` directory syncs to 2.
    pub(crate) fn write(cx: &Cx, dir: &Dir<ReadWrite>, plan: &Plan<'_>, order: Order, most: usize) -> Result<usize, PublishedError> {
        const WHAT: [&str; 3] = ["an object named by its digest", "the prefix", "the head"];
        if order == Order::HeadLast && most >= plan.objects.len() {
            let mut txn = dir.txn();
            let mut written = 0;
            for rank in [0u8, 1u8] {
                for sealed in plan.objects.iter().filter(|sealed| write_rank(&sealed.object) == rank) {
                    let name = sealed.object.name();
                    assert!(is_object_name(name.as_str()), "an object's name is made from a digest's hex alone");
                    let path = match Path::new(name.as_str()) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = txn.abort(cx);
                            return Err(PublishedError::Path(e));
                        }
                    };
                    if let Err(error) = txn.publish(cx, &path, Content::bytes(&sealed.bytes)) {
                        let _ = txn.abort(cx);
                        return Err(PublishedError::Folder { what: WHAT[usize::from(rank)], error });
                    }
                    written += 1;
                }
            }
            if txn.is_empty() {
                txn.abort(cx).map_err(|error| PublishedError::Folder { what: WHAT[0], error })?;
            } else {
                txn.commit_barrier(cx).map_err(|error| PublishedError::Folder { what: WHAT[0], error })?;
            }
            for sealed in plan.objects.iter().filter(|sealed| write_rank(&sealed.object) == 2) {
                let name = sealed.object.name();
                assert!(is_object_name(name.as_str()), "an object's name is made from a digest's hex alone");
                let path = Path::new(name.as_str()).map_err(PublishedError::Path)?;
                dir.publish_synced(cx, &path, Content::bytes(&sealed.bytes)).map_err(|error| PublishedError::Folder { what: WHAT[2], error })?;
                written += 1;
            }
            return Ok(written);
        }
        let ranks: [u8; 3] = match order {
            Order::HeadLast => [0, 1, 2],
            Order::HeadFirst => [2, 1, 0],
        };
        let mut written = 0;
        for rank in ranks {
            for sealed in plan.objects.iter().filter(|sealed| write_rank(&sealed.object) == rank) {
                if written == most {
                    return Ok(written);
                }
                let name = sealed.object.name();
                assert!(is_object_name(name.as_str()), "an object's name is made from a digest's hex alone");
                let path = Path::new(name.as_str()).map_err(PublishedError::Path)?;
                dir.publish_synced(cx, &path, Content::bytes(&sealed.bytes)).map_err(|error| PublishedError::Folder { what: WHAT[usize::from(rank)], error })?;
                written += 1;
            }
        }
        Ok(written)
    }
}
