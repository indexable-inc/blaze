//! shelf-pins (main's design, wave 3): the shelf's programs are pinned one by one.
//!
//! The pin table ([`blaze_pins::CLANG_JSON`], `pins/clang.json`) pins the whole
//! archive by its SHA-256 (`targets`). Beside it, `shelf_programs` holds per host target one row
//! per program placed on the shelf (clang-23, libclang.dylib, lld, llvm-ar, llvm-nm): the
//! member's path in the archive (`member`, under the archive's top folder) and the member's own
//! SHA-256 (`sha256`). A row's place on the shelf is its member without the top folder, one of
//! [`crate::toolchain::SHELF_MEMBERS`].
//!
//! The step that places a program on the shelf ([`place_member`]) checks the extracted member
//! against its row ([`check_member`]) and only then puts it there: a member with no row, or
//! whose digest differs, is refused by a typed error naming the member ([`ShelfPinError`]).
//! What is placed is named by the proof [`PinnedMember`], which only [`check_member`] makes.
//! The host-tools table names a shelf program by its row ([`shelf_program`]), so a tool with no
//! row cannot be granted (host_tools.rs).
//!
//! Sans-IO: the caller extracts the archive, digests the member and puts it on the shelf
//! ([`ShelfPlacer`]).
#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};

use blaze_pins::CLANG_PATH;

use crate::toolchain::{SHELF_MEMBERS, hex32, object, scalar};

/// One program's row: the archive member and its SHA-256.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShelfProgram<'t> {
    /// The row's name (`llvm-nm`): what a host-tool row's `shelf` says.
    pub name: &'t str,
    /// The member's path in the archive (`LLVM-23.1.4-macOS-ARM64/bin/llvm-nm`).
    pub member: &'t str,
    /// Where it is on the shelf: the member without the archive's top folder (`bin/llvm-nm`),
    /// one of [`SHELF_MEMBERS`].
    pub shelf_path: &'t str,
    /// The member's own SHA-256.
    pub sha256: [u8; 32],
}

/// A shelf program that is not pinned, or whose bytes are not the pinned ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShelfPinError<'t> {
    /// No `shelf_programs` row for the target.
    NoTable { target: &'t str },
    /// No row names this program (looked up by a host-tool row's `shelf`).
    NoProgram { target: &'t str, program: &'t str },
    /// No row pins this archive member: it is not placed.
    NoRow { target: &'t str, member: &'t str },
    /// A row is there but `field` is missing or not of its form (`names` unsorted, a `member`
    /// with no top folder or outside [`SHELF_MEMBERS`], a `sha256` not 64 hex digits).
    Malformed { target: &'t str, program: &'t str, field: &'static str },
    /// The extracted member's SHA-256 is not its row's: it is not placed.
    Differs { target: &'t str, member: &'t str, want: [u8; 32], got: [u8; 32] },
}

struct Hex<'b>(&'b [u8; 32]);

impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ShelfPinError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ShelfPinError::NoTable { target } => write!(f, "{CLANG_PATH}: no `shelf_programs` row for {target}: no program is placed on its shelf (shelf-pins)"),
            ShelfPinError::NoProgram { target, program } => {
                write!(f, "{CLANG_PATH}: {target}'s `shelf_programs` has no row `{program}`: it is not on the shelf and cannot be granted (shelf-pins)")
            }
            ShelfPinError::NoRow { target, member } => {
                write!(f, "{member}: no row of {CLANG_PATH}'s `shelf_programs` for {target} pins this archive member: it is not placed on the shelf (shelf-pins)")
            }
            ShelfPinError::Malformed { target, program, field } => {
                write!(f, "{CLANG_PATH}: {target}'s shelf program `{program}` has no `{field}` of its form (shelf-pins)")
            }
            ShelfPinError::Differs { target, member, want, got } => write!(
                f,
                "{member}: sha256 {} after extraction, but {CLANG_PATH}'s `shelf_programs` for {target} pins {}: it is not placed on the shelf (shelf-pins)",
                Hex(got),
                Hex(want)
            ),
        }
    }
}

/// `target`'s row of `shelf_programs` in `text` and its sorted `names` (the row's last field).
fn table<'t>(text: &'t str, target: &'t str) -> Result<(&'t str, &'t str), ShelfPinError<'t>> {
    let (rows, _) = object(text, "shelf_programs", 0).ok_or(ShelfPinError::NoTable { target })?;
    let (row, _) = object(rows, target, 0).ok_or(ShelfPinError::NoTable { target })?;
    if !row.starts_with('{') {
        return Err(ShelfPinError::NoTable { target });
    }
    let names = scalar(row, "names").ok_or(ShelfPinError::Malformed { target, program: "", field: "names" })?;
    Ok((row, names))
}

/// The row `program` of `target`'s table in `row` (one name of `names`).
fn program_row<'t>(row: &'t str, target: &'t str, program: &'t str) -> Result<ShelfProgram<'t>, ShelfPinError<'t>> {
    let malformed = |field| ShelfPinError::Malformed { target, program, field };
    let (body, _) = object(row, program, 0).ok_or(malformed("object"))?;
    if !body.starts_with('{') {
        return Err(malformed("object"));
    }
    let member = scalar(body, "member").ok_or(malformed("member"))?;
    let shelf_path = match member.split_once('/') {
        Some((top, rest)) if !top.is_empty() && SHELF_MEMBERS.iter().any(|m| *m == rest) => rest,
        _ => return Err(malformed("member")),
    };
    let sha256 = scalar(body, "sha256").and_then(hex32).ok_or(malformed("sha256"))?;
    Ok(ShelfProgram { name: program, member, shelf_path, sha256 })
}

/// Every row of `target`'s table in `text` ([`blaze_pins::CLANG_JSON`]), in `names`' (sorted)
/// order; a name out of order or given twice is refused (field `names`).
pub fn shelf_programs<'t, 'a>(text: &'t str, target: &'t str, alloc: Alloc<'a>) -> Result<AVec<'a, ShelfProgram<'t>>, ShelfPinError<'t>> {
    let (row, names) = table(text, target)?;
    let mut out: AVec<'a, ShelfProgram<'t>> = Vec::new_in(alloc);
    let mut last: Option<&str> = None;
    for name in names.split(' ').filter(|n| !n.is_empty()) {
        if last.is_some_and(|l| l >= name) {
            return Err(ShelfPinError::Malformed { target, program: name, field: "names" });
        }
        last = Some(name);
        out.push(program_row(row, target, name)?);
    }
    Ok(out)
}

/// The row named `program` (a host-tool row's `shelf`): refused by name when `names` does not
/// list it ([`ShelfPinError::NoProgram`]), so a tool with no row cannot be granted.
pub fn shelf_program<'t>(text: &'t str, target: &'t str, program: &'t str) -> Result<ShelfProgram<'t>, ShelfPinError<'t>> {
    let (row, names) = table(text, target)?;
    if !names.split(' ').any(|n| n == program) {
        return Err(ShelfPinError::NoProgram { target, program });
    }
    program_row(row, target, program)
}

/// Proof that an extracted member's bytes are its row's: made only by [`check_member`]; the
/// placer puts on the shelf only what one names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PinnedMember<'t> {
    row: ShelfProgram<'t>,
}

impl<'t> PinnedMember<'t> {
    /// The row it was checked against: `member` is where it was extracted, `shelf_path` where it
    /// goes.
    pub fn row(&self) -> &ShelfProgram<'t> {
        &self.row
    }
}

/// Checks `got`, the SHA-256 of an extracted member, against its row: the proof, or
/// [`ShelfPinError::Differs`] naming the member and both digests.
pub fn check_member<'t>(target: &'t str, row: ShelfProgram<'t>, got: [u8; 32]) -> Result<PinnedMember<'t>, ShelfPinError<'t>> {
    if got != row.sha256 {
        return Err(ShelfPinError::Differs { target, member: row.member, want: row.sha256, got });
    }
    Ok(PinnedMember { row })
}

/// The IO a placing step needs: the caller's, over the folder the archive was extracted into and
/// the shelf.
pub trait ShelfPlacer {
    type Error;
    /// The SHA-256 of the extracted member at `member` (its path in the archive).
    fn digest(&mut self, member: &str) -> Result<[u8; 32], Self::Error>;
    /// Puts the checked member at `<shelf>/<shelf_path>` (its mode kept).
    fn put(&mut self, pinned: &PinnedMember<'_>) -> Result<(), Self::Error>;
}

/// Why a member was not placed: its pin refused it, or the placer's IO failed on it.
#[derive(Debug)]
pub enum PlaceError<'t, E> {
    Pin(ShelfPinError<'t>),
    Io { member: &'t str, error: E },
}

/// The step that places the archive member `member` (extracted already) on `target`'s shelf:
/// its row is found by `member` ([`ShelfPinError::NoRow`] when none pins it), its extracted
/// bytes' digest is checked against the row ([`check_member`]), and only then is it put there.
/// Nothing is put when either refuses.
pub fn place_member<'t, P: ShelfPlacer>(
    text: &'t str,
    target: &'t str,
    member: &'t str,
    placer: &mut P,
) -> Result<PinnedMember<'t>, PlaceError<'t, P::Error>> {
    let (row, names) = table(text, target).map_err(PlaceError::Pin)?;
    let mut found: Option<ShelfProgram<'t>> = None;
    for name in names.split(' ').filter(|n| !n.is_empty()) {
        let program = program_row(row, target, name).map_err(PlaceError::Pin)?;
        if program.member == member {
            found = Some(program);
            break;
        }
    }
    let program = found.ok_or(PlaceError::Pin(ShelfPinError::NoRow { target, member }))?;
    let got = placer.digest(member).map_err(|error| PlaceError::Io { member, error })?;
    let pinned = check_member(target, program, got).map_err(PlaceError::Pin)?;
    placer.put(&pinned).map_err(|error| PlaceError::Io { member, error })?;
    Ok(pinned)
}
