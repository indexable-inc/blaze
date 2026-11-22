//! The tar reader: a sans-IO state machine fed the archive in any pieces. Each
//! [`Reader::step`] takes what it can of its input and hands out at most one [`Event`]: an
//! entry, a piece of the current file's bytes, or the end. File bytes are handed out from the
//! caller's input as they come, never buffered; only metadata (a pax header, a GNU long name,
//! at most [`Limits::most_meta`] bytes) and the current entry's names are kept.
//!
//! **What is refused, each naming the entry.** A header whose checksum is wrong; a path or link
//! target that is absolute, has a `..`, a NUL or a `\`, is not UTF-8 or is too long; a device
//! or FIFO; a link, unless [`Links::Inside`]; then a symbolic link whose target leaves the
//! destination, any path through an earlier symbolic link, and a symbolic link at a directory
//! an earlier link's target runs through (which would move it); a file, the files together or
//! the entries past the caller's bounds; and an archive that ends before its two zero blocks.
//!
//! With these held, writing the entries in order under one directory, every link last (as
//! blaze-fetch does), writes nothing outside it and follows no link. The reader knows nothing
//! of what is already on disk: a destination must start empty.

// adapted from makepad/relax (MIT), libs/relax/base/src/tar.rs: the number fields (octal and
// base-256), the checksum, the GNU `L`/`K` and pax `path`/`linkpath`/`size` records and the ustar
// prefix; written here as a state machine fed in pieces, with the path rules above.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{
    Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    error::{Error, Field, Name, PathWhy, Why},
    path::{normalize, resolve, within},
    plant::TarPlant,
};

pub(crate) const BLOCK: usize = 512;

/// Whether an archive may hold links.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Links {
    /// Any link is refused.
    Refuse,
    /// Links are taken when they stay inside (see the module).
    Inside,
}

/// How much an archive may hold: every field is the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of a path or link target.
    pub most_path: u32,
    /// The most bytes of a pax header or GNU long name.
    pub most_meta: u32,
    /// The most bytes of one file.
    pub most_entry: u64,
    /// The most bytes of the files together.
    pub most_total: u64,
    /// The most entries, metadata headers included.
    pub most_entries: u32,
    pub links: Links,
}

/// What an entry is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind<'a> {
    /// A regular file; its bytes follow as [`Event::Data`].
    File,
    Dir,
    /// A symbolic link: its target as written and as resolved from the top (normal form).
    Symlink { target: &'a str, resolved: &'a str },
    /// A hard link to an earlier entry's path (normal form).
    HardLink { target: &'a str },
}

/// One entry's header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'a> {
    /// Its path from the top, in normal form (see [`crate::path`]).
    pub path: &'a str,
    pub kind: Kind<'a>,
    /// The bytes of a file; 0 for anything else.
    pub size: u64,
    /// The permission bits (`0o7777` at most).
    pub mode: u32,
    /// Seconds since 1970 as the header says them.
    pub mtime: u64,
}

/// What a step found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'a> {
    Entry(Entry<'a>),
    /// The next bytes of the current file, from the step's input; `last` on its final piece.
    Data { bytes: &'a [u8], last: bool },
    /// The archive's end: its two zero blocks were read. Nothing more is taken.
    End,
}

/// One step's result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Step<'a> {
    /// Bytes of the step's input taken.
    pub consumed: usize,
    /// `None`: all of the input was taken and more is needed.
    pub event: Option<Event<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// A header block is being taken.
    Header,
    /// One zero block was read: the block being taken must be zero too, the archive's end.
    SecondZero,
    Meta { kind: u8, left: u64, pad: u64 },
    Body { left: u64, pad: u64 },
    Skip { left: u64 },
    End,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tag {
    File,
    Dir,
    Symlink,
    HardLink,
}

/// What the machine found, before it is lent out as an [`Event`].
enum Out {
    Need,
    Entry,
    Data { from: usize, to: usize, last: bool },
    End,
}

/// The tar reader: see the module. `P` is the plant seam (D165): shipped code names
/// [`Reader`], this with [`NoPlant`].
pub struct PlantedReader<A: Allocator, P> {
    limits: Limits,
    state: State,
    header: [u8; BLOCK],
    have: usize,
    meta: Vec<u8, A>,
    /// A GNU or pax long path and link target for the next entry.
    long_path: Option<Vec<u8, A>>,
    long_link: Option<Vec<u8, A>>,
    pax_size: Option<u64>,
    raw: Vec<u8, A>,
    path: Vec<u8, A>,
    target: Vec<u8, A>,
    resolved: Vec<u8, A>,
    /// Every symbolic link so far: `path NUL resolved NUL`, end to end.
    links: Vec<u8, A>,
    tag: Tag,
    size: u64,
    mode: u32,
    mtime: u64,
    /// Bytes of the archive taken, the entry's header offset and index, and the files' total.
    offset: u64,
    entry_at: u64,
    entries: u32,
    total: u64,
    alloc: A,
    plant: P,
}

/// The reader as it ships: [`PlantedReader`] with no plant.
pub type Reader<A> = PlantedReader<A, NoPlant>;

impl<A: Allocator + Clone> PlantedReader<A, NoPlant> {
    /// A reader held to `limits`, its buffers in `alloc`.
    pub fn new_in(limits: Limits, alloc: A) -> Reader<A> {
        PlantedReader::new_planted(limits, alloc, NoPlant)
    }
}

impl<A: Allocator + Clone, P: Planted<TarPlant>> PlantedReader<A, P> {
    /// [`Reader::new_in`] with its plant seam: `plant` is a control's [`TarPlant`].
    pub fn new_planted(limits: Limits, alloc: A, plant: P) -> PlantedReader<A, P> {
        PlantedReader {
            limits,
            state: State::Header,
            header: [0; BLOCK],
            have: 0,
            meta: Vec::new_in(alloc.clone()),
            long_path: None,
            long_link: None,
            pax_size: None,
            raw: Vec::new_in(alloc.clone()),
            path: Vec::new_in(alloc.clone()),
            target: Vec::new_in(alloc.clone()),
            resolved: Vec::new_in(alloc.clone()),
            links: Vec::new_in(alloc.clone()),
            tag: Tag::File,
            size: 0,
            mode: 0,
            mtime: 0,
            offset: 0,
            entry_at: 0,
            entries: 0,
            total: 0,
            alloc,
            plant,
        }
    }

    /// Takes what it can of `input`; see the module.
    pub fn step<'a>(&'a mut self, input: &'a [u8]) -> Result<Step<'a>, Error> {
        let mut at = 0;
        let out = self.advance(input, &mut at);
        self.offset += at as u64;
        let out = out.map_err(|why| self.error(why))?;
        let event = match out {
            Out::Need => None,
            Out::End => Some(Event::End),
            Out::Data { from, to, last } => Some(Event::Data { bytes: &input[from..to], last }),
            Out::Entry => Some(Event::Entry(self.entry())),
        };
        Ok(Step { consumed: at, event })
    }

    fn advance(&mut self, input: &[u8], at: &mut usize) -> Result<Out, Why> {
        loop {
            let rest = &input[*at..];
            match self.state {
                State::End => return Ok(Out::End),
                State::Header | State::SecondZero => {
                    let n = (BLOCK - self.have).min(rest.len());
                    self.header[self.have..self.have + n].copy_from_slice(&rest[..n]);
                    self.have += n;
                    *at += n;
                    if self.have < BLOCK {
                        return Ok(Out::Need);
                    }
                    self.have = 0;
                    // `offset` counts the input of earlier steps; this step's is `at`.
                    self.entry_at = self.offset + *at as u64 - BLOCK as u64;
                    if let Some(out) = self.header()? {
                        return Ok(out);
                    }
                }
                State::Meta { kind, left, pad } => {
                    let n = (left as usize).min(rest.len());
                    self.meta.extend_from_slice(&rest[..n]);
                    *at += n;
                    if n as u64 == left {
                        self.state = State::Skip { left: pad };
                        self.extension(kind)?;
                    } else {
                        self.state = State::Meta { kind, left: left - n as u64, pad };
                        return Ok(Out::Need);
                    }
                }
                State::Skip { left } => {
                    let n = left.min(rest.len() as u64) as usize;
                    *at += n;
                    if n as u64 == left {
                        self.state = State::Header;
                    } else {
                        self.state = State::Skip { left: left - n as u64 };
                        return Ok(Out::Need);
                    }
                }
                State::Body { left, pad } => {
                    if rest.is_empty() {
                        return Ok(Out::Need);
                    }
                    let n = left.min(rest.len() as u64) as usize;
                    let from = *at;
                    *at += n;
                    let last = n as u64 == left;
                    self.state = match last {
                        true => State::Skip { left: pad },
                        false => State::Body { left: left - n as u64, pad },
                    };
                    return Ok(Out::Data { from, to: from + n, last });
                }
            }
        }
    }

    /// A whole header block: an end block, a metadata header, or an entry (`Some`).
    fn header(&mut self) -> Result<Option<Out>, Why> {
        let h = &self.header;
        let second = self.state == State::SecondZero;
        if h.iter().all(|&b| b == 0) {
            self.state = match second {
                true => State::End,
                false => State::SecondZero,
            };
            return Ok(second.then_some(Out::End));
        }
        if second {
            return Err(Why::EndBlock);
        }
        self.path.clear();
        self.entries += 1;
        if self.entries > self.limits.most_entries {
            return Err(Why::EntriesOver { most: self.limits.most_entries });
        }
        let computed: u64 =
            h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) }).sum();
        let stored = number(&h[148..156], Field::Checksum)?;
        if stored != computed {
            return Err(Why::Checksum { stored, computed });
        }
        let kind = h[156];
        let size = number(&h[124..136], Field::Size)?;
        let pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
        match kind {
            b'L' | b'K' | b'x' => {
                if size > u64::from(self.limits.most_meta) {
                    return Err(Why::MetaOver { size, most: self.limits.most_meta });
                }
                self.meta.clear();
                self.meta.try_reserve(size as usize).map_err(Why::Alloc)?;
                self.state = State::Meta { kind, left: size, pad };
                Ok(None)
            }
            // Global pax attributes: none changes a path or size, so they are read past.
            b'g' => {
                if size > u64::from(self.limits.most_meta) {
                    return Err(Why::MetaOver { size, most: self.limits.most_meta });
                }
                self.state = State::Skip { left: size + pad };
                Ok(None)
            }
            _ => self.entry_header(kind, size),
        }
    }

    fn entry_header(&mut self, kind: u8, size: u64) -> Result<Option<Out>, Why> {
        let h = self.header;
        let ustar = &h[257..263] == b"ustar\0";
        self.raw.clear();
        match (self.long_path.take(), ustar && h[345] != 0) {
            (Some(long), _) => self.raw.try_extend_from_slice(&long).map_err(Why::Alloc)?,
            (None, true) => {
                self.raw.try_extend_from_slice(cstring(&h[345..500])).map_err(Why::Alloc)?;
                self.raw.try_extend_from_slice(b"/").map_err(Why::Alloc)?;
                self.raw.try_extend_from_slice(cstring(&h[..100])).map_err(Why::Alloc)?;
            }
            (None, false) => self.raw.try_extend_from_slice(cstring(&h[..100])).map_err(Why::Alloc)?,
        }
        // A pax `size` stands for the header's (which holds at most 8 GiB in octal).
        let size = self.pax_size.take().unwrap_or(size);
        let pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
        let long_link = self.long_link.take();
        self.mode = (number(&h[100..108], Field::Mode)? & 0o7777) as u32;
        self.mtime = number(&h[136..148], Field::Mtime)?;
        self.tag = match kind {
            0 | b'0' | b'7' if !ustar && self.raw.last() == Some(&b'/') => Tag::Dir,
            0 | b'0' | b'7' => Tag::File,
            b'5' => Tag::Dir,
            b'1' => Tag::HardLink,
            b'2' => Tag::Symlink,
            b'3' | b'4' | b'6' => return Err(Why::Device(kind)),
            other => return Err(Why::Kind(other)),
        };
        // The name first, so a refusal from here on names the entry.
        self.path.try_extend_from_slice(&self.raw).map_err(Why::Alloc)?;
        normalize(&self.raw, self.limits.most_path, &mut self.resolved, self.plant)?;
        core::mem::swap(&mut self.path, &mut self.resolved);
        let file = self.tag == Tag::File;
        if file {
            if size > self.limits.most_entry {
                return Err(Why::EntryOver { size, most: self.limits.most_entry });
            }
            self.total = self.total.saturating_add(size);
            if self.total > self.limits.most_total {
                return Err(Why::TotalOver { most: self.limits.most_total });
            }
        } else if size > u64::from(self.limits.most_meta) {
            // A directory or link has no bytes; a header that says it has many is refused
            // rather than read past.
            return Err(Why::MetaOver { size, most: self.limits.most_meta });
        }
        if self.path.is_empty() && self.tag != Tag::Dir {
            return Err(Why::Path(PathWhy::Empty));
        }
        if self.through_link(&self.path) {
            return Err(Why::ThroughLink);
        }
        if matches!(self.tag, Tag::Symlink | Tag::HardLink) {
            if self.limits.links == Links::Refuse {
                return Err(Why::LinksRefused);
            }
            self.target.clear();
            let written = long_link.as_deref().unwrap_or(cstring(&h[157..257]));
            self.target.try_extend_from_slice(written).map_err(Why::Alloc)?;
            self.link()?;
        }
        self.size = if file { size } else { 0 };
        self.state = match (file, size) {
            (true, 0) => State::Skip { left: 0 },
            (true, _) => State::Body { left: size, pad },
            (false, _) => State::Skip { left: size + pad },
        };
        if self.path.is_empty() {
            // The top directory itself (`./`): nothing to make.
            return Ok(None);
        }
        Ok(Some(Out::Entry))
    }

    /// A link's target checked and, for a symbolic link, resolved and recorded.
    fn link(&mut self) -> Result<(), Why> {
        match self.tag {
            Tag::HardLink => {
                normalize(&self.target, self.limits.most_path, &mut self.resolved, self.plant).map_err(|why| match why {
                    Why::Path(why) => Why::Target(why),
                    other => other,
                })?;
                if self.resolved.is_empty() {
                    return Err(Why::Target(PathWhy::Empty));
                }
                if self.through_link(&self.resolved) {
                    return Err(Why::ThroughLink);
                }
            }
            _ => {
                let (links, plant) = (&self.links, self.plant);
                let through = |p: &[u8]| !plant.has(TarPlant::ThroughLinkUnchecked) && is_link(links, p);
                resolve(&self.path, &self.target, self.limits.most_path, through, &mut self.resolved)?;
                // A link at a directory an earlier link's target runs through moves that target.
                let path = &self.path;
                if pairs(&self.links).any(|(_, target)| within(target, path) && target != &path[..]) {
                    return Err(Why::UnderTarget);
                }
                let resolved = &self.resolved;
                self.links.try_reserve(path.len() + resolved.len() + 2).map_err(Why::Alloc)?;
                self.links.extend_from_slice(path);
                self.links.push(0);
                self.links.extend_from_slice(resolved);
                self.links.push(0);
            }
        }
        Ok(())
    }

    /// Whether `path` is, or runs through, an earlier symbolic link.
    fn through_link(&self, path: &[u8]) -> bool {
        !self.plant.has(TarPlant::ThroughLinkUnchecked) && pairs(&self.links).any(|(link, _)| within(path, link))
    }

    /// A metadata header's bytes taken into the next entry's extensions.
    fn extension(&mut self, kind: u8) -> Result<(), Why> {
        let mut value = Vec::new_in(self.alloc.clone());
        match kind {
            b'L' => {
                value.try_extend_from_slice(cstring(&self.meta)).map_err(Why::Alloc)?;
                self.long_path = Some(value);
            }
            b'K' => {
                value.try_extend_from_slice(cstring(&self.meta)).map_err(Why::Alloc)?;
                self.long_link = Some(value);
            }
            _ => {
                let mut data = &self.meta[..];
                while !data.is_empty() {
                    let (key, val, len) = pax_record(data)?;
                    match key {
                        b"path" => {
                            let mut v = Vec::new_in(self.alloc.clone());
                            v.try_extend_from_slice(val).map_err(Why::Alloc)?;
                            self.long_path = Some(v);
                        }
                        b"linkpath" => {
                            let mut v = Vec::new_in(self.alloc.clone());
                            v.try_extend_from_slice(val).map_err(Why::Alloc)?;
                            self.long_link = Some(v);
                        }
                        b"size" => self.pax_size = Some(decimal(val)?),
                        _ => {}
                    }
                    data = &data[len..];
                }
            }
        }
        Ok(())
    }

    fn entry(&self) -> Entry<'_> {
        // Every path and target here passed `normalize` or `syntax`, which refuse non-UTF-8.
        fn text(bytes: &[u8]) -> &str {
            core::str::from_utf8(bytes).unwrap_or("")
        }
        let kind = match self.tag {
            Tag::File => Kind::File,
            Tag::Dir => Kind::Dir,
            Tag::Symlink => Kind::Symlink { target: text(&self.target), resolved: text(&self.resolved) },
            Tag::HardLink => Kind::HardLink { target: text(&self.resolved) },
        };
        Entry { path: text(&self.path), kind, size: self.size, mode: self.mode, mtime: self.mtime }
    }

    fn error(&self, why: Why) -> Error {
        Error { entry: self.entries.saturating_sub(1), at: self.entry_at, name: Name::of(&self.path), why }
    }
}

impl<A: Allocator, P> PlantedReader<A, P> {
    /// The input ended: an error unless the archive did.
    pub fn finish(&self) -> Result<(), Error> {
        match self.state {
            State::End => Ok(()),
            _ => Err(Error {
                entry: self.entries.saturating_sub(1),
                at: self.offset,
                name: Name::NONE,
                why: Why::Truncated,
            }),
        }
    }

    /// Bytes of the archive taken so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

/// Every `(link, resolved target)` of `links`.
fn pairs(links: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut parts = links.split(|&b| b == 0);
    core::iter::from_fn(move || {
        let link = parts.next().filter(|l| !l.is_empty())?;
        let target = parts.next()?;
        Some((link, target))
    })
}

fn is_link(links: &[u8], path: &[u8]) -> bool {
    pairs(links).any(|(link, _)| link == path)
}

/// The bytes before the first NUL.
fn cstring(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
}

/// A number field: base-256 when its first byte has the top bit, else octal digits after
/// spaces, ended by a NUL or space and then only those.
pub(crate) fn number(field: &[u8], which: Field) -> Result<u64, Why> {
    let bad = Why::Number(which);
    if field[0] & 0x80 != 0 {
        if field[0] & 0x40 != 0 {
            return Err(bad);
        }
        return field[1..].iter().try_fold(u64::from(field[0] & 0x7f), |n, &b| {
            n.checked_mul(256).and_then(|n| n.checked_add(u64::from(b))).ok_or(bad)
        });
    }
    let mut digits = field.iter().copied().skip_while(|&b| b == b' ');
    let mut n = 0u64;
    while let Some(b) = digits.next() {
        if b == 0 || b == b' ' {
            if digits.any(|b| b != 0 && b != b' ') {
                return Err(bad);
            }
            break;
        }
        if !b.is_ascii_digit() || b > b'7' {
            return Err(bad);
        }
        n = n.checked_mul(8).and_then(|n| n.checked_add(u64::from(b - b'0'))).ok_or(bad)?;
    }
    Ok(n)
}

/// Decimal digits, at least one, none else.
fn decimal(bytes: &[u8]) -> Result<u64, Why> {
    if bytes.is_empty() {
        return Err(Why::Number(Field::Pax));
    }
    bytes.iter().try_fold(0u64, |n, &b| match b.is_ascii_digit() {
        true => n.checked_mul(10).and_then(|n| n.checked_add(u64::from(b - b'0'))).ok_or(Why::Number(Field::Pax)),
        false => Err(Why::Number(Field::Pax)),
    })
}

/// One pax record at the start of `data`: its key, its value and its whole length.
fn pax_record(data: &[u8]) -> Result<(&[u8], &[u8], usize), Why> {
    let space = data.iter().position(|&b| b == b' ').ok_or(Why::PaxRecord)?;
    let len = decimal(&data[..space]).map_err(|_| Why::PaxRecord)?;
    let len = usize::try_from(len).map_err(|_| Why::PaxRecord)?;
    if len <= space + 1 || len > data.len() || data[len - 1] != b'\n' {
        return Err(Why::PaxRecord);
    }
    let record = &data[space + 1..len - 1];
    let eq = record.iter().position(|&b| b == b'=').ok_or(Why::PaxRecord)?;
    Ok((&record[..eq], &record[eq + 1..], len))
}
