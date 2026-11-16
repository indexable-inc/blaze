//! Records by key (drive.md, "The system side", "Store"). The trait is the seam the plain store
//! (blaze-cas `plain::Driven`, store-next.md) plugs into once it has a bstd driver: its
//! `open(root, settings)` then `machine(&[checkout, home])`, `get_into` by `locate` and one
//! `pread`, and after the answer `seal()` then `compact(n)`, its `report()` into the profile.
//! [`PackStore`] is the stand-in: one pack a command under `.blaze/v2/packs/`, written whole
//! and never rewritten (a key holds one value, rule 21). An old home's `.blaze/v2/records/`
//! (one file a record, before 2026-11-16) is not read: its first command rebuilds.

use blaze_drive::profile::Profile;
use blaze_drive::record::Stored;
use blaze_drive::{Key, Kind, Name};
use blaze_ids::ContentId;

use core::fmt;
use core::time::Duration;

use blaze_own::Alloc;
use blaze_remote::PublicKey;
use blaze_remote::read::{Answer, BatchError, Ed25519Keys, HeadCheck, Limits, Namespace, Object, Origin, OriginError, Reader, SignatureWhy, Unfetched, fetch};
use bstd::Stopped;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, File, Fs, FsError, FsKind, ReadOnly, ReadWrite};
use std::alloc::Global;
use bstd::mem::Bytes;
use bstd::out::Out;
use bstd::path::Path;
use bstd::Deadline;
use bstd::net::{Addr, Net};
use bstd::url::Url;
use blaze_h1::{Limits as H1Limits, Range as H1Range, Redirects};
use blaze_h1_net::{Ask, Client, FetchWhy, NoTls};

use crate::CliError;

pub trait Store {
    /// The record at `key` into `into`; `false` when there is none.
    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError>;
    /// `bytes` at `key`. A key already held is left as it is: two builders of one key write
    /// equal bytes (rule 25).
    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError>;
    /// Every record put so far to disk, before anything that names them (the answer's
    /// pointer) is written.
    fn seal(&mut self, cx: &Cx) -> Result<(), CliError>;
    /// After the answer is printed: the plain store seals and compacts here and reports into
    /// `profile`. Never waited on by the answer.
    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError>;
    /// The `CrateScope` subjects of crates whose records hold nothing of a project (the
    /// toolchain's), told by the names step once its world is made: their records are put in
    /// the machine store (remote-read.md, "One chain of stores").
    fn public_crates(&mut self, subjects: &[ContentId]);
}

/// The project's store, under the checkout.
const PACKS: &str = ".blaze/v2/packs";
/// The machine store, under the user's home (remote-read.md, "One chain of stores"): records
/// this machine computed whose key holds nothing of a project. Beneath [`HOME_FOLDER`].
pub(crate) const SHARED: &str = "v2/shared";
/// blaze's folder in the user's home, made by the first file made in it.
const HOME_FOLDER: &str = ".blaze";
/// A store's record of the boot its packs were last known whole in, in its folder.
const BOOT: &str = "boot";
const BOOT_MAGIC: &[u8; 4] = b"BLZB";
/// A pack's last bytes: `BLZP`, the format, the record count, the index's offset, the digest
/// of the payloads (every byte before the index).
const FOOTER: usize = 56;
const MAGIC: &[u8; 4] = b"BLZP";
/// 2 since 2026-11-16 (the payloads' digest); 1 had none, and is refused, removed and made again.
const FORMAT: u32 = 2;
/// One index row: the key, the payload's offset and length, four zero bytes.
const ROW: usize = 48;
/// Past this many packs, the store merges them all into one after the answer.
pub(crate) const PACKS_MOST: usize = 8;

/// The boot this command runs in: a driver fact (macOS `kern.bootsessionuuid`, Linux
/// `/proc/sys/kernel/random/boot_id`), passed in, never in a key. A power cut makes a new
/// boot; everything a boot wrote and nothing lost power since is whole in the page cache, so a
/// pack sealed in this boot is trusted as written, and one from another is read whole once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Boot {
    Known([u8; 16]),
    /// The driver has no fact for it (`Io::boot` refused): no pack is trusted unread, each
    /// command verifies.
    Unknown,
}

/// A planted bug of the store's simulation tests (`tests` below): each is one the oracle
/// must catch. [`StorePlant::None`] outside tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StorePlant {
    None,
    /// The merge removes the old packs before the merged one is published.
    MergeRemovesFirst,
    /// A pack from an earlier boot is taken without its once-a-boot read.
    VerifySkipped,
    /// A pack vouches for itself: its index matching its name stands for the boot record.
    SelfVouching,
    /// The chain puts every record in the machine store, a project's too ([`Layered`]).
    PrivateIntoMachine,
}

/// A pack's index, read once at the open: rows sorted by key.
struct Pack {
    file: File<ReadOnly>,
    rows: std::vec::Vec<(Key, u64, u32), Global>,
    /// Removed by another command's merge since this one listed it ([`PackStore::get_into`]).
    gone: bool,
}

impl Pack {
    /// Whether its name is gone: asked after a read that failed or came back short, to tell a
    /// pack another command's merge removed (a miss) from one cut short (refused by name).
    fn removed(&self, cx: &Cx) -> Result<bool, CliError> {
        match self.file.len(cx) {
            Ok(_) => Ok(false),
            Err(error) if error.kind == FsKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    fn find(&self, key: Key) -> Option<(u64, u32)> {
        let at = self.rows.binary_search_by(|row| row.0.cmp(&key)).ok()?;
        Some((self.rows[at].1, self.rows[at].2))
    }
}

/// The local store: one pack a command (drive.md, "The system side", "Store"). A command's
/// records leave in ONE file, written with one create, one write and one rename after its
/// last record and before its answer's pointer, never one file a record (708 renames were
/// 105 ms of tempo's cold check, 148 µs each). A pack is the records sorted by key, then an
/// index of [`ROW`]s sorted by key, then a [`FOOTER`]: equal record sets make equal bytes
/// (rule 25), and a pack is named by the digest of its index and footer, so two commands
/// sealing at once on one home never write one name with two contents (a key holds one value,
/// rule 21: the index says the whole pack). The open lists the folder once and reads each
/// pack's footer and index with two `pread`s; a home with no pack is cold, known from that one
/// listing: no lookup asks the disk. A get asks this command's records first, then each pack.
/// Past `packs_most` packs, `after_answer` merges them into one.
///
/// What a crash may do, and what it may not (the simulation tests below). The home is a cache
/// of records the driver can make again: it must never hand back wrong bytes, and it need not
/// survive a power cut. Nothing on a command's path is synced: a pack, the boot record and
/// `last` are published unsynced. A record lost is made again (a cert, a root or a page the
/// driver does not get is work, not an error). A record is never read torn: the footer holds
/// the digest of the payloads, and the name the digest of the index and footer, and a pack
/// sealed in another boot than this one is read whole and checked against both once, at its
/// first open in this boot, then named in the boot record ([`BOOT`]); one that fails is
/// removed, said, and its records made again. A pack sealed in this boot is trusted as
/// written: no power was lost since, so the page cache holds what was written.
pub struct PackStore {
    packs_dir: Dir<ReadWrite>,
    /// With their names' numbers (for the merge), in the listing's order.
    packs: std::vec::Vec<(u64, Pack), Global>,
    /// This command's records: their bytes end to end, and (key, offset, length) sorted.
    written: std::vec::Vec<u8, Global>,
    index: std::vec::Vec<(Key, u64, u32), Global>,
    /// Packs the open found torn or foreign and removed: said by the caller, and made again.
    pub(crate) refused: u32,
    /// Packs read whole and checked at this open (their first in this boot).
    pub(crate) verified: u32,
    boot: Boot,
    packs_most: usize,
    plant: StorePlant,
    /// Reads of footers and indexes, reset after each.
    scratch: bstd::mem::Frame,
}

/// A pack's name: the first 8 bytes of the digest of its index and footer, in hex. The footer
/// holds the payloads' digest, so the name stands for every byte.
fn pack_number(bytes: &[u8]) -> u64 {
    let index_at = le_u64(&bytes[bytes.len() - FOOTER + 16..]) as usize;
    index_number(&bytes[index_at..])
}

/// [`pack_number`] of a pack's index and footer, as read.
fn index_number(index_and_footer: &[u8]) -> u64 {
    let digest = blaze_ids::ContentId::of("blaze-cli pack index", index_and_footer);
    le_u64(&digest.0[..8])
}

fn payloads_digest(payloads: &[u8]) -> [u8; 32] {
    blaze_ids::ContentId::of("blaze-cli pack payloads", payloads).0
}

/// The boot record: `BLZB`, the boot, then the numbers of the packs known whole in it, sorted.
/// Read as nothing trusted when it is absent, malformed, of another boot, or the boot unknown.
fn read_boot(cx: &Cx, packs_dir: &Dir<ReadWrite>, boot: Boot, scratch: &bstd::mem::Frame) -> Result<std::vec::Vec<u64, Global>, CliError> {
    let mut trusted = std::vec::Vec::new_in(Global);
    let Boot::Known(now) = boot else { return Ok(trusted) };
    let mut bytes = Bytes::new_in(scratch, "the boot record");
    match packs_dir.read_all(cx, &Path::new(BOOT)?, &mut bytes) {
        Ok(_) => {}
        Err(error) if error.kind == FsKind::NotFound => return Ok(trusted),
        Err(error) => return Err(error.into()),
    }
    let bytes = bytes.as_slice();
    if bytes.len() < 20 || &bytes[..4] != BOOT_MAGIC || bytes[4..20] != now || (bytes.len() - 20) % 8 != 0 {
        return Ok(trusted);
    }
    trusted.extend(bytes[20..].chunks_exact(8).map(le_u64));
    if !trusted.is_sorted() {
        trusted.clear();
    }
    Ok(trusted)
}

fn pack_name(number: u64) -> Result<Path, CliError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = [0u8; 16];
    for (at, slot) in name.iter_mut().enumerate() {
        *slot = HEX[((number >> ((15 - at) * 4)) & 15) as usize];
    }
    let name = core::str::from_utf8(&name).map_err(|_| CliError::Text { what: "a pack's hex" })?;
    Ok(Path::new(name)?)
}

fn le_u32(bytes: &[u8]) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(word)
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

/// Why a pack was not taken: torn or foreign (removed and made again), or gone (another
/// command's merge removed it after this one listed it: its records are in that merge's pack).
enum Untaken {
    Refused,
    Gone,
    Failed(CliError),
}

impl From<FsError> for Untaken {
    fn from(error: FsError) -> Untaken {
        match error.kind {
            FsKind::NotFound => Untaken::Gone,
            _ => Untaken::Failed(CliError::Fs(error)),
        }
    }
}

impl PackStore {
    /// The folder need not exist: the first pack written makes it.
    pub fn open(cx: &Cx, root: &Dir<ReadWrite>, boot: Boot) -> Result<PackStore, CliError> {
        PackStore::open_with(cx, root, boot, PACKS_MOST, StorePlant::None)
    }

    /// [`PackStore::open`], merging past `packs_most` packs, with `plant`'s bug planted.
    pub(crate) fn open_with(cx: &Cx, root: &Dir<ReadWrite>, boot: Boot, packs_most: usize, plant: StorePlant) -> Result<PackStore, CliError> {
        PackStore::open_in(cx, root, PACKS, boot, packs_most, plant)
    }

    /// The pack store in `folder` under `root`: the project's ([`PACKS`] under the checkout)
    /// or the machine's ([`SHARED`] under the home). One kind of store, one set of rules.
    pub(crate) fn open_in(cx: &Cx, root: &Dir<ReadWrite>, folder: &str, boot: Boot, packs_most: usize, plant: StorePlant) -> Result<PackStore, CliError> {
        let cx = cx.span("store open");
        let folder = Path::new(folder)?;
        let packs_dir = root.subdir(&folder);
        let mut scratch = bstd::mem::Frame::with_reserve(bstd::mem::TASK_FRAME, bstd::mem::Tag::Other)
            .map_err(|_| CliError::Text { what: "the store's frame could not be reserved" })?;
        let trusted = read_boot(&cx, &packs_dir, boot, &scratch)?;
        scratch.reset();
        let mut store = PackStore {
            packs_dir,
            packs: std::vec::Vec::new_in(Global),
            written: std::vec::Vec::new_in(Global),
            index: std::vec::Vec::new_in(Global),
            refused: 0,
            verified: 0,
            boot,
            packs_most,
            plant,
            scratch,
        };
        if let Some(entries) = root.entries(&cx, &folder)? {
            for i in 0..entries.len() {
                let name = entries.name(i);
                let number = match (name.len(), u64::from_str_radix(name, 16)) {
                    (16, Ok(number)) => number,
                    // The boot record, or a publish's temporary file (one in flight, or a
                    // crash's leftover): never a pack until its rename names it.
                    _ => continue,
                };
                let known = trusted.binary_search(&number).is_ok();
                let taken = PackStore::read_pack(&cx, &store.packs_dir, number, !known, &store.scratch, plant);
                store.scratch.reset();
                match taken {
                    Ok(pack) => {
                        store.verified += u32::from(!known);
                        store.packs.push((number, pack));
                    }
                    Err(Untaken::Gone) => {}
                    // Torn or foreign: never read around. It is removed, said, and its records
                    // made again; the rest of the home stands, each pack checked on its own.
                    Err(Untaken::Refused) => {
                        store.packs_dir.remove(&cx, &pack_name(number)?, Durable::No)?;
                        store.refused += 1;
                    }
                    Err(Untaken::Failed(error)) => return Err(error),
                }
            }
        }
        if store.verified > 0 || store.refused > 0 || store.packs.len() != trusted.len() {
            store.write_boot(&cx)?;
        }
        Ok(store)
    }

    /// The boot record as this store holds it: every pack it took, known whole in this boot.
    /// Unsynced: a power cut makes a new boot, which reads a record of the old as nothing.
    fn write_boot(&self, cx: &Cx) -> Result<(), CliError> {
        let Boot::Known(now) = self.boot else { return Ok(()) };
        let mut numbers: std::vec::Vec<u64, Global> = std::vec::Vec::with_capacity_in(self.packs.len(), Global);
        numbers.extend(self.packs.iter().map(|(number, _)| *number));
        numbers.sort_unstable();
        numbers.dedup();
        let mut bytes = std::vec::Vec::with_capacity_in(20 + numbers.len() * 8, Global);
        bytes.extend_from_slice(BOOT_MAGIC);
        bytes.extend_from_slice(&now);
        for number in numbers {
            bytes.extend_from_slice(&number.to_le_bytes());
        }
        self.packs_dir.publish(cx, &Path::new(BOOT)?, Content::bytes(&bytes), Durable::No)?;
        Ok(())
    }

    /// Pack `number`'s index, checked at the open: the footer's magic and format, its count
    /// and index offset against the file's length (the index ends where the footer begins),
    /// each row's payload inside the payloads, and the rows sorted by key. With `verify` (a
    /// pack not known whole in this boot), every byte too, once: the index and footer against
    /// the name, the payloads against the footer's digest. A pack that fails any is
    /// [`Untaken::Refused`].
    fn read_pack(cx: &Cx, dir: &Dir<ReadWrite>, number: u64, verify: bool, scratch: &bstd::mem::Frame, plant: StorePlant) -> Result<Pack, Untaken> {
        let name = pack_name(number).map_err(Untaken::Failed)?;
        let file = dir.open_read(cx, &name)?;
        let len = file.len(cx)?;
        if len < FOOTER as u64 {
            return Err(Untaken::Refused);
        }
        let mut footer = Bytes::new_in(scratch, "a pack's footer");
        file.read_at(cx, len - FOOTER as u64, FOOTER, &mut footer)?;
        let footer = footer.as_slice();
        if footer.len() != FOOTER || &footer[..4] != MAGIC || le_u32(&footer[4..]) != FORMAT {
            return Err(Untaken::Refused);
        }
        let (count, index_at) = (le_u64(&footer[8..]), le_u64(&footer[16..]));
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&footer[24..56]);
        let rows_len = count.checked_mul(ROW as u64).filter(|bytes| index_at.checked_add(*bytes) == Some(len - FOOTER as u64));
        let Some(rows_len) = rows_len.and_then(|bytes| usize::try_from(bytes).ok()) else {
            return Err(Untaken::Refused);
        };
        // The index and the footer in one read: the name is their digest.
        let mut index = Bytes::new_in(scratch, "a pack's index");
        file.read_at(cx, index_at, rows_len + FOOTER, &mut index)?;
        if index.len() != rows_len + FOOTER {
            return Err(Untaken::Refused);
        }
        if verify {
            let vouched = match plant {
                StorePlant::VerifySkipped => true,
                // The pack's own index standing for its payloads.
                StorePlant::SelfVouching => index_number(index.as_slice()) == number,
                _ => {
                    let mut payloads = Bytes::new_in(scratch, "a pack's payloads, read once a boot");
                    let whole = usize::try_from(index_at).map_err(|_| Untaken::Refused)?;
                    file.read_at(cx, 0, whole, &mut payloads)?;
                    index_number(index.as_slice()) == number && payloads.len() == whole && payloads_digest(payloads.as_slice()) == digest
                }
            };
            if !vouched {
                return Err(Untaken::Refused);
            }
        }
        let mut rows = std::vec::Vec::with_capacity_in(rows_len / ROW, Global);
        for row in index.as_slice()[..rows_len].chunks_exact(ROW) {
            let mut key = [0u8; 32];
            key.copy_from_slice(&row[..32]);
            let (at, size) = (le_u64(&row[32..]), le_u32(&row[40..]));
            if at.checked_add(u64::from(size)).is_none_or(|end| end > index_at) {
                return Err(Untaken::Refused);
            }
            rows.push((Key(blaze_ids::ContentId(key)), at, size));
        }
        if !rows.is_sorted_by(|a, b| a.0 < b.0) {
            return Err(Untaken::Refused);
        }
        Ok(Pack { file, rows, gone: false })
    }

    /// `bytes` published unsynced as the pack they name, and taken: known whole in this boot.
    fn publish_pack(&mut self, cx: &Cx, bytes: &[u8]) -> Result<u64, CliError> {
        let number = pack_number(bytes);
        self.packs_dir.publish(cx, &pack_name(number)?, Content::bytes(bytes), Durable::No)?;
        let taken = PackStore::read_pack(cx, &self.packs_dir, number, false, &self.scratch, self.plant);
        self.scratch.reset();
        match taken {
            Ok(pack) => self.packs.push((number, pack)),
            // Another command's merge took it already: its records are in that merge's pack.
            Err(Untaken::Gone) => {}
            Err(Untaken::Refused) => return Err(CliError::Text { what: "a pack under .blaze/v2/packs read back as torn just after its publish" }),
            Err(Untaken::Failed(error)) => return Err(error),
        }
        Ok(number)
    }

    /// This command's records as one pack, if it wrote any: sorted by key, one publish (one
    /// create, write and rename, nothing synced), then the boot record naming it, before the
    /// answer's pointer is written. A command that made no record writes nothing.
    pub fn seal(&mut self, cx: &Cx) -> Result<(), CliError> {
        if self.index.is_empty() {
            return Ok(());
        }
        let cx = cx.span("store seal");
        let bytes = pack_bytes(self.index.iter().map(|(key, at, len)| (*key, &self.written[*at as usize..*at as usize + *len as usize])), self.index.len())?;
        self.publish_pack(&cx, &bytes)?;
        self.write_boot(&cx)?;
        self.written.clear();
        self.index.clear();
        Ok(())
    }

    fn held(&self, key: Key) -> bool {
        self.index.binary_search_by(|row| row.0.cmp(&key)).is_ok() || self.packs.iter().any(|(_, pack)| pack.find(key).is_some())
    }

    /// Every pack merged into one (a key's records are equal, rule 21), published, named in the
    /// boot record, and only then the old ones removed: nothing synced; a crash between keeps
    /// both, equal records twice, or loses records the driver makes again, never tears one.
    fn merge(&mut self, cx: &Cx) -> Result<(), CliError> {
        let cx = cx.span("store merge");
        let mut rows: std::vec::Vec<(Key, usize, usize), Global> = std::vec::Vec::new_in(Global);
        let mut payloads = Bytes::new_in(&self.scratch, "the packs' records");
        let mut merged = std::vec::Vec::new_in(Global);
        for (number, pack) in self.packs.iter() {
            let mark = (rows.len(), payloads.len());
            let mut whole = true;
            for (key, at, len) in pack.rows.iter() {
                let start = payloads.len();
                let read = pack.file.read_at(&cx, *at, *len as usize, &mut payloads);
                if matches!(read, Ok(_)) && payloads.len() - start == *len as usize {
                    rows.push((*key, start, payloads.len()));
                    continue;
                }
                // Merged away by another command since the open: its records stand there.
                if pack.removed(&cx)? {
                    whole = false;
                    break;
                }
                read?;
                return Err(CliError::Text { what: "a pack under .blaze/v2/packs is shorter than its index" });
            }
            if whole {
                merged.push(*number);
            } else {
                rows.truncate(mark.0);
                payloads.truncate(mark.1);
            }
        }
        rows.sort_by_key(|row| row.0);
        rows.dedup_by_key(|row| row.0);
        let all = payloads.as_slice();
        let bytes = pack_bytes(rows.iter().map(|(key, start, end)| (*key, &all[*start..*end])), rows.len())?;
        drop(payloads);
        let removes_first = self.plant == StorePlant::MergeRemovesFirst;
        if removes_first {
            for gone in merged.iter() {
                self.packs_dir.remove(&cx, &pack_name(*gone)?, Durable::No)?;
            }
        }
        let old = core::mem::replace(&mut self.packs, std::vec::Vec::new_in(Global));
        let number = self.publish_pack(&cx, &bytes)?;
        self.write_boot(&cx)?;
        if !removes_first {
            // A pack whose records equal the merge's has its name: it is the merge, kept.
            for gone in merged.iter().filter(|gone| **gone != number) {
                self.packs_dir.remove(&cx, &pack_name(*gone)?, Durable::No)?;
            }
        }
        drop(old);
        Ok(())
    }
}

/// A pack's bytes from records sorted by key: each payload at an 8-aligned offset, the
/// index, the footer.
fn pack_bytes<'b>(records: impl Iterator<Item = (Key, &'b [u8])> + Clone, count: usize) -> Result<std::vec::Vec<u8, Global>, CliError> {
    let too_big = || CliError::Text { what: "a record past 4 GiB" };
    let payloads: usize = records.clone().map(|(_, bytes)| bytes.len().next_multiple_of(8)).sum();
    let mut out = std::vec::Vec::with_capacity_in(payloads + count * ROW + FOOTER, Global);
    let mut rows = std::vec::Vec::with_capacity_in(count * ROW, Global);
    for (key, bytes) in records {
        let at = out.len() as u64;
        out.extend_from_slice(bytes);
        out.resize(out.len().next_multiple_of(8), 0);
        rows.extend_from_slice(&key.0.0);
        rows.extend_from_slice(&at.to_le_bytes());
        rows.extend_from_slice(&u32::try_from(bytes.len()).map_err(|_| too_big())?.to_le_bytes());
        rows.extend_from_slice(&[0; 4]);
    }
    let index_at = out.len() as u64;
    let digest = payloads_digest(&out);
    out.extend_from_slice(&rows);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(count as u64).to_le_bytes());
    out.extend_from_slice(&index_at.to_le_bytes());
    out.extend_from_slice(&digest);
    Ok(out)
}

impl Store for PackStore {
    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError> {
        if let Ok(at) = self.index.binary_search_by(|row| row.0.cmp(&key)) {
            let (_, start, len) = self.index[at];
            into.try_extend(&self.written[start as usize..start as usize + len as usize]).map_err(|stopped| CliError::Remote(RemoteError::Copy(stopped)))?;
            return Ok(true);
        }
        for (_, pack) in self.packs.iter_mut().filter(|(_, pack)| !pack.gone) {
            if let Some((at, len)) = pack.find(key) {
                let start = into.len();
                let read = pack.file.read_at(cx, at, len as usize, into);
                if matches!(read, Ok(_)) && into.len() - start == len as usize {
                    return Ok(true);
                }
                into.truncate(start);
                // Another command's merge removed it since the open: its records are in a pack
                // this one did not list, so this key is a miss, made again.
                if pack.removed(cx)? {
                    pack.gone = true;
                    continue;
                }
                read?;
                return Err(CliError::Text { what: "a pack under .blaze/v2/packs is shorter than its index" });
            }
        }
        Ok(false)
    }

    fn put(&mut self, _cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        if self.held(key) {
            return Ok(());
        }
        let len = u32::try_from(bytes.len()).map_err(|_| CliError::Text { what: "a record past 4 GiB" })?;
        let at = self.written.len() as u64;
        self.written.extend_from_slice(bytes);
        let place = self.index.binary_search_by(|row| row.0.cmp(&key)).unwrap_or_else(|place| place);
        self.index.insert(place, (key, at, len));
        Ok(())
    }

    fn seal(&mut self, cx: &Cx) -> Result<(), CliError> {
        PackStore::seal(self, cx)
    }

    /// One pack store alone keeps every record: nothing to route.
    fn public_crates(&mut self, _subjects: &[ContentId]) {}

    fn after_answer(&mut self, cx: &Cx, _profile: &mut Profile) -> Result<(), CliError> {
        self.seal(cx)?;
        if self.packs.len() > self.packs_most {
            self.merge(cx)?;
        }
        Ok(())
    }
}

/// An origin named on the command line (remote-read.md): `--origin <folder> --origin-key <hex>`
/// after the checkout. Nothing ambient (rule 75): with no `--origin`, nothing is read but the
/// local store. The folder is beneath the start directory; the key is the one Ed25519 public
/// key whose signed head is believed.
pub struct NamedOrigin {
    at: OriginAt,
    check: OneKey,
}

/// Where a named origin's objects are: a folder, or an `http://` base address.
enum OriginAt {
    Folder(Dir<ReadWrite>),
    Http(HttpAt),
}

/// An `http://host:port/prefix` origin: the base it was given, and the network narrowed to
/// that one address (nothing else is reached; nothing ambient).
pub struct HttpAt {
    base: Url,
    net: Net,
}

/// The one trusted key of a named origin, owned, as the read path's head check.
struct OneKey {
    keys: [PublicKey; 1],
}

impl HeadCheck for OneKey {
    fn check(&self, namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy> {
        Ed25519Keys { namespace: Namespace::PUBLIC, keys: &self.keys }.check(namespace, signer, domain, message, signature)
    }
}

impl NamedOrigin {
    /// The origin the arguments name (main.rs reads them): `--origin` and `--origin-key`
    /// together or neither.
    pub fn of(cx: &Cx, fs: &Fs, net: Option<&Net>, folder: Option<&str>, key: Option<&str>) -> Result<Option<NamedOrigin>, CliError> {
        match (folder, key) {
            (None, None) => Ok(None),
            (Some(folder), Some(key)) => {
                let key = PublicKey::from_hex(key).ok_or(CliError::Usage("--origin-key is not 64 hex digits (an Ed25519 public key)"))?;
                let at = if folder.starts_with("https://") {
                    return Err(CliError::Usage("--origin https://: blaze-h1-net has no TLS yet (its `NoTls`); name an http:// origin or a folder"));
                } else if folder.starts_with("http://") {
                    let net = net.ok_or(CliError::Usage("--origin http://: this host offers no network"))?;
                    OriginAt::Http(HttpAt::of(folder, net)?)
                } else {
                    OriginAt::Folder(fs.open_dir(cx, &Path::new(folder)?)?)
                };
                Ok(Some(NamedOrigin { at, check: OneKey { keys: [key] } }))
            }
            _ => Err(CliError::Usage("--origin and --origin-key are named together")),
        }
    }
}

/// Why a read from a named origin stopped the command (not a miss: a miss computes locally).
#[derive(Debug)]
pub enum RemoteError {
    /// The read path refused the batch: past `keys_most`, or begun mid-batch.
    Batch(BatchError),
    /// A fetched record could not be copied into the request's buffer.
    Copy(Stopped),
}

impl core::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RemoteError::Batch(error) => write!(f, "the origin's read path: {error}"),
            RemoteError::Copy(stopped) => write!(f, "a record fetched from the origin: {stopped}"),
        }
    }
}

/// A folder as an origin: each ask one ranged read through bstd's files, bounded by the ask's
/// deadline. An object that is not there is `Missing`; any other failure `Unreadable`.
struct FolderOrigin<'o> {
    cx: &'o Cx,
    folder: &'o Dir<ReadWrite>,
}

impl Origin for FolderOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        let failed = |error: FsError| if error.kind == FsKind::NotFound { OriginError::Missing } else { OriginError::Unreadable };
        // The name is built from a digest's hex only (`Object::name`): nothing fetched is a path.
        let name = object.name();
        let path = Path::new(name.as_str()).map_err(|_| OriginError::Unreadable)?;
        let cx = self.cx.until(Deadline::at(Duration::from_nanos(deadline_ns)));
        let file = self.folder.open_read(&cx, &path).map_err(failed)?;
        file.read_into(into, at).wait(&cx).map_err(failed)
    }
}

impl HttpAt {
    /// The origin at `base` (`http://host:port[/prefix]`), the network narrowed to its address.
    fn of(base: &str, net: &Net) -> Result<HttpAt, CliError> {
        let base = Url::parse(base.trim_end_matches('/')).map_err(|_| CliError::Usage("--origin http://: not a URL"))?;
        let origin = base.origin().ok_or(CliError::Usage("--origin http://: the URL names no host"))?;
        // clone: the origin's address, a refcount.
        let net = net.connect_only(&Addr::Tcp(origin.at().clone()));
        Ok(HttpAt { base, net })
    }
}

/// An `http://` origin: each ask one ranged GET of `<base>/<object name>` through blaze-h1-net
/// on bstd's `Net` (remote-read.md, "HTTP in the tree"). The reply is bounded before it is read
/// (`most_body` is the ask's length), its `Content-Range` must be the range asked (blaze-h1's
/// `Response::ranged`), a redirect is refused (`most: 0`), and the ask's deadline ends every
/// wait. A 404 is `Missing`, a 416 (asked past the object's end) no bytes, anything else
/// `Unreadable` (blaze-h1-net's tests name each refusal). Nothing here trusts the bytes: the
/// read machine checks the signed head and the digest chain.
struct HttpOrigin<'o> {
    cx: &'o Cx,
    at: &'o HttpAt,
    /// The URL's text and the reply's body, reused from ask to ask.
    url: blaze_own::Vec<u8, Global>,
    body: blaze_own::Vec<u8, Global>,
}

impl<'o> HttpOrigin<'o> {
    fn new(cx: &'o Cx, at: &'o HttpAt) -> HttpOrigin<'o> {
        HttpOrigin { cx, at, url: blaze_own::Vec::new_in(Global), body: blaze_own::Vec::new_in(Global) }
    }
}

impl Origin for HttpOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        if into.is_empty() {
            return Ok(0);
        }
        // The name is built from a digest's hex only (`Object::name`): nothing fetched is a path.
        let name = object.name();
        self.url.clear();
        let joined = [self.at.base.as_str().as_bytes(), b"/", name.as_str().as_bytes()];
        for part in joined {
            self.url.try_extend_from_slice(part).map_err(|_| OriginError::Unreadable)?;
        }
        let text = core::str::from_utf8(&self.url).map_err(|_| OriginError::Unreadable)?;
        let url = Url::parse(text).map_err(|_| OriginError::Unreadable)?;
        let limits = H1Limits { most_head: 16 << 10, most_headers: 64, most_body: into.len() as u64 };
        let redirects = Redirects { most: 0, cross_scheme: false, cross_origin: false };
        let client = Client { net: &self.at.net, tls: &NoTls, limits, redirects, agent: "blaze", alloc: Global };
        let range = H1Range { first: at, last: Some(at + into.len() as u64 - 1) };
        let ask = Ask { method: "GET", url: &url, headers: &[], body: &[], range: Some(range) };
        let cx = self.cx.until(Deadline::at(Duration::from_nanos(deadline_ns)));
        self.body.clear();
        match client.send(&cx, &ask, &mut self.body) {
            Ok(_) => {
                // `most_body` held the body to `into.len()`.
                let got = self.body.len().min(into.len());
                into[..got].copy_from_slice(&self.body[..got]);
                Ok(got)
            }
            Err(error) => {
                let status = match &error.why {
                    FetchWhy::Http(blaze_h1::Error { why: blaze_h1::Why::RangeStatus { status }, .. }) => Some(*status),
                    _ => None,
                };
                match status {
                    Some(404) => Err(OriginError::Missing),
                    Some(416) => Ok(0),
                    _ => Err(OriginError::Unreadable),
                }
            }
        }
    }
}

/// Which records belong in the machine store (remote-read.md, "One chain of stores"): the one
/// seam where the choice is made (rule 78). The command's is [`ToolchainCrates`].
pub trait Sharing {
    /// The crates whose records hold nothing of a project, by their `CrateScope` subjects.
    fn add_public(&mut self, subjects: &[ContentId]);
    /// Whether `bytes`, a record about to be put, belongs in the machine store.
    fn shared(&self, bytes: &[u8]) -> bool;
}

/// A drive record is public when its name and every name it reads are the `CrateScope` of a
/// toolchain crate (target.md, "Namespaces"). Anything else, a certificate, a root, a page or
/// a record that read one checkout file, stays the project's.
pub struct ToolchainCrates {
    /// Sorted, without repeats.
    subjects: std::vec::Vec<ContentId, Global>,
}

impl ToolchainCrates {
    pub fn new() -> ToolchainCrates {
        ToolchainCrates { subjects: std::vec::Vec::new_in(Global) }
    }
}

impl Sharing for ToolchainCrates {
    fn add_public(&mut self, subjects: &[ContentId]) {
        self.subjects.extend_from_slice(subjects);
        self.subjects.sort_unstable();
        self.subjects.dedup();
    }

    fn shared(&self, bytes: &[u8]) -> bool {
        let public = |name: Name| name.kind == Kind::CrateScope && self.subjects.binary_search(&name.subject).is_ok();
        match blaze_drive::record::open(bytes) {
            Ok(Stored::Whole(view)) => public(view.name) && view.reads().all(|(name, _)| public(name)),
            Ok(Stored::Shape(view)) => public(view.name) && view.names().all(public),
            // Not a drive record (a certificate, a root, a page): the project's.
            Err(_) => false,
        }
    }
}

/// Where the machine store is this command: the home is passed in from the command's start
/// (rule 75) and reached through the effect layer as a second tree ([`Fs::home`]), wherever
/// it is.
pub enum Machine {
    /// blaze's folder in the home ([`HOME_FOLDER`]): the machine store is [`SHARED`] in it.
    Home(Dir<ReadWrite>),
    /// No machine store this command, and why (said once by [`Layered::open`]).
    Off(MachineOff),
}

/// Why a command has no machine store.
#[derive(Debug)]
pub enum MachineOff {
    /// The environment names no home.
    NoHome,
    /// The home could not be opened, bstd saying why (and naming it).
    Home(bstd::fs::HomeError),
}

impl fmt::Display for MachineOff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MachineOff::NoHome => f.write_str("HOME is not set"),
            MachineOff::Home(error) => write!(f, "{error}"),
        }
    }
}

impl Machine {
    /// From `HOME` as main.rs read it (absolute): blaze's folder in it, wherever it is.
    pub fn of(cx: &Cx, fs: &Fs, home: Option<&str>) -> Result<Machine, CliError> {
        let Some(home) = home else { return Ok(Machine::Off(MachineOff::NoHome)) };
        match fs.home(cx, home, &Path::new(HOME_FOLDER)?) {
            Ok(dir) => Ok(Machine::Home(dir)),
            Err(error) => Ok(Machine::Off(MachineOff::Home(error))),
        }
    }
}

/// The chain of stores a get goes down (remote-read.md, "One chain of stores"): the project's
/// store, then the machine store, then a named origin. A key missed by both stores is asked of
/// the origin; what comes back is checked by the read path against the signed head's chain of
/// digests, written through to the project's store, then answered. What is not fetched (the
/// origin unreadable, late, refused, short) is a typed miss: one line on stderr says what and
/// why for each kind, and the request computes the record locally. That is not a fallback
/// (remote-read.md, "Not fetched"): the local computation is the record's definition and the
/// origin a cache of it byte for byte, so both routes give equal bytes. A put goes to the
/// machine store when [`Sharing`] says the record holds nothing of a project, else to the
/// project's: the machine store holds only what this machine computed.
pub struct Layered<'r, S: Sharing = ToolchainCrates> {
    local: PackStore,
    machine: Option<PackStore>,
    sharing: S,
    plant: StorePlant,
    err: &'r Out,
    /// Records the machine store answered and took this command.
    machine_found: u32,
    machine_put: u32,
    remote: Option<Remote<'r>>,
}

struct Remote<'r> {
    named: &'r NamedOrigin,
    reader: Reader<'r>,
    clock: Clock,
    err: &'r Out,
    alloc: Alloc<'r>,
    /// Time spent reading the origin and checking what came back.
    spent_ns: u64,
    /// One bit for each kind of "not fetched" already said.
    said: u8,
}

impl<'r> Layered<'r, ToolchainCrates> {
    /// The project's store under `root`, then the machine store, then `origin` when one is
    /// named. The read path's tables live in `alloc`, the command's frame.
    #[allow(clippy::too_many_arguments)]
    pub fn open(cx: &Cx, root: &Dir<ReadWrite>, machine: &Machine, boot: Boot, origin: Option<&'r NamedOrigin>, clock: Clock, err: &'r Out, alloc: Alloc<'r>) -> Result<Layered<'r>, CliError> {
        let remote = origin.map(|named| Remote {
            named,
            reader: Reader::new_in(Namespace::PUBLIC, &named.check, Limits::DEFAULT, alloc),
            clock,
            err,
            alloc,
            spent_ns: 0,
            said: 0,
        });
        let home = match machine {
            Machine::Home(home) => Some(home),
            Machine::Off(why) => {
                err.line(cx, format_args!("blaze: machine store off: {why}; the toolchain's records are kept in the project's store"))?;
                None
            }
        };
        Layered::open_chain(cx, root, home, boot, ToolchainCrates::new(), PACKS_MOST, StorePlant::None, err, remote)
    }
}

impl<'r, S: Sharing> Layered<'r, S> {
    #[allow(clippy::too_many_arguments)]
    fn open_chain(cx: &Cx, root: &Dir<ReadWrite>, home: Option<&Dir<ReadWrite>>, boot: Boot, sharing: S, packs_most: usize, plant: StorePlant, err: &'r Out, remote: Option<Remote<'r>>) -> Result<Layered<'r, S>, CliError> {
        // A planted bug is the machine store's (the project's has its own tests above).
        let local = PackStore::open_in(cx, root, PACKS, boot, packs_most, StorePlant::None)?;
        if local.refused > 0 {
            err.line(cx, format_args!("blaze: {} torn pack(s) under {PACKS} (a crash while one was written?): removed; their records are made again", local.refused))?;
        }
        let machine = match home {
            Some(home) => {
                let machine = PackStore::open_in(cx, home, SHARED, boot, packs_most, plant)?;
                if machine.refused > 0 {
                    err.line(cx, format_args!("blaze: {} torn pack(s) under the home's {SHARED} (a crash while one was written?): removed; their records are made again", machine.refused))?;
                }
                Some(machine)
            }
            None => None,
        };
        Ok(Layered { local, machine, sharing, plant, err, machine_found: 0, machine_put: 0, remote })
    }
}

impl Remote<'_> {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.clock.now().since_start().as_nanos()).unwrap_or(u64::MAX)
    }

    /// One "not fetched" line for each kind, the first time it happens in this command.
    fn say(&mut self, cx: &Cx, key: Key, why: &Unfetched) -> Result<(), CliError> {
        let kind = match why {
            Unfetched::Origin { .. } => 0,
            Unfetched::Short { .. } => 1,
            Unfetched::Late { .. } => 2,
            Unfetched::Refused(_) => 3,
            Unfetched::Limit { .. } => 4,
        };
        if self.said & (1 << kind) != 0 {
            return Ok(());
        }
        self.said |= 1 << kind;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut short = [0u8; 12];
        for (at, byte) in key.0.0[..6].iter().enumerate() {
            short[at * 2] = HEX[usize::from(byte >> 4)];
            short[at * 2 + 1] = HEX[usize::from(byte & 15)];
        }
        let short = core::str::from_utf8(&short).map_err(|_| CliError::Text { what: "a key's hex" })?;
        self.err.line(cx, format_args!("blaze: origin: record {short}… not fetched, computed here: {why}"))?;
        Ok(())
    }
}

impl<S: Sharing> Store for Layered<'_, S> {
    fn public_crates(&mut self, subjects: &[ContentId]) {
        self.sharing.add_public(subjects);
    }

    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError> {
        if self.local.get_into(cx, key, into)? {
            return Ok(true);
        }
        if let Some(machine) = &mut self.machine
            && machine.get_into(cx, key, into)?
        {
            self.machine_found += 1;
            return Ok(true);
        }
        let Some(remote) = &mut self.remote else { return Ok(false) };
        let began = remote.now_ns();
        let clock = remote.clock.clone();
        let mut now = || u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
        // A batch of one key: the request asks one record at a time (blaze-drive's `Get`).
        // `BatchError` is only a batch past `keys_most` or one begun mid-batch: neither can be.
        let fetched = match &remote.named.at {
            OriginAt::Folder(folder) => fetch(&mut remote.reader, &mut FolderOrigin { cx, folder }, &mut now, core::slice::from_ref(&key.0), remote.alloc),
            OriginAt::Http(at) => fetch(&mut remote.reader, &mut HttpOrigin::new(cx, at), &mut now, core::slice::from_ref(&key.0), remote.alloc),
        };
        fetched.map_err(|error| CliError::Remote(RemoteError::Batch(error)))?;
        let answer = remote.reader.answer(0);
        let found = match answer {
            Answer::Found { .. } => {
                let bytes = remote.reader.found(0).unwrap_or(&[]);
                // Written through before it is answered: the next command finds it here.
                self.local.put(cx, key, bytes)?;
                into.try_extend(bytes).map_err(|stopped| CliError::Remote(RemoteError::Copy(stopped)))?;
                true
            }
            Answer::Absent | Answer::Pending => false,
            Answer::Unfetched(why) => {
                remote.say(cx, key, &why)?;
                false
            }
        };
        remote.spent_ns += remote.now_ns().saturating_sub(began);
        Ok(found)
    }

    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        let shared = self.plant == StorePlant::PrivateIntoMachine || self.sharing.shared(bytes);
        match (&mut self.machine, shared) {
            (Some(machine), true) => {
                self.machine_put += 1;
                machine.put(cx, key, bytes)
            }
            _ => self.local.put(cx, key, bytes),
        }
    }

    /// Both stores before anything names their records: the answer's pointer names project
    /// records whose reads may be the machine store's.
    fn seal(&mut self, cx: &Cx) -> Result<(), CliError> {
        if let Some(machine) = &mut self.machine {
            machine.seal(cx)?;
        }
        self.local.seal(cx)
    }

    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError> {
        self.local.after_answer(cx, profile)?;
        if let Some(machine) = &mut self.machine {
            machine.after_answer(cx, profile)?;
            // Facts of the run, not news: the profile's rows (and `--stats`), never plain stderr.
            profile.machine_found = self.machine_found;
            profile.machine_put = self.machine_put;
            profile.machine_packs = u32::try_from(machine.packs.len()).unwrap_or(u32::MAX);
            profile.machine_read_whole = machine.verified;
        }
        if let Some(remote) = &self.remote {
            let r = remote.reader.report();
            remote.err.line(
                cx,
                format_args!(
                    "blaze: origin: {} keys asked, {} found, {} absent, {} not fetched; {} range reads (head {}, manifest {}, summary {}, index {}, payload {}), {} late; {} bytes fetched for {} used; {:.3} ms",
                    r.keys,
                    r.found,
                    r.absent,
                    r.unfetched,
                    r.asks,
                    r.head_asks,
                    r.manifest_asks,
                    r.summary_asks,
                    r.page_asks,
                    r.payload_asks,
                    r.late,
                    r.bytes_fetched,
                    r.bytes_used,
                    remote.spent_ns as f64 / 1e6,
                ),
            )?;
        }
        Ok(())
    }
}

/// The pack store's proof (rule 43): simulations, never review. Each record's bytes are a
/// function of its key, so any bytes a get returns are checked without a model. A witness
/// file, written unsynced after each seal with the boot it was written in, names the keys
/// sealed: within one boot (no power lost) none of them may be lost; after a power cut any may.
#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape, check as sim_check};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::{BUDGET, Given, Seeds, seeded};

    use super::*;

    /// The keys a test draws from.
    const KEYS: u32 = 40;
    const WITNESS: &str = "witness";

    /// The simulated machine's boot: a new one at every power cut, as the kernel's.
    fn boot_of(io: &Io) -> Boot {
        let cuts = io.with_sim(|sim| sim.counts().cuts).unwrap_or(0);
        let mut boot = [0u8; 16];
        boot[..8].copy_from_slice(&cuts.to_le_bytes());
        boot[8] = 1;
        Boot::Known(boot)
    }

    fn boot_bytes(boot: Boot) -> [u8; 16] {
        match boot {
            Boot::Known(bytes) => bytes,
            Boot::Unknown => [0; 16],
        }
    }

    fn key(k: u32) -> Key {
        Key(blaze_ids::ContentId::of("blaze-cli store test", &k.to_le_bytes()))
    }

    /// Key `k`'s one value (rule 21): its length and bytes from `k`, some empty, some past a
    /// sector.
    fn payload(k: u32) -> std::vec::Vec<u8> {
        let len = (k as usize * 131) % 700;
        (0..len).map(|i| (k as usize * 7 + i * 13) as u8).collect()
    }

    /// Every key: a get is the key's value or a miss, never other bytes; each in `witnessed`
    /// is held. A finding panics: it fails the seed (an error would be a restart).
    fn oracle(cx: &Cx, store: &mut impl Store, witnessed: &[u32]) -> Result<std::vec::Vec<u32>, CliError> {
        let frame = cx.frame();
        let mut held = std::vec::Vec::new();
        for k in 0..KEYS {
            let mut into = Bytes::new_in(frame, "a record read back");
            if store.get_into(cx, key(k), &mut into)? {
                assert!(into.as_slice() == payload(k).as_slice(), "store oracle: key {k} read back as other bytes ({} for {})", into.len(), payload(k).len());
                held.push(k);
            } else {
                assert!(!witnessed.contains(&k), "store oracle: key {k}, which a witness stands on, is lost");
            }
        }
        Ok(held)
    }

    /// The keys the witness names, when it was written in `boot`; none from another boot.
    fn read_witness(cx: &Cx, root: &Dir<ReadWrite>, boot: Boot) -> Result<std::vec::Vec<u32>, CliError> {
        let mut bytes = Bytes::new_in(cx.frame(), "the witness");
        match root.read_all(cx, &Path::new(WITNESS)?, &mut bytes) {
            Ok(_) => {}
            Err(error) if error.kind == FsKind::NotFound => return Ok(std::vec::Vec::new()),
            Err(error) => return Err(error.into()),
        }
        let bytes = bytes.as_slice();
        if bytes.len() < 16 || bytes[..16] != boot_bytes(boot) {
            return Ok(std::vec::Vec::new());
        }
        Ok(bytes[16..].chunks_exact(4).map(le_u32).collect())
    }

    /// One start of the program the runner cuts and restarts, as a command is: the oracle on
    /// what the home holds, then rounds of puts, a seal, the witness (unsynced, as `last`)
    /// and a merge past two packs.
    fn program(cx: &Cx, io: &Io, plant: StorePlant) -> Result<(), CliError> {
        let root = io.need_fs().map_err(CliError::Missing)?.root();
        let boot = boot_of(io);
        let witnessed = read_witness(cx, &root, boot)?;
        let mut store = PackStore::open_with(cx, &root, boot, 2, plant)?;
        oracle(cx, &mut store, &witnessed)?;
        for round in 0..8u32 {
            for k in round * 4..(round * 4 + 7).min(KEYS) {
                store.put(cx, key(k), &payload(k))?;
            }
            store.seal(cx)?;
            let held = oracle(cx, &mut store, &witnessed)?;
            let mut witness = std::vec::Vec::from(boot_bytes(boot));
            witness.extend(held.iter().flat_map(|k| k.to_le_bytes()));
            root.publish(cx, &Path::new(WITNESS)?, Content::bytes(&witness), Durable::No)?;
            let mut profile = Profile::default();
            store.after_answer(cx, &mut profile)?;
            let witnessed = read_witness(cx, &root, boot)?;
            oracle(cx, &mut store, &witnessed)?;
        }
        Ok(())
    }

    fn crashes(count: u32, plant: StorePlant) -> Result<bstd::test::Passed, bstd::test::Failed> {
        seeded("blaze-cli", module_path!(), "pack_store_crashes", Seeds::DEFAULT.from(0, count), Given::NOTHING, |cx: &Cx, io: Io| {
            program(cx, &io, plant)
        })
    }

    /// (1) and (2): power cut at effects drawn by each seed (several a seed, the program
    /// started again after each, in a new boot), I/O failures injected, delays, nothing synced:
    /// no get returns bytes other than its record's, none a witness of this boot names is
    /// lost, and the healed last start answers.
    #[test]
    fn a_crash_or_failure_anywhere_keeps_each_record_whole_or_absent() {
        match crashes(1000, StorePlant::None) {
            Ok(passed) => {
                // Proof the faults ran (rule 61).
                assert!(passed.counts.cuts > 500, "too few power cuts: {:?}", passed.counts);
                assert!(passed.counts.failures > 500, "too few failures: {:?}", passed.counts);
            }
            Err(failed) => panic!("{failed}"),
        }
    }

    fn caught(plant: StorePlant, says: &str) {
        match crashes(1000, plant) {
            Ok(passed) => panic!("the planted {plant:?} was not caught in {} starts", passed.starts),
            Err(failed) => {
                let said = std::format!("{failed}");
                assert!(said.contains(says), "the planted {plant:?} failed otherwise: {said}");
            }
        }
    }

    #[test]
    fn the_oracle_catches_a_merge_that_removes_first() {
        caught(StorePlant::MergeRemovesFirst, "is lost");
    }

    // The two below need a power cut that keeps an unsynced rename and leaves the pack's
    // footer whole over wrong payloads: the disk model tears each unsynced sector on its own
    // (blaze-core `world::disk::Torn::Sectors`, 2026-11-16), so a pack's end may survive while
    // its middle is a hole of zeros.
    #[test]
    fn the_oracle_catches_a_skipped_verification_after_a_power_cut() {
        caught(StorePlant::VerifySkipped, "store oracle");
    }

    #[test]
    fn the_oracle_catches_a_pack_vouching_for_itself_after_a_power_cut() {
        caught(StorePlant::SelfVouching, "store oracle");
    }

    /// The chain's test sharing: a record is public when its length is even, a function of
    /// its key, so the oracle knows which store each belongs in.
    struct EvenLengths;

    impl Sharing for EvenLengths {
        fn add_public(&mut self, _subjects: &[ContentId]) {}

        fn shared(&self, bytes: &[u8]) -> bool {
            bytes.len() % 2 == 0
        }
    }

    /// The user's home, absolute, outside the project's folder.
    const HOME: &str = "/home/sim";
    /// The project's folder, beneath the simulation's root.
    const PROJECT: &str = "work/project";

    /// A home with a file where blaze's folder should be: no machine store, and the one line
    /// [`Layered::open`] says names the home.
    #[test]
    fn a_home_that_cannot_be_made_turns_the_machine_store_off_by_name() {
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let fs = io.need_fs().expect("fs");
        fs.root().publish(&cx, &Path::new("home/sim/.blaze").expect("a path"), Content::bytes(b"in the way"), Durable::No).expect("a file in the way");
        match Machine::of(&cx, &fs, Some(HOME)).expect("no harness error") {
            Machine::Home(_) => panic!("a file at the home's folder was taken as the folder"),
            Machine::Off(why) => {
                let said = std::format!("{why}");
                assert!(said.contains(HOME) && said.contains("directory"), "the line names the home: {said}");
            }
        }
    }

    /// The machine store as another command finds it: no record of a project in it.
    fn machine_alone(cx: &Cx, home: &Dir<ReadWrite>, boot: Boot, plant: StorePlant) -> Result<(), CliError> {
        let mut machine = PackStore::open_in(cx, home, SHARED, boot, 2, plant)?;
        for k in 0..KEYS {
            let mut into = Bytes::new_in(cx.frame(), "a machine record read back");
            if machine.get_into(cx, key(k), &mut into)? {
                assert!(payload(k).len() % 2 == 0, "machine oracle: key {k}, a project's record, is in the machine store");
            }
        }
        Ok(())
    }

    /// One start of a command whose chain is the project's store and the machine store under
    /// `home` (remote-read.md, "One chain of stores"): every get through the chain is its
    /// record or a miss and none a witness names is lost, and the machine store holds no
    /// project record, before and after rounds of puts, seals and merges.
    fn chained(cx: &Cx, io: &Io, plant: StorePlant) -> Result<(), CliError> {
        // The project's folder and the home apart, as on a real machine: the home reached
        // only as `Machine::of` reaches it, from its absolute path.
        let fs = io.need_fs().map_err(CliError::Missing)?;
        let root = fs.root().subdir(&Path::new(PROJECT)?);
        let home = match Machine::of(cx, &fs, Some(HOME))? {
            Machine::Home(home) => home,
            // A failure the simulation injected into the home's open ends this start as any
            // failed effect does (a real command goes on with the store off and says so).
            Machine::Off(MachineOff::Home(bstd::fs::HomeError { why: bstd::fs::HomeWhy::Fs(error), .. })) => return Err(error.into()),
            Machine::Off(why) => panic!("the simulation's home was refused: {why}"),
        };
        let err = io.need_err().map_err(CliError::Missing)?;
        let boot = boot_of(io);
        let witnessed = read_witness(cx, &root, boot)?;
        machine_alone(cx, &home, boot, plant)?;
        let mut store = Layered::open_chain(cx, &root, Some(&home), boot, EvenLengths, 2, plant, &err, None)?;
        oracle(cx, &mut store, &witnessed)?;
        for round in 0..8u32 {
            for k in round * 4..(round * 4 + 7).min(KEYS) {
                store.put(cx, key(k), &payload(k))?;
            }
            store.seal(cx)?;
            let held = oracle(cx, &mut store, &witnessed)?;
            let mut witness = std::vec::Vec::from(boot_bytes(boot));
            witness.extend(held.iter().flat_map(|k| k.to_le_bytes()));
            root.publish(cx, &Path::new(WITNESS)?, Content::bytes(&witness), Durable::No)?;
            let mut profile = Profile::default();
            store.after_answer(cx, &mut profile)?;
            let witnessed = read_witness(cx, &root, boot)?;
            oracle(cx, &mut store, &witnessed)?;
        }
        drop(store);
        machine_alone(cx, &home, boot, plant)
    }

    fn chain_crashes(count: u32, plant: StorePlant) -> Result<bstd::test::Passed, bstd::test::Failed> {
        seeded("blaze-cli", module_path!(), "pack_store_chain", Seeds::DEFAULT.from(0, count), Given::NOTHING, |cx: &Cx, io: Io| {
            chained(cx, &io, plant)
        })
    }

    /// Power cuts, failures and delays anywhere in a chain of two pack stores: each record
    /// whole or absent through the chain, none a witness of this boot names lost, and the
    /// machine store holding only what is public.
    #[test]
    fn a_crash_anywhere_in_the_chain_keeps_records_whole_and_the_machine_store_public() {
        match chain_crashes(1000, StorePlant::None) {
            Ok(passed) => {
                // Proof the faults ran (rule 61).
                assert!(passed.counts.cuts > 500, "too few power cuts: {:?}", passed.counts);
                assert!(passed.counts.failures > 500, "too few failures: {:?}", passed.counts);
            }
            Err(failed) => panic!("{failed}"),
        }
    }

    fn chain_caught(plant: StorePlant, says: &str) {
        match chain_crashes(1000, plant) {
            Ok(passed) => panic!("the planted {plant:?} was not caught in {} starts", passed.starts),
            Err(failed) => {
                let said = std::format!("{failed}");
                assert!(said.contains(says), "the planted {plant:?} failed otherwise: {said}");
            }
        }
    }

    #[test]
    fn the_chain_oracle_catches_a_project_record_in_the_machine_store() {
        chain_caught(StorePlant::PrivateIntoMachine, "machine oracle");
    }

    /// The machine store's packs are published unsynced like the project's: taking one from
    /// an earlier boot without its once-a-boot read hands back torn bytes.
    #[test]
    fn the_chain_oracle_catches_a_machine_pack_taken_unverified() {
        chain_caught(StorePlant::VerifySkipped, "store oracle");
    }

    fn sim(tape: &Tape) -> Result<(Io, Cx), Broke> {
        let io = Io::simulated(SimWorld::new(tape.below(1 << 30, "seed") as u64, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        Ok((io, cx))
    }

    fn harness(what: &'static str) -> impl Fn(CliError) -> Broke {
        move |error| Broke::Harness(std::format!("{what}: {error}"))
    }

    /// The open's checks in the boot after the one that sealed: a pack cut to any length, or
    /// with any bit of its payloads or footer changed (what a power cut leaves of an unsynced
    /// write), is refused, removed and said; nothing is read from it; the other packs stand.
    fn torn(tape: &Tape, plant: StorePlant) -> Result<(), Broke> {
        let (io, cx) = sim(tape)?;
        let root = io.need_fs().map_err(|e| Broke::Harness(std::format!("{e}")))?.root();
        let sealed_in = Boot::Known([1; 16]);
        let mut store = PackStore::open_with(&cx, &root, sealed_in, PACKS_MOST, plant).map_err(harness("open"))?;
        for k in 0..KEYS / 2 {
            store.put(&cx, key(k), &payload(k)).map_err(harness("put"))?;
        }
        store.seal(&cx).map_err(harness("seal"))?;
        for k in KEYS / 2..KEYS {
            store.put(&cx, key(k), &payload(k)).map_err(harness("put"))?;
        }
        store.seal(&cx).map_err(harness("seal"))?;
        let victim = store.packs[tape.below(2, "which pack") as usize].0;
        let leaf = pack_name(victim).map_err(harness("name"))?;
        let name = Path::new(&std::format!("{PACKS}/{}", leaf.as_str())).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        let mut bytes = Bytes::new_in(cx.frame(), "the pack");
        root.read_all(&cx, &name, &mut bytes).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        let mut bytes = bytes.as_slice().to_vec();
        let index_at = le_u64(&bytes[bytes.len() - FOOTER + 16..]) as usize;
        match tape.below(3, "how torn") {
            0 => bytes.truncate(tape.below(bytes.len() as u64, "cut at") as usize),
            1 => {
                let at = bytes.len() - FOOTER + tape.below(FOOTER as u64, "footer byte") as usize;
                bytes[at] ^= 1 << tape.below(8, "bit");
            }
            _ if index_at > 0 => {
                let at = tape.below(index_at as u64, "payload byte") as usize;
                bytes[at] ^= 1 << tape.below(8, "bit");
            }
            _ => bytes.truncate(0),
        }
        root.publish(&cx, &name, Content::bytes(&bytes), Durable::No).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        drop(store);
        let mut store = match PackStore::open_with(&cx, &root, Boot::Known([2; 16]), PACKS_MOST, plant) {
            Ok(store) => store,
            Err(error) => return Err(Broke::Oracle(std::format!("store oracle: a torn pack refused the open: {error}"))),
        };
        let frame = cx.frame();
        let mut held = 0;
        for k in 0..KEYS {
            let mut into = Bytes::new_in(frame, "a record read back");
            match store.get_into(&cx, key(k), &mut into) {
                Ok(true) if into.as_slice() == payload(k).as_slice() => held += 1,
                Ok(true) => return Err(Broke::Oracle(std::format!("store oracle: key {k} read back as other bytes"))),
                Ok(false) => {}
                Err(error) => return Err(Broke::Oracle(std::format!("store oracle: a get after a torn pack failed: {error}"))),
            }
        }
        if store.refused != 1 || held != KEYS / 2 {
            return Err(Broke::Oracle(std::format!("store oracle: {} refused, {held} held: one pack refused and the other whole", store.refused)));
        }
        Ok(())
    }

    #[test]
    fn a_torn_pack_is_refused_and_the_rest_stands() {
        sim_check(Config::cases(500), |tape| torn(tape, StorePlant::None));
    }

    #[test]
    fn the_torn_oracle_catches_a_skipped_verification() {
        blaze_sim::control(Config::cases(2000), StorePlant::None, StorePlant::VerifySkipped, "store oracle", torn);
    }

    #[test]
    fn the_torn_oracle_catches_a_pack_vouching_for_itself() {
        blaze_sim::control(Config::cases(2000), StorePlant::None, StorePlant::SelfVouching, "store oracle", torn);
    }

    /// A pack sealed in this boot is taken without a read of its payloads; one from another
    /// boot is read once and then named in the boot record, so the next open reads none.
    #[test]
    fn a_pack_is_read_whole_once_a_boot() {
        sim_check(Config::cases(50), |tape| {
            let (io, cx) = sim(tape)?;
            let root = io.need_fs().map_err(|e| Broke::Harness(std::format!("{e}")))?.root();
            let (one, two) = (Boot::Known([1; 16]), Boot::Known([2; 16]));
            let mut store = PackStore::open_with(&cx, &root, one, PACKS_MOST, StorePlant::None).map_err(harness("open"))?;
            for k in 0..KEYS {
                store.put(&cx, key(k), &payload(k)).map_err(harness("put"))?;
            }
            store.seal(&cx).map_err(harness("seal"))?;
            let counts = |boot| PackStore::open_with(&cx, &root, boot, PACKS_MOST, StorePlant::None).map(|store| (store.verified, store.packs.len())).map_err(harness("open again"));
            let seen = [counts(one)?, counts(two)?, counts(two)?, counts(Boot::Unknown)?];
            if seen != [(0, 1), (1, 1), (0, 1), (1, 1)] {
                return Err(Broke::Oracle(std::format!("store oracle: (verified, packs) at the opens {seen:?}")));
            }
            Ok(())
        });
    }

    /// (3) Two commands on one home, their steps interleaved: puts, seals, gets, merges and
    /// opens again, each drawn. Every get is the key's value or a miss; a fresh open at the end
    /// holds every key either sealed.
    fn two(tape: &Tape, plant: StorePlant) -> Result<(), Broke> {
        let (io, cx) = sim(tape)?;
        let root = io.need_fs().map_err(|e| Broke::Harness(std::format!("{e}")))?.root();
        let open = || PackStore::open_with(&cx, &root, Boot::Known([1; 16]), 2, plant).map_err(harness("open"));
        let mut stores = [open()?, open()?];
        let mut sealed = [false; KEYS as usize];
        let mut put = [std::vec::Vec::new(), std::vec::Vec::new()];
        let frame = cx.frame();
        for _ in 0..tape.below(60, "steps") {
            let who = tape.below(2, "which command") as usize;
            let store = &mut stores[who];
            match tape.below(5, "step") {
                0 => {
                    let k = tape.below(KEYS as u64, "key") as u32;
                    store.put(&cx, key(k), &payload(k)).map_err(harness("put"))?;
                    put[who].push(k);
                }
                1 => {
                    store.seal(&cx).map_err(harness("seal"))?;
                    for k in put[who].drain(..) {
                        sealed[k as usize] = true;
                    }
                }
                2 => {
                    let k = tape.below(KEYS as u64, "key") as u32;
                    let mut into = Bytes::new_in(frame, "a record read back");
                    let got = store.get_into(&cx, key(k), &mut into).map_err(|e| Broke::Oracle(std::format!("store oracle: a get beside another command failed: {e}")))?;
                    if got && into.as_slice() != payload(k).as_slice() {
                        return Err(Broke::Oracle(std::format!("store oracle: key {k} read back as other bytes")));
                    }
                }
                3 => {
                    store.seal(&cx).map_err(harness("seal"))?;
                    for k in put[who].drain(..) {
                        sealed[k as usize] = true;
                    }
                    if store.packs.len() > 1 {
                        store.merge(&cx).map_err(|e| Broke::Oracle(std::format!("store oracle: a merge beside another command failed: {e}")))?;
                    }
                }
                _ => {
                    put[who].clear();
                    *store = open()?;
                }
            }
        }
        let mut fresh = open()?;
        for k in 0..KEYS {
            let mut into = Bytes::new_in(frame, "a record read back");
            let got = fresh.get_into(&cx, key(k), &mut into).map_err(harness("get"))?;
            if got && into.as_slice() != payload(k).as_slice() {
                return Err(Broke::Oracle(std::format!("store oracle: key {k} read back as other bytes")));
            }
            if sealed[k as usize] && !got {
                return Err(Broke::Oracle(std::format!("store oracle: key {k}, sealed by one command, lost beside the other")));
            }
        }
        Ok(())
    }

    #[test]
    fn two_commands_on_one_home_lose_nothing() {
        sim_check(Config::cases(1000), |tape| two(tape, StorePlant::None));
    }

    /// (2) A full disk at the seal: a typed failure; the home still opens, holds every record
    /// sealed before, and none other.
    #[test]
    fn a_full_disk_at_the_seal_fails_by_name_and_keeps_the_home() {
        sim_check(Config::cases(200), |tape| {
            let (io, cx) = sim(tape)?;
            let root = io.need_fs().map_err(|e| Broke::Harness(std::format!("{e}")))?.root();
            let mut store = PackStore::open_with(&cx, &root, Boot::Known([1; 16]), PACKS_MOST, StorePlant::None).map_err(harness("open"))?;
            for k in 0..KEYS / 2 {
                store.put(&cx, key(k), &payload(k)).map_err(harness("put"))?;
            }
            store.seal(&cx).map_err(harness("seal"))?;
            for k in KEYS / 2..KEYS {
                store.put(&cx, key(k), &payload(k)).map_err(harness("put"))?;
            }
            let packs = blaze_core::FileName::new(PACKS).ok_or(Broke::Harness(std::string::String::from("the packs' folder as a name")))?;
            let room = tape.below(4096, "room");
            io.with_sim(|sim| sim.fill(packs, room));
            match store.seal(&cx) {
                Err(CliError::Fs(error)) if matches!(error.kind, FsKind::Driver(..)) || std::format!("{error}").contains("space") => {}
                Err(other) => return Err(Broke::Oracle(std::format!("store oracle: a full disk failed the seal as {other}"))),
                Ok(()) => return Err(Broke::Oracle(std::string::String::from("store oracle: a seal past a full disk succeeded"))),
            }
            let mut store = PackStore::open_with(&cx, &root, Boot::Known([1; 16]), PACKS_MOST, StorePlant::None).map_err(harness("open after"))?;
            let frame = cx.frame();
            for k in 0..KEYS {
                let mut into = Bytes::new_in(frame, "a record read back");
                let got = store.get_into(&cx, key(k), &mut into).map_err(harness("get"))?;
                if got != (k < KEYS / 2) || (got && into.as_slice() != payload(k).as_slice()) {
                    return Err(Broke::Oracle(std::format!("store oracle: key {k} after a full disk: held {got}")));
                }
            }
            Ok(())
        });
    }
}
