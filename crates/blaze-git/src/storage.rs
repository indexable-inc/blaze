//! Pluggable sans-IO Git storage traits ([`ObjectStore`], [`RefStore`], [`RepoStore`]), canonical
//! Git object encoders/parsers, and an allocator-explicit in-memory [`MemStore`].
//!
//! Every engine component (3-way ORT merge, PACK v2 writer/reader, wire protocol v2/v1 server &
//! client, `infect`/`disinfect`) talks ONLY to [`ObjectStore`] and [`RefStore`], so the engine can
//! run over `.bgit` mmap slabs, legacy `.git`, in-memory `MemStore`, `Sim`, or any custom backend.

#![blaze::sans_io]

use core::cmp::Ordering;

use blaze_git_fetch::{Kind as ObjectKind, Oid};
use blaze_hash::{BuildKeyHasher, FixedState, Map, MapOpts};
use blaze_memchr::memchr;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{ErrorKind, GitError, GitFile, refs::ref_name_safe};

/// The 20-byte all-zero OID used in Git wire protocols to represent absence (create / delete).
pub const ZERO_OID: Oid = Oid([0u8; 20]);

/// Target of a Git reference (`HEAD` or `refs/...`).
#[derive(PartialEq, Eq, Debug)]
pub enum RefTarget<'a> {
    Direct(Oid),
    Symbolic(AString<'a>),
}

impl<'a> RefTarget<'a> {
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> RefTarget<'b> {
        match self {
            RefTarget::Direct(oid) => RefTarget::Direct(*oid),
            RefTarget::Symbolic(s) => RefTarget::Symbolic(String::from_str_in(s.as_str(), alloc)),
        }
    }
}

/// One advertised or listed reference entry.
#[derive(PartialEq, Eq, Debug)]
pub struct RefEntry<'a> {
    pub name: AString<'a>,
    pub target: RefTarget<'a>,
    /// For annotated tags (or symbolic refs pointing to them), the peeled non-tag target OID.
    pub peeled: Option<Oid>,
}

/// A compare-and-swap reference update request.
#[derive(PartialEq, Eq, Debug)]
pub struct RefUpdate<'a> {
    pub name: AString<'a>,
    /// Expected current OID:
    /// - `None`: unconditional update (force / local reset)
    /// - `Some(ZERO_OID)`: ref must not exist yet (create)
    /// - `Some(oid)`: ref must currently resolve to `oid`
    pub old: Option<Oid>,
    /// Desired new OID (`ZERO_OID` deletes the ref).
    pub new: Oid,
}

/// Result of one [`RefUpdate`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefUpdateStatus {
    Ok,
    CasMismatch { expected: Oid, actual: Option<Oid> },
    InvalidName,
    RejectedAtomic,
}

/// Pluggable sans-IO Git object store (commits, trees, blobs, tags keyed by SHA-1 [`Oid`]).
pub trait ObjectStore<'a> {
    /// Returns whether `oid` exists in this store.
    fn has_object(&self, oid: &Oid) -> bool;

    /// Reads object `oid` into `out` (cleared first). Returns `Ok(None)` if absent.
    fn read_object(&self, oid: &Oid, out: &mut AVec<'a, u8>) -> Result<Option<ObjectKind>, GitError>;

    /// Returns the raw (uncompressed) byte length of `oid` without necessarily reading or
    /// decompressing its payload. Stores like `.bgit` `BGIX` answer in O(log N) with zero
    /// decompression; the default falls back to [`Self::read_object`].
    fn object_size(&self, oid: &Oid, alloc: Alloc<'a>) -> Result<Option<u64>, GitError> {
        let mut scratch = Vec::new_in(alloc);
        Ok(self.read_object(oid, &mut scratch)?.map(|_| scratch.len() as u64))
    }

    /// Stores `(kind, body)` and returns its exact SHA-1 [`Oid`]. Idempotent if already present.
    fn write_object(&mut self, kind: ObjectKind, body: &[u8]) -> Result<Oid, GitError>;

    /// Appends all object OIDs currently stored into `out`.
    fn list_oids(&self, out: &mut AVec<'a, Oid>) -> Result<(), GitError>;
}

/// Pluggable sans-IO Git reference store (`HEAD` + `refs/*` with atomic CAS transactions).
pub trait RefStore<'a> {
    fn read_head(&self, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError>;
    fn write_head(&mut self, target: &RefTarget<'_>) -> Result<(), GitError>;
    fn read_ref(&self, name: &str, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError>;
    fn resolve_ref(&self, name: &str) -> Result<Option<Oid>, GitError>;
    fn list_refs(
        &self,
        prefix: &str,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, RefEntry<'a>>,
    ) -> Result<(), GitError>;
    fn apply_ref_updates(
        &mut self,
        updates: &[RefUpdate<'_>],
        atomic: bool,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, RefUpdateStatus>, GitError>;
}

/// A full repository store combining objects and refs.
pub trait RepoStore<'a>: ObjectStore<'a> + RefStore<'a> {}
impl<'a, T: ObjectStore<'a> + RefStore<'a>> RepoStore<'a> for T {}

/// Compare two names in Git's canonical tree ordering (directories sort as if suffixed by `'/'`).
#[inline]
pub fn git_tree_name_cmp(a: &[u8], a_is_dir: bool, b: &[u8], b_is_dir: bool) -> Ordering {
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        Ordering::Equal => {
            let ca = a.get(n).copied().or_else(|| a_is_dir.then_some(b'/'));
            let cb = b.get(n).copied().or_else(|| b_is_dir.then_some(b'/'));
            ca.cmp(&cb)
        }
        ord => ord,
    }
}

/// One borrowed entry for encoding a Git tree object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TreeBuildEntry<'b> {
    pub mode: u32,
    pub name: &'b [u8],
    pub oid: Oid,
}

/// Encodes `entries` in Git's canonical tree order (`<octal-mode> <name>\0<20-byte-oid>`) into
/// `out` (cleared first). Sorts a temporary index slice in `alloc` so the caller need not pre-sort.
pub fn encode_tree<'a>(
    entries: &[TreeBuildEntry<'_>],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    out.clear();
    let mut order: AVec<'a, usize> = Vec::with_capacity_in(entries.len(), alloc);
    for i in 0..entries.len() {
        order.push(i);
    }
    order.sort_by(|&a, &b| {
        let ea = entries[a];
        let eb = entries[b];
        git_tree_name_cmp(ea.name, ea.mode == 0o040000, eb.name, eb.mode == 0o040000)
    });
    for idx in order {
        let e = entries[idx];
        // Git omits leading zeros on tree entry modes (`40000`, `100644`, `100755`, `120000`, `160000`).
        let mut octal = [0u8; 11];
        let mut pos = octal.len();
        let mut m = e.mode;
        if m == 0 {
            pos -= 1;
            octal[pos] = b'0';
        } else {
            while m != 0 {
                pos -= 1;
                octal[pos] = b'0' + (m & 7) as u8;
                m >>= 3;
            }
        }
        out.extend_from_slice(&octal[pos..]);
        out.push(b' ');
        out.extend_from_slice(e.name);
        out.push(0);
        out.extend_from_slice(&e.oid.0);
    }
    Ok(())
}

/// Author or committer identity line (`Name <email> <timestamp> <tz>`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signature<'b> {
    pub name: &'b str,
    pub email: &'b str,
    pub timestamp: i64,
    /// Timezone string such as `"+0000"` or `"-0800"`.
    pub tz: &'b str,
}

/// Encodes a canonical Git commit object body into `out` (cleared first).
pub fn encode_commit<'a>(
    tree: Oid,
    parents: &[Oid],
    author: Signature<'_>,
    committer: Signature<'_>,
    message: &str,
    out: &mut AVec<'a, u8>,
) {
    out.clear();
    out.extend_from_slice(b"tree ");
    out.extend_from_slice(&tree.hex());
    out.push(b'\n');
    for p in parents {
        out.extend_from_slice(b"parent ");
        out.extend_from_slice(&p.hex());
        out.push(b'\n');
    }
    write_sig_line(b"author ", author, out);
    write_sig_line(b"committer ", committer, out);
    out.push(b'\n');
    out.extend_from_slice(message.as_bytes());
}

fn write_sig_line<'a>(prefix: &[u8], sig: Signature<'_>, out: &mut AVec<'a, u8>) {
    out.extend_from_slice(prefix);
    out.extend_from_slice(sig.name.as_bytes());
    out.extend_from_slice(b" <");
    out.extend_from_slice(sig.email.as_bytes());
    out.extend_from_slice(b"> ");
    write_i64_decimal(sig.timestamp, out);
    out.push(b' ');
    out.extend_from_slice(sig.tz.as_bytes());
    out.push(b'\n');
}

fn write_i64_decimal<'a>(val: i64, out: &mut AVec<'a, u8>) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut mag = val.unsigned_abs();
    if val < 0 {
        out.push(b'-');
    }
    loop {
        at -= 1;
        digits[at] = b'0' + (mag % 10) as u8;
        mag /= 10;
        if mag == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}

/// Parsed fields of a Git annotated tag object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TagObject<'b> {
    pub object: Oid,
    pub target_kind: ObjectKind,
    pub name: &'b [u8],
}

/// Parses an annotated Git tag object header (`object <hex>\ntype <kind>\ntag <name>\n`).
pub fn parse_tag(oid: Oid, body: &[u8]) -> Result<TagObject<'_>, GitError> {
    let bad = |at: usize| GitError { file: GitFile::TagObject(oid), at: at as u64, kind: ErrorKind::Malformed };
    let mut pos = 0usize;
    let mut object = None;
    let mut target_kind = None;
    let mut name: Option<&[u8]> = None;
    while pos < body.len() {
        let nl = memchr(b'\n', &body[pos..]).map_or(body.len(), |i| pos + i);
        let line = &body[pos..nl];
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix(b"object ") {
            object = Some(Oid::parse_hex(rest).ok_or_else(|| bad(pos))?);
        } else if let Some(rest) = line.strip_prefix(b"type ") {
            target_kind = Some(match rest {
                b"commit" => ObjectKind::Commit,
                b"tree" => ObjectKind::Tree,
                b"blob" => ObjectKind::Blob,
                b"tag" => ObjectKind::Tag,
                _ => return Err(bad(pos)),
            });
        } else if let Some(rest) = line.strip_prefix(b"tag ") {
            name = Some(rest);
        }
        pos = nl + 1;
    }
    Ok(TagObject {
        object: object.ok_or_else(|| bad(0))?,
        target_kind: target_kind.ok_or_else(|| bad(0))?,
        name: name.ok_or_else(|| bad(0))?,
    })
}

/// Peels an OID through up to `max_depth` annotated tags to its underlying non-tag object OID.
/// Returns `Ok(None)` if `oid` is not an annotated tag.
pub fn peel_tag<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    oid: Oid,
    max_depth: u8,
    alloc: Alloc<'a>,
) -> Result<Option<Oid>, GitError> {
    let mut buf = Vec::new_in(alloc);
    let mut cur = oid;
    let mut peeled = false;
    for _ in 0..max_depth {
        let Some(kind) = store.read_object(&cur, &mut buf)? else {
            return Ok(if peeled { Some(cur) } else { None });
        };
        if kind != ObjectKind::Tag {
            return Ok(if peeled { Some(cur) } else { None });
        }
        let tag = parse_tag(cur, &buf)?;
        cur = tag.object;
        peeled = true;
    }
    Err(GitError { file: GitFile::TagObject(cur), at: 0, kind: ErrorKind::RefCycle })
}

struct StoredObject<'a> {
    kind: ObjectKind,
    body: AVec<'a, u8>,
}

/// Pure sans-IO in-memory [`RepoStore`] in the caller's [`Alloc`].
///
/// Ideal for deterministic simulations, tests, ephemeral server/client mirrors, and staging
/// conversions before flushing to disk.
pub struct MemStore<'a> {
    alloc: Alloc<'a>,
    objects: Map<Oid, StoredObject<'a>, BuildKeyHasher, Alloc<'a>>,
    head: Option<RefTarget<'a>>,
    refs: Map<AString<'a>, RefTarget<'a>, FixedState, Alloc<'a>>,
}

impl<'a> MemStore<'a> {
    pub fn new(alloc: Alloc<'a>) -> MemStore<'a> {
        MemStore {
            alloc,
            objects: Map::new(MapOpts {
                hasher: BuildKeyHasher::default(),
                alloc,
                capacity: None,
            }),
            head: None,
            refs: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
        }
    }

    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub fn ref_count(&self) -> usize {
        self.refs.len()
    }

    /// Sets a ref directly (symbolic or direct).
    pub fn set_ref(&mut self, name: &str, target: RefTarget<'a>) -> Result<(), GitError> {
        if !ref_name_safe(name) {
            return Err(GitError { file: GitFile::Ref, at: 0, kind: ErrorKind::Malformed });
        }
        let key = String::from_str_in(name, self.alloc);
        self.refs.insert(key, target);
        Ok(())
    }

    /// Resolves a ref name (including `"HEAD"`) following up to 16 symbolic hops.
    fn resolve_internal(&self, start: &str) -> Result<Option<Oid>, GitError> {
        let mut cur_buf = String::from_str_in(start, self.alloc);
        for _ in 0..16 {
            let target = if cur_buf.as_str() == "HEAD" {
                self.head.as_ref()
            } else {
                self.refs.get(cur_buf.as_str())
            };
            match target {
                None => return Ok(None),
                Some(RefTarget::Direct(oid)) => return Ok(Some(*oid)),
                Some(RefTarget::Symbolic(next)) => {
                    let next_name = String::from_str_in(next.as_str(), self.alloc);
                    cur_buf = next_name;
                }
            }
        }
        Err(GitError { file: GitFile::Ref, at: 0, kind: ErrorKind::RefCycle })
    }
}

impl<'a> ObjectStore<'a> for MemStore<'a> {
    fn has_object(&self, oid: &Oid) -> bool {
        self.objects.contains_key(oid)
    }

    fn read_object(&self, oid: &Oid, out: &mut AVec<'a, u8>) -> Result<Option<ObjectKind>, GitError> {
        out.clear();
        let Some(obj) = self.objects.get(oid) else {
            return Ok(None);
        };
        out.extend_from_slice(&obj.body);
        Ok(Some(obj.kind))
    }

    fn write_object(&mut self, kind: ObjectKind, body: &[u8]) -> Result<Oid, GitError> {
        let oid = Oid::of(kind, body);
        if self.objects.contains_key(&oid) {
            return Ok(oid);
        }
        let mut copy = Vec::with_capacity_in(body.len(), self.alloc);
        copy.extend_from_slice(body);
        self.objects.insert(oid, StoredObject { kind, body: copy });
        Ok(oid)
    }

    fn list_oids(&self, out: &mut AVec<'a, Oid>) -> Result<(), GitError> {
        for oid in self.objects.keys() {
            out.push(*oid);
        }
        out.sort_unstable();
        Ok(())
    }
}

impl<'a> RefStore<'a> for MemStore<'a> {
    fn read_head(&self, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError> {
        Ok(self.head.as_ref().map(|h| h.clone_in(alloc)))
    }

    fn write_head(&mut self, target: &RefTarget<'_>) -> Result<(), GitError> {
        self.head = Some(target.clone_in(self.alloc));
        Ok(())
    }

    fn read_ref(&self, name: &str, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError> {
        if name == "HEAD" {
            return self.read_head(alloc);
        }
        Ok(self.refs.get(name).map(|r| r.clone_in(alloc)))
    }

    fn resolve_ref(&self, name: &str) -> Result<Option<Oid>, GitError> {
        self.resolve_internal(name)
    }

    fn list_refs(
        &self,
        prefix: &str,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, RefEntry<'a>>,
    ) -> Result<(), GitError> {
        out.clear();
        for (name, target) in self.refs.iter() {
            if !prefix.is_empty() && !name.as_str().starts_with(prefix) {
                continue;
            }
            let peeled = match target {
                RefTarget::Direct(oid) => peel_tag(self, *oid, 8, alloc)?,
                RefTarget::Symbolic(_) => {
                    if let Some(resolved) = self.resolve_internal(name.as_str())? {
                        peel_tag(self, resolved, 8, alloc)?
                    } else {
                        None
                    }
                }
            };
            out.push(RefEntry {
                name: String::from_str_in(name.as_str(), alloc),
                target: target.clone_in(alloc),
                peeled,
            });
        }
        out.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Ok(())
    }

    fn apply_ref_updates(
        &mut self,
        updates: &[RefUpdate<'_>],
        atomic: bool,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, RefUpdateStatus>, GitError> {
        let mut statuses = Vec::with_capacity_in(updates.len(), alloc);
        let mut all_ok = true;
        for u in updates {
            if !ref_name_safe(u.name.as_str()) {
                statuses.push(RefUpdateStatus::InvalidName);
                all_ok = false;
                continue;
            }
            let actual = self.resolve_internal(u.name.as_str())?;
            if let Some(expected) = u.old {
                let matches = if expected == ZERO_OID {
                    actual.is_none()
                } else {
                    actual == Some(expected)
                };
                if !matches {
                    statuses.push(RefUpdateStatus::CasMismatch { expected, actual });
                    all_ok = false;
                    continue;
                }
            }
            statuses.push(RefUpdateStatus::Ok);
        }
        if atomic && !all_ok {
            for s in statuses.iter_mut() {
                if *s == RefUpdateStatus::Ok {
                    *s = RefUpdateStatus::RejectedAtomic;
                }
            }
            return Ok(statuses);
        }
        for (u, st) in updates.iter().zip(statuses.iter()) {
            if *st != RefUpdateStatus::Ok {
                continue;
            }
            if u.new == ZERO_OID {
                self.refs.remove(u.name.as_str());
            } else {
                let key = String::from_str_in(u.name.as_str(), self.alloc);
                self.refs.insert(key, RefTarget::Direct(u.new));
            }
        }
        Ok(statuses)
    }
}
