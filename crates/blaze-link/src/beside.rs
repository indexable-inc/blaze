//! An image's debug info as a file beside it (docs/v2/plan/design/debug-side-file.md): what
//! [`crate::Debug::Beside`] moves out of the executable, kept as a patch of the last one.
//!
//! The side file is a debug file every reader finds unaided: on ELF `<name>.debug`, named by
//! the executable's `.gnu_debuglink` and carrying its build id note; on Mach-O the DWARF file
//! of `<name>.dSYM`, carrying its `LC_UUID`. Inside is DWARF 4 laid out so that one function
//! is one record:
//!
//! - `.debug_info`: one unit, then a subprogram of [`DIE`] bytes per function at a fixed
//!   place (its *slot*), so a function's edit rewrites its own entry. A slot whose function
//!   is gone holds an empty range, which names no address, and is given to the next new
//!   function.
//! - `.debug_line`: one program whose header names the files, then one sequence per function,
//!   in any order (DWARF lets sequences be). A changed function's sequence is appended and
//!   its old one overwritten with operations that make no row ([`dwarf::dead`]).
//! - `.debug_str`: names, appended.
//! - the executable's symbol table and its names, copied, since a reader that takes the side
//!   file takes its symbols too (std's backtrace names a native function from them).
//! - ELF: the executable's program headers with nothing in the file, as
//!   `objcopy --only-keep-debug` leaves them. Mach-O: the bundle's `Info.plist`
//!   ([`DebugWrite::plist`]), as `dsymutil` writes it.
//!
//! Each section has room to grow, as an image's regions have. What does not fit a patch (no
//! room, a file the header does not name, more dead rows than live) lays the file out whole,
//! and [`DebugWrite::whole`] says which. The build id is the executable's, read from its
//! file, and on ELF the file's last four bytes are chosen so its CRC-32 is the one the
//! executable's `.gnu_debuglink` holds ([`link_crc`]: of the build id, so the executable is
//! written without the side file's bytes, and a side file of another generation is refused by
//! a debugger's CRC check).
//!
//! An update follows the link's change ([`Delta`], [`Given::Changed`]): only the functions
//! the link added, removed or named anew, and those with a line in a span table it gave anew,
//! are encoded; the rest keep the record and hash they have. An update with no change at hand
//! (the first, a rollback, a full layout, a link whose side file was never asked for) encodes
//! every function and compares hashes, and says why ([`DebugWrite::all`]).
//!
//! Sans IO: [`Kept::update`] reads the executable through a closure and gives the bytes to
//! write; who writes them clones the last file, writes the ranges and renames.

use std::ops::Range;

use blaze_codegen::InstanceKey;
use blaze_ids::{ContentId, Hasher};

use crate::Format;
use crate::dwarf::{self, Row};
use crate::unwind::uleb;

/// A subprogram entry's bytes: its abbreviation, a name's offset (or 4 unread bytes), an
/// 8-byte address and a 4-byte size.
pub(crate) const DIE: usize = 17;
/// The unit's header and its own entry, before the first subprogram.
const INFO_HEAD: usize = 46;
/// Where the unit's address range is in `.debug_info`: its low address, then its length.
const UNIT_RANGE: usize = 30;
/// A vendor attribute (`DW_AT_lo_user..DW_AT_hi_user`) no reader knows, so each skips its 4
/// bytes by its form: what an unnamed subprogram holds where a named one holds its name, so
/// both are [`DIE`] bytes.
const DW_AT_BLAZE_PAD: u64 = 0x3b01;
const DW_FORM_DATA4: u8 = 0x06;
const DW_FORM_DATA8: u8 = 0x07;

/// The blocks a copied table is compared in.
const BLOCK: usize = 4096;
/// The least room a growing section keeps past what it holds.
const ROOM: u64 = 4096;

const ABBREV: usize = 0;
const INFO: usize = 1;
const STR: usize = 2;
const LINE: usize = 3;
const SYMTAB: usize = 4;
const STRTAB: usize = 5;
const DWARF_NAMES: [&str; 4] = [".debug_abbrev", ".debug_info", ".debug_str", ".debug_line"];

/// ELF: the build id note's place in the side file, and the most bytes it may take. The
/// executable's program headers follow, at [`PHDRS_AT`].
const NOTE_AT: usize = 64;
const NOTE_ROOM: usize = 64;
const PHDRS_AT: usize = NOTE_AT + NOTE_ROOM;
const PHDR: usize = 56;
const PT_NOTE: u32 = 4;
const ELF_BUILD_ID: &str = ".note.gnu.build-id";

const SHT_NULL: u32 = 0;
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_NOBITS: u32 = 8;
const SHF_ALLOC: u64 = 2;
const SHDR: usize = 64;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_DSYM: u32 = 0xa;
const LC_SYMTAB: u32 = 0x2;
const LC_SEGMENT_64: u32 = 0x19;
const LC_UUID: u32 = 0x1b;
const S_ATTR_DEBUG: u32 = 0x0200_0000;
const NLIST: u64 = 16;

/// A placed function as the side file records it: its rows are `lines[lines]` of
/// [`Input::lines`].
#[derive(Clone, Debug)] // clone: tests' inputs
pub(crate) struct Func<'a> {
    pub key: InstanceKey,
    pub addr: u64,
    pub size: u64,
    pub name: Option<&'a str>,
    pub lines: Range<u32>,
}

/// A line-table row with its file by name: from code offset `at` of its function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Lined<'a> {
    pub at: u32,
    pub file: &'a str,
    pub line: u32,
    pub column: u32,
}

/// What a side file is made from: the image's name, format, build id and generation, and its
/// Rust functions by address then key, each once: all of them, or those a link changed
/// ([`Given`]).
pub(crate) struct Input<'a> {
    pub format: Format,
    /// The image's file name.
    pub name: &'a str,
    pub id: [u8; 16],
    /// The image's generation: what [`Kept::at`] is after this update.
    pub generation: u64,
    pub functions: &'a [Func<'a>],
    pub lines: &'a [Lined<'a>],
    pub given: Given<'a>,
}

/// Which functions an [`Input`] holds.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Given<'a> {
    /// Every function of the image: each is encoded and compared with its record, and a
    /// record of none of them is gone.
    All,
    /// The functions a link since the last update changed or may have ([`Delta`]); `gone` are
    /// those it removed, and `count` is how many functions the image has now. Only these are
    /// encoded. Records that then number other than `count` are not the image's: the update
    /// asks for all of them ([`Kept::update`] gives `None`).
    Changed { gone: &'a [InstanceKey], count: usize },
}

/// What a stage changed that a side file records, against the generation it was staged from:
/// what lets [`crate::Image::debug_file`] encode the functions of an edit and not of the
/// program. Kept on the state the stage made.
#[derive(Debug)]
pub(crate) struct Delta {
    /// The generation the stage was made from.
    pub base: u64,
    /// The stage laid the image out whole, or is one that records no delta: every function
    /// may have moved.
    pub full: bool,
    /// The functions added, removed or named anew, in any order.
    pub keys: Vec<InstanceKey>,
    /// The span tables given anew: every function with a line in one may have other rows.
    pub anchors: Vec<ContentId>,
}

impl Default for Delta {
    fn default() -> Delta {
        // alloc: none, empty
        Delta { base: u64::MAX, full: true, keys: Vec::new(), anchors: Vec::new() }
    }
}

/// What to write of an image's debug side file so it describes the image as committed
/// ([`crate::Image::debug_file`]). Who writes it: with a `base`, clone the side file last
/// written (whose id is `base`), else start from an empty file; set its length to `len`;
/// write each of `writes` at its offset; rename it over the last one, unsynced (it is derived
/// data: a torn one is made again from the image, [`crate::Image::debug_file_whole`]).
/// [`DebugWrite::apply`] is that, on bytes.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests keep a write to plant a bug in
pub struct DebugWrite {
    /// Where the side file goes, from the executable's directory: `app.debug`, or
    /// `app.dSYM/Contents/Resources/DWARF/app`.
    pub name: String,
    /// The build id of the executable it describes, which it carries.
    pub id: [u8; 16],
    /// The build id the side file these writes are over carries; `None` when they are the
    /// whole file.
    pub base: Option<[u8; 16]>,
    pub len: u64,
    /// By offset, none overlapping.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// The function records written: entries of `.debug_info` and sequences of `.debug_line`.
    pub records: u64,
    /// The functions whose rows were encoded to make this: those of the link's change when
    /// the update followed one, every function otherwise ([`DebugWrite::all`] says why).
    pub encoded: u64,
    /// Why every function was encoded and compared, when the update did not follow a
    /// link's change.
    pub all: Option<&'static str>,
    /// Why the file was laid out whole, when it was.
    pub whole: Option<&'static str>,
    /// Mach-O: the bundle's `Contents/Info.plist`, which lldb and Spotlight read to take the
    /// directory for a dSYM. The same bytes every update of one image: who writes the side
    /// file writes it where it is not there.
    pub plist: Option<Vec<u8>>,
}

impl DebugWrite {
    /// The bytes written.
    pub fn written(&self) -> u64 {
        self.writes.iter().map(|it| it.1.len() as u64).sum()
    }

    /// The side file after these writes, given the one before them (`None`: there is none).
    /// An error names what is wrong with `old`: a patch is of the file whose build id is
    /// [`DebugWrite::base`] and of no other.
    pub fn apply(&self, format: Format, old: Option<&[u8]>) -> Result<Vec<u8>, String> {
        // alloc: the side file, which the caller writes
        let mut out = match (self.base, old) {
            (None, _) => Vec::new(),
            (Some(base), Some(old)) => match id_of(format, old) {
                Some(held) if held == base => old.to_vec(),
                held => {
                    // alloc: error path
                    return Err(format!(
                        "{}: the side file there carries the build id {held:02x?}, and this patch is of the one \
                         carrying {base:02x?}: write it whole",
                        self.name
                    ));
                }
            },
            (Some(base), None) => {
                // alloc: error path
                return Err(format!(
                    "{}: no side file there, and this patch is of the one carrying the build id {base:02x?}: \
                     write it whole",
                    self.name
                ));
            }
        };
        out.resize(self.len as usize, 0);
        for (at, bytes) in &self.writes {
            let at = *at as usize;
            match out.get_mut(at..at + bytes.len()) {
                Some(into) => into.copy_from_slice(bytes),
                None => {
                    // alloc: error path
                    return Err(format!(
                        "{}: a write of {} bytes at {at:#x}, past the file's {:#x}",
                        self.name,
                        bytes.len(),
                        self.len
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// The side file's path from the executable's directory.
pub(crate) fn file_name(format: Format, image: &str) -> Option<String> {
    match format {
        // alloc: once per side file write
        Format::Elf => Some(format!("{image}.debug")),
        // alloc: as above
        Format::MachO => Some(format!("{image}.dSYM/Contents/Resources/DWARF/{image}")),
        Format::Pe | Format::Wasm => None,
    }
}

/// A `.dSYM` bundle's `Info.plist` for the image `image`, as `dsymutil` writes one; none for
/// a format whose side file is no bundle.
fn plist(format: Format, image: &str) -> Option<Vec<u8>> {
    if format != Format::MachO {
        return None;
    }
    // alloc: once per side file update of a Mach-O image, under a kilobyte
    let mut name = String::with_capacity(image.len());
    for c in image.chars() {
        match c {
            '&' => name.push_str("&amp;"),
            '<' => name.push_str("&lt;"),
            '>' => name.push_str("&gt;"),
            c => name.push(c),
        }
    }
    let pairs = [
        ("CFBundleDevelopmentRegion", "English"),
        ("CFBundleIdentifier", ""),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundlePackageType", "dSYM"),
        ("CFBundleSignature", "????"),
        ("CFBundleShortVersionString", "1.0"),
        ("CFBundleVersion", "1"),
    ];
    // alloc: as above
    let mut out = String::with_capacity(768);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple Computer//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n\t<dict>\n");
    for (key, value) in pairs {
        out.push_str("\t\t<key>");
        out.push_str(key);
        out.push_str("</key>\n\t\t<string>");
        match key {
            "CFBundleIdentifier" => {
                out.push_str("com.apple.xcode.dsym.");
                out.push_str(&name);
            }
            _ => out.push_str(value),
        }
        out.push_str("</string>\n");
    }
    out.push_str("\t</dict>\n</plist>\n");
    Some(out.into_bytes())
}

/// Where a side file of `format` holds its build id.
fn id_at(format: Format) -> Option<usize> {
    match format {
        // The note's header (name size, descriptor size, type) and its name `GNU\0`.
        Format::Elf => Some(NOTE_AT + 16),
        // The header, then `LC_UUID`'s command and size.
        Format::MachO => Some(32 + 8),
        Format::Pe | Format::Wasm => None,
    }
}

/// The build id a side file's bytes carry: the executable's it describes.
pub fn id_of(format: Format, side: &[u8]) -> Option<[u8; 16]> {
    let at = id_at(format)?;
    side.get(at..at + 16)?.try_into().ok()
}

// CRC-32 (IEEE 802.3, reflected, as zlib's and `.gnu_debuglink`'s).

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut value = n as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 { 0xedb8_8320 ^ (value >> 1) } else { value >> 1 };
            bit += 1;
        }
        table[n] = value;
        n += 1;
    }
    table
}

static CRC: [u32; 256] = crc_table();

/// The CRC-32 of `bytes`, as a debugger checks a `.gnu_debuglink` with.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut reg = !0u32;
    for byte in bytes {
        reg = CRC[((reg ^ u32::from(*byte)) & 0xff) as usize] ^ (reg >> 8);
    }
    !reg
}

/// The four bytes that, after bytes whose CRC-32 is `crc`, make the whole's CRC-32 `want`.
/// Feeding four bytes `x` to the register `r` is feeding four zero bytes to `r ^ x`, and a
/// zero byte's step is undone by the table entry whose top byte is the register's (each
/// entry's top byte is its own): so `x` is `r` xor the register four zero bytes before
/// `!want`.
fn crc_fix(crc: u32, want: u32) -> [u8; 4] {
    let mut reg = !want;
    for _ in 0..4 {
        let top = reg >> 24;
        let at = CRC.iter().position(|it| it >> 24 == top).expect("each top byte is one entry's");
        reg = ((reg ^ CRC[at]) << 8) | at as u32;
    }
    (reg ^ !crc).to_le_bytes()
}

/// The CRC-32 an ELF side file of the executable with build id `id` has: the id's first four
/// bytes. The executable's `.gnu_debuglink` holds it, and [`Kept`] makes the file's so.
pub(crate) fn link_crc(id: &[u8; 16]) -> u32 {
    u32::from_le_bytes([id[0], id[1], id[2], id[3]])
}

/// An ELF executable's `.gnu_debuglink`: the side file's name, padded to 4 bytes, and its
/// CRC-32.
pub(crate) fn debuglink(image: &str, id: &[u8; 16]) -> Vec<u8> {
    // alloc: once per stage of an ELF image with a side file, a few bytes
    let mut out = Vec::with_capacity(image.len() + 16);
    out.extend_from_slice(image.as_bytes());
    out.extend_from_slice(b".debug\0");
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(&link_crc(id).to_le_bytes());
    out
}

/// A section's place in the side file: `len` bytes held of `cap` kept, from `at`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Slot {
    at: u64,
    cap: u64,
    len: u64,
}

impl Slot {
    fn range(&self) -> Range<usize> {
        self.at as usize..(self.at + self.len) as usize
    }
}

/// One function's record: its entry's slot, its sequence's place in `.debug_line` and its
/// name's in `.debug_str`, and the hash of all the three are made from.
#[derive(Clone, Copy, Debug)]
struct Record {
    slot: u32,
    hash: ContentId,
    seq_at: u32,
    seq_len: u32,
    name_at: Option<u32>,
    /// The update that last saw its function.
    seen: u64,
}

/// Why a patch was not made.
enum Stop {
    /// The file is laid out whole, for this reason.
    Whole(&'static str),
    Error(String),
}

impl From<String> for Stop {
    fn from(what: String) -> Stop {
        Stop::Error(what)
    }
}

/// The side file as last given, and where each function's record is in it: what the next
/// [`Kept::update`] is a patch of. Holds the file's bytes (its sections with their room).
pub(crate) struct Kept {
    file: Vec<u8>,
    /// The build id the file carries; `None` before the first.
    id: Option<[u8; 16]>,
    format: Format,
    name: String,
    /// The bytes before the first section: the headers, with their room.
    head_cap: u64,
    head_len: u64,
    slots: [Slot; 6],
    /// By key.
    records: Vec<(InstanceKey, Record)>,
    /// The entry slots given out, free ones included.
    entries: u32,
    /// Free entry slots, highest first: the lowest is given first.
    free: Vec<u32>,
    /// The line header's files, in its order, and each by name.
    files: Vec<String>,
    by_name: Vec<(String, u32)>,
    /// The bytes of `.debug_line` that are overwritten sequences.
    dead: u64,
    turn: u64,
    /// The image generation the file describes; `None` with no file.
    at: Option<u64>,
    /// The unit's address range: of every function recorded since the file was laid out.
    low: u64,
    high: u64,
    /// Why the last update that followed a change stopped and asked for every function.
    asked: Option<&'static str>,
    /// The functions the update in hand encoded.
    encoded: u64,
}

impl Kept {
    pub(crate) fn new() -> Kept {
        Kept {
            file: Vec::new(),
            id: None,
            format: Format::Elf,
            // alloc: none, empty
            name: String::new(),
            head_cap: 0,
            head_len: 0,
            slots: [Slot::default(); 6],
            records: Vec::new(),
            entries: 0,
            free: Vec::new(),
            files: Vec::new(),
            by_name: Vec::new(),
            dead: 0,
            turn: 0,
            at: None,
            low: 0,
            high: 0,
            asked: None,
            encoded: 0,
        }
    }

    /// The image generation the side file last given describes; `None` when the next update
    /// lays it out whole.
    pub(crate) fn at(&self) -> Option<u64> {
        self.id.and(self.at)
    }

    /// The bytes it holds kept.
    pub(crate) fn held(&self) -> u64 {
        (self.file.capacity()
            + self.records.capacity() * size_of::<(InstanceKey, Record)>()
            + self.files.iter().map(|it| 2 * it.capacity()).sum::<usize>()) as u64
    }

    /// Forgets the file last given: the next update lays it out whole.
    pub(crate) fn forget(&mut self) {
        self.id = None;
    }

    /// The side file's whole bytes as last given, for a test's oracle.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.file
    }

    /// What brings the side file to `input`: a patch of the file last given where it fits one,
    /// else the whole file, saying why. `exe` reads the executable's file (`exe(at, out)`
    /// fills `out` from offset `at`; what is past the file stays zero).
    ///
    /// `None` for an input of a change ([`Given::Changed`]) that the file can't take as one
    /// (there is no file, it has no room, the records would not number the image's
    /// functions): the caller gives every function, and that update says why
    /// ([`DebugWrite::all`]).
    pub(crate) fn update(
        &mut self,
        input: &Input<'_>,
        exe: &impl Fn(u64, &mut [u8]),
        all: Option<&'static str>,
    ) -> Result<Option<DebugWrite>, String> {
        let changed = matches!(input.given, Given::Changed { .. });
        self.encoded = 0;
        let Some(name) = file_name(input.format, input.name) else {
            // alloc: error path
            return Err(format!(
                "{}: a debug side file of a {:?} image, which blaze writes none for: link it with its debug info \
                 inside (`split-debuginfo = \"off\"`) or with none",
                input.name, input.format
            ));
        };
        let table = table_of(input.format, exe)?;
        // Why every function is encoded, when they are: the caller's reason, or the one the
        // update of a change stopped for.
        let asked = match changed {
            true => None,
            false => self.asked.take(),
        };
        let all = match changed {
            true => None,
            false => asked.or(all).or(Some("no link's change was at hand")),
        };
        let why = match self.id {
            None => asked.unwrap_or("no side file was written yet"),
            Some(_) if self.format != input.format || self.name != input.name => {
                "the image has another name or format"
            }
            Some(base) => match self.patch(input, &table, exe) {
                Ok((writes, records)) => {
                    self.id = Some(input.id);
                    self.at = Some(input.generation);
                    return Ok(Some(DebugWrite {
                        name,
                        id: input.id,
                        base: Some(base),
                        len: self.file.len() as u64,
                        writes,
                        records,
                        encoded: self.encoded,
                        all,
                        whole: None,
                        plist: plist(input.format, input.name),
                    }));
                }
                Err(Stop::Whole(why)) => why,
                Err(Stop::Error(what)) => {
                    // The model was written part way: the next update is of no file.
                    self.id = None;
                    return Err(what);
                }
            },
        };
        self.id = None;
        if changed {
            // A whole file is made of every function, which a change does not hold.
            self.asked = Some(why);
            return Ok(None);
        }
        self.whole(input, &table, exe)?;
        self.id = Some(input.id);
        self.at = Some(input.generation);
        Ok(Some(DebugWrite {
            name,
            id: input.id,
            base: None,
            len: self.file.len() as u64,
            // clone: the whole file, once per whole layout; the model keeps its own to patch
            writes: vec![(0, self.file.clone())],
            records: input.functions.len() as u64,
            encoded: self.encoded,
            all,
            whole: Some(why),
            plist: plist(input.format, input.name),
        }))
    }

    /// `function`'s rows with their files by number, into `rows`: `None` for a file the line
    /// header does not name.
    fn rows(&self, input: &Input<'_>, function: &Func<'_>, rows: &mut Vec<Row>) -> Option<()> {
        rows.clear();
        let mut last: Option<(&str, u32)> = None;
        for line in &input.lines[function.lines.start as usize..function.lines.end as usize] {
            let file = match last {
                Some((name, file)) if name == line.file => file,
                _ => {
                    let at = self
                        .by_name
                        .binary_search_by(|it| it.0.as_str().cmp(line.file))
                        .ok()?;
                    let file = self.by_name[at].1;
                    last = Some((line.file, file));
                    file
                }
            };
            rows.push(Row { at: line.at, file, line: line.line, column: line.column });
        }
        Some(())
    }

    fn find(&self, key: &InstanceKey) -> Result<usize, usize> {
        self.records.binary_search_by(|it| it.0.cmp(key))
    }

    /// The whole file, from nothing kept.
    fn whole(
        &mut self,
        input: &Input<'_>,
        table: &Table,
        exe: &impl Fn(u64, &mut [u8]),
    ) -> Result<(), String> {
        self.format = input.format;
        // alloc: once per whole layout, the image's name
        self.name = input.name.to_owned();
        self.records.clear();
        self.free.clear();
        self.files.clear();
        self.by_name.clear();
        self.dead = 0;
        self.turn = 1;
        // The files in the order the functions, by address, first name them: a fresh side
        // file is a function of the image alone.
        for function in input.functions {
            let mut last = "";
            for line in &input.lines[function.lines.start as usize..function.lines.end as usize] {
                if line.file == last {
                    continue;
                }
                last = line.file;
                if let Err(at) = self.by_name.binary_search_by(|it| it.0.as_str().cmp(line.file)) {
                    // alloc: once per source file per whole layout
                    self.by_name.insert(at, (line.file.to_owned(), self.files.len() as u32));
                    // alloc: as above
                    self.files.push(line.file.to_owned());
                }
            }
        }
        // alloc: once per whole layout, a name per source file
        let names: Vec<&str> = self.files.iter().map(String::as_str).collect();
        let mut line = dwarf::line_header(&names)?;
        // alloc: once per whole layout, the sections
        let mut info = vec![0u8; INFO_HEAD];
        let mut strings = dwarf::PRODUCER.to_vec();
        let mut rows = Vec::new();
        self.records.reserve(input.functions.len());
        for (slot, function) in input.functions.iter().enumerate() {
            // alloc: error path
            let lost = || format!("a file of {:?} left the line header as it was made", function.key);
            self.rows(input, function, &mut rows).ok_or_else(lost)?;
            let sequence = sequence_of(function, &rows, self.files.len())?;
            self.encoded += 1;
            let size = size_of_function(function)?;
            let name_at = match function.name {
                Some(name) => Some(push_name(&mut strings, name)?),
                None => None,
            };
            let at = info.len();
            info.resize(at + DIE, 0);
            entry(&mut info[at..], name_at, function.addr, size);
            let record = Record {
                slot: slot as u32,
                hash: hash_of(function, &sequence),
                seq_at: offset(line.len(), ".debug_line")?,
                seq_len: offset(sequence.len(), ".debug_line")?,
                name_at,
                seen: 1,
            };
            line.extend_from_slice(&sequence);
            self.records.push((function.key, record));
        }
        self.entries = input.functions.len() as u32;
        self.records.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if let Some(twice) = self.records.windows(2).find(|it| it[0].0 == it[1].0) {
            // alloc: error path
            return Err(format!("the function {:?} is placed twice: one record names one place", twice[0].0));
        }
        info.push(0);
        let info_len = info.len();
        self.low = input.functions.iter().map(|it| it.addr).min().unwrap_or(0);
        self.high = input.functions.iter().map(|it| it.addr + it.size).max().unwrap_or(0);
        info_head(&mut info[..INFO_HEAD], info_len, self.low, self.high)?;
        let line_len = offset(line.len() - 4, ".debug_line")?;
        line[..4].copy_from_slice(&line_len.to_le_bytes());
        let abbrev = abbrev();
        let (symtab, strtab) = table.copy(exe);

        // The headers' length does not depend on where the sections are: made once to size
        // their room, then again with the sections' places.
        let lens = [abbrev.len(), info.len(), strings.len(), line.len(), symtab.len(), strtab.len()];
        let mut slots = [Slot::default(); 6];
        for (slot, len) in slots.iter_mut().zip(lens) {
            slot.len = len as u64;
        }
        let head_len = table.head(input, &slots)?.len() as u64;
        self.head_cap = (2 * head_len).max(head_len + ROOM).next_multiple_of(16);
        let mut at = self.head_cap;
        for (nth, slot) in slots.iter_mut().enumerate() {
            slot.at = at;
            slot.cap = match nth {
                ABBREV => slot.len.next_multiple_of(16),
                _ => (2 * slot.len).max(slot.len + ROOM).next_multiple_of(16),
            };
            at += slot.cap;
        }
        let end = at;
        let len = match input.format {
            Format::Elf => end + 4,
            _ => end,
        };
        if u32::try_from(len).is_err() {
            // alloc: error path
            return Err(format!("{}: {len:#x} bytes of debug info, past the 4 GiB 32-bit DWARF reaches", input.name));
        }
        self.slots = slots;
        let head = table.head(input, &slots)?;
        self.head_len = head.len() as u64;
        self.file.clear();
        self.file.resize(len as usize, 0);
        self.file[..head.len()].copy_from_slice(&head);
        for (slot, bytes) in slots.iter().zip([&abbrev, &info, &strings, &line, &symtab, &strtab]) {
            self.file[slot.range()].copy_from_slice(bytes);
        }
        self.seal(input, end);
        Ok(())
    }

    /// ELF: the last four bytes, so the file's CRC-32 is [`link_crc`] of the build id.
    fn seal(&mut self, input: &Input<'_>, end: u64) {
        if input.format == Format::Elf {
            let end = end as usize;
            let fix = crc_fix(crc32(&self.file[..end]), link_crc(&input.id));
            self.file[end..end + 4].copy_from_slice(&fix);
        }
    }

    /// The file last given brought to `input` in place: the ranges written, by offset, and
    /// how many function records were.
    fn patch(
        &mut self,
        input: &Input<'_>,
        table: &Table,
        exe: &impl Fn(u64, &mut [u8]),
    ) -> Result<(Vec<(u64, Vec<u8>)>, u64), Stop> {
        self.turn += 1;
        let turn = self.turn;
        // alloc: once per update, the ranges written
        let mut dirty: Vec<(u64, u64)> = Vec::new();
        let mut rows = Vec::new();
        let mut written = 0u64;
        let [_, info, strings, line, ..] = self.slots;
        let (mut str_len, mut line_len) = (strings.len, line.len);
        for function in input.functions {
            if self.rows(input, function, &mut rows).is_none() {
                return Err(Stop::Whole("a function has a line in a source file the line header does not name"));
            }
            let sequence = sequence_of(function, &rows, self.files.len())?;
            self.encoded += 1;
            let hash = hash_of(function, &sequence);
            let found = self.find(&function.key);
            if let Ok(at) = found
                && self.records[at].1.hash == hash
            {
                if self.records[at].1.seen == turn {
                    // alloc: error path
                    return Err(Stop::Error(format!(
                        "the function {:?} is placed twice: one record names one place",
                        function.key
                    )));
                }
                self.records[at].1.seen = turn;
                continue;
            }
            let size = size_of_function(function)?;
            let old = found.ok().map(|at| self.records[at].1);
            if old.is_some_and(|it| it.seen == turn) {
                // alloc: error path
                return Err(Stop::Error(format!(
                    "the function {:?} is placed twice: one record names one place",
                    function.key
                )));
            }
            // Its name where it was, when it is the name it had.
            let kept_name = old.and_then(|it| it.name_at).filter(|at| {
                function.name.is_some_and(|name| {
                    let from = strings.at as usize + *at as usize;
                    self.file.get(from..from + name.len() + 1).is_some_and(|held| {
                        &held[..name.len()] == name.as_bytes() && held[name.len()] == 0
                    })
                })
            });
            let name_at = match (function.name, kept_name) {
                (None, _) => None,
                (Some(_), Some(at)) => Some(at),
                (Some(name), None) => {
                    if name.contains('\0') {
                        // alloc: error path
                        return Err(Stop::Error(format!("the function name {name:?}, which DWARF can't hold")));
                    }
                    if str_len + name.len() as u64 + 1 > strings.cap {
                        return Err(Stop::Whole("no room for a name in .debug_str"));
                    }
                    let from = (strings.at + str_len) as usize;
                    self.file[from..from + name.len()].copy_from_slice(name.as_bytes());
                    self.file[from + name.len()] = 0;
                    dirty.push((from as u64, name.len() as u64 + 1));
                    let at = str_len as u32;
                    str_len += name.len() as u64 + 1;
                    Some(at)
                }
            };
            if let Some(old) = old {
                self.kill(line, old, &mut dirty);
            }
            let slot = match (old, self.free.last().copied()) {
                (Some(old), _) => old.slot,
                (None, Some(free)) => {
                    self.free.pop();
                    free
                }
                (None, None) => {
                    if (INFO_HEAD + DIE * (self.entries as usize + 1) + 1) as u64 > info.cap {
                        return Err(Stop::Whole("no room for a function in .debug_info"));
                    }
                    self.entries += 1;
                    self.entries - 1
                }
            };
            if line_len + sequence.len() as u64 > line.cap {
                return Err(Stop::Whole("no room for a function's rows in .debug_line"));
            }
            let seq_at = line_len as u32;
            let from = (line.at + line_len) as usize;
            self.file[from..from + sequence.len()].copy_from_slice(&sequence);
            dirty.push((from as u64, sequence.len() as u64));
            line_len += sequence.len() as u64;
            let at = info.at as usize + INFO_HEAD + DIE * slot as usize;
            entry(&mut self.file[at..at + DIE], name_at, function.addr, size);
            dirty.push((at as u64, DIE as u64));
            let record =
                Record { slot, hash, seq_at, seq_len: sequence.len() as u32, name_at, seen: turn };
            match found {
                Ok(at) => self.records[at].1 = record,
                Err(at) => self.records.insert(at, (function.key, record)),
            }
            written += 1;
        }
        // The functions gone: each entry an empty range, each sequence no rows, by slot. Of
        // every function, those no longer among them; of a change, those it says are gone.
        // alloc: once per update that removes a function
        let mut gone: Vec<(InstanceKey, Record)> = match input.given {
            Given::All => self.records.iter().filter(|it| it.1.seen != turn).copied().collect(),
            Given::Changed { gone, .. } => gone
                .iter()
                .filter_map(|key| self.find(key).ok().map(|at| self.records[at]))
                .filter(|it| it.1.seen != turn)
                .collect(),
        };
        if !gone.is_empty() {
            gone.sort_unstable_by_key(|it| it.1.slot);
            gone.dedup_by_key(|it| it.1.slot);
            for (_, record) in &gone {
                self.kill(line, *record, &mut dirty);
                let at = info.at as usize + INFO_HEAD + DIE * record.slot as usize;
                entry(&mut self.file[at..at + DIE], None, 0, 0);
                dirty.push((at as u64, DIE as u64));
                self.free.push(record.slot);
                written += 1;
            }
            self.free.sort_unstable_by(|a, b| b.cmp(a));
            match input.given {
                Given::All => self.records.retain(|it| it.1.seen == turn),
                Given::Changed { .. } => {
                    for (key, _) in &gone {
                        if let Ok(at) = self.find(key) {
                            self.records.remove(at);
                        }
                    }
                }
            }
        }
        // A change that leaves other than the image's functions recorded was not all that
        // changed (the link dropped a function nothing named): every function is compared.
        if let Given::Changed { count, .. } = input.given
            && self.records.len() != count
        {
            return Err(Stop::Whole("the records after the link's change are not one a function of the image"));
        }
        match input.given {
            Given::All => {
                self.low = input.functions.iter().map(|it| it.addr).min().unwrap_or(0);
                self.high = input.functions.iter().map(|it| it.addr + it.size).max().unwrap_or(0);
            }
            Given::Changed { .. } => {
                for function in input.functions {
                    self.low = self.low.min(function.addr);
                    self.high = self.high.max(function.addr + function.size);
                }
            }
        }
        if self.dead > ROOM && 2 * self.dead > line_len {
            return Err(Stop::Whole("more than half of .debug_line is rows of functions since changed"));
        }
        // The unit: its length, its range, the entry that ends its children.
        let info_len = (INFO_HEAD + DIE * self.entries as usize + 1) as u64;
        self.file[(info.at + info_len) as usize - 1] = 0;
        let mut head = [0u8; INFO_HEAD];
        head.copy_from_slice(&self.file[info.at as usize..info.at as usize + INFO_HEAD]);
        let mut unit = head;
        info_head(&mut unit, info_len as usize, self.low, self.high)?;
        if unit != head || info_len != info.len {
            self.file[info.at as usize..info.at as usize + INFO_HEAD].copy_from_slice(&unit);
            dirty.push((info.at, INFO_HEAD as u64));
            dirty.push((info.at + info_len - 1, 1));
        }
        if line_len != line.len {
            let len = (line_len - 4) as u32;
            self.file[line.at as usize..line.at as usize + 4].copy_from_slice(&len.to_le_bytes());
            dirty.push((line.at, 4));
        }
        self.slots[INFO].len = info_len;
        self.slots[STR].len = str_len;
        self.slots[LINE].len = line_len;
        // The executable's symbols, by block.
        let (symtab, strtab) = table.copy(exe);
        for (nth, bytes) in [(SYMTAB, &symtab), (STRTAB, &strtab)] {
            let slot = self.slots[nth];
            if bytes.len() as u64 > slot.cap {
                return Err(Stop::Whole("no room for the executable's symbols"));
            }
            let held = &mut self.file[slot.at as usize..(slot.at + slot.cap) as usize];
            let old_len = slot.len as usize;
            for (block, new) in bytes.chunks(BLOCK).enumerate() {
                let from = block * BLOCK;
                if held[from..from + new.len()] != *new {
                    held[from..from + new.len()].copy_from_slice(new);
                    dirty.push((slot.at + from as u64, new.len() as u64));
                }
            }
            if old_len > bytes.len() {
                held[bytes.len()..old_len].fill(0);
                dirty.push((slot.at + bytes.len() as u64, (old_len - bytes.len()) as u64));
            }
            self.slots[nth].len = bytes.len() as u64;
        }
        // The headers: the build id, each section's size.
        let end = self.slots[STRTAB].at + self.slots[STRTAB].cap;
        let head = table.head(input, &self.slots)?;
        if head.len() as u64 > self.head_cap {
            return Err(Stop::Whole("no room for the headers"));
        }
        let old_len = self.head_len as usize;
        if self.file[..old_len] != head[..] {
            self.file[..old_len.max(head.len())].fill(0);
            self.file[..head.len()].copy_from_slice(&head);
            dirty.push((0, old_len.max(head.len()) as u64));
            self.head_len = head.len() as u64;
        }
        if input.format == Format::Elf {
            self.seal(input, end);
            dirty.push((end, 4));
        }
        dirty.sort_unstable();
        // alloc: once per update, the bytes written
        let mut writes: Vec<(u64, Vec<u8>)> = Vec::with_capacity(dirty.len());
        let mut reach = 0u64;
        for (at, len) in dirty {
            let (from, to) = (at.max(reach), at + len);
            if to <= from {
                continue;
            }
            let bytes = &self.file[from as usize..to as usize];
            match writes.last_mut() {
                Some(last) if last.0 + last.1.len() as u64 == from => last.1.extend_from_slice(bytes),
                // alloc: the bytes of one written range
                _ => writes.push((from, bytes.to_vec())),
            }
            reach = to;
        }
        Ok((writes, written))
    }

    /// Overwrites `record`'s sequence with operations that make no row.
    fn kill(&mut self, line: Slot, record: Record, dirty: &mut Vec<(u64, u64)>) {
        if record.seq_len == 0 {
            return;
        }
        let from = (line.at + u64::from(record.seq_at)) as usize;
        dwarf::dead(&mut self.file[from..from + record.seq_len as usize]);
        dirty.push((from as u64, u64::from(record.seq_len)));
        self.dead += u64::from(record.seq_len);
    }
}

fn offset(value: usize, section: &str) -> Result<u32, String> {
    // alloc: error path
    u32::try_from(value).map_err(|_| format!("{section} past the 4 GiB 32-bit DWARF reaches"))
}

fn size_of_function(function: &Func<'_>) -> Result<u32, String> {
    u32::try_from(function.size).map_err(|_| {
        // alloc: error path
        format!(
            "the function {:?} is {:#x} bytes: a side file's entry holds a size below 4 GiB",
            function.key, function.size
        )
    })
}

fn sequence_of(function: &Func<'_>, rows: &[Row], files: usize) -> Result<Vec<u8>, String> {
    dwarf::sequence(function.addr, function.size, rows, files)
        // alloc: error path
        .map_err(|what| format!("debug info of {:?}: {what}", function.key))
}

/// What a function's record is made from: its address, size and name, and its sequence
/// (its rows, with its address and its files' numbers).
fn hash_of(function: &Func<'_>, sequence: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze v2 link debug record 1");
    hasher.update(&function.addr.to_le_bytes());
    hasher.update(&function.size.to_le_bytes());
    match function.name {
        Some(name) => {
            hasher.update(&[1]);
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.update(sequence);
    hasher.finish()
}

fn push_name(strings: &mut Vec<u8>, name: &str) -> Result<u32, String> {
    if name.contains('\0') {
        // alloc: error path
        return Err(format!("the function name {name:?}, which DWARF can't hold"));
    }
    let at = offset(strings.len(), ".debug_str")?;
    strings.extend_from_slice(name.as_bytes());
    strings.push(0);
    Ok(at)
}

/// A subprogram's entry: named by its name's offset in `.debug_str`, or unnamed.
fn entry(out: &mut [u8], name: Option<u32>, addr: u64, size: u32) {
    out[0] = if name.is_some() { dwarf::NAMED } else { dwarf::UNNAMED };
    out[1..5].copy_from_slice(&name.unwrap_or(0).to_le_bytes());
    out[5..13].copy_from_slice(&addr.to_le_bytes());
    out[13..17].copy_from_slice(&size.to_le_bytes());
}

/// The unit's header and entry, [`INFO_HEAD`] bytes, for a `.debug_info` of `len` bytes
/// (entries and end included) whose functions are within `low..high`.
fn info_head(head: &mut [u8], len: usize, low: u64, high: u64) -> Result<(), String> {
    let len = offset(len - 4, ".debug_info")?;
    head[..4].copy_from_slice(&len.to_le_bytes());
    head[4..6].copy_from_slice(&4u16.to_le_bytes());
    head[6..10].copy_from_slice(&0u32.to_le_bytes());
    head[10] = 8;
    head[11] = dwarf::UNIT;
    head[12..16].copy_from_slice(&0u32.to_le_bytes());
    head[16..18].copy_from_slice(&dwarf::DW_LANG_RUST.to_le_bytes());
    head[18..24].copy_from_slice(b"blaze\0");
    head[24..26].copy_from_slice(b".\0");
    head[26..30].copy_from_slice(&0u32.to_le_bytes());
    head[UNIT_RANGE..UNIT_RANGE + 8].copy_from_slice(&low.to_le_bytes());
    head[UNIT_RANGE + 8..INFO_HEAD].copy_from_slice(&(high - low).to_le_bytes());
    Ok(())
}

/// The abbreviations: the unit, a named subprogram and an unnamed one, both of [`DIE`] bytes.
fn abbrev() -> Vec<u8> {
    // alloc: once per whole layout, under 64 bytes
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&[dwarf::UNIT, dwarf::DW_TAG_COMPILE_UNIT, 1]);
    out.extend_from_slice(&[
        dwarf::DW_AT_PRODUCER,
        dwarf::DW_FORM_STRP,
        dwarf::DW_AT_LANGUAGE,
        dwarf::DW_FORM_DATA2,
        dwarf::DW_AT_NAME,
        dwarf::DW_FORM_STRING,
        dwarf::DW_AT_COMP_DIR,
        dwarf::DW_FORM_STRING,
        dwarf::DW_AT_STMT_LIST,
        dwarf::DW_FORM_SEC_OFFSET,
        dwarf::DW_AT_LOW_PC,
        dwarf::DW_FORM_ADDR,
        dwarf::DW_AT_HIGH_PC,
        DW_FORM_DATA8,
        0,
        0,
    ]);
    out.extend_from_slice(&[dwarf::NAMED, dwarf::DW_TAG_SUBPROGRAM, 0]);
    out.extend_from_slice(&[
        dwarf::DW_AT_LINKAGE_NAME,
        dwarf::DW_FORM_STRP,
        dwarf::DW_AT_LOW_PC,
        dwarf::DW_FORM_ADDR,
        dwarf::DW_AT_HIGH_PC,
        DW_FORM_DATA4,
        0,
        0,
    ]);
    out.extend_from_slice(&[dwarf::UNNAMED, dwarf::DW_TAG_SUBPROGRAM, 0]);
    uleb(&mut out, DW_AT_BLAZE_PAD);
    out.extend_from_slice(&[
        DW_FORM_DATA4,
        dwarf::DW_AT_LOW_PC,
        dwarf::DW_FORM_ADDR,
        dwarf::DW_AT_HIGH_PC,
        DW_FORM_DATA4,
        0,
        0,
    ]);
    out.push(0);
    out.resize(out.len().next_multiple_of(16), 0);
    out
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

fn read(exe: &impl Fn(u64, &mut [u8]), at: u64, len: usize) -> Vec<u8> {
    // alloc: once per update, a header or a table of the executable
    let mut out = vec![0; len];
    exe(at, &mut out);
    out
}

/// What a side file takes of the executable's own headers: the sections (ELF) or segments
/// (Mach-O) its addresses are in, its build id and where its symbol table is.
enum Table {
    Elf {
        /// The ELF header.
        header: [u8; 64],
        /// Each section header, with its name.
        sections: Vec<(String, [u8; SHDR])>,
        /// The build id note's bytes, and its address.
        note: Vec<u8>,
        note_addr: u64,
        /// The program headers, as the executable has them.
        phdrs: Vec<u8>,
        /// The indexes of `.symtab` and its string table, and each one's place in the file.
        symtab: Option<(usize, usize)>,
        symbols: (u64, u64),
        strings: (u64, u64),
    },
    MachO {
        /// The header's magic, CPU type and subtype.
        cpu: [u8; 12],
        uuid: [u8; 16],
        /// Each segment's load command, its file ranges zeroed.
        segments: Vec<Vec<u8>>,
        /// Where the highest segment ends in memory.
        top: u64,
        symbols: (u64, u64),
        strings: (u64, u64),
    },
}

/// The executable's headers, read through `exe`. An error names what a debugger could not
/// do without: a build id.
fn table_of(format: Format, exe: &impl Fn(u64, &mut [u8])) -> Result<Table, String> {
    match format {
        Format::Elf => {
            let mut header = [0u8; 64];
            exe(0, &mut header);
            if header[..4] != [0x7f, b'E', b'L', b'F'] || header[4] != 2 || header[5] != 1 {
                return Err("the executable is not a 64-bit little-endian ELF file: no side file is made of it".into());
            }
            let (shoff, shnum, shstrndx) =
                (u64_at(&header, 0x28), usize::from(u16_at(&header, 0x3c)), usize::from(u16_at(&header, 0x3e)));
            if usize::from(u16_at(&header, 0x3a)) != SHDR || shstrndx >= shnum {
                return Err("the executable's ELF section headers are not 64 bytes each with their names among them".into());
            }
            let headers = read(exe, shoff, shnum * SHDR);
            let of = |nth: usize| &headers[nth * SHDR..(nth + 1) * SHDR];
            let names =
                read(exe, u64_at(of(shstrndx), 24), u64_at(of(shstrndx), 32) as usize);
            // alloc: once per update, a name and a header per section
            let mut sections = Vec::with_capacity(shnum);
            let (mut note, mut note_addr, mut symtab) = (Vec::new(), 0, None);
            let phnum = usize::from(u16_at(&header, 0x38));
            if phnum != 0 && usize::from(u16_at(&header, 0x36)) != PHDR {
                return Err("the executable's ELF program headers are not 56 bytes each".into());
            }
            let phdrs = read(exe, u64_at(&header, 0x20), phnum * PHDR);
            let (mut symbols, mut strings) = ((0, 0), (0, 0));
            for nth in 0..shnum {
                let header: [u8; SHDR] = of(nth).try_into().expect("64 bytes");
                let from = (u32_at(&header, 0) as usize).min(names.len());
                let len = names[from..].iter().position(|it| *it == 0).unwrap_or(0);
                // alloc: as above
                let name = String::from_utf8_lossy(&names[from..from + len]).into_owned();
                if name == ELF_BUILD_ID {
                    note = read(exe, u64_at(&header, 24), u64_at(&header, 32) as usize);
                    note_addr = u64_at(&header, 16);
                }
                if u32_at(&header, 4) == SHT_SYMTAB {
                    let link = u32_at(&header, 40) as usize;
                    if link >= shnum {
                        return Err("the executable's .symtab names a string table past its sections".into());
                    }
                    symtab = Some((nth, link));
                    symbols = (u64_at(&header, 24), u64_at(&header, 32));
                    strings = (u64_at(of(link), 24), u64_at(of(link), 32));
                }
                sections.push((name, header));
            }
            if note.len() < 32 || note.len() > NOTE_ROOM || u32_at(&note, 4) < 16 {
                // alloc: error path
                return Err(format!(
                    "the executable's {ELF_BUILD_ID} is {} bytes, not a note of a 16-byte build id: a debugger could \
                     not tell a side file is this executable's",
                    note.len()
                ));
            }
            Ok(Table::Elf { header, sections, note, note_addr, phdrs, symtab, symbols, strings })
        }
        Format::MachO => {
            let header = read(exe, 0, 32);
            if u32_at(&header, 0) != MH_MAGIC_64 {
                return Err("the executable is not a 64-bit Mach-O file: no side file is made of it".into());
            }
            let commands = read(exe, 32, u32_at(&header, 20) as usize);
            // alloc: once per update, a command per segment
            let mut segments = Vec::new();
            let (mut uuid, mut top) = (None, 0u64);
            let (mut symbols, mut strings) = ((0, 0), (0, 0));
            let mut at = 0;
            for _ in 0..u32_at(&header, 16) {
                if at + 8 > commands.len() {
                    return Err("the executable's load commands end inside one".into());
                }
                let (kind, size) = (u32_at(&commands, at), u32_at(&commands, at + 4) as usize);
                if size < 8 || at + size > commands.len() {
                    return Err("the executable's load commands end inside one".into());
                }
                let command = &commands[at..at + size];
                match kind {
                    LC_UUID if size >= 24 => {
                        uuid = Some(command[8..24].try_into().expect("16 bytes"));
                    }
                    LC_SYMTAB if size >= 24 => {
                        symbols = (u64::from(u32_at(command, 8)), u64::from(u32_at(command, 12)) * NLIST);
                        strings = (u64::from(u32_at(command, 16)), u64::from(u32_at(command, 20)));
                    }
                    LC_SEGMENT_64 if size >= 72 && &command[8..15] != b"__DWARF" => {
                        // alloc: as above
                        let mut segment = command.to_vec();
                        top = top.max(u64_at(&segment, 24).saturating_add(u64_at(&segment, 32)));
                        // Nothing of the segment is in the side file: no file range, and each
                        // section's bytes and relocations at none.
                        segment[40..56].fill(0);
                        let sections = u32_at(&segment, 64) as usize;
                        if 72 + sections * 80 > size {
                            return Err("a segment of the executable ends inside its sections".into());
                        }
                        for nth in 0..sections {
                            let section = 72 + nth * 80;
                            segment[section + 48..section + 52].fill(0);
                            segment[section + 56..section + 64].fill(0);
                        }
                        segments.push(segment);
                    }
                    _ => {}
                }
                at += size;
            }
            let Some(uuid) = uuid else {
                return Err(
                    "the executable has no LC_UUID: a debugger could not tell a side file is this executable's".into()
                );
            };
            let cpu = header[..12].try_into().expect("12 bytes");
            Ok(Table::MachO { cpu, uuid, segments, top, symbols, strings })
        }
        Format::Pe | Format::Wasm => {
            // alloc: error path
            Err(format!("a debug side file of a {format:?} image, which blaze writes none for"))
        }
    }
}

impl Table {
    /// The executable's symbol table and its names, copied.
    fn copy(&self, exe: &impl Fn(u64, &mut [u8])) -> (Vec<u8>, Vec<u8>) {
        let (Table::Elf { symbols, strings, .. } | Table::MachO { symbols, strings, .. }) = self;
        (read(exe, symbols.0, symbols.1 as usize), read(exe, strings.0, strings.1 as usize))
    }

    /// The side file's bytes before its sections, for sections at `slots`.
    fn head(&self, input: &Input<'_>, slots: &[Slot; 6]) -> Result<Vec<u8>, String> {
        match self {
            Table::Elf { header, sections, note, note_addr, phdrs, symtab, .. } => {
                // The executable's sections keep their indexes, which its symbols name: one
                // in memory is here with no bytes, the build id note with its own, the symbol
                // table and its names with their copies, and any other is no section.
                // alloc: once per update, the headers (a few kilobytes)
                let mut names = vec![0u8];
                let mut headers: Vec<[u8; SHDR]> = Vec::with_capacity(sections.len() + 5);
                fn name(names: &mut Vec<u8>, name: &str) -> u32 {
                    let at = names.len() as u32;
                    names.extend_from_slice(name.as_bytes());
                    names.push(0);
                    at
                }
                for (nth, (section, exe)) in sections.iter().enumerate() {
                    let mut out = *exe;
                    let alloc = u64_at(exe, 8) & SHF_ALLOC != 0;
                    let table = symtab.filter(|it| it.0 == nth || it.1 == nth);
                    if nth == 0 {
                        out = [0; SHDR];
                    } else if alloc && section == ELF_BUILD_ID {
                        out[24..32].copy_from_slice(&(NOTE_AT as u64).to_le_bytes());
                    } else if alloc {
                        out[4..8].copy_from_slice(&SHT_NOBITS.to_le_bytes());
                        out[24..32].copy_from_slice(&(NOTE_AT as u64).to_le_bytes());
                        // What it links to is not here to be read.
                        out[40..48].fill(0);
                    } else if let Some((symbols, _)) = table {
                        let slot = if nth == symbols { slots[SYMTAB] } else { slots[STRTAB] };
                        out[24..32].copy_from_slice(&slot.at.to_le_bytes());
                        out[32..40].copy_from_slice(&slot.len.to_le_bytes());
                    } else {
                        out = [0; SHDR];
                        out[4..8].copy_from_slice(&SHT_NULL.to_le_bytes());
                    }
                    if out != [0; SHDR] {
                        let at = name(&mut names, section);
                        out[..4].copy_from_slice(&at.to_le_bytes());
                    }
                    headers.push(out);
                }
                for (nth, section) in DWARF_NAMES.iter().enumerate() {
                    let mut out = [0u8; SHDR];
                    out[..4].copy_from_slice(&name(&mut names, section).to_le_bytes());
                    out[4..8].copy_from_slice(&SHT_PROGBITS.to_le_bytes());
                    out[24..32].copy_from_slice(&slots[nth].at.to_le_bytes());
                    out[32..40].copy_from_slice(&slots[nth].len.to_le_bytes());
                    out[48..56].copy_from_slice(&1u64.to_le_bytes());
                    headers.push(out);
                }
                let mut out = [0u8; SHDR];
                out[..4].copy_from_slice(&name(&mut names, ".shstrtab").to_le_bytes());
                out[4..8].copy_from_slice(&SHT_STRTAB.to_le_bytes());
                let names_at = PHDRS_AT + phdrs.len();
                out[24..32].copy_from_slice(&(names_at as u64).to_le_bytes());
                out[32..40].copy_from_slice(&(names.len() as u64).to_le_bytes());
                out[48..56].copy_from_slice(&1u64.to_le_bytes());
                headers.push(out);
                if headers.len() >= 0xff00 {
                    // alloc: error path
                    return Err(format!("{} sections, more than ELF numbers", headers.len()));
                }
                let shoff = (names_at + names.len()).next_multiple_of(8);
                // alloc: as above
                let mut head = vec![0u8; shoff + headers.len() * SHDR];
                head[..64].copy_from_slice(header);
                // The executable's program headers, as `objcopy --only-keep-debug` leaves
                // them: every segment where it is in memory and with nothing in the file, but
                // the note that is the build id. gdb and lldb take the load addresses of a
                // separate debug file from them, as they do a distribution's.
                head[0x20..0x28].copy_from_slice(&match phdrs.is_empty() {
                    true => 0u64.to_le_bytes(),
                    false => (PHDRS_AT as u64).to_le_bytes(),
                });
                head[PHDRS_AT..names_at].copy_from_slice(phdrs);
                for phdr in head[PHDRS_AT..names_at].chunks_exact_mut(PHDR) {
                    let build_id = u32_at(phdr, 0) == PT_NOTE && u64_at(phdr, 16) == *note_addr;
                    let (at, len) = match build_id {
                        true => (NOTE_AT as u64, note.len() as u64),
                        false => (u64_at(phdr, 8), 0),
                    };
                    phdr[8..16].copy_from_slice(&at.to_le_bytes());
                    phdr[32..40].copy_from_slice(&len.to_le_bytes());
                    if build_id {
                        phdr[40..48].copy_from_slice(&len.to_le_bytes());
                    }
                }
                head[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
                head[0x3c..0x3e].copy_from_slice(&(headers.len() as u16).to_le_bytes());
                head[0x3e..0x40].copy_from_slice(&(headers.len() as u16 - 1).to_le_bytes());
                head[NOTE_AT..NOTE_AT + note.len()].copy_from_slice(note);
                head[names_at..names_at + names.len()].copy_from_slice(&names);
                for (nth, header) in headers.iter().enumerate() {
                    head[shoff + nth * SHDR..shoff + (nth + 1) * SHDR].copy_from_slice(header);
                }
                if head[NOTE_AT + 16..NOTE_AT + 32] != input.id {
                    // alloc: error path
                    return Err(format!(
                        "{}: the executable's build id note holds {:02x?}, and the image's id is {:02x?}: the side \
                         file would describe another executable",
                        input.name,
                        &head[NOTE_AT + 16..NOTE_AT + 32],
                        input.id
                    ));
                }
                Ok(head)
            }
            Table::MachO { cpu, uuid, segments, top, .. } => {
                if *uuid != input.id {
                    // alloc: error path
                    return Err(format!(
                        "{}: the executable's LC_UUID is {uuid:02x?}, and the image's id is {:02x?}: the side file \
                         would describe another executable",
                        input.name, input.id
                    ));
                }
                let dwarf = &slots[..4];
                let (from, to) = (dwarf[0].at, dwarf[3].at + dwarf[3].cap);
                let base = top.next_multiple_of(0x4000);
                // alloc: once per update, the load commands (a few kilobytes)
                let mut commands = Vec::with_capacity(1024);
                commands.extend_from_slice(&LC_UUID.to_le_bytes());
                commands.extend_from_slice(&24u32.to_le_bytes());
                commands.extend_from_slice(uuid);
                for segment in segments {
                    commands.extend_from_slice(segment);
                }
                commands.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
                commands.extend_from_slice(&(72 + 4 * 80u32).to_le_bytes());
                commands.extend_from_slice(b"__DWARF\0\0\0\0\0\0\0\0\0");
                commands.extend_from_slice(&base.to_le_bytes());
                commands.extend_from_slice(&(to - from).next_multiple_of(0x1000).to_le_bytes());
                commands.extend_from_slice(&from.to_le_bytes());
                commands.extend_from_slice(&(to - from).to_le_bytes());
                // Readable, never mapped: maxprot and initprot as dsymutil writes them.
                commands.extend_from_slice(&7u32.to_le_bytes());
                commands.extend_from_slice(&3u32.to_le_bytes());
                commands.extend_from_slice(&4u32.to_le_bytes());
                commands.extend_from_slice(&0u32.to_le_bytes());
                for (slot, name) in dwarf.iter().zip(DWARF_NAMES) {
                    let mut section = [0u8; 80];
                    // `.debug_line` is `__debug_line`, as Apple's tools name it.
                    section[..2].copy_from_slice(b"__");
                    section[2..2 + name.len() - 1].copy_from_slice(&name.as_bytes()[1..]);
                    section[16..23].copy_from_slice(b"__DWARF");
                    section[32..40].copy_from_slice(&(base + slot.at - from).to_le_bytes());
                    section[40..48].copy_from_slice(&slot.len.to_le_bytes());
                    let at = offset(slot.at as usize, "the side file")?;
                    section[48..52].copy_from_slice(&at.to_le_bytes());
                    section[64..68].copy_from_slice(&S_ATTR_DEBUG.to_le_bytes());
                    commands.extend_from_slice(&section);
                }
                commands.extend_from_slice(&LC_SYMTAB.to_le_bytes());
                commands.extend_from_slice(&24u32.to_le_bytes());
                for value in [
                    slots[SYMTAB].at,
                    slots[SYMTAB].len / NLIST,
                    slots[STRTAB].at,
                    slots[STRTAB].len,
                ] {
                    commands.extend_from_slice(&offset(value as usize, "the side file")?.to_le_bytes());
                }
                // alloc: as above
                let mut head = Vec::with_capacity(32 + commands.len());
                head.extend_from_slice(cpu);
                head.extend_from_slice(&MH_DSYM.to_le_bytes());
                head.extend_from_slice(&(segments.len() as u32 + 3).to_le_bytes());
                head.extend_from_slice(&(commands.len() as u32).to_le_bytes());
                head.extend_from_slice(&[0; 8]);
                head.extend_from_slice(&commands);
                Ok(head)
            }
        }
    }
}

#[cfg(test)]
mod tests;
