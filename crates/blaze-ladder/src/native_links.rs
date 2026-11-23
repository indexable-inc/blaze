//! A crate's link table as bytes, and the program's native image made from those tables (run.md,
//! "What a run takes from the request, and the native image as a task"; piece
//! native-drive-kinds).
//!
//! - The TABLE is front2 links.rs's [`CrateLinks`] of one crate, made by its `CrateScope` job
//!   from the names that job already holds ([`scope_table`]: nothing re-made, rule 77) and filed
//!   as a section of that job's record. The record lists a `Links` part whose source is the
//!   table's digest ([`table_digest`]): a re-solve that leaves the table equal moves no reader;
//!   a private `#[no_mangle]` fn added moves it. A crate whose table is refused files the
//!   refusal as its table (state 1), so the crate's check goes on and only a program that links
//!   it fails, naming the crate and the rule.
//! - The IMAGE is natives.rs's BNL2 record, made by [`program_image`] from each crate's table
//!   ([`open_table`]) and its build script's outputs record, in the order the caller gives
//!   (reverse topological, the program's crate first).
//!
//! Table form (little endian): [`TABLE_MAGIC`] (4), [`TABLE_FORM`] (u16), state (u8: 0 a
//! table, 1 refused), one zero byte; refused: the words (u32 length, UTF-8); a table: decls,
//! foreign and exports counts (u32 each), then each decl (name, kind u8, modifiers u8: bit 0
//! whole-archive, bit 1 verbatim, the block's item index u32), each foreign name, each export
//! (name, item key 32 bytes, identifier); a name is a u32 length and its UTF-8. Positions are
//! item indices of the text's outline (rule 71); the lists are in links.rs's order (decls in
//! source order, foreign and exports sorted), so one crate gives one byte string anywhere.

use blaze_build_script::outputs::OutputsRef;
use blaze_hash::FixedState;
use blaze_ids::producer::{ProducerId, Whole};
use blaze_ids::{ContentId, Hasher};
pub use blaze_native::Platform;
use blaze_own::{AVec, Alloc, Bump};
use blaze_scope::resolve::CrateNames;
use blaze_surface::ItemIndex;

use crate::front2::links::{CrateExport, CrateLinks, LinkDecl, LinkKind, LinksError, Modifiers, scope_links};
pub use crate::natives::Asked;
use crate::natives::{CrateNative, NativeError, NativeInputs, NativeSummary, asked, native_key, native_layout, native_library};
pub use blaze_native::libraries::{Library, LibraryKind, Linked, Source};
pub use blaze_native::tbd::TbdError;

pub const TABLE_MAGIC: [u8; 4] = *b"BZLK";
/// The table's FORMAT number: a change of the layout above says so here.
pub const TABLE_FORM: u16 = 1;
const DIGEST_DOMAIN: &str = "blaze-ladder link table digest";
const HEAD: usize = 8;

/// The digest a `Links` part's source and out are: of the table's whole bytes.
pub fn table_digest(table: &[u8]) -> ContentId {
    let mut h = Hasher::new(DIGEST_DOMAIN);
    h.update(&(table.len() as u64).to_le_bytes()).update(table);
    h.finish()
}

/// The table of the crate `c` resolved by its `CrateScope` job, as bytes appended to `out`. A
/// refused table is written as its refusal's words (state 1), never as an empty table.
pub fn scope_table<'a>(c: &CrateNames<'a>, out: &mut AVec<'_, u8>, bump: Bump<'a>) -> Result<(), TableError> {
    match scope_links(c, bump) {
        Ok(links) => write_table(&links, out),
        Err(refused) => {
            let mut words = AVec::new_in(bump.alloc());
            write_words(&mut words, refused).map_err(|_| TableError::Alloc)?;
            head(out, 1)?;
            put_str(out, core::str::from_utf8(&words).map_err(|_| TableError::Alloc)?)
        }
    }
}

/// `links` as a state-0 table.
pub fn write_table(links: &CrateLinks<'_>, out: &mut AVec<'_, u8>) -> Result<(), TableError> {
    head(out, 0)?;
    for n in [links.decls.len(), links.foreign.len(), links.exports.len()] {
        put_u32(out, n)?;
    }
    for d in links.decls.iter() {
        put_str(out, d.name)?;
        let kind = match d.kind {
            LinkKind::Dylib => 0u8,
            LinkKind::Static => 1,
            LinkKind::Framework => 2,
        };
        let mods = u8::from(d.modifiers.whole_archive) | (u8::from(d.modifiers.verbatim) << 1);
        push_all(out, &[kind, mods])?;
        push_all(out, &d.block.0.to_le_bytes())?;
    }
    for name in links.foreign.iter() {
        put_str(out, name)?;
    }
    for e in links.exports.iter() {
        put_str(out, e.name)?;
        push_all(out, &e.item.0.0)?;
        put_str(out, e.ident)?;
    }
    Ok(())
}

/// Why a table could not be written or read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableError {
    Alloc,
    /// A list longer than a u32 counts.
    TooLong,
    /// Not a table of this form: `why` names what was wrong, `at` the byte.
    Form { at: usize, why: &'static str },
}

/// A table read back: the crate's link facts, or its refusal's words.
pub enum Table<'a> {
    Links(CrateLinks<'a>),
    Refused(&'a str),
}

/// Reads a table written by [`write_table`] or [`scope_table`], every range checked once; the
/// names borrow `bytes`.
pub fn open_table<'a>(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<Table<'a>, TableError> {
    let mut r = Reader { b: bytes, at: 0 };
    let magic = r.take(4)?;
    if magic != TABLE_MAGIC {
        return Err(TableError::Form { at: 0, why: "no link table mark" });
    }
    let form = u16::from_le_bytes([r.u8()?, r.u8()?]);
    if form != TABLE_FORM {
        return Err(TableError::Form { at: 4, why: "a link table of another form" });
    }
    let state = r.u8()?;
    r.take(1)?;
    let table = match state {
        1 => Table::Refused(r.str()?),
        0 => {
            let (nd, nf, ne) = (r.u32()?, r.u32()?, r.u32()?);
            let mut links = CrateLinks { decls: AVec::new_in(alloc), foreign: AVec::new_in(alloc), exports: AVec::new_in(alloc) };
            for _ in 0..nd {
                let name = r.str()?;
                let at = r.at;
                let kind = match r.u8()? {
                    0 => LinkKind::Dylib,
                    1 => LinkKind::Static,
                    2 => LinkKind::Framework,
                    _ => return Err(TableError::Form { at, why: "a link kind byte past framework" }),
                };
                let mods = r.u8()?;
                if mods > 3 {
                    return Err(TableError::Form { at: at + 1, why: "a modifiers byte with bits past verbatim" });
                }
                let modifiers = Modifiers { whole_archive: mods & 1 != 0, verbatim: mods & 2 != 0 };
                let block = ItemIndex(r.u32()?);
                links.decls.try_push(LinkDecl { name, kind, modifiers, block }).map_err(|_| TableError::Alloc)?;
            }
            for _ in 0..nf {
                let name = r.str()?;
                links.foreign.try_push(name).map_err(|_| TableError::Alloc)?;
            }
            for _ in 0..ne {
                let name = r.str()?;
                let mut key = [0u8; 32];
                key.copy_from_slice(r.take(32)?);
                let ident = r.str()?;
                let item = blaze_id::ItemKey(ContentId(key));
                links.exports.try_push(CrateExport { name, item, ident }).map_err(|_| TableError::Alloc)?;
            }
            Table::Links(links)
        }
        _ => return Err(TableError::Form { at: 6, why: "a link table state past refused" }),
    };
    if r.at != bytes.len() {
        return Err(TableError::Form { at: r.at, why: "bytes after the link table's last list" });
    }
    Ok(table)
}

/// One crate of the program as [`program_image`] takes it.
pub struct ProgramCrate<'a> {
    /// The crate's name, as messages and the image's key name it.
    pub package: &'a str,
    /// Its build script's outputs record and that record's out, when it has a script.
    pub outputs: Option<(ContentId, OutputsRef<'a>)>,
    pub links: &'a CrateLinks<'a>,
}

/// The image's key (natives.rs `native_key`: the link line, the root and export names, the
/// libraries; never a Rust body).
pub fn program_key<'a>(
    crates: &'a [ProgramCrate<'a>],
    installs: &'a [Linked<'a>],
    platform: Platform,
    producer: &'a ProducerId<Whole>,
    alloc: Alloc<'a>,
) -> Result<ContentId, NativeError<'a>> {
    let natives = as_natives(crates, alloc)?;
    native_key(&NativeInputs { platform, producer, crates: &natives, installs }, alloc)
}

/// The libraries the program's link line asks for, each once, in link order, by where its
/// install name comes from (natives.rs `asked`): the caller reads each SDK stub
/// (`Source::Stub`) through the file system it holds and hands the libraries back resolved as
/// [`program_key`]'s and [`program_image`]'s `installs`.
pub fn program_libraries<'a>(crates: &'a [ProgramCrate<'a>], platform: Platform, producer: &'a ProducerId<Whole>, alloc: Alloc<'a>) -> Result<AVec<'a, Asked<'a>>, NativeError<'a>> {
    let natives = as_natives(crates, alloc)?;
    asked(&NativeInputs { platform, producer, crates: &natives, installs: &[] }, alloc)
}

/// The image's bytes appended to `out` (natives.rs `native_library`): a program with no C
/// input still gets a real image naming only the system library.
pub fn program_image<'a>(
    crates: &'a [ProgramCrate<'a>],
    installs: &'a [Linked<'a>],
    platform: Platform,
    producer: &'a ProducerId<Whole>,
    seed: FixedState,
    out: &mut AVec<'_, u8>,
    bump: Bump<'a>,
) -> Result<NativeSummary, NativeError<'a>> {
    let natives = as_natives(crates, bump.alloc())?;
    native_library(&NativeInputs { platform, producer, crates: &natives, installs }, seed, out, bump)
}

/// The laid-out native image at image-relative addresses (base 0) for `blaze export`'s native
/// merge (`blaze_export::ProgramIn::natives`).
pub fn program_layout<'a>(
    crates: &'a [ProgramCrate<'a>],
    installs: &'a [Linked<'a>],
    platform: Platform,
    producer: &'a ProducerId<Whole>,
    seed: FixedState,
    bump: Bump<'a>,
) -> Result<blaze_native::image::Image<'a>, NativeError<'a>> {
    let natives = as_natives(crates, bump.alloc())?;
    native_layout(&NativeInputs { platform, producer, crates: &natives, installs }, seed, bump)
}

fn as_natives<'a>(crates: &'a [ProgramCrate<'a>], alloc: Alloc<'a>) -> Result<AVec<'a, CrateNative<'a>>, NativeError<'a>> {
    let mut natives: AVec<'a, CrateNative<'a>> = AVec::try_with_capacity_in(crates.len(), alloc).map_err(|_| NativeError::Alloc)?;
    for c in crates.iter() {
        natives.try_push(CrateNative { package: c.package, outputs: c.outputs, links: c.links }).map_err(|_| NativeError::Alloc)?;
    }
    Ok(natives)
}

fn head(out: &mut AVec<'_, u8>, state: u8) -> Result<(), TableError> {
    push_all(out, &TABLE_MAGIC)?;
    push_all(out, &TABLE_FORM.to_le_bytes())?;
    push_all(out, &[state, 0])
}

fn push_all(out: &mut AVec<'_, u8>, bytes: &[u8]) -> Result<(), TableError> {
    out.try_reserve(bytes.len()).map_err(|_| TableError::Alloc)?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_u32(out: &mut AVec<'_, u8>, n: usize) -> Result<(), TableError> {
    let n = u32::try_from(n).map_err(|_| TableError::TooLong)?;
    push_all(out, &n.to_le_bytes())
}

fn put_str(out: &mut AVec<'_, u8>, s: &str) -> Result<(), TableError> {
    put_u32(out, s.len())?;
    push_all(out, s.as_bytes())
}

/// A refusal's words, as `{:?}` prints the typed error (it names the crate and the item).
fn write_words(out: &mut AVec<'_, u8>, refused: LinksError<'_>) -> core::fmt::Result {
    struct Sink<'o, 'v>(&'o mut AVec<'v, u8>);
    impl core::fmt::Write for Sink<'_, '_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0.try_reserve(s.len()).map_err(|_| core::fmt::Error)?;
            self.0.extend_from_slice(s.as_bytes());
            Ok(())
        }
    }
    core::fmt::write(&mut Sink(out), format_args!("the crate's link table was refused (front2 links.rs): {refused:?}"))
}

struct Reader<'b> {
    b: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], TableError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.b.len()).ok_or(TableError::Form { at: self.at, why: "the link table is cut short" })?;
        let bytes = &self.b[self.at..end];
        self.at = end;
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8, TableError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, TableError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn str(&mut self) -> Result<&'b str, TableError> {
        let at = self.at;
        let n = self.u32()? as usize;
        core::str::from_utf8(self.take(n)?).map_err(|_| TableError::Form { at, why: "a name that is not UTF-8" })
    }
}

#[cfg(test)]
#[path = "native_links_tests.rs"]
mod tests;
