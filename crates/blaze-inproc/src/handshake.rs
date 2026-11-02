//! The start of a run, frame by frame, as the daemon ([`crate::host`]) and the program's
//! start hook ([`crate::runtime`]) speak it:
//!
//! 1. HELLO, hook to daemon: the token from `BLAZE_INPROC`, the process ID, [`VERSION`], the
//!    page, and where the image's headers are in the process ([`Hello`]).
//! 2. GRANT, daemon to hook: the format, the linked header and span, the objects (the image's
//!    code memory, the fence's control words, the asks, the cells, the objects of fixed maps),
//!    the overlays, and the dependency images' words the hook writes and binds once they are
//!    mapped ([`Grant`], [`crate::bind`]); the objects come with it or after it
//!    ([`crate::sys`]).
//! 3. On Windows only, MAP for the control words and the asks (the daemon maps every view) and
//!    OVERLAY, with which the hook asks the daemon to lay the overlays over its image;
//!    OVERLAID or REFUSED answers.
//! 4. STARTED, hook to daemon: the thread that runs `main`. Or FAILED, with why.
//!
//! Every body is bounds-checked and refused whole when any field is out of its range.

use std::ops::Range;

use crate::frame::{FrameError, Reader, Writer};
use crate::image::Layout;
use crate::sys::Rights;
use crate::{Access, Error, Format, ImageWrite};

/// The variable naming the host: `<address>\t<token in hex>`. The start hook takes it out of
/// the environment before the program sees it.
pub const HOST_VAR: &str = "BLAZE_INPROC";

/// The version of the handshake; a host and a hook of another refuse each other.
pub(crate) const VERSION: u32 = 4;

/// The most objects and overlays one grant carries.
const MOST: usize = 64;

/// The most writes, and the most lookups, one grant carries.
pub(crate) const MOST_BINDS: usize = 1 << 22;

/// The longest name a lookup may have.
pub(crate) const MOST_NAME: usize = 1 << 16;

/// The bytes of one encoded overlay: object, offset, length, address, access, place, image.
pub(crate) const OVERLAY_BYTES: usize = 31;

/// The bytes of one encoded write: place, value, whether it is an address.
const WRITE_BYTES: usize = 17;

/// The fewest bytes of one encoded lookup: place, addend, weak, the name's length, one byte.
const LOOKUP_BYTES: usize = 22;

fn bad(what: &'static str) -> impl Fn(FrameError) -> Error {
    move |it| Error::new(format!("{what}: {it}"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hello {
    pub(crate) token: [u8; 16],
    pub(crate) pid: u32,
    pub(crate) version: u32,
    pub(crate) page: u64,
    /// Where the image's headers are in the process: its slide plus the linked header.
    pub(crate) header: u64,
}

impl Hello {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        Writer::new(out).raw(&self.token).u32(self.pid).u32(self.version).u64(self.page).u64(self.header);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<Hello, Error> {
        let bad = bad("a start hook's hello");
        let mut read = Reader::new(body);
        let hello = Hello {
            token: read.array().map_err(&bad)?,
            pid: read.u32().map_err(&bad)?,
            version: read.u32().map_err(&bad)?,
            page: read.u64().map_err(&bad)?,
            header: read.u64().map_err(&bad)?,
        };
        read.finish().map_err(&bad)?;
        Ok(hello)
    }
}

/// Where an overlay goes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Place {
    /// Over one of the image's code segments, from the same file offset of the image object.
    Code,
    /// Inside a reservation of the image: the arena, the cells, a fixed map.
    Reserved,
}

/// `len` bytes of object `object` from `offset`, mapped at the linked address `at` with
/// `access`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Overlay {
    pub(crate) object: u32,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) at: u64,
    pub(crate) access: Access,
    pub(crate) place: Place,
    /// A fixed map of a dependency image (`blaze-premap`'s), whose words that name its own
    /// addresses are linked at its base: it runs only unslid ([`crate::bind::check_slide`]).
    pub(crate) image: bool,
}

impl Overlay {
    pub(crate) fn range(&self) -> Range<u64> {
        self.at..self.at.saturating_add(self.len)
    }
}

/// A word the start hook binds to `name` ([`crate::ImageLookup`]), the name's bytes at
/// `name..name + len` of [`Grant::names`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Lookup {
    pub(crate) place: u64,
    pub(crate) addend: i64,
    pub(crate) weak: bool,
    pub(crate) name: u32,
    pub(crate) len: u32,
}

/// What the daemon gives a start hook. Objects: the image's code memory first (rights
/// [`Rights::Code`]), the fence's control words second and the asks ([`crate::ask`]) third
/// (both [`Rights::Words`]), then the cells and the objects of fixed maps.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Grant {
    pub(crate) format: Format,
    pub(crate) header: u64,
    pub(crate) span: Range<u64>,
    pub(crate) objects: Vec<(u64, Rights)>,
    /// By address, none overlapping another.
    pub(crate) overlays: Vec<Overlay>,
    /// The words of dependency images the hook stores once the overlays are mapped: each whole
    /// inside an overlay of [`Overlay::image`] and [`Access::Copy`] ([`Grant::check_binds`]).
    pub(crate) writes: Vec<ImageWrite>,
    pub(crate) lookups: Vec<Lookup>,
    /// Every lookup's name, one after another.
    pub(crate) names: String,
}

fn rights_tag(rights: Rights) -> u8 {
    match rights {
        Rights::Code => 1,
        Rights::Words => 2,
    }
}

fn place_tag(place: Place) -> u8 {
    match place {
        Place::Code => 1,
        Place::Reserved => 2,
    }
}

impl Grant {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.reserve(
            41 + 9 * self.objects.len()
                + OVERLAY_BYTES * self.overlays.len()
                + WRITE_BYTES * self.writes.len()
                + (LOOKUP_BYTES - 1) * self.lookups.len()
                + self.names.len(),
        );
        let mut write = Writer::new(out);
        write.u8(self.format.tag()).u64(self.header).u64(self.span.start).u64(self.span.end);
        write.u32(self.objects.len() as u32);
        for &(len, rights) in &self.objects {
            write.u64(len).u8(rights_tag(rights));
        }
        write.u32(self.overlays.len() as u32);
        for overlay in &self.overlays {
            write
                .u32(overlay.object)
                .u64(overlay.offset)
                .u64(overlay.len)
                .u64(overlay.at)
                .u8(overlay.access.tag())
                .u8(place_tag(overlay.place))
                .u8(u8::from(overlay.image));
        }
        write.u32(self.writes.len() as u32);
        for it in &self.writes {
            write.u64(it.place).u64(it.value).u8(u8::from(it.address));
        }
        write.u32(self.lookups.len() as u32);
        for it in &self.lookups {
            write.u64(it.place).i64(it.addend).u8(u8::from(it.weak)).str(self.name(it));
        }
    }

    /// The name of `lookup`, one of this grant's.
    pub(crate) fn name(&self, lookup: &Lookup) -> &str {
        let start = lookup.name as usize;
        self.names.get(start..start + lookup.len as usize).unwrap_or("")
    }

    /// Refuses a write or lookup that is not 8 bytes whole inside an overlay of a dependency
    /// image mapped as a private copy ([`Overlay::image`], [`Access::Copy`]), a lookup whose
    /// name is empty, longer than [`MOST_NAME`], has a NUL or is not where [`Grant::names`]
    /// has it, and more than [`MOST_BINDS`] of either. What a hook may store before the
    /// program runs is only ever a word of its own copy of a dependency image's data.
    pub(crate) fn check_binds(&self) -> Result<(), Error> {
        if self.writes.len() > MOST_BINDS || self.lookups.len() > MOST_BINDS {
            return Err(Error::new(format!( // alloc: error path
                "a grant of {} writes and {} lookups, past the most of {MOST_BINDS}",
                self.writes.len(),
                self.lookups.len()
            )));
        }
        let inside = |place: u64| {
            let at = self.overlays.partition_point(|it| it.at <= place);
            at != 0 && {
                let overlay = &self.overlays[at - 1];
                overlay.image
                    && overlay.access == Access::Copy
                    && place.checked_add(8).is_some_and(|end| end <= overlay.range().end)
            }
        };
        const OUTSIDE: &str = "not 8 bytes inside a dependency image's private copy";
        for it in &self.writes {
            if !inside(it.place) {
                return Err(Error::new(format!("a write at {:#x}: {OUTSIDE}", it.place))); // alloc: error path
            }
        }
        for it in &self.lookups {
            let name = self
                .names
                .get(it.name as usize..(it.name as usize).saturating_add(it.len as usize))
                .ok_or_else(|| Error::new(format!("a lookup at {:#x} of a name out of the grant's names", it.place)))?; // alloc: error path
            if name.is_empty() || name.len() > MOST_NAME || name.as_bytes().contains(&0) {
                return Err(Error::new(format!( // alloc: error path
                    "a lookup at {:#x} of a name of {} bytes: empty, past {MOST_NAME} or with a NUL",
                    it.place,
                    name.len()
                )));
            }
            if !inside(it.place) {
                return Err(Error::new(format!("the lookup of `{name}` at {:#x}: {OUTSIDE}", it.place))); // alloc: error path
            }
        }
        Ok(())
    }

    /// The grant of `body`, refused unless every object and overlay is one a hook may map:
    /// the code memory never shared writable, shared words never executable and only in a
    /// reservation, code only from the code memory and executable, nothing out of its object
    /// or the span, no overlay over another.
    pub(crate) fn decode(body: &[u8]) -> Result<Grant, Error> {
        let bad = bad("the daemon's grant");
        let mut read = Reader::new(body);
        let tag = read.u8().map_err(&bad)?;
        let format = Format::from_tag(tag).ok_or_else(|| Error::new(format!("a grant of format {tag}")))?;
        let header = read.u64().map_err(&bad)?;
        let span = read.u64().map_err(&bad)?..read.u64().map_err(&bad)?;
        let count = read.count(9).map_err(&bad)?;
        if !(3..=MOST).contains(&count) {
            return Err(Error::new(format!("a grant of {count} objects")));
        }
        let mut objects = Vec::with_capacity(count);
        for _ in 0..count {
            let len = read.u64().map_err(&bad)?;
            let rights = match read.u8().map_err(&bad)? {
                1 => Rights::Code,
                2 => Rights::Words,
                other => return Err(Error::new(format!("a granted object of rights {other}"))),
            };
            objects.push((len, rights));
        }
        if objects[0].1 != Rights::Code || objects[1].1 != Rights::Words || objects[2].1 != Rights::Words {
            return Err(Error::new("a grant whose first objects are not the code memory, the control words and the asks"));
        }
        let count = read.count(OVERLAY_BYTES).map_err(&bad)?;
        if count > MOST {
            return Err(Error::new(format!("a grant of {count} overlays")));
        }
        let mut overlays: Vec<Overlay> = Vec::with_capacity(count);
        for _ in 0..count {
            let object = read.u32().map_err(&bad)?;
            let offset = read.u64().map_err(&bad)?;
            let len = read.u64().map_err(&bad)?;
            let at = read.u64().map_err(&bad)?;
            let tag = read.u8().map_err(&bad)?;
            let access = Access::from_tag(tag).ok_or_else(|| Error::new(format!("an overlay of access {tag}")))?;
            let place = match read.u8().map_err(&bad)? {
                1 => Place::Code,
                2 => Place::Reserved,
                other => return Err(Error::new(format!("an overlay of place {other}"))),
            };
            let image = match read.u8().map_err(&bad)? {
                0 => false,
                1 => true,
                other => return Err(Error::new(format!("an overlay of image flag {other}"))), // alloc: error path
            };
            let overlay = Overlay { object, offset, len, at, access, place, image };
            let Some(&(object_len, rights)) = objects.get(object as usize).filter(|_| object != 1 && object != 2) else {
                return Err(Error::new(format!("an overlay of object {object} of {}", objects.len())));
            };
            if image && (object == 0 || rights != Rights::Code || place != Place::Reserved) {
                return Err(Error::new(format!( // alloc: error path
                    "an overlay of object {object} ({rights:?}) at {at:#x} over {place:?} flagged a dependency image: only a \
                     fixed map is one"
                )));
            }
            let allowed = match (rights, place, access) {
                (Rights::Code, Place::Code, Access::Execute) => object == 0,
                (Rights::Code, Place::Reserved, access) => access != Access::Shared,
                (Rights::Words, Place::Reserved, Access::Shared) => true,
                _ => false,
            };
            if !allowed {
                return Err(Error::new(format!(
                    "an overlay of object {object} ({rights:?}) at {at:#x} as {access:?} over {place:?}"
                )));
            }
            let fits = len != 0
                && offset.checked_add(len).is_some_and(|end| end <= object_len)
                && at.checked_add(len).is_some_and(|end| at >= span.start && end <= span.end);
            if !fits {
                return Err(Error::new(format!(
                    "an overlay of {len:#x} bytes from offset {offset:#x} of object {object} at {at:#x}: out of its object \
                     or the span {:#x}..{:#x}",
                    span.start, span.end
                )));
            }
            if let Some(last) = overlays.last()
                && last.range().end > at
            {
                return Err(Error::new(format!("overlays at {:#x} and {at:#x} out of order or overlapping", last.at)));
            }
            overlays.push(overlay);
        }
        let flag = |what: &'static str, value: u8| match value {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::new(format!("{what} of {other}"))), // alloc: error path
        };
        let count = read.count(WRITE_BYTES).map_err(&bad)?;
        if count > MOST_BINDS {
            return Err(Error::new(format!("a grant of {count} writes, past the most of {MOST_BINDS}"))); // alloc: error path
        }
        let mut writes = Vec::with_capacity(count);
        for _ in 0..count {
            let place = read.u64().map_err(&bad)?;
            let value = read.u64().map_err(&bad)?;
            let address = flag("a write's address flag", read.u8().map_err(&bad)?)?;
            writes.push(ImageWrite { place, value, address });
        }
        let count = read.count(LOOKUP_BYTES).map_err(&bad)?;
        if count > MOST_BINDS {
            return Err(Error::new(format!("a grant of {count} lookups, past the most of {MOST_BINDS}"))); // alloc: error path
        }
        let mut lookups = Vec::with_capacity(count);
        let mut names = String::with_capacity(read.left() - (LOOKUP_BYTES - 1) * count);
        for _ in 0..count {
            let place = read.u64().map_err(&bad)?;
            let addend = read.i64().map_err(&bad)?;
            let weak = flag("a lookup's weak flag", read.u8().map_err(&bad)?)?;
            let name = read.str().map_err(&bad)?;
            if name.is_empty() || name.len() > MOST_NAME {
                return Err(Error::new(format!("a lookup at {place:#x} of a name of {} bytes", name.len()))); // alloc: error path
            }
            let start = names.len() as u32;
            names.push_str(name);
            lookups.push(Lookup { place, addend, weak, name: start, len: name.len() as u32 });
        }
        read.finish().map_err(&bad)?;
        let grant = Grant { format, header, span, objects, overlays, writes, lookups, names };
        grant.check_binds()?;
        Ok(grant)
    }

    /// Checks the grant against `own`, the layout the hook read from its own image in memory,
    /// with pages of `page` bytes: every code segment is overlaid whole from its own file
    /// offset, everything else lands inside a reservation. Compared relative to each one's
    /// header, so a header the system loader moved changes nothing.
    pub(crate) fn check(&self, own: &Layout, page: u64) -> Result<(), Error> {
        if self.format != own.format {
            return Err(Error::new(format!("a grant for a {:?} image, where this one is {:?}", self.format, own.format)));
        }
        let theirs = |at: u64| at.wrapping_sub(self.header);
        let ours = |at: u64| at.wrapping_sub(own.header);
        if theirs(self.span.start) != ours(own.span.start) || theirs(self.span.end) != ours(own.span.end) {
            return Err(Error::new(format!(
                "the daemon's image spans {:#x}..{:#x} from its header {:#x}, this one {:#x}..{:#x} from {:#x}: not the \
                 executable the daemon wrote",
                self.span.start, self.span.end, self.header, own.span.start, own.span.end, own.header
            )));
        }
        for overlay in &self.overlays {
            if !(overlay.at | overlay.len | overlay.offset).is_multiple_of(page) {
                return Err(Error::new(format!("an overlay at {:#x} of {:#x} bytes: not whole pages", overlay.at, overlay.len)));
            }
            let at = theirs(overlay.at);
            let fits = match overlay.place {
                Place::Code => own.code.iter().any(|it| ours(it.linked) == at && it.offset == overlay.offset && it.len == overlay.len),
                Place::Reserved => own
                    .reserved
                    .iter()
                    .any(|it| ours(it.start) <= at && at.checked_add(overlay.len).is_some_and(|end| end <= ours(it.end))),
            };
            if !fits {
                return Err(Error::new(format!(
                    "the daemon's overlay of {:#x} bytes at {:#x} ({:?}, file offset {:#x}) is not {} of this image",
                    overlay.len,
                    overlay.at,
                    overlay.place,
                    overlay.offset,
                    match overlay.place {
                        Place::Code => "a code segment",
                        Place::Reserved => "inside a reservation",
                    }
                )));
            }
        }
        for segment in &own.code {
            let covered = self.overlays.iter().any(|it| it.place == Place::Code && theirs(it.at) == ours(segment.linked));
            if !covered {
                return Err(Error::new(format!(
                    "the code segment at {:#x} has no overlay: it would run from the file, not the code memory",
                    segment.linked
                )));
            }
        }
        Ok(())
    }
}

/// A MAP frame's body: the whole of granted object `object`, anywhere, with `access`.
pub(crate) fn encode_map(object: u32, access: Access, out: &mut Vec<u8>) {
    Writer::new(out).u32(object).u8(access.tag());
}

pub(crate) fn decode_map(body: &[u8]) -> Result<(u32, Access), Error> {
    let bad = bad("a start hook's map");
    let mut read = Reader::new(body);
    let object = read.u32().map_err(&bad)?;
    let tag = read.u8().map_err(&bad)?;
    let access = Access::from_tag(tag).ok_or_else(|| Error::new(format!("a map of access {tag}")))?;
    read.finish().map_err(&bad)?;
    Ok((object, access))
}

pub(crate) fn hex(bytes: &[u8; 16]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

pub(crate) fn unhex(text: &[u8]) -> Option<[u8; 16]> {
    if text.len() != 32 {
        return None;
    }
    let digit = |it: u8| match it {
        b'0'..=b'9' => Some(it - b'0'),
        b'a'..=b'f' => Some(it - b'a' + 10),
        _ => None,
    };
    let mut out = [0; 16];
    for (index, pair) in text.chunks_exact(2).enumerate() {
        out[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

/// The address and token of a `BLAZE_INPROC` value.
pub(crate) fn host_var(value: &[u8]) -> Result<(&str, [u8; 16]), Error> {
    let tab = value
        .iter()
        .position(|it| *it == b'\t')
        .ok_or_else(|| Error::new(format!("{HOST_VAR} names no token")))?;
    let address =
        std::str::from_utf8(&value[..tab]).map_err(|_| Error::new(format!("{HOST_VAR} names an address that is not UTF-8")))?;
    let token = unhex(&value[tab + 1..])
        .ok_or_else(|| Error::new(format!("{HOST_VAR} has a token that is not 32 lowercase hex digits")))?;
    Ok((address, token))
}
