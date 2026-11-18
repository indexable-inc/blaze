//! `engine-check`'s stored answer (engine.md E6, the incremental half): the engine's answer and
//! causes as pages (blaze-engine pages.rs), the leaf table of what the request read (sweep.rs
//! `Leaves`), and a sealed pointer naming both (seal.rs), under one root of the store per
//! checkout and tool table ([`root_name`]).
//!
//! The write, after the answer and before the store's one commit: the pages not under the last
//! root, the answer's root, the sealed pointer; then the root name pinned to the pointer and
//! `<name>.pages` to the answer's root and every page (a root's keys are a set to the store, so
//! the pointer is alone under its own). The commit after the answer makes both visible at once
//! (the store's generation flip); a crash before it leaves the last answer whole (pages_sim.rs).
//!
//! The read, before any work: the root name's one key is the sealed pointer; the sweep
//! follows its leaf table; nothing moved, nothing owed and no findings kept is the certificate,
//! from the pointer and the printed lines' record (engine_lines.rs, one get), no page read.
//! Else the pointer names the answer's root, and each page is checked against its key
//! ([`load_answer`]). One that cannot be read is [`Unreadable`]: a note says which and why, and
//! the request answers in full and writes its answer anew (a stated path, rule 3). A pointer of
//! an older form is an old home's answer: reported as such and rebuilt (rule 26).
//!
//! ```text
//! the root name   engine.<16 hex of H(checkout, every kind's tool)>
//! it names        the sealed pointer's key; `<it>.pages` names the answer root, the page keys
//!                 and the lines record's key
//! sealed pointer  seal.rs form: last = "BEL3" | answer root key [32] | lines key [32] |
//!                 files u32 | printed u8 (bit 0 names checked, 1 bodies whole, 2 every
//!                 printed line is in the lines record) | findings u32 | owed u32 (the
//!                 certificate's facts); leaves = the BCL table
//! ```

use core::fmt;
use core::fmt::Write as _;

use blaze_core::Driver;
use blaze_drive::Key;
use blaze_engine::Tools;
use blaze_engine::pages::{self, Kept, PagesError, Root};
use blaze_ids::ContentId;
use blaze_own::SharedAlloc;
use bstd::Cx;
use bstd::mem::Frame;

use crate::store::{CommandStore, Sharing, Store as _};
use crate::sweep::{Leaves, Rows};
use crate::{CliError, seal};

/// The pointer's form inside its seal.
pub(crate) const POINTER_MAGIC: &[u8; 4] = b"BEL3";
/// The magic, the answer's root, the lines record's key, then the [`Facts`]: files u32, printed
/// u8, findings u32, owed u32.
const POINTER_BYTES: usize = 4 + 32 + 32 + 4 + 1 + 4 + 4;
const POINTER_DOMAIN: &str = "blaze-cli engine-check pointer";

/// The root the checkout's stored answer hangs from, for one table of tools: another build of
/// blaze or another tool's form has its own (a machine's path is hashed here alone: a root is
/// local, never shared).
pub(crate) fn root_name<'f>(checkout: &str, tools: &Tools, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let mut input: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::new_in(frame);
    input.extend_from_slice(checkout.as_bytes());
    for tool in tools.0.iter() {
        input.extend_from_slice(&tool.0.0);
    }
    let id = ContentId::of("blaze-cli engine-check root", &input);
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("engine.");
    for byte in &id.0[..8] {
        // A frame string's write fails only when the frame is full: the name is then short and
        // names no root, so the request answers in full.
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// Why the stored answer is not read: the request answers in full and writes it anew.
#[derive(Debug)]
pub(crate) enum Unreadable {
    /// The root names other than one key (its pointer's).
    Names { bytes: usize },
    /// A record the root names is not in the store.
    Missing { key: Key },
    /// The pointer is of an older form (`BEL1`, `BEL2`, or older still): an old home's answer.
    Older,
    /// The printed lines' record (engine_lines.rs).
    Lines(crate::engine_lines::LinesError),
    /// The pointer's write was cut short.
    Torn(seal::Torn),
    /// A page or the answer's root (pages.rs).
    Pages(PagesError),
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Unreadable::Older = self {
            return write!(f, "the stored engine answer is an old home's answer (its pointer is of an older form than `BEL3`), rebuilt (rule 26)");
        }
        write!(f, "the stored engine answer is not read (engine_answer.rs `load`): ")?;
        match self {
            Unreadable::Names { bytes } => write!(f, "its root names {bytes} bytes, not its pointer's one key")?,
            Unreadable::Missing { key } => write!(f, "{key:?}, which its root names, is not in the store")?,
            Unreadable::Older => {}
            Unreadable::Lines(why) => write!(f, "{why}")?,
            Unreadable::Torn(torn) => write!(f, "{torn}")?,
            Unreadable::Pages(why) => write!(f, "{why}")?,
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

impl Facts {
    /// The facts of `kept`: one pass over its rows, at the write.
    pub(crate) fn of(kept: &Kept<'_>, printed: Printed) -> Facts {
        let answer = &kept.answer;
        let files = answer.rows.values().filter(|row| row.phase.stage == blaze_engine::Stage::Outlines).count();
        Facts {
            files: u32::try_from(files).unwrap_or(u32::MAX),
            printed,
            findings: u32::try_from(answer.with_findings.len()).unwrap_or(u32::MAX),
            owed: u32::try_from(answer.owed_cells.len() + answer.owed_tasks.len()).unwrap_or(u32::MAX),
        }
    }
}

/// The pointer as read: the answer's root, the certificate's facts, and the leaf table of what
/// the last request read.
pub(crate) struct Pointer<'m> {
    pub(crate) root: Key,
    /// The printed lines' record (engine_lines.rs).
    pub(crate) lines: Key,
    pub(crate) facts: Facts,
    pub(crate) leaves: Leaves<'m>,
}

fn key_at(bytes: &[u8], at: usize) -> Key {
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[at..at + 32]);
    Key(ContentId(id))
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// The answer the pointer names, every page checked: its tables and causes, and its root
/// (whose pages the next write does not put again).
pub(crate) fn load_answer<'m, 'a, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, root_key: Key, alloc: SharedAlloc<'a>) -> Result<Result<(Kept<'a>, Root<'a>), Unreadable>, CliError> {
    // Each record through the store; a store's own error stops the command after the decode.
    let mut failed: Option<CliError> = None;
    let mut root_bytes: Option<&'m [u8]> = None;
    let decoded = pages::decode(
        root_key,
        |key| match store.get(cx, key) {
            Ok(found) => {
                let bytes = found.map(|held| held.bytes);
                if key == root_key {
                    root_bytes = bytes;
                }
                bytes
            }
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
    let Some(root_bytes) = root_bytes else { return Ok(Err(Unreadable::Missing { key: root_key })) };
    match pages::decode_root(root_key, root_bytes, alloc) {
        Ok(root) => Ok(Ok((kept, root))),
        Err(why) => Ok(Err(Unreadable::Pages(why))),
    }
}

/// The pointer the root `name` names, read in place: `None` when there is none.
pub(crate) fn load_pointer<'m, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'m, '_, D, S>, name: &str) -> Result<Option<Result<Pointer<'m>, Unreadable>>, CliError> {
    let Some(keys) = store.named(name)? else { return Ok(None) };
    if keys.len() != 32 {
        return Ok(Some(Err(Unreadable::Names { bytes: keys.len() })));
    }
    let first = key_at(keys, 0);
    let Some(held) = store.get(cx, first)? else { return Ok(Some(Err(Unreadable::Missing { key: first }))) };
    let sealed: &'m [u8] = held.bytes;
    if !sealed.starts_with(seal::SEAL_MAGIC) {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let (pointer, leaves) = match seal::open(sealed, seal::Plant::None) {
        Ok(opened) => opened,
        Err(torn) => return Ok(Some(Err(Unreadable::Torn(torn)))),
    };
    if pointer.len() != POINTER_BYTES || !pointer.starts_with(POINTER_MAGIC) {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let (leaves, older) = Leaves::of(leaves)?;
    if older.is_some() {
        return Ok(Some(Err(Unreadable::Older)));
    }
    let facts = Facts { files: word(pointer, 68), printed: Printed::of_byte(pointer[72]), findings: word(pointer, 73), owed: word(pointer, 77) };
    if pointer[72] > 7 {
        return Ok(Some(Err(Unreadable::Older)));
    }
    Ok(Some(Ok(Pointer { root: key_at(pointer, 4), lines: key_at(pointer, 36), facts, leaves })))
}

/// The printed lines' record the pointer names, every entry checked (engine_lines.rs `decode`).
pub(crate) fn load_lines<'m, 'a, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    key: Key,
    alloc: SharedAlloc<'a>,
) -> Result<Result<crate::engine_lines::Last<'m, 'a>, Unreadable>, CliError> {
    let Some(held) = store.get(cx, key)? else { return Ok(Err(Unreadable::Missing { key })) };
    let bytes: &'m [u8] = held.bytes;
    Ok(crate::engine_lines::decode(bytes, alloc).map_err(Unreadable::Lines))
}

/// What one write put.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Saved {
    /// Records put: the new pages, the answer's root and the pointer.
    pub(crate) puts: u32,
    /// The answer's pages in all, and their bytes.
    pub(crate) pages: u32,
    pub(crate) bytes: u64,
}

/// `kept` and the leaf table of `rows` stored under the root `name` over `last` (its pages not
/// put again); visible from the store's commit after the answer.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn save<'m, D: Driver, S: Sharing>(
    cx: &Cx,
    store: &mut CommandStore<'m, '_, D, S>,
    name: &str,
    kept: &Kept<'_>,
    printed: Printed,
    lines: (Key, &[u8]),
    last: Option<&Root<'_>>,
    rows: &Rows<'_>,
    alloc: SharedAlloc<'_>,
    frame: &Frame,
) -> Result<Saved, CliError> {
    let encoded = pages::encode(kept, last, alloc);
    let mut saved = Saved { puts: 0, pages: 0, bytes: encoded.bytes };
    for (key, bytes) in encoded.puts.iter() {
        store.put(cx, *key, bytes)?;
        saved.puts += 1;
    }
    // The printed lines, under the key of their bytes: equal lines are one record.
    store.put(cx, lines.0, lines.1)?;
    saved.puts += 1;
    let leaves = Leaves::bytes_in(rows, frame)?;
    let mut pointer: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::with_capacity_in(POINTER_BYTES, frame);
    pointer.extend_from_slice(POINTER_MAGIC);
    pointer.extend_from_slice(&encoded.root.0.0);
    pointer.extend_from_slice(&lines.0.0.0);
    let facts = Facts::of(kept, printed);
    pointer.extend_from_slice(&facts.files.to_le_bytes());
    pointer.push(facts.printed.byte());
    pointer.extend_from_slice(&facts.findings.to_le_bytes());
    pointer.extend_from_slice(&facts.owed.to_le_bytes());
    let sealed = seal::seal(&pointer, &leaves, frame)?;
    let key = Key(ContentId::of(POINTER_DOMAIN, &sealed));
    store.put(cx, key, &sealed)?;
    saved.puts += 1;
    let mut pins = encoded.pins(alloc);
    saved.pages = u32::try_from(pins.len() - 1).unwrap_or(u32::MAX);
    pins.push(lines.0);
    // Two roots, both in the one commit: a root's keys are a set to the store (not an order), so
    // the pointer has a root of its own and the answer's records another.
    store.pin(name, &[key])?;
    store.pin(bstd::mem::format_in!(frame, "{name}.pages").as_str(), &pins)?;
    Ok(saved)
}
