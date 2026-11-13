//! The start image: a launched program's whole state ([`Program`]) and the answer its next
//! launcher is told (the laid `Tell::Image` frame), as one store record, so a daemon that
//! starts again holds the program it held without a build, a front end or a first call.
//!
//! [`Program::kept`] gives the image and its key when the program changed since it last gave
//! one; [`Program::restored`] reads it back with no database. A program restored is the one
//! that was kept: given the same later messages, pieces and builds, it says the same bytes
//! (`tests::a_restored_program_tells_what_the_kept_one_tells`).
//!
//! The idea, a program's start linked as one image before `main` and kept with what it is of,
//! is Relax's (makepad, `libs/relax/compiler/src/cache/image.rs`, its `prelink`; MIT,
//! Copyright (c) 2026 makepad), read, not copied. Relax rewrites its manifest's image in
//! place; here an image is a record by content, and the journal's mark of the program moves.
//!
//! # Layout
//!
//! Little-endian; a count before what it counts.
//!
//! ```text
//! "bzstart1" | [32] check = BLAKE3("blaze v2 launch start body 1", all that follows) |
//! [32] program | [32] record | [32] policy | [32] symbols | [32] maker | u32 len + root |
//! u8 arch | u8 os | u32 page | u64 said | u64 txn | [32] entry |
//! u32 n | n slots, by slot: Function |
//! u32 n | n functions with no slot, by identity: Function |
//! u32 n | n statics, by number: ([32] item | [32] instance | u32 len + bind::encode(bound)) |
//! u32 n | n u32 inits (statics' numbers) | u32 n | n u32 start (slots) |
//! u32 n | n u32 needed (slots, first asked first) |
//! u32 n | n symbols, by name: (u32 len + name | bind's Names) |
//! u32 declared | u32 statics defined | u32 n | n (u8 has | [32] record id): the frame's binds by slot |
//! u32 len + frame (the Tell::Image frame) |
//! u64 at | u64 len | "bzstartE": the trailer, `at` the offset of the frame's first byte, `len` its length
//!
//! a Function is: [32] identity | u8 has | [32] bind | u32 len + ask | u8 held | (held: u32 len + bind::encode(bound))
//! ```
//!
//! A reader with no decoder (the launcher, blaze-certify's `slot::START_HEAD`) finds the
//! [`HEAD`]'s four words at fixed offsets and the frame through the trailer at the end.
//!
//! [`decode`] refuses bytes of another layout (`bzstart0`, and a bind record of
//! `bzbind01` inside), cut short, whose check is not the hash of what follows it, of another
//! program, or whose slots, statics' numbers or records' names do not fit, each naming the
//! offset (another program: both ids). A refused image is a build, never read as this layout.
//!
//! # The key
//!
//! [`start_key`] covers every input of the image's bytes (rule 21):
//!
//! | input | covered by |
//! |---|---|
//! | the program (its path) | [`Of::program`] |
//! | the build: every input of the unit | [`Of::record`], the unit's whole record key |
//! | how its records are compiled, its own symbols, the code that makes and lays its records and this image | [`Of::under`] (`policy`, `symbols`, `maker`) and [`START_DOMAIN`] |
//! | the checkout it was built in | [`Of::root`] |
//! | which slots were given and bound, the `needed` order, every record and ask, the frame, its system and page size | `check`, the hash of the body |
//!
//! Two runs that asked for other slots are two keys; the mark of the program
//! (blaze-cargo, `launch\0<path>`) names which image is its.

use std::sync::Arc;

use blaze_cas::Key;
use blaze_codegen::StaticId;
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use blaze_launch::wire::{self, RecordId, Tell};

use crate::bind::{self, Bound, Reader, Under};
use crate::program::{Function, Piece, Plant, Program, Refusal, Welcome};

/// The domain of a start image's key; another layout or another formula is another domain.
pub const START_DOMAIN: &str = "blaze v2 launch start 1";

/// The domain of a start image's check, the hash of its body.
const BODY_DOMAIN: &str = "blaze v2 launch start body 1";

const MAGIC: &[u8; 8] = b"bzstart1";

/// The last 8 bytes of a start image, after the frame's offset and length.
const TRAILER: &[u8; 8] = b"bzstartE";

/// The fixed head of a start image: `"bzstart1" | [32] check | [32] program | [32] record`.
/// The launcher reads `check` at 8..40 and `program` at 40..72; blaze-certify's
/// `slot::START_HEAD` reads `record` at 72..104.
pub const HEAD: usize = 104;

/// What a launcher that started from a start image holds (its welcome), read back from the
/// image: see [`table_of`].
#[derive(PartialEq, Eq, Debug)]
pub struct Table {
    /// The unit's whole record key: the build the image is of.
    pub record: ContentId,
    /// The slots its frame declares.
    pub declared: u32,
    /// The statics its frame defines.
    pub statics: u32,
    /// The frame's binds by slot: the record each declared slot is bound to.
    pub bound: Vec<Option<RecordId>>,
}

/// The [`Table`] of a start image: its `record`, `declared`, `statics defined` and the frame's
/// binds by slot, with every refusal [`decode`] makes (the image is decoded whole against the
/// program it names, so a table is never read from bytes a restore would refuse).
pub fn table_of(image: &[u8]) -> Result<Table, String> {
    let Some(program) = image.get(40..72) else {
        // alloc: an error
        return Err(format!("a start image of {} bytes is cut short before its program's id at offset 40", image.len()));
    };
    let program: [u8; 32] = program.try_into().expect("32 bytes");
    let (restored, of) = decode(image, &program, Plant::None)?;
    let welcome = restored.welcome.expect("decode sets the kept answer");
    Ok(Table { record: of.record, declared: welcome.declared, statics: welcome.statics, bound: welcome.bound })
}

/// Where the body the check covers begins: after the magic and the check.
const BODY: usize = 8 + 32;

/// What a start image is of, besides the program's own state.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: the driver's copy of what it keeps
pub struct Of {
    /// The program's id: of its path.
    pub program: [u8; 32],
    /// The unit's whole record key: of every input of the build the image holds.
    pub record: ContentId,
    /// The policy, own symbols and record maker the build's bind records are made under.
    pub under: Under,
    /// The checkout's directory.
    pub root: String,
}

/// The store key of a start image of `of` whose body hashes to `check`: over the program, the
/// record, the policy, symbols and maker, the root (its length first) and `check`.
pub fn start_key(of: &Of, check: &ContentId) -> Key {
    key_planted(of, check, Plant::None)
}

/// [`start_key`], with a planted bug of the tests' (`Plant::KeyOfIdentityAlone`) or none.
pub(crate) fn key_planted(of: &Of, check: &ContentId, plant: Plant) -> Key {
    let mut hasher = Hasher::new(START_DOMAIN);
    hasher
        .update(&of.program)
        .id(&of.record)
        .id(&of.under.policy)
        .id(&of.under.symbols)
        .id(&of.under.maker)
        .part(of.root.as_bytes());
    if plant != Plant::KeyOfIdentityAlone {
        hasher.id(check);
    }
    Key(hasher.finish())
}

/// The check of a start image's `body` (all that follows the check).
fn check_of(body: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(BODY_DOMAIN);
    hasher.update(body);
    hasher.finish()
}

fn put_u32(out: &mut Vec<u8>, value: usize) -> Result<(), Refusal> {
    let value = u32::try_from(value).map_err(|_| {
        format!("a start image counts {value} of something, past what its layout holds") // alloc: an error
    })?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Refusal> {
    put_u32(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_bind(out: &mut Vec<u8>, bind: &Option<Key>) {
    match bind {
        Some(key) => {
            out.push(1);
            out.extend_from_slice(&key.0.0);
        }
        None => {
            out.push(0);
            out.extend_from_slice(&[0; 32]);
        }
    }
}

fn put_function(out: &mut Vec<u8>, identity: &ContentId, function: Option<&Function>) -> Result<(), Refusal> {
    out.extend_from_slice(&identity.0);
    put_bind(out, &function.and_then(|it| it.bind));
    put_bytes(out, function.map_or(&[][..], |it| it.ask.as_slice()))?;
    match function.and_then(|it| it.bound.as_ref()) {
        Some(bound) => {
            out.push(1);
            put_bytes(out, &bind::encode(bound))?;
        }
        None => out.push(0),
    }
    Ok(())
}

fn arch_byte(arch: wire::Arch) -> u8 {
    match arch {
        wire::Arch::X86_64 => 0,
        wire::Arch::Aarch64 => 1,
    }
}

fn os_byte(os: wire::Os) -> u8 {
    match os {
        wire::Os::Linux => 0,
        wire::Os::Macos => 1,
    }
}

/// The start image of `program`, of `of`, and its check: `program` has answered a `Hello`
/// and its kept answer (`welcome`) is of what it holds now ([`Program::kept`] makes it so).
pub(crate) fn encode(program: &Program, of: &Of) -> Result<(Vec<u8>, ContentId), Refusal> {
    let (Some(arch), Some(entry), Some(welcome)) = (program.arch, program.entry, program.welcome.as_ref()) else {
        return Err("a start image of a program no launcher was answered for".to_owned()); // alloc: an error
    };
    let slot_of = |identity: &ContentId, what: &str| {
        program.slots.get(identity).copied().ok_or_else(|| {
            format!("the program's {what} {identity:?} has no slot, and its start image names it by slot") // alloc: an error
        })
    };
    // alloc: the image's bytes, which the store keeps
    let mut out = Vec::with_capacity(BODY + 256 + welcome.frame.len() + 128 * program.identities.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[0; 32]);
    out.extend_from_slice(&of.program);
    out.extend_from_slice(&of.record.0);
    out.extend_from_slice(&of.under.policy.0);
    out.extend_from_slice(&of.under.symbols.0);
    out.extend_from_slice(&of.under.maker.0);
    put_bytes(&mut out, of.root.as_bytes())?;
    out.push(arch_byte(arch));
    out.push(os_byte(welcome.os));
    out.extend_from_slice(&welcome.page.to_le_bytes());
    out.extend_from_slice(&program.said.to_le_bytes());
    out.extend_from_slice(&program.txn.to_le_bytes());
    out.extend_from_slice(&entry.0);

    put_u32(&mut out, program.identities.len())?;
    for identity in &program.identities {
        put_function(&mut out, identity, program.functions.get(identity))?;
    }
    // alloc: the functions no slot is given, sorted: an image is of no map's order (rule 25)
    let mut unslotted: Vec<(&ContentId, &Function)> =
        program.functions.iter().filter(|(identity, _)| !program.slots.contains_key(*identity)).collect();
    unslotted.sort_unstable_by_key(|it| *it.0);
    put_u32(&mut out, unslotted.len())?;
    for (identity, function) in unslotted {
        put_function(&mut out, identity, Some(function))?;
    }

    put_u32(&mut out, program.statics.len())?;
    for (id, bound) in &program.statics {
        out.extend_from_slice(&id.0.0.0);
        out.extend_from_slice(&id.1.0);
        put_bytes(&mut out, &bind::encode(bound))?;
    }
    put_u32(&mut out, program.inits.len())?;
    for number in &program.inits {
        out.extend_from_slice(&number.to_le_bytes());
    }
    put_u32(&mut out, program.start.len())?;
    for identity in &program.start {
        out.extend_from_slice(&slot_of(identity, "start function")?.to_le_bytes());
    }
    put_u32(&mut out, program.needed.len())?;
    for slot in &program.needed {
        out.extend_from_slice(&slot.to_le_bytes());
    }
    put_u32(&mut out, program.symbols.len())?;
    for (name, names) in &program.symbols {
        put_bytes(&mut out, name.as_bytes())?;
        bind::put_names(&mut out, names);
    }

    out.extend_from_slice(&welcome.declared.to_le_bytes());
    out.extend_from_slice(&welcome.statics.to_le_bytes());
    put_u32(&mut out, welcome.bound.len())?;
    for record in &welcome.bound {
        match record {
            Some(id) => {
                out.push(1);
                out.extend_from_slice(&id.0);
            }
            None => {
                out.push(0);
                out.extend_from_slice(&[0; 32]);
            }
        }
    }
    put_bytes(&mut out, &welcome.frame)?;
    let frame_at = out.len() - welcome.frame.len();
    out.extend_from_slice(&(frame_at as u64).to_le_bytes());
    out.extend_from_slice(&(welcome.frame.len() as u64).to_le_bytes());
    out.extend_from_slice(TRAILER);

    let check = check_of(&out[BODY..]);
    out[8..BODY].copy_from_slice(&check.0);
    Ok((out, check))
}

/// Writes the check of `bytes`' body into them: a test's image changed after it was kept,
/// so that what reads it gets past the check to what it changed.
#[cfg(test)]
pub(crate) fn seal(bytes: &mut [u8]) {
    let check = check_of(&bytes[BODY..]);
    bytes[8..BODY].copy_from_slice(&check.0);
}

/// A count read at the reader, refused where it counts more of things at least `least`
/// bytes each than the bytes left hold: no count past the bytes is believed.
fn count(from: &mut Reader<'_>, least: usize, what: &str) -> Result<usize, String> {
    let at = from.at;
    let count = from.u32(what)? as usize;
    if count > from.bytes.len().saturating_sub(from.at) / least {
        return Err(from.refuse(at, format_args!("its {what} is {count}, more than its bytes hold")));
    }
    Ok(count)
}

/// One `Function` of the layout: its identity, bind key, ask, and its bound record, if held.
fn get_function(from: &mut Reader<'_>, plant: Plant) -> Result<(ContentId, Function, Option<Bound>), String> {
    let identity = from.id("a function's identity")?;
    let bind = from.bind()?;
    let len = from.u32("a function's ask's length")? as usize;
    let ask = from.take(len, "a function's ask")?;
    // alloc: the ask, held by the program
    let ask = if plant == Plant::ForgetsAsks { Vec::new() } else { ask.to_vec() };
    let held_at = from.at;
    let held = match from.u8("a function's held flag")? {
        0 => None,
        1 => {
            let len = from.u32("a function's record's length")? as usize;
            let at = from.at;
            let bytes = from.take(len, "a function's bind record")?;
            let bound = bind::decode(bytes)
                .map_err(|why| from.refuse(at, format_args!("the record of the function {identity:?}: {why}")))?;
            if bound.slot != identity {
                return Err(from.refuse(
                    at,
                    format_args!("the function {identity:?} holds the record of the slot identity {:?}", bound.slot),
                ));
            }
            Some(bound)
        }
        other => return Err(from.refuse(held_at, format_args!("a function's held flag is {other}"))),
    };
    Ok((identity, Function { bound: None, bind, ask }, held))
}

fn slot_number(from: &mut Reader<'_>, slots: usize, what: &str) -> Result<u32, String> {
    let at = from.at;
    let slot = from.u32(what)?;
    if slot as usize >= slots {
        return Err(from.refuse(at, format_args!("a {what} is slot {slot}, and the image gives {slots} slots")));
    }
    Ok(slot)
}

/// The program a start image holds, and what it is of; see [`Program::restored`]. With a
/// planted bug of the tests' (`Plant::DropsNeeded`, `Plant::ForgetsAsks`) or none.
pub(crate) fn decode(bytes: &[u8], program: &[u8; 32], plant: Plant) -> Result<(Program, Of), String> {
    let mut from = Reader { bytes, at: 0, what: "a start image" };
    let magic = from.take(8, "layout's name")?;
    if magic != MAGIC {
        return Err(from.refuse(
            0,
            format_args!(
                "its layout is {:?}, and this blaze reads {:?}",
                String::from_utf8_lossy(magic),
                String::from_utf8_lossy(MAGIC)
            ),
        ));
    }
    let check = from.id("check")?;
    let body = check_of(&bytes[BODY..]);
    if check != body {
        return Err(from.refuse(
            8,
            format_args!("its check is {check:?}, and the {} bytes after it hash to {body:?}", bytes.len() - BODY),
        ));
    }
    let of_program: [u8; 32] = from.take(32, "program's id")?.try_into().expect("32 bytes");
    if of_program != *program {
        return Err(format!(
            // alloc: an error
            "a start image of the program {:?} was read for the program {:?}",
            ContentId(of_program),
            ContentId(*program)
        ));
    }
    let record = from.id("record key")?;
    let policy = from.id("policy")?;
    let symbols_id = from.id("symbols' id")?;
    let maker = from.id("record maker")?;
    let len = from.u32("root's length")? as usize;
    let root = from.str(len, "root")?.to_owned(); // alloc: the root, held by `Of`
    let arch_at = from.at;
    let arch = match from.u8("arch")? {
        0 => wire::Arch::X86_64,
        1 => wire::Arch::Aarch64,
        other => return Err(from.refuse(arch_at, format_args!("its arch is {other}, which this layout has none of"))),
    };
    let os_at = from.at;
    let os = match from.u8("os")? {
        0 => wire::Os::Linux,
        1 => wire::Os::Macos,
        other => return Err(from.refuse(os_at, format_args!("its os is {other}, which this layout has none of"))),
    };
    let page = from.u32("page size")?;
    let said = from.u64("last message's id")?;
    let txn = from.u64("last transaction")?;
    let entry_at = from.at;
    let entry = from.id("entry")?;

    // alloc (each push below): the restored program's tables, once a restore
    let mut restored = Program::new();
    // The records held, checked together against the symbols below before any is taken.
    let mut functions: Vec<Bound> = Vec::new(); // alloc: once a restore

    let slots = count(&mut from, 32 + 33 + 4 + 1, "count of slots")?;
    restored.identities.reserve(slots);
    for slot in 0..slots {
        let at = from.at;
        let (identity, function, held) = get_function(&mut from, plant)?;
        if restored.slots.insert(identity, slot as u32).is_some() {
            return Err(from.refuse(at, format_args!("slot {slot} is of {identity:?}, which an earlier slot is of")));
        }
        restored.identities.push(identity);
        restored.functions.insert(identity, function);
        if let Some(bound) = held {
            functions.push(bound);
        }
    }
    let unslotted = count(&mut from, 32 + 33 + 4 + 1, "count of functions with no slot")?;
    let mut last: Option<ContentId> = None;
    for _ in 0..unslotted {
        let at = from.at;
        let (identity, function, held) = get_function(&mut from, plant)?;
        if restored.slots.contains_key(&identity) || last.is_some_and(|it| it >= identity) {
            return Err(from.refuse(
                at,
                format_args!("the function {identity:?} with no slot is not after the last, once, and of no slot"),
            ));
        }
        last = Some(identity);
        restored.functions.insert(identity, function);
        if let Some(bound) = held {
            functions.push(bound);
        }
    }

    let count_statics = count(&mut from, 68, "count of statics")?;
    let mut statics: Vec<(StaticId, Bound)> = Vec::with_capacity(count_statics); // alloc: once a restore
    for number in 0..count_statics {
        let at = from.at;
        let item = ItemKey(from.id("a static's item")?);
        let instance = from.id("a static's instance")?;
        let len = from.u32("a static's record's length")? as usize;
        let record_at = from.at;
        let bytes = from.take(len, "a static's bind record")?;
        let bound = bind::decode(bytes)
            .map_err(|why| from.refuse(record_at, format_args!("the record of static {number}: {why}")))?;
        let id = (item, instance);
        match restored.numbers.binary_search_by_key(&id, |it| it.0) {
            Ok(_) => return Err(from.refuse(at, format_args!("static {number} is {id:?}, which an earlier static is"))),
            Err(to) => restored.numbers.insert(to, (id, number as u32)),
        }
        statics.push((id, bound));
    }
    let inits = count(&mut from, 4, "count of initializers")?;
    for _ in 0..inits {
        let at = from.at;
        let number = from.u32("an initializer")?;
        if number as usize >= count_statics {
            return Err(from.refuse(
                at,
                format_args!("an initializer is static {number}, and the image holds {count_statics}"),
            ));
        }
        restored.inits.push(number);
    }
    let starts = count(&mut from, 4, "count of start functions")?;
    for _ in 0..starts {
        let slot = slot_number(&mut from, slots, "start function")?;
        restored.start.push(restored.identities[slot as usize]);
    }
    let needed = count(&mut from, 4, "count of slots the runs called")?;
    restored.is_needed = vec![false; slots]; // alloc: the program's table
    for _ in 0..needed {
        let at = from.at;
        let slot = slot_number(&mut from, slots, "slot the runs called")?;
        if restored.is_needed[slot as usize] {
            return Err(from.refuse(at, format_args!("the slots the runs called name slot {slot} twice")));
        }
        restored.is_needed[slot as usize] = true;
        restored.needed.push(slot);
    }
    let symbols = count(&mut from, 4 + 5, "count of symbols")?;
    for _ in 0..symbols {
        let at = from.at;
        let len = from.u32("a symbol's length")? as usize;
        let name = from.str(len, "a symbol's name")?.to_owned(); // alloc: the name, held by the program
        let names = bind::get_names(&mut from)?;
        if restored.symbols.last().is_some_and(|it| it.0 >= name) {
            let said = blaze_report::say!("its symbol ", blaze_report::say::Code(&name), " is not after the last, once");
            return Err(from.refuse(at, format_args!("{}", said.flat())));
        }
        restored.symbols.push((name, names));
    }

    let declared_at = from.at;
    let declared = from.u32("slots the frame declares")?;
    let defined = from.u32("statics the frame defines")?;
    if declared as usize > slots || defined as usize > count_statics {
        return Err(from.refuse(
            declared_at,
            format_args!(
                "its frame declares {declared} slots and defines {defined} statics, and the image holds {slots} and \
                 {count_statics}"
            ),
        ));
    }
    let binds_at = from.at;
    let binds = count(&mut from, 33, "count of the frame's binds")?;
    if binds != declared as usize {
        return Err(from.refuse(binds_at, format_args!("its frame binds {binds} slots and declares {declared}")));
    }
    let mut bound: Vec<Option<RecordId>> = Vec::with_capacity(binds); // alloc: a launcher's table, kept
    for _ in 0..binds {
        let at = from.at;
        let has = from.u8("a frame's bind flag")?;
        let id = from.id("a frame's bound record")?;
        bound.push(match has {
            0 if id == ContentId::default() => None,
            1 => Some(RecordId(id.0)),
            _ => return Err(from.refuse(at, format_args!("a frame's bind flag is {has} with record {id:?}"))),
        });
    }
    let len = from.u32("frame's length")? as usize;
    let frame_at = from.at;
    let frame = from.take(len, "frame")?;
    match wire::split(frame) {
        Ok(Some((item, took))) if took == frame.len() && matches!(Tell::decode(item), Ok((_, Tell::Image { .. }))) => {}
        _ => return Err(from.refuse(frame_at, format_args!("its frame is no one whole `Tell::Image`"))),
    }
    let trailer_at = from.at;
    let named_at = from.u64("trailer's frame offset")?;
    let named_len = from.u64("trailer's frame length")?;
    let end = from.take(8, "trailer's name")?;
    if end != TRAILER || named_at != frame_at as u64 || named_len != len as u64 {
        return Err(from.refuse(
            trailer_at,
            format_args!(
                "its trailer {:?} names a frame at {named_at} of {named_len} bytes, and its frame is at {frame_at} of {len}",
                String::from_utf8_lossy(end)
            ),
        ));
    }
    if from.at != bytes.len() {
        return Err(from.refuse(from.at, format_args!("{} bytes follow its frame", bytes.len() - from.at)));
    }
    if !restored.slots.contains_key(&entry) {
        return Err(from.refuse(entry_at, format_args!("its entry {entry:?} has no slot")));
    }

    // Every record's names fit the program's slots, statics and symbols, as a build's would.
    let piece = Piece { functions, statics };
    restored.check(&piece, &restored.symbols, false).map_err(|why| from.refuse(BODY, format_args!("{why}")))?;
    for function in piece.functions {
        let held = restored.functions.get_mut(&function.slot).expect("each held record's function was read above");
        held.bound = Some(Arc::new(function));
    }
    restored.statics = piece.statics.into_iter().map(|(id, bound)| (id, Arc::new(bound))).collect(); // alloc: the program's table

    restored.arch = Some(arch);
    restored.entry = Some(entry);
    restored.said = said;
    restored.txn = txn;
    restored.version = 0;
    let of_kept = (0, restored.needed.len());
    restored.kept_of = Some((0, restored.needed.len(), said));
    restored.kept_check = Some(check.0);
    restored.hello = Some((os, page));
    // alloc: the kept answer's frame, held by the program
    restored.welcome = Some(Welcome { of: of_kept, os, page, frame: frame.to_vec(), declared, bound, statics: defined });
    if plant == Plant::DropsNeeded {
        restored.needed.clear();
        restored.is_needed.iter_mut().for_each(|it| *it = false);
    }
    let of = Of { program: of_program, record, under: Under { policy, symbols: symbols_id, maker }, root };
    Ok((restored, of))
}
