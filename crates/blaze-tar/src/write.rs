//! The deterministic tar writer: ustar headers whose every field is a function of the entry's
//! path, kind and size. Times are 0, owners 0 with no names, modes 0o644 (0o755 for an
//! executable or a directory), devices 0. A path that fits splits into ustar's prefix and name
//! at its last `/` that fits; one that does not, or a link target past 100 bytes, gets a pax
//! header of fixed name before it. So equal entries give equal bytes, on any machine.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::{
    AllocFailure, Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    error::{Name, PathWhy, Why},
    path::normalize,
    plant::TarPlant,
    read::BLOCK,
};

/// What to write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Make<'a> {
    File { exec: bool },
    Dir,
    /// A symbolic link to `target`, written as given (the reader checks it).
    Symlink { target: &'a str },
}

/// Why a write was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteWhy {
    /// The path is not one the reader would take as it is.
    Path(PathWhy),
    /// A link target that is empty or holds a NUL.
    Target(PathWhy),
    /// The entry's bytes were not the size its header gave.
    Size { declared: u64, written: u64 },
    /// An entry begun before the last ended, bytes with no file open, or anything after the end.
    Order,
    Alloc(AllocFailure),
}

/// A refused write, naming the entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriteError {
    pub name: Name,
    pub why: WriteWhy,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze_tar: writing {}: ", self.name)?;
        match self.why {
            WriteWhy::Path(why) => write!(f, "the path {why}"),
            WriteWhy::Target(why) => write!(f, "the link target {why}"),
            WriteWhy::Size { declared, written } => {
                write!(f, "{written} bytes written to an entry of {declared}")
            }
            WriteWhy::Order => f.write_str("out of order (begin, data, end; then finish)"),
            WriteWhy::Alloc(failure) => write!(f, "{failure}"),
        }
    }
}

impl core::error::Error for WriteError {}

/// Where a writer is: one state, no flags beside it (C5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Between entries: the next is begun, or the archive finished.
    Between,
    /// An entry is open: `written` of its `size` bytes so far.
    Open { name: Name, size: u64, written: u64 },
    /// The end blocks are written: nothing more is taken.
    Finished,
}

/// The tar writer: [`Writer::begin`], [`Writer::data`] to the size given, [`Writer::end`], for
/// each entry, then [`Writer::finish`]. `P` is the plant seam (D165): shipped code names
/// [`Writer`], this with [`NoPlant`].
pub struct PlantedWriter<P> {
    stage: Stage,
    plant: P,
}

/// The writer as it ships: [`PlantedWriter`] with no plant.
pub type Writer = PlantedWriter<NoPlant>;

/// The largest size an 11-digit octal field holds.
const OCTAL_MOST: u64 = (1 << 33) - 1;
const PAX_NAME: &[u8] = b"././@PaxHeader";

impl PlantedWriter<NoPlant> {
    #[expect(clippy::new_without_default, reason = "blaze's types have no `Default`: a value is made by name")]
    pub const fn new() -> Writer {
        PlantedWriter::new_planted(NoPlant)
    }
}

impl<P: Planted<TarPlant>> PlantedWriter<P> {
    /// [`Writer::new`] with its plant seam: `plant` is a control's [`TarPlant`].
    pub const fn new_planted(plant: P) -> PlantedWriter<P> {
        PlantedWriter { stage: Stage::Between, plant }
    }

    /// An entry's header (and a pax header before it, if its names need one).
    pub fn begin<B: Allocator + Clone>(
        &mut self,
        path: &str,
        make: Make<'_>,
        size: u64,
        out: &mut Vec<u8, B>,
    ) -> Result<(), WriteError> {
        let name = Name::of(path.as_bytes());
        let fail = |why| WriteError { name, why };
        if self.stage != Stage::Between {
            return Err(fail(WriteWhy::Order));
        }
        check_path(path.as_bytes(), out.allocator().clone(), self.plant).map_err(|why| fail(WriteWhy::Path(why)))?;
        let size = match make {
            Make::File { .. } => size,
            Make::Dir | Make::Symlink { .. } => 0,
        };
        let target = match make {
            Make::Symlink { target } => {
                if target.is_empty() {
                    return Err(fail(WriteWhy::Target(PathWhy::Empty)));
                }
                if target.as_bytes().contains(&0) {
                    return Err(fail(WriteWhy::Target(PathWhy::Nul)));
                }
                target.as_bytes()
            }
            _ => b"",
        };
        // The name as written: a directory's ends in `/`.
        let mut full = Vec::new_in(out.allocator().clone());
        full.try_reserve(path.len() + 1).map_err(|e| fail(WriteWhy::Alloc(e)))?;
        full.extend_from_slice(path.as_bytes());
        if make == Make::Dir {
            full.push(b'/');
        }
        let split = split(&full);
        let long_link = target.len() > 100;
        let big = size > OCTAL_MOST;
        let alloc = |e| fail(WriteWhy::Alloc(e));
        if split.is_none() || long_link || big {
            let mut records = Vec::new_in(out.allocator().clone());
            if split.is_none() {
                pax_record(b"path", &full, &mut records).map_err(alloc)?;
            }
            if long_link {
                pax_record(b"linkpath", target, &mut records).map_err(alloc)?;
            }
            if big {
                let mut digits = [0u8; 20];
                let text = decimal(size, &mut digits);
                pax_record(b"size", text, &mut records).map_err(alloc)?;
            }
            let pax = header(b"", PAX_NAME, b'x', 0o644, records.len() as u64, b"");
            out.try_reserve(2 * BLOCK + records.len()).map_err(alloc)?;
            out.extend_from_slice(&pax);
            out.extend_from_slice(&records);
            pad(records.len() as u64, out);
        }
        let (prefix, name_part) = split.unwrap_or((&[], truncate(&full, 100)));
        let (kind, mode) = match make {
            Make::File { exec: false } => (b'0', 0o644),
            Make::File { exec: true } => (b'0', 0o755),
            Make::Dir => (b'5', 0o755),
            Make::Symlink { .. } => (b'2', 0o777),
        };
        let field_size = match self.plant.has(TarPlant::SizeShort) && size > 0 {
            true => size - 1,
            false => size,
        };
        let block = header(prefix, name_part, kind, mode, field_size.min(OCTAL_MOST), truncate(target, 100));
        out.try_reserve(BLOCK).map_err(alloc)?;
        out.extend_from_slice(&block);
        self.stage = Stage::Open { name, size, written: 0 };
        Ok(())
    }

    /// The next bytes of the open file.
    pub fn data<B: Allocator>(&mut self, bytes: &[u8], out: &mut Vec<u8, B>) -> Result<(), WriteError> {
        let Stage::Open { name, size, written } = &mut self.stage else {
            return Err(WriteError { name: Name::NONE, why: WriteWhy::Order });
        };
        let now = *written + bytes.len() as u64;
        if now > *size {
            return Err(WriteError { name: *name, why: WriteWhy::Size { declared: *size, written: now } });
        }
        out.try_extend_from_slice(bytes).map_err(|e| WriteError { name: *name, why: WriteWhy::Alloc(e) })?;
        *written = now;
        Ok(())
    }

    /// The open entry ended: its bytes padded to a block.
    pub fn end<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), WriteError> {
        let Stage::Open { name, size, written } = self.stage else {
            return Err(WriteError { name: Name::NONE, why: WriteWhy::Order });
        };
        self.stage = Stage::Between;
        if written != size {
            return Err(WriteError { name, why: WriteWhy::Size { declared: size, written } });
        }
        out.try_reserve(BLOCK).map_err(|e| WriteError { name, why: WriteWhy::Alloc(e) })?;
        pad(size, out);
        Ok(())
    }

    /// The archive's end: two zero blocks.
    pub fn finish<B: Allocator>(&mut self, out: &mut Vec<u8, B>) -> Result<(), WriteError> {
        if self.stage != Stage::Between {
            return Err(WriteError { name: Name::NONE, why: WriteWhy::Order });
        }
        out.try_extend_from_slice(&[0; 2 * BLOCK]).map_err(|e| WriteError { name: Name::NONE, why: WriteWhy::Alloc(e) })?;
        self.stage = Stage::Finished;
        Ok(())
    }

    /// One whole entry: [`Writer::begin`], its bytes, [`Writer::end`].
    pub fn entry<B: Allocator + Clone>(
        &mut self,
        path: &str,
        make: Make<'_>,
        bytes: &[u8],
        out: &mut Vec<u8, B>,
    ) -> Result<(), WriteError> {
        self.begin(path, make, bytes.len() as u64, out)?;
        if matches!(make, Make::File { .. }) {
            self.data(bytes, out)?;
        }
        self.end(out)
    }
}

/// A path the reader takes as it is: normal form, and nothing it refuses.
fn check_path<B: Allocator>(path: &[u8], alloc: B, plant: impl Planted<TarPlant>) -> Result<(), PathWhy> {
    let mut normal = Vec::new_in(alloc);
    match normalize(path, u32::MAX, &mut normal, plant) {
        Ok(()) if normal.is_empty() => Err(PathWhy::Empty),
        Ok(()) if normal[..] != *path => Err(PathWhy::NotNormal),
        Ok(()) => Ok(()),
        Err(Why::Path(why)) => Err(why),
        // `normalize` refuses only paths, and memory, which a short path's copy is not worth
        // a variant for: the copy is the writer's own check.
        Err(_) => Err(PathWhy::TooLong { len: path.len() as u32, most: u32::MAX }),
    }
}

/// `full` as ustar's (prefix, name): the name at most 100 bytes, the prefix at most 155, split
/// at a `/`; `None` if no split fits.
fn split(full: &[u8]) -> Option<(&[u8], &[u8])> {
    if full.len() <= 100 {
        return Some((&[], full));
    }
    // Split at the first `/` that leaves the name at most 100 bytes; a directory's own
    // trailing `/` stays with its name.
    let body = full.strip_suffix(b"/").unwrap_or(full);
    for (i, &b) in body.iter().enumerate() {
        if b == b'/' && full.len() - i - 1 <= 100 {
            return match i <= 155 && i > 0 {
                true => Some((&full[..i], &full[i + 1..])),
                false => None,
            };
        }
    }
    None
}

fn truncate(bytes: &[u8], most: usize) -> &[u8] {
    &bytes[..bytes.len().min(most)]
}

/// One ustar header block.
fn header(prefix: &[u8], name: &[u8], kind: u8, mode: u32, size: u64, link: &[u8]) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name);
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], 0);
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    octal(&mut h[329..337], 0);
    octal(&mut h[337..345], 0);
    h[345..345 + prefix.len()].copy_from_slice(prefix);
    h[148..156].fill(b' ');
    let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
    octal(&mut h[148..155], sum);
    h
}

/// `value` as zero-padded octal digits filling `field` but its last byte, which is NUL.
fn octal(field: &mut [u8], mut value: u64) {
    let digits = field.len() - 1;
    for i in (0..digits).rev() {
        field[i] = b'0' + (value & 7) as u8;
        value >>= 3;
    }
    field[digits] = 0;
}

fn decimal(mut value: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buf[at..];
        }
    }
}

/// A pax record `<len> <key>=<value>\n`, whose length counts its own digits.
fn pax_record<B: Allocator>(key: &[u8], value: &[u8], out: &mut Vec<u8, B>) -> Result<(), AllocFailure> {
    let rest = 1 + key.len() + 1 + value.len() + 1;
    let mut len = rest + 1;
    let mut digits = [0u8; 20];
    while decimal(len as u64, &mut digits).len() + rest != len {
        len += 1;
    }
    out.try_reserve(len)?;
    out.extend_from_slice(decimal(len as u64, &mut digits));
    out.push(b' ');
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
    Ok(())
}

/// Zeros to the end of the block `size` bytes of data end in.
fn pad<B: Allocator>(size: u64, out: &mut Vec<u8, B>) {
    let pad = ((BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64) as usize;
    out.extend_from_slice(&[0; BLOCK][..pad]);
}
