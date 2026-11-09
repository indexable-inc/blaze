//! Every file the load reads, through the database (docs/v2/plan.md section 2).
//!
//! - A path, absolute as cargo names it, is read through its longest [`Mount`]: the view's
//!   files (`Cx::file`) or another root's (`Cx::file_at`). [`LoadFile`] and [`LoadDir`] are one
//!   query per path, so a mount added for a fetched source reruns them alone and cuts off.
//! - A path under no mount is never read: it is [`Found::Unmounted`], which the load turns into
//!   a fetch: a registry's or git's by what names it, any other a
//!   [`crate::query::FetchKind::Local`] the daemon reads from the disk where it is (a `path`
//!   dependency outside the checkout), mounts as a root keyed by its bytes, and asks again.
//!   Outside every mount only the files the client sent are (`Invocation::files`), and a file
//!   of a directory above the view or the cwd, or of `$CARGO_HOME`, that it did not send is
//!   absent.
//! - blaze-manifest reads through a [`Snap`], which [`read_through`] makes over the step's
//!   `Cx`: each file is asked of the database when blaze-manifest asks for it, once, and its
//!   bytes are lent from where the database keeps them. A TOML file is read as its
//!   [`TomlFile`], its canonical form, so a comment edit cuts off there, and a manifest is
//!   parsed once, by its [`super::manifest::ManifestQuery`], whichever step reads it.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::marker::PhantomData;
use std::path::{Component, Path};

use blaze_format::{FormatError, Persist, SchemaHash, Slice, Writer};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_manifest::files::{FileKind, Files};
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use cargo_util_schemas::manifest::TomlManifest;
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use std::alloc::Global;

use super::config::ConfigOf;
use super::invocation::Invocation;
use super::key_id;
use super::Unread;
use crate::query::{Cbor, Fetch, FetchKind, Mount, Origin, Versioned, producer};

/// What a path is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Found<T> {
    /// Under no mount of the view: not fetched yet.
    Unmounted,
    Absent,
    Here(T),
}

/// [`Found`] as written: a file's bytes are one CBOR byte string, not an array of integers.
#[derive(Serialize, Deserialize)]
enum Written<T> {
    Unmounted,
    Absent,
    Here(T),
}

impl<T> Found<T> {
    fn written<'a, W>(&'a self, how: impl FnOnce(&'a T) -> W) -> Written<W> {
        match self {
            Found::Unmounted => Written::Unmounted,
            Found::Absent => Written::Absent,
            Found::Here(it) => Written::Here(how(it)),
        }
    }

    fn read<W>(written: Written<W>, how: impl FnOnce(W) -> T) -> Found<T> {
        match written {
            Written::Unmounted => Found::Unmounted,
            Written::Absent => Found::Absent,
            Written::Here(it) => Found::Here(how(it)),
        }
    }
}

impl Serialize for Found<Vec<u8>> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        self.written(|bytes| Raw(Cow::Borrowed(bytes))).serialize(to)
    }
}

impl<'de> Deserialize<'de> for Found<Vec<u8>> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        Ok(Found::read(Written::<Raw<'de>>::deserialize(from)?, |raw| raw.0.into_owned()))
    }
}

impl Serialize for Found<Names> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        self.written(|names| names).serialize(to)
    }
}

impl<'de> Deserialize<'de> for Found<Names> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        Ok(Found::read(Written::<Names>::deserialize(from)?, |names| names))
    }
}

/// Bytes written as one CBOR byte string and read in place where the decoder can.
pub(crate) struct Raw<'a>(pub Cow<'a, [u8]>);

impl Serialize for Raw<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(&self.0)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Raw<'a> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Self, D::Error> {
        struct Bytes<'a>(PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for Bytes<'a> {
            type Value = Raw<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_borrowed_bytes<E: de::Error>(self, bytes: &'de [u8]) -> Result<Raw<'a>, E> {
                Ok(Raw(Cow::Borrowed(bytes)))
            }
            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Raw<'a>, E> {
                Ok(Raw(Cow::Owned(bytes.to_vec())))
            }
            fn visit_byte_buf<E: de::Error>(self, bytes: Vec<u8>) -> Result<Raw<'a>, E> {
                Ok(Raw(Cow::Owned(bytes)))
            }
        }
        from.deserialize_bytes(Bytes(PhantomData))
    }
}

impl Versioned for Found<Vec<u8>> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo found file 2");
}

impl Versioned for Found<Names> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo found dir 2");
}

/// Where every directory cargo names is, and what the client sent of the rest. `mounts` and
/// `outside` are sorted by path, so a lookup is a binary search per ancestor.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct MountTable {
    pub mounts: Vec<Mount>,
    /// The files outside every mount the client sent, by absolute path.
    #[serde(with = "outside_bytes")]
    pub outside: Vec<(String, Vec<u8>)>,
    /// The directories outside every mount whose files cargo looks for (the view's and the
    /// cwd's ancestors, `$CARGO_HOME`): a file of one the client did not send is absent.
    pub known: Vec<String>,
}

impl Versioned for MountTable {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo mount table 3");
}

mod outside_bytes {
    use std::borrow::Cow;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Raw;

    pub fn serialize<S: Serializer>(files: &[(String, Vec<u8>)], to: S) -> Result<S::Ok, S::Error> {
        to.collect_seq(files.iter().map(|(at, bytes)| (at, Raw(Cow::Borrowed(bytes)))))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        from: D,
    ) -> Result<Vec<(String, Vec<u8>)>, D::Error> {
        let files = Vec::<(String, Raw<'de>)>::deserialize(from)?;
        Ok(files.into_iter().map(|(at, raw)| (at, raw.0.into_owned())).collect())
    }
}

/// A mount table as a read looks a path up in it: three columns sorted by path, read by index.
/// The view's is a [`MountRef`] over the bytes its [`Mounts`] query keeps; a fixed
/// configuration's is its own `&MountTable`.
trait Table<'a>: Copy {
    fn mounts(self) -> usize;
    /// Mount `at`'s path, as bytes: what a search compares.
    fn mount_at(self, at: usize) -> &'a [u8];
    /// Mount `at`: its path, origin and path under the origin's root.
    fn mount(self, at: usize) -> (&'a str, Origin, &'a str);
    fn outsides(self) -> usize;
    fn outside_at(self, at: usize) -> &'a [u8];
    fn outside(self, at: usize) -> &'a [u8];
    fn knowns(self) -> usize;
    fn known(self, at: usize) -> &'a [u8];
}

impl<'a> Table<'a> for &'a MountTable {
    fn mounts(self) -> usize {
        self.mounts.len()
    }

    fn mount_at(self, at: usize) -> &'a [u8] {
        self.mounts[at].at.as_bytes()
    }

    fn mount(self, at: usize) -> (&'a str, Origin, &'a str) {
        let mount = &self.mounts[at];
        (&mount.at, mount.origin, &mount.under)
    }

    fn outsides(self) -> usize {
        self.outside.len()
    }

    fn outside_at(self, at: usize) -> &'a [u8] {
        self.outside[at].0.as_bytes()
    }

    fn outside(self, at: usize) -> &'a [u8] {
        &self.outside[at].1
    }

    fn knowns(self) -> usize {
        self.known.len()
    }

    fn known(self, at: usize) -> &'a [u8] {
        self.known[at].as_bytes()
    }
}

/// The first of `0..len` for which `before` is false; `before` holds for a prefix of them.
fn partition(len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (0, len);
    while low < high {
        let mid = low + (high - low) / 2;
        match before(mid) {
            true => low = mid + 1,
            false => high = mid,
        }
    }
    low
}

/// The index of `path` in a sorted column of `len` paths, `at` reading each.
fn search<'a>(len: usize, path: &[u8], at: impl Fn(usize) -> &'a [u8]) -> Option<usize> {
    let found = partition(len, |it| at(it) < path);
    (found < len && at(found) == path).then_some(found)
}

/// The view's [`MountTable`] as its [`Mounts`] query keeps it, laid out so that a read finds a
/// path by binary search where the bytes lie: nothing decoded or allocated, whatever the number
/// of sources and index files mounted. All words are little-endian `u32`s:
///
/// - the head: how many mounts, files outside and known directories there are;
/// - each mount ([`MOUNT`] bytes): its path and the path under its root as (offset, length) of
///   the texts, its origin's tag (0 the view, 1 a root), a zero word, and the root's 32 bytes
///   (zeros for the view);
/// - each file outside: its path and its bytes as (offset, length);
/// - each known directory: its path as (offset, length);
/// - the texts and bytes, in that order; offsets count from the first byte of the head.
pub(crate) struct MountIndex(Vec<u8>);

const HEAD: usize = 12;
const MOUNT: usize = 56;
const OUTSIDE: usize = 16;
const KNOWN: usize = 8;

impl MountIndex {
    pub(crate) fn of(table: &MountTable) -> MountIndex {
        fn word(bytes: &mut Vec<u8>, value: usize) {
            let value = u32::try_from(value).expect("a mount table under 4 GiB");
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let (mounts, outside, known) = (&table.mounts, &table.outside, &table.known);
        let fixed = HEAD + mounts.len() * MOUNT + outside.len() * OUTSIDE + known.len() * KNOWN;
        let texts = mounts.iter().map(|it| it.at.len() + it.under.len()).sum::<usize>()
            + outside.iter().map(|(at, bytes)| at.len() + bytes.len()).sum::<usize>()
            + known.iter().map(String::len).sum::<usize>();
        let mut bytes = Vec::with_capacity(fixed + texts);
        word(&mut bytes, mounts.len());
        word(&mut bytes, outside.len());
        word(&mut bytes, known.len());
        let mut next = fixed;
        let mut span = |bytes: &mut Vec<u8>, len: usize| {
            word(bytes, next);
            word(bytes, len);
            next += len;
        };
        for mount in mounts {
            span(&mut bytes, mount.at.len());
            span(&mut bytes, mount.under.len());
            let (tag, root) = match mount.origin {
                Origin::View => (0, [0; 32]),
                Origin::Root(root) => (1, root.0),
            };
            word(&mut bytes, tag);
            word(&mut bytes, 0);
            bytes.extend_from_slice(&root);
        }
        for (at, file) in outside {
            span(&mut bytes, at.len());
            span(&mut bytes, file.len());
        }
        for dir in known {
            span(&mut bytes, dir.len());
        }
        debug_assert_eq!(bytes.len(), fixed);
        for mount in mounts {
            bytes.extend_from_slice(mount.at.as_bytes());
            bytes.extend_from_slice(mount.under.as_bytes());
        }
        for (at, file) in outside {
            bytes.extend_from_slice(at.as_bytes());
            bytes.extend_from_slice(file);
        }
        for dir in known {
            bytes.extend_from_slice(dir.as_bytes());
        }
        MountIndex(bytes)
    }
}

impl Persist for MountIndex {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo mount index 1");
    type Ref<'a> = MountRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let data = out.push_slice::<u8>(&self.0);
        let at = out.next::<Slice<u8>>();
        out.push(&Slice::<u8>::between(at, data, self.0.len()))
    }

    fn read(buffer: &[u8]) -> Result<MountRef<'_>, FormatError> {
        let slice = blaze_format::open_as::<Slice<u8>>(buffer, MountIndex::SCHEMA)?;
        MountRef::new(slice.follow(buffer)?)
    }
}

/// A [`MountIndex`] read in place. Its head is checked when it is opened; a span past the end
/// or a path that is not UTF-8 is a bug of the writer, which only [`MountIndex::of`] is, and
/// panics naming it, as [`crate::query::CborRef::get`] does.
#[derive(Clone, Copy)]
pub(crate) struct MountRef<'a> {
    bytes: &'a [u8],
    mounts: usize,
    outside: usize,
    known: usize,
}

impl<'a> MountRef<'a> {
    fn new(bytes: &'a [u8]) -> Result<MountRef<'a>, FormatError> {
        let short = FormatError::Invalid { at: 0, what: "a mount index shorter than its head" };
        let count = |at: usize| {
            bytes
                .get(at..at + 4)
                .map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]) as usize)
        };
        let (Some(mounts), Some(outside), Some(known)) = (count(0), count(4), count(8)) else {
            return Err(short);
        };
        let fixed = HEAD + mounts * MOUNT + outside * OUTSIDE + known * KNOWN;
        if fixed > bytes.len() {
            return Err(short);
        }
        Ok(MountRef { bytes, mounts, outside, known })
    }

    fn word(self, at: usize) -> usize {
        let it = &self.bytes[at..at + 4];
        u32::from_le_bytes([it[0], it[1], it[2], it[3]]) as usize
    }

    /// The bytes of the (offset, length) at `at`.
    fn span(self, at: usize) -> &'a [u8] {
        let bytes: &'a [u8] = self.bytes;
        let (start, len) = (self.word(at), self.word(at + 4));
        bytes
            .get(start..start + len)
            .unwrap_or_else(|| panic!("a kept mount index names bytes past its end"))
    }

    fn text(self, at: usize) -> &'a str {
        std::str::from_utf8(self.span(at))
            .unwrap_or_else(|_| panic!("a kept mount index has a path that is not UTF-8"))
    }

    fn mount_record(self, at: usize) -> usize {
        HEAD + at * MOUNT
    }

    fn outside_record(self, at: usize) -> usize {
        HEAD + self.mounts * MOUNT + at * OUTSIDE
    }

    fn known_record(self, at: usize) -> usize {
        HEAD + self.mounts * MOUNT + self.outside * OUTSIDE + at * KNOWN
    }

    /// The mount at `at` exactly, owned.
    pub(crate) fn get(self, at: &str) -> Option<Mount> {
        let found = search(self.mounts, at.as_bytes(), |it| self.mount_at(it))?;
        let (path, origin, under) = self.mount(found);
        Some(Mount { at: path.to_owned(), origin, under: under.to_owned() })
    }
}

impl<'a> Table<'a> for MountRef<'a> {
    fn mounts(self) -> usize {
        self.mounts
    }

    fn mount_at(self, at: usize) -> &'a [u8] {
        self.span(self.mount_record(at))
    }

    fn mount(self, at: usize) -> (&'a str, Origin, &'a str) {
        let record = self.mount_record(at);
        let origin = match self.word(record + 16) {
            0 => Origin::View,
            _ => {
                let bytes: &'a [u8] = self.bytes;
                let mut root = [0; 32];
                root.copy_from_slice(&bytes[record + 24..record + MOUNT]);
                Origin::Root(ContentId(root))
            }
        };
        (self.text(record), origin, self.text(record + 8))
    }

    fn outsides(self) -> usize {
        self.outside
    }

    fn outside_at(self, at: usize) -> &'a [u8] {
        self.span(self.outside_record(at))
    }

    fn outside(self, at: usize) -> &'a [u8] {
        self.span(self.outside_record(at) + 8)
    }

    fn knowns(self) -> usize {
        self.known
    }

    fn known(self, at: usize) -> &'a [u8] {
        self.span(self.known_record(at))
    }
}

/// The view's [`MountTable`]: of the invocation, the mounts alone, so a change to the command
/// line reruns no read.
pub(crate) struct Mounts;

impl Query for Mounts {
    const KIND: QueryKind = QueryKind(0x6012);
    const NAME: &'static str = "cargo mounts";
    const PRODUCER: ContentId = producer("blaze v2 cargo mounts 2");
    type Key = ();
    type Value = MountIndex;

    fn input(key: &()) -> ContentId {
        key_id("blaze v2 cargo mounts key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<MountIndex, Cancelled> {
        let configured = cx.get::<Invocation>(&())?.get();
        Ok(MountIndex::of(&configured.mounts))
    }
}

/// A file by absolute path.
pub(crate) struct LoadFile;

impl Query for LoadFile {
    const KIND: QueryKind = QueryKind(0x6013);
    const NAME: &'static str = "cargo file";
    const PRODUCER: ContentId = producer("blaze v2 cargo file 1");
    type Key = String;
    type Value = Cbor<Found<Vec<u8>>>;

    fn input(key: &String) -> ContentId {
        key_id("blaze v2 cargo file key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, path: &String) -> Result<Cbor<Found<Vec<u8>>>, Cancelled> {
        let table = cx.get::<Mounts>(&())?;
        Ok(Cbor(file_through(cx, table, path, true)?))
    }
}

fn file_through<'a>(
    cx: &mut Cx<'_>,
    table: impl Table<'a>,
    path: &'a str,
    view: bool,
) -> Result<Found<Vec<u8>>, Cancelled> {
    Ok(match locate(table, path) {
        Some((Origin::View, under)) if view => match cx.file(&under)? {
            Some(file) => Found::Here(file.bytes.to_vec()),
            None => Found::Absent,
        },
        Some((Origin::View, _)) => Found::Unmounted,
        Some((Origin::Root(root), under)) => match cx.file_at(root, &under)? {
            Some(file) => Found::Here(file.bytes.to_vec()),
            None => Found::Absent,
        },
        None => match search(table.outsides(), path.as_bytes(), |at| table.outside_at(at)) {
            Some(at) => Found::Here(table.outside(at).to_vec()),
            None if known_file(table, path) => Found::Absent,
            None => Found::Unmounted,
        },
    })
}

/// A directory's names by absolute path.
pub(crate) struct LoadDir;

impl Query for LoadDir {
    const KIND: QueryKind = QueryKind(0x6014);
    const NAME: &'static str = "cargo dir";
    const PRODUCER: ContentId = producer("blaze v2 cargo dir 1");
    type Key = String;
    type Value = Cbor<Found<Vec<(String, bool)>>>;

    fn input(key: &String) -> ContentId {
        key_id("blaze v2 cargo dir key 1", key)
    }

    fn execute(
        cx: &mut Cx<'_>,
        path: &String,
    ) -> Result<Cbor<Found<Vec<(String, bool)>>>, Cancelled> {
        let table = cx.get::<Mounts>(&())?;
        Ok(Cbor(dir_through(cx, table, path, true)?))
    }
}

type Names = Vec<(String, bool)>;

/// A listing sorted by name, so [`Snap::kind`] finds a name by binary search; the tree's are
/// already, which the sort sees in one pass.
fn sorted(names: &[(String, bool)]) -> Found<Names> {
    let mut names = names.to_vec();
    names.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    Found::Here(names)
}

fn dir_through<'a>(
    cx: &mut Cx<'_>,
    table: impl Table<'a>,
    path: &'a str,
    view: bool,
) -> Result<Found<Names>, Cancelled> {
    Ok(match locate(table, path) {
        Some((Origin::View, under)) if view => match cx.dir(&under)? {
            Some(names) => sorted(names),
            None => Found::Absent,
        },
        Some((Origin::View, _)) => Found::Unmounted,
        Some((Origin::Root(root), under)) => match cx.dir_at(root, &under)? {
            Some(names) => sorted(names),
            None => Found::Absent,
        },
        None if known_dir(table, path) => {
            let prefix: Cow<'_, str> =
                if path == "/" { Cow::Borrowed("/") } else { Cow::Owned(format!("{path}/")) };
            let prefix = prefix.as_bytes();
            let mut names: HashMap<&str, bool, Global> = HashMap::new_in(Global);
            let first = partition(table.mounts(), |at| table.mount_at(at) < prefix);
            for at in
                (first..table.mounts()).take_while(|&at| table.mount_at(at).starts_with(prefix))
            {
                let (mount_at, origin, mount_under) = table.mount(at);
                let rest = &mount_at[prefix.len()..];
                match rest.split_once('/') {
                    Some((dir, _)) => names.insert(dir, true),
                    // A mount of one file (a sparse index's) is a file of the directory.
                    None => {
                        let dir = match origin {
                            Origin::View if view => cx.file(mount_under)?.is_none(),
                            Origin::View => continue,
                            Origin::Root(root) => cx.file_at(root, mount_under)?.is_none(),
                        };
                        names.insert(rest, dir)
                    }
                };
            }
            let first = partition(table.outsides(), |at| table.outside_at(at) < prefix);
            for at in
                (first..table.outsides()).take_while(|&at| table.outside_at(at).starts_with(prefix))
            {
                let Ok(rest) = std::str::from_utf8(&table.outside_at(at)[prefix.len()..]) else {
                    continue;
                };
                match rest.split_once('/') {
                    Some((dir, _)) => names.insert(dir, true),
                    None => names.insert(rest, false),
                };
            }
            let mut names: Names =
                names.into_iter().map(|(name, dir)| (name.to_owned(), dir)).collect();
            names.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
            Found::Here(names)
        }
        None => Found::Unmounted,
    })
}

/// A path read through a [`MountTable`] of its own, never the view's: what a
/// [`super::config::ConfigOf::Fixed`] configuration reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct FixedPath {
    pub table: MountTable,
    pub path: String,
}

/// A file of a [`FixedPath`]: a mount of the view's own files is under no mount for it.
pub(crate) struct FixedFile;

impl Query for FixedFile {
    const KIND: QueryKind = QueryKind(0x6018);
    const NAME: &'static str = "cargo fixed file";
    const PRODUCER: ContentId = producer("blaze v2 cargo fixed file 1");
    type Key = FixedPath;
    type Value = Cbor<Found<Vec<u8>>>;

    fn input(key: &FixedPath) -> ContentId {
        key_id("blaze v2 cargo fixed file key 2", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &FixedPath) -> Result<Cbor<Found<Vec<u8>>>, Cancelled> {
        Ok(Cbor(file_through(cx, &key.table, &key.path, false)?))
    }
}

pub(crate) struct FixedDir;

impl Query for FixedDir {
    const KIND: QueryKind = QueryKind(0x601d);
    const NAME: &'static str = "cargo fixed dir";
    const PRODUCER: ContentId = producer("blaze v2 cargo fixed dir 1");
    type Key = FixedPath;
    type Value = Cbor<Found<Names>>;

    fn input(key: &FixedPath) -> ContentId {
        key_id("blaze v2 cargo fixed dir key 2", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &FixedPath) -> Result<Cbor<Found<Names>>, Cancelled> {
        Ok(Cbor(dir_through(cx, &key.table, &key.path, false)?))
    }
}

/// The file at `path` (absolute, as cargo names it) through the view's mounts.
pub(crate) fn read_file(cx: &mut Cx<'_>, path: &str) -> Result<Found<Vec<u8>>, Cancelled> {
    Ok(cx.get::<LoadFile>(&path.to_owned())?.get())
}

/// The names in the directory at `path`, sorted, each with whether it is a directory.
pub(crate) fn read_dir(
    cx: &mut Cx<'_>,
    path: &str,
) -> Result<Found<Vec<(String, bool)>>, Cancelled> {
    Ok(cx.get::<LoadDir>(&path.to_owned())?.get())
}

/// `path`'s longest mount: its origin and the path under the origin's root. The mounts are
/// sorted by `at`, so the longest is the first of `path` and the directories above it, nearest
/// first, that one is at: a binary search each, and nothing allocated unless the mount is
/// under a directory of its root.
fn locate<'a>(table: impl Table<'a>, path: &'a str) -> Option<(Origin, Cow<'a, str>)> {
    let mut at = path;
    loop {
        if let Some(found) = search(table.mounts(), at.as_bytes(), |it| table.mount_at(it)) {
            let (mount_at, origin, mount_under) = table.mount(found);
            let rest = under(mount_at, path)?;
            let joined = match (mount_under, rest) {
                ("", rest) => Cow::Borrowed(rest),
                (under, "") => Cow::Borrowed(under),
                (under, rest) => Cow::Owned(format!("{under}/{rest}")),
            };
            return Some((origin, joined));
        }
        at = match at.rsplit_once('/') {
            Some(("", _)) if at != "/" => "/",
            Some((above, _)) if !above.is_empty() => above,
            _ if !at.is_empty() => "",
            _ => return None,
        };
    }
}

/// What is left of `path` under `at`: "" for `at` itself.
fn under<'p>(at: &str, path: &'p str) -> Option<&'p str> {
    if at == "/" || at.is_empty() {
        return path.strip_prefix('/');
    }
    let rest = path.strip_prefix(at)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

/// Whether `path` is a known directory or a known directory's `.cargo`, the directories whose
/// files [`known_file`] takes: `known` is sorted.
fn known_dir<'a>(table: impl Table<'a>, path: &str) -> bool {
    let known = |dir: &str| search(table.knowns(), dir.as_bytes(), |at| table.known(at)).is_some();
    if known(path) {
        return true;
    }
    match path.rsplit_once('/') {
        Some((above, ".cargo")) => known(if above.is_empty() { "/" } else { above }),
        _ => false,
    }
}

/// Whether `path` is a file of a known directory, or of a known directory's `.cargo`: `known`
/// is sorted.
fn known_file<'a>(table: impl Table<'a>, path: &str) -> bool {
    let Some((dir, _)) = path.rsplit_once('/') else { return false };
    known_dir(table, if dir.is_empty() { "/" } else { dir })
}

/// `path` as the load names it: absolute, `/`-separated, `.` and `..` taken out. `None` for a
/// path that is not absolute or not UTF-8.
pub(crate) fn normal(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut absolute = false;
    // On Windows a host path's drive is the first part, as the daemon names it: `C:\a` is `/C:/a`.
    let mut drive = None;
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::Prefix(prefix) => match prefix.kind() {
                std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) if cfg!(windows) => {
                    drive = Some(char::from(letter.to_ascii_uppercase()));
                }
                _ => return None,
            },
        }
    }
    if !absolute {
        return None;
    }
    match drive {
        Some(letter) => Some(format!("/{letter}:/{}", parts.join("/"))),
        None => Some(format!("/{}", parts.join("/"))),
    }
}

/// An absolute `/`-separated path without `.`, `..` or empty parts: `/a/b`, or `C:/a/b` with
/// its drive kept. A `..` at the root stays at the root.
pub fn normal_text(path: &str) -> String {
    let (root, rest) = match path.strip_prefix('/') {
        Some(rest) => ("", rest),
        None => path.split_once('/').unwrap_or((path, "")),
    };
    let mut out = String::with_capacity(path.len() + 1);
    out.push_str(root);
    let base = out.len();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let cut = out[base..].rfind('/').map_or(base, |at| base + at);
                out.truncate(cut);
            }
            part => {
                out.push('/');
                out.push_str(part);
            }
        }
    }
    if out.len() == base {
        out.push('/');
    }
    out
}

#[cfg(test)]
mod normal_text_tests {
    use super::normal_text;

    /// The `normalize` of load-cargo's v2 `files.rs` (deleted 2026-10-30), kept to check the
    /// one that replaced it and blaze-units' `load/source/tree.rs` copy.
    fn load_cargo(path: &str) -> String {
        let (root, rest) = match path.strip_prefix('/') {
            Some(rest) => ("", rest),
            None => path.split_once('/').unwrap_or((path, "")),
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in rest.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        format!("{root}/{}", parts.join("/"))
    }

    /// `load/source/tree.rs`' `normalize`, which took absolute view paths only.
    fn tree(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        format!("/{}", parts.join("/"))
    }

    /// Every path of up to five of these parts: absolute, with a drive, and neither.
    #[test]
    fn a_path_is_normal_as_both_old_copies_made_it() {
        let parts = ["", ".", "..", "a", "bc", "C:"];
        let mut paths = vec![String::new()];
        let mut last = vec![String::new()];
        for _ in 0..5 {
            last = last
                .iter()
                .flat_map(|it| parts.iter().map(move |part| format!("{it}/{part}")))
                .collect();
            paths.extend(last.iter().cloned());
        }
        let more: Vec<String> = paths
            .iter()
            .map(|it| format!("C:{it}"))
            .chain(paths.iter().map(|it| format!("a{it}")))
            .collect();
        paths.extend(more);
        for path in &paths {
            let new = normal_text(path);
            assert_eq!(new, load_cargo(path), "`{path}`");
            if path.starts_with('/') {
                assert_eq!(new, tree(path), "`{path}`");
            }
        }
        assert_eq!(normal_text("/a/./b/../c"), "/a/c");
        assert_eq!(normal_text("C:/a/../.."), "C:/");
        assert_eq!(normal_text("/"), "/");
    }
}

/// [`normal`], borrowing a path that already is normal: the paths blaze-manifest asks for are.
fn normal_ref(path: &Path) -> Option<Cow<'_, str>> {
    // On Windows `Path::join` puts a `\` in the daemon's `/C:/a`: `normal` takes it out.
    if let Some(text) = path.to_str()
        && !(cfg!(windows) && text.contains('\\'))
        && (text == "/"
            || (text.starts_with('/')
                && text[1..].split('/').all(|it| !matches!(it, "" | "." | ".."))))
    {
        return Some(Cow::Borrowed(text));
    }
    normal(path).map(Cow::Owned)
}

/// The directories above `path`, nearest first, `/` last.
pub(crate) fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = path;
    while let Some((above, _)) = at.rsplit_once('/') {
        let above = if above.is_empty() { "/" } else { above };
        out.push(above.to_owned());
        if above == "/" {
            break;
        }
        at = above;
    }
    out
}

/// Whether `path` is read as TOML: a manifest or a config file, kept in its canonical form.
fn is_toml(path: &str) -> bool {
    path.ends_with(".toml") || path.ends_with("/.cargo/config")
}

fn canonical_toml(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let table: toml::Table = toml::from_str(text).ok()?;
    let again = toml::to_string(&table).ok()?;
    if again.as_bytes() == bytes {
        return Some(again);
    }
    // Kept only when it reads back as the same table.
    (toml::from_str::<toml::Table>(&again).ok()? == table).then_some(again)
}

/// A path read with a configuration: the key of a [`TomlFile`] and of a
/// [`super::manifest::ManifestQuery`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct PathKey {
    pub path: String,
    pub config: ConfigOf,
}

/// A TOML file (a manifest, a config file) through its configuration's mounts, in its
/// canonical form: its table written again, so an edit to a comment or the layout cuts off
/// here. A file that does not parse is kept as it is, for its error.
pub(crate) struct TomlFile;

impl Query for TomlFile {
    const KIND: QueryKind = QueryKind(0x601e);
    const NAME: &'static str = "cargo toml file";
    const PRODUCER: ContentId = producer("blaze v2 cargo toml file 1");
    type Key = PathKey;
    type Value = Cbor<Found<Vec<u8>>>;

    fn input(key: &PathKey) -> ContentId {
        key_id("blaze v2 cargo toml file key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &PathKey) -> Result<Cbor<Found<Vec<u8>>>, Cancelled> {
        Ok(Cbor(match key.config.read_file(cx, &key.path)? {
            Found::Here(bytes) => {
                Found::Here(canonical_toml(&bytes).map_or(bytes, String::into_bytes))
            }
            other => other,
        }))
    }
}

/// A kept [`Found<Vec<u8>>`] read in place: the file's bytes are the database's.
pub(crate) fn lent(kept: &[u8]) -> Found<&[u8]> {
    let written = blaze_protocol::cbor::from_slice::<Written<Raw<'_>>>(kept)
        .unwrap_or_else(|error| panic!("a kept file does not decode: {error}"));
    Found::read(written, |raw| match raw.0 {
        Cow::Borrowed(bytes) => bytes,
        Cow::Owned(_) => panic!("a kept file's bytes were copied out of the database"),
    })
}

/// A kept listing read in place: its names are the database's.
fn lent_names(kept: &[u8]) -> Found<Vec<(&str, bool)>> {
    let written = blaze_protocol::cbor::from_slice::<Written<Vec<(&str, bool)>>>(kept)
        .unwrap_or_else(|error| panic!("a kept listing does not decode: {error}"));
    Found::read(written, |names| names)
}

/// What a step reads through: the step's `Cx`, lent to each read, and the keys a read asks by,
/// kept so that a read of a file builds none.
pub(crate) struct Reader<'r, 'db> {
    cx: RefCell<&'r mut Cx<'db>>,
    /// The key of a read through the view's mounts ([`LoadFile`], [`LoadDir`]).
    path: RefCell<String>,
    /// The key of a read through a fixed configuration's mounts ([`FixedFile`], [`FixedDir`]),
    /// its table made once.
    fixed: Option<RefCell<FixedPath>>,
    /// The key of a [`TomlFile`] or a manifest.
    keyed: RefCell<PathKey>,
    /// Each listing read so far, decoded once.
    dirs: RefCell<HashMap<String, Found<Vec<(&'db str, bool)>>, Global>>,
    /// A read was cancelled: every read after it is too, without asking.
    cancelled: Cell<bool>,
    /// Every path read that is under no mount, in the order read, each with whether it was
    /// only asked whether it is there ([`Snap::kind`]), never read.
    unmounted: RefCell<Vec<(String, bool)>>,
    /// A [`Snap::kind`] is asking: what it finds under no mount is a probe.
    probing: Cell<bool>,
}

impl<'r, 'db> Reader<'r, 'db> {
    fn new(cx: &'r mut Cx<'db>, config: &ConfigOf) -> Reader<'r, 'db> {
        let fixed = match config {
            ConfigOf::View => None,
            ConfigOf::Fixed { mounts, cwd, env } => {
                // alloc: the fixed configuration's table, once a step.
                Some(RefCell::new(FixedPath {
                    table: ConfigOf::table(mounts, cwd, env),
                    path: String::new(),
                }))
            }
        };
        Reader {
            cx: RefCell::new(cx),
            // alloc: grown to the longest path the step reads, then reused by every read.
            path: RefCell::new(String::new()),
            fixed,
            // clone: the configuration, once a step, into the key its reads reuse.
            keyed: RefCell::new(PathKey { path: String::new(), config: config.clone() }),
            dirs: RefCell::new(HashMap::new_in(Global)),
            cancelled: Cell::new(false),
            // alloc: none until a path is under no mount.
            unmounted: RefCell::new(Vec::new()),
            probing: Cell::new(false),
        }
    }

    /// `ask` of the step's `Cx`; `None` once a read was cancelled.
    fn ask<T>(&self, ask: impl FnOnce(&mut Cx<'db>) -> Result<T, Cancelled>) -> Option<T> {
        if self.cancelled.get() {
            return None;
        }
        let mut cx = self.cx.try_borrow_mut().unwrap_or_else(|_| {
            panic!("a file is read while the load's `Cx` is lent (`Snap::with_cx`)")
        });
        match ask(&mut cx) {
            Ok(it) => Some(it),
            Err(Cancelled) => {
                self.cancelled.set(true);
                None
            }
        }
    }

    fn saw_unmounted(&self, path: &str) {
        let probe = self.probing.get();
        let mut unmounted = self.unmounted.borrow_mut();
        match unmounted.iter_mut().find(|(it, _)| it == path) {
            // Read for its contents as well as probed: read.
            Some((_, probed)) => *probed &= probe,
            // alloc: a path the daemon is asked to mount, once each.
            None => unmounted.push((path.to_owned(), probe)),
        }
    }

    /// The file at `path` (normal), where the database keeps it: a TOML file its [`TomlFile`],
    /// any other as it is.
    fn file(&self, path: &str) -> Option<Found<&'db [u8]>> {
        if !is_toml(path) {
            return self.raw(path);
        }
        let mut key = self.keyed.borrow_mut();
        key.path.clear();
        key.path.push_str(path);
        let kept = self.ask(|cx| Ok(cx.get::<TomlFile>(&key)?.bytes()))?;
        Some(self.seen(path, lent(kept)))
    }

    /// The file at `path` (normal) as it is, a TOML file too: what an error's line and column
    /// are counted in.
    fn raw(&self, path: &str) -> Option<Found<&'db [u8]>> {
        let kept = match &self.fixed {
            None => {
                let mut key = self.path.borrow_mut();
                key.clear();
                key.push_str(path);
                self.ask(|cx| Ok(cx.get::<LoadFile>(&key)?.bytes()))?
            }
            Some(fixed) => {
                let mut key = fixed.borrow_mut();
                key.path.clear();
                key.path.push_str(path);
                self.ask(|cx| Ok(cx.get::<FixedFile>(&key)?.bytes()))?
            }
        };
        Some(self.seen(path, lent(kept)))
    }

    fn seen<T: PartialEq>(&self, path: &str, found: Found<T>) -> Found<T> {
        if found == Found::Unmounted {
            self.saw_unmounted(path);
        }
        found
    }

    /// `then` of the listing of the directory at `path` (normal), read once a step.
    fn dir<T>(
        &self,
        path: &str,
        then: impl FnOnce(&Found<Vec<(&'db str, bool)>>) -> T,
    ) -> Option<T> {
        self.listing(path, true, then)
    }

    /// [`Reader::dir`], where `record` says whether a listing under no mount makes the step's
    /// answer the error naming it: not when the caller answers without it as well as with it.
    fn listing<T>(
        &self,
        path: &str,
        record: bool,
        then: impl FnOnce(&Found<Vec<(&'db str, bool)>>) -> T,
    ) -> Option<T> {
        if let Some(found) = self.dirs.borrow().get(path) {
            if record && matches!(found, Found::Unmounted) {
                self.saw_unmounted(path);
            }
            return Some(then(found));
        }
        let kept = match &self.fixed {
            None => {
                let mut key = self.path.borrow_mut();
                key.clear();
                key.push_str(path);
                self.ask(|cx| Ok(cx.get::<LoadDir>(&key)?.bytes()))?
            }
            Some(fixed) => {
                let mut key = fixed.borrow_mut();
                key.path.clear();
                key.path.push_str(path);
                self.ask(|cx| Ok(cx.get::<FixedDir>(&key)?.bytes()))?
            }
        };
        let found = lent_names(kept);
        let found = if record { self.seen(path, found) } else { found };
        let answer = then(&found);
        // alloc: the listing's path, once a step; its names stay the database's.
        self.dirs.borrow_mut().insert(path.to_owned(), found);
        Some(answer)
    }

    /// The manifest at `path` (normal), as its [`super::manifest::ManifestQuery`] parsed it.
    fn manifest(&self, path: &str) -> Option<super::manifest::Parsed> {
        let mut key = self.keyed.borrow_mut();
        key.path.clear();
        key.path.push_str(path);
        // alloc: the manifest decoded, which blaze-manifest takes owned.
        self.ask(|cx| Ok(cx.get::<super::manifest::ManifestQuery>(&key)?.get()))
    }
}

/// The `Files` blaze-manifest reads through in a load query: each file read from the database
/// when it is asked for, through the step's `Cx` (so its query's edges are the files it read),
/// its bytes lent from where the database keeps them. A TOML file is its canonical form
/// ([`TomlFile`]) and a manifest is parsed once ([`super::manifest::ManifestQuery`]).
#[derive(Clone, Copy)]
pub struct Snap<'r, 'db>(&'r Reader<'r, 'db>);

impl fmt::Debug for Snap<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Snap")
    }
}

impl<'db> Snap<'_, 'db> {
    /// `f` of the step's `Cx`, for a query the step asks itself. No file is read while it has it.
    pub(crate) fn with_cx<T>(self, f: impl FnOnce(&mut Cx<'db>) -> T) -> T {
        let mut cx =
            self.0.cx.try_borrow_mut().unwrap_or_else(|_| panic!("the load's `Cx` is lent twice"));
        f(&mut cx)
    }
}

fn not_found() -> std::io::Error {
    std::io::Error::from_raw_os_error(2)
}

fn interrupted() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "the load was cancelled")
}

fn unmounted(path: &str) -> std::io::Error {
    // alloc: an error's message.
    std::io::Error::other(format!("`{path}` is under no mount of the view"))
}

impl Snap<'_, '_> {
    /// [`Files::kind`], with what it finds under no mount a probe.
    fn kind_of(&self, path: &Path) -> Option<FileKind> {
        let path = normal_ref(path)?;
        if let Some((parent, name)) = path.rsplit_once('/') {
            let parent = if parent.is_empty() { "/" } else { parent };
            // The parent's listing only saves the reads below: a parent under no mount (the
            // home directory above a known `$CARGO_HOME`) is no error of the load's.
            let listed = self.0.listing(parent, false, |found| match found {
                Found::Here(names) => Some(names.binary_search_by(|(it, _)| (*it).cmp(name)).ok().map(|at| match names[at].1 {
                    true => FileKind::Dir,
                    false => FileKind::File,
                })),
                Found::Absent | Found::Unmounted => None,
            })?;
            if let Some(kind) = listed {
                return kind;
            }
        }
        if self.0.dir(&path, |found| matches!(found, Found::Here(_)))? {
            return Some(FileKind::Dir);
        }
        match self.0.file(&path)? {
            Found::Here(_) => Some(FileKind::File),
            Found::Absent | Found::Unmounted => None,
        }
    }

    /// The manifest at `path` read as it is and parsed as blaze-manifest's own `manifest`
    /// does: what gives the error of one that is not there or does not parse, its line and
    /// column the file's, not its canonical form's.
    fn parse(&self, path: &Path) -> anyhow::Result<TomlManifest> {
        // alloc: an error's context, made only when the read fails.
        let failed = |error: std::io::Error| {
            anyhow::Error::new(error).context(format!("failed to read `{}`", path.display()))
        };
        let Some(normal) = normal_ref(path) else { return Err(failed(not_found())) };
        match self.0.raw(&normal) {
            Some(Found::Here(bytes)) => blaze_manifest::manifest::parse_manifest(path, bytes),
            Some(Found::Unmounted) => Err(failed(unmounted(&normal))),
            Some(Found::Absent) => Err(failed(not_found())),
            None => Err(failed(interrupted())),
        }
    }
}

impl Files for Snap<'_, '_> {
    fn read(&self, path: &Path) -> std::io::Result<Cow<'_, [u8]>> {
        let Some(path) = normal_ref(path) else { return Err(not_found()) };
        match self.0.file(&path) {
            Some(Found::Here(bytes)) => Ok(Cow::Borrowed(bytes)),
            Some(Found::Unmounted) => Err(unmounted(&path)),
            Some(Found::Absent) => Err(not_found()),
            None => Err(interrupted()),
        }
    }

    /// The manifest's [`super::manifest::ManifestQuery`]: parsed once, whichever step reads it.
    /// One that is not there or does not parse is read again here for its error.
    fn manifest(&self, path: &Path) -> anyhow::Result<TomlManifest> {
        use super::manifest::Parsed;
        let Some(normal) = normal_ref(path) else { return self.parse(path) };
        match self.0.manifest(&normal) {
            Some(Parsed::Here(manifest)) => Ok(manifest),
            Some(Parsed::Absent | Parsed::Unmounted | Parsed::Unparsed) | None => {
                self.parse(path)?;
                anyhow::bail!("`{}` parses as it is but not in its manifest query", path.display())
            }
        }
    }

    /// From the parent directory's names, so an edit to a file's contents is never read by
    /// asking whether it is there; a path its parent's names don't answer for (a mount's own
    /// directory) is asked of itself. A listing is sorted by name.
    fn kind(&self, path: &Path) -> Option<FileKind> {
        let probing = self.0.probing.replace(true);
        let kind = self.kind_of(path);
        self.0.probing.set(probing);
        kind
    }

    fn list(&self, path: &Path) -> std::io::Result<Vec<(String, bool)>> {
        let Some(path) = normal_ref(path) else { return Err(not_found()) };
        let listed = self.0.dir(&path, |found| match found {
            // alloc: the names, which the trait returns owned.
            Found::Here(names) => {
                Ok(names.iter().map(|(name, dir)| ((*name).to_owned(), *dir)).collect())
            }
            Found::Unmounted => Err(unmounted(&path)),
            Found::Absent => Err(not_found()),
        });
        listed.unwrap_or_else(|| Err(interrupted()))
    }

    /// A view has no links, so a path that is there is its own canonical path.
    fn canonicalize(&self, path: &Path) -> std::io::Result<std::path::PathBuf> {
        match self.kind(path) {
            Some(_) => {
                Ok(std::path::PathBuf::from(normal_ref(path).ok_or_else(not_found)?.as_ref()))
            }
            None => Err(not_found()),
        }
    }
}

/// Runs `step` once over a [`Snap`] of `cx`, reading through `config`'s mounts. The paths it
/// read that are under no mount make its answer [`Unread::Needs`] of each, sorted, as a
/// [`FetchKind::Local`]: the daemon reads them from the disk, mounts them and asks again. A
/// read cancelled while it ran cancels it, whatever it answered.
pub(crate) fn read_through<'db, R>(
    cx: &mut Cx<'db>,
    config: &ConfigOf,
    step: impl FnOnce(Snap<'_, 'db>) -> Result<R, Cancelled>,
) -> Result<Result<R, Unread>, Cancelled> {
    let reader = Reader::new(cx, config);
    let answer = step(Snap(&reader));
    if reader.cancelled.get() {
        return Err(Cancelled);
    }
    let answer = answer?;
    let mut unmounted = reader.unmounted.take();
    if unmounted.is_empty() {
        return Ok(Ok(answer));
    }
    unmounted.sort_unstable();
    Ok(Err(Unread::Needs(unmounted.into_iter().map(|(path, probe)| local_fetch(path, probe)).collect())))
}

/// The fetch of a path under no mount: read from the disk where it is ([`FetchKind::Local`]),
/// `probe` when it was only asked whether it is there.
pub(crate) fn local_fetch(path: String, probe: bool) -> Fetch {
    // clone: the path names the fetch, its place and where it is read from.
    Fetch { what: path.clone(), url: format!("file://{path}"), kind: FetchKind::Local { probe }, at: path }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(at: &str, origin: Origin, under: &str) -> Mount {
        Mount { at: at.to_owned(), origin, under: under.to_owned() }
    }

    #[test]
    fn the_longest_mount_is_a_paths() {
        let lib = Origin::Root(ContentId([1; 32]));
        let mut mounts = vec![
            mount("/w2", Origin::View, "other"),
            mount("/w", Origin::View, ""),
            mount("/w/vendor/x", lib, "x-1.0"),
        ];
        mounts.sort();
        let table = MountTable { mounts, outside: Vec::new(), known: Vec::new() };
        longest_mounts("the table", &table, lib);
        let kept = kept(&table);
        longest_mounts("its index", <MountIndex as Persist>::read(&kept).unwrap(), lib);

        let root = MountTable {
            mounts: vec![mount("/", lib, "r")],
            outside: Vec::new(),
            known: Vec::new(),
        };
        assert_eq!(locate(&root, "/a/b").map(|(_, it)| it.into_owned()), Some("r/a/b".to_owned()));
        assert_eq!(locate(&root, "/").map(|(_, it)| it.into_owned()), Some("r".to_owned()));
    }

    fn longest_mounts<'a>(what: &str, table: impl Table<'a>, lib: Origin) {
        let at =
            |path: &'a str| locate(table, path).map(|(origin, under)| (origin, under.into_owned()));
        assert_eq!(at("/w/Cargo.toml"), Some((Origin::View, "Cargo.toml".to_owned())), "{what}");
        assert_eq!(at("/w"), Some((Origin::View, String::new())), "{what}");
        assert_eq!(
            at("/w/vendor/x/src/lib.rs"),
            Some((lib, "x-1.0/src/lib.rs".to_owned())),
            "{what}"
        );
        assert_eq!(at("/w/vendor/x"), Some((lib, "x-1.0".to_owned())), "{what}");
        assert_eq!(at("/w/vendor/xy"), Some((Origin::View, "vendor/xy".to_owned())), "{what}");
        assert_eq!(at("/w2/a"), Some((Origin::View, "other/a".to_owned())), "{what}");
        assert_eq!(at("/wx/a"), None, "{what}: a mount is a directory, not a prefix");
        assert_eq!(at("/"), None, "{what}");
    }

    /// `table` as the database keeps the [`Mounts`] query's value.
    fn kept(table: &MountTable) -> blaze_format::Aligned {
        blaze_format::Aligned::copy_from(&blaze_format::to_bytes(&MountIndex::of(table)))
    }

    /// The index has every mount, file and directory of its table, read where its bytes lie.
    #[test]
    fn a_table_is_read_in_place() {
        let library = Origin::Root(ContentId([9; 32]));
        let table = MountTable {
            mounts: vec![mount("/rustc/0123/library", library, ""), mount("/w", Origin::View, "")],
            outside: vec![("/home/u/.cargo/config.toml".to_owned(), b"[build]\n".to_vec())],
            known: vec!["/".to_owned(), "/home/u".to_owned()],
        };
        let kept = kept(&table);
        let index = <MountIndex as Persist>::read(&kept).unwrap();
        assert_eq!(index.get("/w"), Some(table.mounts[1].clone()));
        assert_eq!(index.get("/rustc/0123/library"), Some(table.mounts[0].clone()));
        assert_eq!(index.get("/rustc"), None);
        assert_eq!((index.mounts(), index.outsides(), index.knowns()), (2, 1, 2));
        assert_eq!(index.outside_at(0), b"/home/u/.cargo/config.toml");
        assert_eq!(index.outside(0), b"[build]\n");
        assert!(
            kept.as_ptr_range().contains(&index.outside(0).as_ptr()),
            "the bytes are the kept bytes"
        );
        assert_eq!(index.known(1), b"/home/u");
        assert!(known_file(index, "/home/u/.cargo/config"));
        assert!(!known_file(index, "/etc/x"));

        let empty = MountTable { mounts: Vec::new(), outside: Vec::new(), known: Vec::new() };
        let kept = self::kept(&empty);
        let index = <MountIndex as Persist>::read(&kept).unwrap();
        assert_eq!((index.mounts(), index.outsides(), index.knowns()), (0, 0, 0));
        assert_eq!(locate(index, "/w/a"), None);
        assert!(
            MountRef::new(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err(),
            "a head naming a mount it lacks"
        );

        let file = Found::Here(vec![7u8; 64]);
        let written = blaze_protocol::cbor::to_vec(&file).unwrap();
        assert!(written.len() < 64 + 16, "{} bytes", written.len());
        assert_eq!(blaze_protocol::cbor::from_slice::<Found<Vec<u8>>>(&written).unwrap(), file);
    }

    #[test]
    fn paths_are_normal() {
        assert_eq!(normal(Path::new("/w/a/../b/./Cargo.toml")).as_deref(), Some("/w/b/Cargo.toml"));
        assert_eq!(normal(Path::new("/")).as_deref(), Some("/"));
        assert_eq!(normal(Path::new("w/a")), None);
        assert_eq!(ancestors("/w/a/b"), vec!["/w/a", "/w", "/"]);
    }

    #[test]
    fn outside_the_mounts_only_known_directories_have_files() {
        let mut known: Vec<String> = ["/home/u/.cargo", "/home/u", "/"].map(str::to_owned).to_vec();
        known.sort();
        let table = MountTable { mounts: Vec::new(), outside: Vec::new(), known };
        assert!(known_file(&table, "/home/u/.cargo/config.toml"));
        assert!(known_file(&table, "/home/u/.cargo/config"));
        assert!(known_file(&table, "/.cargo/config.toml"));
        assert!(known_file(&table, "/Cargo.toml"));
        assert!(!known_file(&table, "/etc/passwd/x"));
        assert!(!known_file(&table, "/home/u/.cargo/registry/src/x/Cargo.toml"));
    }

    /// The config chain lists each directory's `.cargo` up to `/`: a known directory's
    /// `.cargo` is listed like the directory, where a checkout's first build read `/.cargo`.
    #[test]
    fn a_known_directorys_cargo_directory_is_listed_as_its_files_are_read() {
        let mut known: Vec<String> = ["/w", "/"].map(str::to_owned).to_vec();
        known.sort();
        let table = MountTable { mounts: Vec::new(), outside: Vec::new(), known };
        for dir in ["/", "/.cargo", "/w", "/w/.cargo"] {
            assert!(known_dir(&table, dir), "{dir}");
        }
        for dir in ["/etc", "/w/src", "/w/.cargo/x", "/.cargo/.cargo"] {
            assert!(!known_dir(&table, dir), "{dir}");
        }
    }

    /// A read of a path through the view's mounts costs a binary search per directory above
    /// it, in the kept bytes: nothing allocated, however many sources are mounted.
    #[test]
    fn a_lookup_allocates_nothing() {
        let mut mounts: Vec<Mount> = (0..2000)
            .map(|at| {
                mount(
                    &format!("/c/registry/src/index/pkg-{at:04}-1.0.0"),
                    Origin::Root(ContentId([3; 32])),
                    "",
                )
            })
            .collect();
        mounts.push(mount("/w", Origin::View, ""));
        mounts.sort();
        let table = MountTable { mounts, outside: Vec::new(), known: vec!["/".to_owned()] };
        let kept = kept(&table);
        let index = <MountIndex as Persist>::read(&kept).unwrap();
        let path = "/c/registry/src/index/pkg-1234-1.0.0/src/lib.rs";
        let found = blaze_alloc_count::assert_at_most(0, "a mount lookup", || locate(index, path));
        assert_eq!(found, Some((Origin::Root(ContentId([3; 32])), Cow::Borrowed("src/lib.rs"))));
        let found = blaze_alloc_count::assert_at_most(0, "a mount lookup", || {
            locate(index, "/w/Cargo.toml")
        });
        assert_eq!(found, Some((Origin::View, Cow::Borrowed("Cargo.toml"))));
        assert!(blaze_alloc_count::assert_at_most(0, "a known file", || known_file(
            index,
            "/Cargo.toml"
        )));
    }

    /// A kept file and listing are read where the database keeps them, and read in place
    /// allocate nothing past the listing's vector.
    #[test]
    fn kept_bytes_and_names_are_lent() {
        let file = blaze_protocol::cbor::to_vec(&Found::Here(b"[package]\n".to_vec())).unwrap();
        let found =
            blaze_alloc_count::assert_at_most(0, "a kept file read in place", || lent(&file));
        let Found::Here(bytes) = found else { panic!("{found:?}") };
        assert_eq!(bytes, b"[package]\n");
        assert!(file.as_ptr_range().contains(&bytes.as_ptr()), "the bytes are the kept bytes");
        assert_eq!(
            lent(&blaze_protocol::cbor::to_vec(&Found::<Vec<u8>>::Unmounted).unwrap()),
            Found::Unmounted
        );

        let names: Found<Names> =
            Found::Here(vec![("Cargo.toml".to_owned(), false), ("src".to_owned(), true)]);
        let dir = blaze_protocol::cbor::to_vec(&names).unwrap();
        let found = blaze_alloc_count::assert_at_most(1, "a kept listing read in place", || {
            lent_names(&dir)
        });
        let Found::Here(listed) = found else { panic!("{found:?}") };
        assert_eq!(listed, [("Cargo.toml", false), ("src", true)]);
        assert!(dir.as_ptr_range().contains(&listed[1].0.as_ptr()), "the names are the kept bytes");
    }

    #[test]
    fn manifests_and_config_files_are_toml() {
        assert!(is_toml("/w/Cargo.toml"));
        assert!(is_toml("/home/u/.cargo/config.toml"));
        assert!(is_toml("/home/u/.cargo/config"));
        assert!(!is_toml("/w/Cargo.lock"));
        assert!(!is_toml("/w/config"));
    }

    #[test]
    fn already_normal_paths_are_borrowed() {
        assert!(matches!(
            normal_ref(Path::new("/w/a/Cargo.toml")),
            Some(Cow::Borrowed("/w/a/Cargo.toml"))
        ));
        assert!(matches!(normal_ref(Path::new("/")), Some(Cow::Borrowed("/"))));
        assert_eq!(normal_ref(Path::new("/w/./a/../b")).as_deref(), Some("/w/b"));
        assert_eq!(normal_ref(Path::new("/w/a/")).as_deref(), Some("/w/a"));
        assert_eq!(normal_ref(Path::new("w")), None);
    }

    #[test]
    fn a_comment_leaves_the_canonical_form() {
        let one = "[package]\nname = \"a\" # the name\nversion = \"0.1.0\"\n";
        let two = "# a comment\n[package]\nname    = \"a\"\n\nversion = \"0.1.0\"\n";
        assert_eq!(canonical_toml(one.as_bytes()), canonical_toml(two.as_bytes()));
        // A table keeps the file's order (`package.metadata`'s is `cargo metadata`'s), so
        // keys moved are another form.
        let moved = "[package]\nversion = \"0.1.0\"\nname = \"a\"\n";
        assert_ne!(canonical_toml(one.as_bytes()), canonical_toml(moved.as_bytes()));
        assert_eq!(canonical_toml(b"[package"), None);
    }
}
