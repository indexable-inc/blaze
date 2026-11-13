//! The daemon's local facts, kept as VFS marks ([`blaze_vfs::Vfs::mark`]) so a start finds them
//! in the journal without reading or hashing anything that did not change: each placed
//! output's stamp and content, each checkout's carry (the session saved and the root it was
//! saved at) and blaze's own copy of the library. One fact is a file of the home's state and
//! no mark: the hash of blaze's own executable ([`tool_hash`], `<state>/tool`), which the
//! client and the daemon's start read before any journal is open, and every reader of the
//! process's hash reads from then on ([`blaze_profile::identity::seed`]).
//!
//! A fact tied to a file is a stamp and when it was looked at: it vouches for its content only
//! while the file's stamp now is that stamp, and only if the stamp was steady when looked at
//! (taken its window, [`blaze_vfs::window_ns`], or more after the file's change), as the
//! watcher's column.
//! Each value is little-endian, a stamp as [`Stamp::encode`], and starts with [`LAYOUT`]: a
//! value of another layout is refused by name, never read as this one.

use std::fmt;

use blaze_core::Stamp;
use blaze_ids::ContentId;

/// The layout of every value here; a value starting with another is refused.
pub(crate) const LAYOUT: u8 = 1;
/// The prefix of a placed output's mark; the output's absolute path follows.
pub(crate) const PLACED: &str = "placed\0";
/// The prefix of a checkout's carry mark; the checkout's directory follows.
pub(crate) const CARRY: &str = "carry\0";
/// The prefix of a crate's body entries' mark ([`BodiesMark`]): the checkout's directory, a
/// NUL and the crate's stable name follow.
pub(crate) const BODIES: &str = "bodies\0";
/// The mark of the standard library blaze keeps its own copy of ([`LibraryMark`]).
pub(crate) const LIBRARY: &str = "library";
/// The prefix of a launched program's start image mark ([`LaunchMark`]); the program's path
/// (the one its link was asked for) follows.
pub(crate) const LAUNCH: &str = "launch\0";

/// The tool's executable as last looked at, and its hash: 1 + 36 + 8 + 32 bytes, the layout
/// of `<state>/tool` ([`tool_hash`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Tool {
    pub exe: Stamp,
    pub looked_ns: u64,
    pub hash: [u8; 32],
}

/// A placed output as last looked at, and its content: 1 + 36 + 8 + 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placed {
    pub stamp: Stamp,
    pub looked_ns: u64,
    pub content: ContentId,
}

/// A checkout's carry: the session saved, and the root it was saved at: 1 + 32 + 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CarryMark {
    pub session: blaze_cas::Key,
    pub root: ContentId,
}

/// A crate's body entries' session in a checkout (`front_carry::bodies::Kept`): the root it was
/// last saved under, its puts durable: 1 + 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BodiesMark {
    pub session: blaze_cas::Key,
}

/// Which start image is a launched program's (`blaze_launch_host::start`): the unit's whole
/// record key of the build it holds, the image's store key, its put durable when marked, and
/// the path the program runs from (`serve::work::LaunchKept::runs_as`), which a restored
/// program is asked for by: 1 + 32 + 32 + (4 + path) bytes.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: none made; compared by the tests
pub(crate) struct LaunchMark {
    pub record: ContentId,
    pub start: blaze_cas::Key,
    pub runs_as: String,
}

/// blaze's own copy of the standard library as last mounted: its `DIGEST` file's stamp when
/// looked at, and the fingerprint and place it was mounted under
/// (`Mounts::libraries`): 1 + 36 + 8 + (4 + fingerprint) + (4 + place) bytes. The copy is
/// blaze's to write and no one else's (`toolchain::library::owned`), so its `DIGEST` as it was
/// stands for every file under it, as the store's packs stand for themselves at a start.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct LibraryMark {
    pub digest_file: Stamp,
    pub looked_ns: u64,
    pub fingerprint: String,
    pub at: String,
}

/// A mark's value this blaze does not read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Undecoded {
    /// It starts with another layout than [`LAYOUT`].
    Layout { name: String, found: u8 },
    /// It is not the length its layout has.
    Length { name: String, len: usize },
}

impl fmt::Display for Undecoded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Undecoded::Layout { name, found } => f.write_str(
                &blaze_report::say!(
                    "the local mark ",
                    blaze_report::say::Code(&name.escape_debug().to_string()), // alloc: an error's words
                    " is of layout ",
                    blaze_report::say::Count(u64::from(*found)),
                    ", not the ",
                    blaze_report::say::Count(u64::from(LAYOUT)),
                    " this blaze reads (serve/local.rs LAYOUT)"
                )
                .flat(),
            ),
            Undecoded::Length { name, len } => f.write_str(
                &blaze_report::say!(
                    "the local mark ",
                    blaze_report::say::Code(&name.escape_debug().to_string()), // alloc: an error's words
                    " is ",
                    blaze_report::say::Count(*len as u64),
                    " bytes, not the length its layout has (serve/local.rs)"
                )
                .flat(),
            ),
        }
    }
}

impl std::error::Error for Undecoded {}

const FACT: usize = 1 + Stamp::BYTES + 8 + 32;
const CARRY_BYTES: usize = 1 + 32 + 32;
const BODIES_BYTES: usize = 1 + 32;
/// A launch mark's length before the path it runs from's bytes: the layout, two keys and the
/// path's length.
const LAUNCH_HEAD: usize = 1 + 32 + 32 + 4;

/// The value's bytes past its layout byte, if it is `len` long and of [`LAYOUT`].
fn body<'a>(name: &str, bytes: &'a [u8], len: usize) -> Result<&'a [u8], Undecoded> {
    match bytes.first() {
        // alloc: an error path, the mark's name.
        None => Err(Undecoded::Length { name: name.to_owned(), len: 0 }),
        // alloc: an error path, the mark's name.
        Some(&found) if found != LAYOUT => Err(Undecoded::Layout { name: name.to_owned(), found }),
        // alloc: an error path, the mark's name.
        Some(_) if bytes.len() != len => {
            Err(Undecoded::Length { name: name.to_owned(), len: bytes.len() })
        }
        Some(_) => Ok(&bytes[1..]),
    }
}

fn word(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"))
}

fn id(bytes: &[u8]) -> [u8; 32] {
    bytes[..32].try_into().expect("32 bytes")
}

/// A stamp, when it was looked at and 32 bytes: [`Tool`]'s and [`Placed`]'s layout.
fn encode_fact(stamp: &Stamp, looked_ns: u64, id: &[u8; 32]) -> Vec<u8> {
    // alloc: the mark's value, which the VFS keeps.
    let mut out = Vec::with_capacity(FACT);
    out.push(LAYOUT);
    stamp.encode(&mut out);
    out.extend_from_slice(&looked_ns.to_le_bytes());
    out.extend_from_slice(id);
    out
}

fn decode_fact(name: &str, bytes: &[u8]) -> Result<(Stamp, u64, [u8; 32]), Undecoded> {
    let body = body(name, bytes, FACT)?;
    let stamp = Stamp::decode(body).expect("the length is checked");
    let rest = &body[Stamp::BYTES..];
    Ok((stamp, word(rest), id(&rest[8..])))
}

impl Tool {
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode_fact(&self.exe, self.looked_ns, &self.hash)
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<Tool, Undecoded> {
        let (exe, looked_ns, hash) = decode_fact(name, bytes)?;
        Ok(Tool { exe, looked_ns, hash })
    }
}

impl Placed {
    pub(crate) fn encode(&self) -> Vec<u8> {
        encode_fact(&self.stamp, self.looked_ns, &self.content.0)
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<Placed, Undecoded> {
        let (stamp, looked_ns, content) = decode_fact(name, bytes)?;
        Ok(Placed { stamp, looked_ns, content: ContentId(content) })
    }
}

impl CarryMark {
    pub(crate) fn encode(&self) -> Vec<u8> {
        // alloc: the mark's value, which the VFS keeps.
        let mut out = Vec::with_capacity(CARRY_BYTES);
        out.push(LAYOUT);
        out.extend_from_slice(&self.session.0.0);
        out.extend_from_slice(&self.root.0);
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<CarryMark, Undecoded> {
        let body = body(name, bytes, CARRY_BYTES)?;
        Ok(CarryMark {
            session: blaze_cas::Key(ContentId(id(body))),
            root: ContentId(id(&body[32..])),
        })
    }
}

impl BodiesMark {
    pub(crate) fn encode(&self) -> Vec<u8> {
        // alloc: the mark's value, which the VFS keeps.
        let mut out = Vec::with_capacity(BODIES_BYTES);
        out.push(LAYOUT);
        out.extend_from_slice(&self.session.0.0);
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<BodiesMark, Undecoded> {
        let body = body(name, bytes, BODIES_BYTES)?;
        Ok(BodiesMark { session: blaze_cas::Key(ContentId(id(body))) })
    }
}

impl LaunchMark {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let runs_as = self.runs_as.as_bytes();
        // alloc: the mark's value, which the VFS keeps.
        let mut out = Vec::with_capacity(LAUNCH_HEAD + runs_as.len());
        out.push(LAYOUT);
        out.extend_from_slice(&self.record.0);
        out.extend_from_slice(&self.start.0.0);
        let len = u32::try_from(runs_as.len()).expect("a program's path is under 4 GiB");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(runs_as);
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<LaunchMark, Undecoded> {
        // alloc: an error path, the mark's name.
        let short = || Undecoded::Length { name: name.to_owned(), len: bytes.len() };
        match bytes.first() {
            // alloc: an error path, the mark's name.
            Some(&found) if found != LAYOUT => return Err(Undecoded::Layout { name: name.to_owned(), found }),
            Some(_) if bytes.len() >= LAUNCH_HEAD => {}
            _ => return Err(short()),
        }
        let (runs_as, rest) = text_of(&bytes[1 + 32 + 32..]).ok_or_else(short)?;
        if !rest.is_empty() {
            return Err(short());
        }
        Ok(LaunchMark {
            record: ContentId(id(&bytes[1..])),
            start: blaze_cas::Key(ContentId(id(&bytes[1 + 32..]))),
            runs_as,
        })
    }
}

impl LibraryMark {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let (fingerprint, at) = (self.fingerprint.as_bytes(), self.at.as_bytes());
        // alloc: the mark's value, which the VFS keeps.
        let mut out = Vec::with_capacity(1 + Stamp::BYTES + 8 + 8 + fingerprint.len() + at.len());
        out.push(LAYOUT);
        self.digest_file.encode(&mut out);
        out.extend_from_slice(&self.looked_ns.to_le_bytes());
        for text in [fingerprint, at] {
            let len = u32::try_from(text.len()).expect("a library's fingerprint and place are under 4 GiB");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(text);
        }
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> Result<LibraryMark, Undecoded> {
        // alloc: an error path, the mark's name.
        let short = || Undecoded::Length { name: name.to_owned(), len: bytes.len() };
        let head = 1 + Stamp::BYTES + 8;
        match bytes.first() {
            // alloc: an error path, the mark's name.
            Some(&found) if found != LAYOUT => {
                return Err(Undecoded::Layout { name: name.to_owned(), found });
            }
            Some(_) if bytes.len() >= head => {}
            _ => return Err(short()),
        }
        let digest_file = Stamp::decode(&bytes[1..]).expect("the length is checked");
        let looked_ns = word(&bytes[1 + Stamp::BYTES..]);
        let rest: &[u8] = &bytes[head..];
        let (fingerprint, rest) = text_of(rest).ok_or_else(short)?;
        let (at, rest) = text_of(rest).ok_or_else(short)?;
        if !rest.is_empty() {
            return Err(short());
        }
        Ok(LibraryMark { digest_file, looked_ns, fingerprint, at })
    }
}

/// A `u32` length and that many bytes of UTF-8 at the head of `bytes`, and what follows.
fn text_of(bytes: &[u8]) -> Option<(String, &[u8])> {
    let len = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    let text = bytes.get(4..4usize.checked_add(len)?)?;
    // alloc: the mark's own string, once a start
    Some((std::str::from_utf8(text).ok()?.to_owned(), &bytes[4 + len..]))
}

impl LibraryMark {
    /// Whether this mark vouches that blaze's copy of the library is mounted as it says: its
    /// `DIGEST` file's stamp `now` is the one marked, that was steady when marked, and the
    /// mount it names is among `mounted` (`Mounts::libraries`: fingerprint, place) at `at`.
    pub(crate) fn vouches(&self, now: &Stamp, mounted: &[(String, String)], at: &str) -> bool {
        vouches(&self.digest_file, self.looked_ns, now, steady)
            && self.at == at
            && mounted.iter().any(|(fingerprint, place)| *fingerprint == self.fingerprint && *place == self.at)
    }
}

/// Whether `stamp`, looked at at `looked_ns`, was steady: not [`blaze_vfs::racy`], its file's
/// change (the later of its mtime and ctime) at least its window ([`blaze_vfs::window_ns`], by
/// the file system's clock) before. A racy stamp vouches for nothing: a second write in the
/// same tick of the file system's clock could leave it over other bytes.
pub(crate) fn steady(stamp: &Stamp, looked_ns: u64) -> bool {
    !blaze_vfs::racy(stamp, looked_ns)
}

/// Whether a fact taken under `saved` at `looked_ns` vouches for the file whose stamp is `now`.
fn vouches(saved: &Stamp, looked_ns: u64, now: &Stamp, steady: fn(&Stamp, u64) -> bool) -> bool {
    !now.is_unknown() && !now.is_gone() && saved == now && steady(saved, looked_ns)
}

/// The stamp of the file at `path` now, through links: its inode, size, modified and change
/// times in nanoseconds, and mode. One look; the error names the path.
pub(crate) fn stamp_now(path: &std::path::Path) -> Result<Stamp, String> {
    use std::os::unix::fs::MetadataExt as _;
    // alloc: an error, naming the path
    let meta = std::fs::metadata(path).map_err(|error| format!("looking at {}: {error}", path.display()))?;
    let ns = |secs: i64, nsec: i64| {
        u64::try_from(secs).unwrap_or(0).saturating_mul(1_000_000_000).saturating_add(u64::try_from(nsec).unwrap_or(0))
    };
    Ok(Stamp {
        ino: meta.ino(),
        size: meta.size(),
        mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
        ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        mode: meta.mode(),
    })
}

/// The hash of blaze's own executable `exe` (the running file:
/// [`blaze_profile::identity::running`]), the one home of it, which every reader of the
/// process's hash reads from here on ([`blaze_profile::identity::seed`]): `builds.blaze`, a
/// crash record's, the certificate's tool, the build id a client compares.
///
/// Read from `cache` (`<state>/tool`, a [`Tool`]) where its stamp is the executable's now,
/// whole, and was steady when written ([`steady`]): no byte of the executable is read. Else
/// hashed ([`blaze_profile::identity::binary_hash`]) between two looks, and where both are
/// equal the cache is written over (`<cache>.part`, renamed over it; not synced: a cache lost
/// to a crash is hashed again). A cache that does not decode (another layout, cut short) is
/// its miss, said at info, and is written over; one that cannot be written is said, and the
/// hash is the answer still.
pub(crate) fn tool_hash(cache: &std::path::Path, exe: &std::path::Path) -> Result<[u8; 32], String> {
    let (hash, _) = tool_hash_by(cache, exe, steady, blaze_profile::identity::binary_hash)?;
    // Seeded here and only here: the one home of the process's hash.
    blaze_profile::identity::seed(hash);
    Ok(hash)
}

/// [`tool_hash`] with `steady` and `hash` (what hashes the executable) given, seeding nothing;
/// whether the cache gave it. For the tests and their controls.
fn tool_hash_by(
    cache: &std::path::Path,
    exe: &std::path::Path,
    steady: fn(&Stamp, u64) -> bool,
    hash: impl FnOnce() -> std::io::Result<[u8; 32]>,
) -> Result<([u8; 32], bool), String> {
    let _span = tracing::info_span!(target: "blaze", "the hash of this blaze's executable, by its stamp", cache = %cache.display()).entered();
    // alloc: an error, once a process
    let unread = |error: String| format!("the hash of blaze's own executable {}: {error}", exe.display());
    let before = stamp_now(exe).map_err(unread)?;
    // alloc: the cache's name, for what it says
    let name = cache.display().to_string();
    let kept = match std::fs::read(cache) {
        Ok(bytes) => match tool_by(&name, Some(&bytes), before, steady) {
            Ok(kept) => kept,
            Err(why) => {
                tracing::info!(target: "blaze", "{why}: the executable is hashed again and the cache written over");
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        // alloc: an error, naming the cache
        Err(error) => return Err(unread(format!("reading its cache {name}: {error}"))),
    };
    if let Some(hash) = kept {
        return Ok((hash, true));
    }
    // Before any loop and of the real disk (`stamp_now`): the system's clock, which a native
    // driver's `wall_ns` reads too.
    let looked_ns = blaze_core::system_wall_ns();
    let hash = {
        let _hashing = tracing::info_span!(target: "blaze", "the hash of this blaze's executable: read it whole").entered();
        hash().map_err(|error| unread(error.to_string()))?
    };
    if stamp_now(exe).is_ok_and(|after| after == before) {
        let bytes = Tool { exe: before, looked_ns, hash }.encode();
        // alloc: the part's name, once a hash
        let mut part = cache.as_os_str().to_owned();
        part.push(".part");
        if let Err(error) = std::fs::write(&part, &bytes).and_then(|()| std::fs::rename(&part, cache)) {
            tracing::warn!(target: "blaze", "writing the hash of blaze's own executable to {name}: {error}; the next start hashes it again");
        }
    }
    Ok((hash, false))
}

/// The tool's hash from the cache's bytes `kept` (named `name`), if its stamp is `now` and was
/// `steady`.
fn tool_by(
    name: &str,
    kept: Option<&[u8]>,
    now: Stamp,
    steady: fn(&Stamp, u64) -> bool,
) -> Result<Option<[u8; 32]>, Undecoded> {
    let Some(kept) = kept else { return Ok(None) };
    let tool = Tool::decode(name, kept)?;
    Ok(vouches(&tool.exe, tool.looked_ns, &now, steady).then_some(tool.hash))
}

/// A placed output's content from its mark `name`, if the mark's stamp is `now` and was steady.
pub(crate) fn placed(
    name: &str,
    mark: Option<&[u8]>,
    now: Stamp,
) -> Result<Option<ContentId>, Undecoded> {
    placed_by(name, mark, now, steady)
}

fn placed_by(
    name: &str,
    mark: Option<&[u8]>,
    now: Stamp,
    steady: fn(&Stamp, u64) -> bool,
) -> Result<Option<ContentId>, Undecoded> {
    let Some(mark) = mark else { return Ok(None) };
    let placed = Placed::decode(name, mark)?;
    Ok(vouches(&placed.stamp, placed.looked_ns, &now, steady).then_some(placed.content))
}

#[cfg(test)]
mod tests {
    use blaze_core::S_IFREG;
    use hegel::generators as gs;

    use super::*;

    fn draw_stamp(tc: &hegel::TestCase) -> Stamp {
        Stamp {
            ino: tc.draw(gs::integers::<u64>()),
            size: tc.draw(gs::integers::<u64>()),
            mtime_ns: tc.draw(gs::integers::<u64>()),
            ctime_ns: tc.draw(gs::integers::<u64>()),
            mode: tc.draw(gs::integers::<u32>()),
        }
    }

    fn draw_id(tc: &hegel::TestCase) -> [u8; 32] {
        let mut id = [0; 32];
        for at in 0..4 {
            id[at * 8..at * 8 + 8].copy_from_slice(&tc.draw(gs::integers::<u64>()).to_le_bytes());
        }
        id
    }

    /// The invariant: every value decodes from its bytes to itself, and encodes again to the
    /// same bytes, of its layout's length. Control: a value one byte short is refused.
    #[hegel::test]
    fn a_value_round_trips_byte_for_byte(tc: hegel::TestCase) {
        let tool = Tool {
            exe: draw_stamp(&tc),
            looked_ns: tc.draw(gs::integers::<u64>()),
            hash: draw_id(&tc),
        };
        let bytes = tool.encode();
        assert_eq!(bytes.len(), 1 + 36 + 8 + 32);
        assert_eq!(Tool::decode("tool", &bytes), Ok(tool));
        assert_eq!(Tool::decode("tool", &bytes).unwrap().encode(), bytes);
        assert!(Tool::decode("tool", &bytes[..bytes.len() - 1]).is_err(), "the control");

        let placed = Placed {
            stamp: draw_stamp(&tc),
            looked_ns: tc.draw(gs::integers::<u64>()),
            content: ContentId(draw_id(&tc)),
        };
        let name = format!("{PLACED}/out/a");
        let bytes = placed.encode();
        assert_eq!(bytes.len(), 1 + 36 + 8 + 32);
        assert_eq!(Placed::decode(&name, &bytes), Ok(placed));
        assert_eq!(Placed::decode(&name, &bytes).unwrap().encode(), bytes);

        let carry = CarryMark {
            session: blaze_cas::Key(ContentId(draw_id(&tc))),
            root: ContentId(draw_id(&tc)),
        };
        let name = format!("{CARRY}/ws");
        let bytes = carry.encode();
        assert_eq!(bytes.len(), 1 + 32 + 32);
        assert_eq!(CarryMark::decode(&name, &bytes), Ok(carry));
        assert_eq!(CarryMark::decode(&name, &bytes).unwrap().encode(), bytes);

        let bodies = BodiesMark { session: blaze_cas::Key(ContentId(draw_id(&tc))) };
        let name = format!("{BODIES}/ws\0core");
        let bytes = bodies.encode();
        assert_eq!(bytes.len(), 1 + 32);
        assert_eq!(BodiesMark::decode(&name, &bytes), Ok(bodies));
        assert_eq!(BodiesMark::decode(&name, &bytes).unwrap().encode(), bytes);
        assert!(BodiesMark::decode(&name, &bytes[..bytes.len() - 1]).is_err(), "the control: cut short");
    }

    /// The invariant (rule 26): a library's mark decodes from its bytes to itself and encodes
    /// again to the same bytes; a value cut short, with a byte past its end, or of another
    /// layout is refused by the mark's name. Control: the cut value is refused.
    #[hegel::test]
    fn a_library_mark_round_trips_byte_for_byte(tc: hegel::TestCase) {
        let fingerprint = format!("{:x}", tc.draw(gs::integers::<u64>()));
        let at = format!("/rustc/{:x}/library", tc.draw(gs::integers::<u64>()));
        let mark = LibraryMark {
            digest_file: draw_stamp(&tc),
            looked_ns: tc.draw(gs::integers::<u64>()),
            fingerprint,
            at,
        };
        let bytes = mark.encode();
        assert_eq!(bytes.len(), 1 + 36 + 8 + 4 + mark.fingerprint.len() + 4 + mark.at.len());
        assert_eq!(LibraryMark::decode(LIBRARY, &bytes), Ok(mark.clone())); // clone: compared
        assert_eq!(LibraryMark::decode(LIBRARY, &bytes).unwrap().encode(), bytes);
        let cut = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        assert!(LibraryMark::decode(LIBRARY, &bytes[..cut]).is_err(), "the control: cut at {cut}");
        let mut longer = bytes.clone(); // clone: a test's value, changed
        longer.push(0);
        assert_eq!(
            LibraryMark::decode(LIBRARY, &longer),
            Err(Undecoded::Length { name: LIBRARY.into(), len: longer.len() })
        );
        let mut other = bytes.clone(); // clone: a test's value, changed
        other[0] = LAYOUT + 1;
        assert_eq!(
            LibraryMark::decode(LIBRARY, &other),
            Err(Undecoded::Layout { name: LIBRARY.into(), found: LAYOUT + 1 })
        );
    }

    /// The invariant: a library's mark vouches only under its own `DIGEST` stamp, steady, for
    /// the place asked, while that mount is held. Control: each condition broken alone refuses.
    #[test]
    fn a_library_mark_vouches_only_for_its_mount_under_its_stamp() {
        const CHANGED: u64 = 1_000 * 1_000_000_000;
        let stamp = Stamp { ino: 3, size: 65, mtime_ns: CHANGED, ctime_ns: CHANGED, mode: S_IFREG | 0o444 };
        let at = "/rustc/abc/library";
        let mark = LibraryMark {
            digest_file: stamp,
            looked_ns: CHANGED + blaze_vfs::COARSE_NS,
            fingerprint: "f".into(),
            at: at.into(),
        };
        let mounted = vec![("f".to_owned(), at.to_owned())];
        assert!(mark.vouches(&stamp, &mounted, at));
        assert!(!mark.vouches(&Stamp { ctime_ns: CHANGED + 1, ..stamp }, &mounted, at), "another stamp");
        assert!(!mark.vouches(&stamp, &[], at), "nothing mounted");
        assert!(!mark.vouches(&stamp, &[("g".to_owned(), at.to_owned())], at), "another fingerprint");
        assert!(!mark.vouches(&stamp, &mounted, "/rustc/def/library"), "another place asked");
        let racy = LibraryMark { looked_ns: CHANGED + 1, ..mark.clone() }; // clone: changed
        assert!(!racy.vouches(&stamp, &mounted, at), "marked inside the racy window");
    }

    /// The invariant: a value of a layout other than [`LAYOUT`] is refused naming the mark and
    /// the layout found, never read as this one; one of the wrong length names its length.
    #[test]
    fn a_value_of_another_layout_is_refused_by_name() {
        let one = Tool { exe: Stamp::GONE, looked_ns: 0, hash: [0; 32] }.encode();
        let session = blaze_cas::Key(ContentId([1; 32]));
        let carry = CarryMark { session, root: ContentId([2; 32]) }.encode();
        for found in [0u8, 2] {
            let mut bytes = one.clone(); // clone: a test's value, changed
            bytes[0] = found;
            let refused = Tool::decode("tool", &bytes).unwrap_err();
            assert_eq!(refused, Undecoded::Layout { name: "tool".into(), found });
            assert!(refused.to_string().contains("`tool`"), "{refused}");
            let placed = format!("{PLACED}/out/a");
            let refused = Placed::decode(&placed, &bytes).unwrap_err();
            assert_eq!(refused, Undecoded::Layout { name: placed, found });
            let mut bytes = carry.clone(); // clone: a test's value, changed
            bytes[0] = found;
            let refused = CarryMark::decode(CARRY, &bytes).unwrap_err();
            assert_eq!(refused, Undecoded::Layout { name: CARRY.into(), found });
            let mut bytes = BodiesMark { session }.encode();
            bytes[0] = found;
            let refused = BodiesMark::decode(BODIES, &bytes).unwrap_err();
            assert_eq!(refused, Undecoded::Layout { name: BODIES.into(), found });
        }
        let refused = BodiesMark::decode(BODIES, &carry).unwrap_err();
        assert_eq!(refused, Undecoded::Length { name: BODIES.into(), len: 1 + 32 + 32 });
        let refused = Tool::decode("tool", &one[..10]).unwrap_err();
        assert_eq!(refused, Undecoded::Length { name: "tool".into(), len: 10 });
        assert_eq!(Tool::decode("tool", &[]), Err(Undecoded::Length { name: "tool".into(), len: 0 }));
        assert_eq!(tool_by("tool", Some(&one[..10]), Stamp::GONE, steady).map(|_| ()), Err(refused));
    }

    /// The invariant (rule 26): a launched program's mark decodes from its bytes to itself and
    /// encodes again to the same bytes, of its layout's length; a value cut short, one byte
    /// longer, of another layout (byte 0 or 2), or of the previous layout (the two keys with
    /// no path the program runs from) is refused naming the mark, never read as this one.
    /// Control: the cut value is refused.
    #[hegel::test]
    fn a_launch_mark_round_trips_and_another_layout_is_refused_by_name(tc: hegel::TestCase) {
        let runs_as = format!("/w/target/debug/app{}", tc.draw(gs::integers::<u32>()));
        let (record, start) = (ContentId(draw_id(&tc)), blaze_cas::Key(ContentId(draw_id(&tc))));
        // clone: a test's value, compared below
        let mark = LaunchMark { record, start, runs_as: runs_as.clone() };
        let name = format!("{LAUNCH}/w/target/debug/deps/app-0123");
        let bytes = mark.encode();
        assert_eq!(bytes.len(), 1 + 32 + 32 + 4 + runs_as.len());
        assert_eq!(LaunchMark::decode(&name, &bytes), Ok(mark));
        // The previous layout: the record and the image's key alone.
        let previous = &bytes[..1 + 32 + 32];
        assert_eq!(LaunchMark::decode(&name, previous), Err(Undecoded::Length { name: name.clone(), len: previous.len() }));
        assert_eq!(LaunchMark::decode(&name, &bytes).unwrap().encode(), bytes);
        let cut = tc.draw(gs::integers::<usize>().max_value(bytes.len() - 1));
        assert_eq!(
            LaunchMark::decode(&name, &bytes[..cut]),
            Err(Undecoded::Length { name: name.clone(), len: cut }),
            "the control: cut at {cut}"
        );
        let mut longer = bytes.clone(); // clone: a test's value, changed
        longer.push(0);
        assert_eq!(LaunchMark::decode(&name, &longer), Err(Undecoded::Length { name: name.clone(), len: longer.len() }));
        for found in [0u8, 2] {
            let mut other = bytes.clone(); // clone: a test's value, changed
            other[0] = found;
            let refused = LaunchMark::decode(&name, &other).unwrap_err();
            assert_eq!(refused, Undecoded::Layout { name: name.clone(), found });
            assert!(refused.to_string().contains("/w/target/debug/deps/app-0123"), "{refused}");
        }
    }

    /// Every case of the window: a stamp changed at `CHANGED` (a whole second: the coarse
    /// window), looked at `after` it, asked of with `now`; the content given or not.
    fn given(steady: fn(&Stamp, u64) -> bool) -> Vec<(u64, bool, bool, bool)> {
        const CHANGED: u64 = 1_000 * 1_000_000_000;
        let mode = S_IFREG | 0o644;
        let stamp = Stamp { ino: 9, size: 4, mtime_ns: CHANGED, ctime_ns: CHANGED, mode };
        let moved = Stamp { mtime_ns: CHANGED + 1, ..stamp };
        let window = blaze_vfs::window_ns(&stamp);
        assert_eq!(window, blaze_vfs::COARSE_NS, "a whole-second stamp's window");
        let mut out = Vec::new();
        for after in [0, 1, blaze_vfs::FINE_NS, window - 1, window, 2 * window] {
            let looked_ns = CHANGED + after;
            for now in [stamp, moved] {
                let mark = Tool { exe: stamp, looked_ns, hash: [3; 32] }.encode();
                let hash = tool_by("tool", Some(&mark), now, steady).unwrap();
                let placed = Placed { stamp, looked_ns, content: ContentId([4; 32]) }.encode();
                let content = placed_by("placed\0/a", Some(&placed), now, steady).unwrap();
                assert_eq!(hash.is_some(), content.is_some(), "one rule for both");
                out.push((after, now == stamp, hash.is_some(), after >= window));
            }
        }
        out
    }

    /// The invariant: content is given only under the equal, steady stamp it was hashed under.
    fn holds(cases: &[(u64, bool, bool, bool)]) -> bool {
        cases.iter().all(|&(_, equal, given, steady)| given == (equal && steady))
    }

    /// Control: with `steady` always true, a stamp inside the racy window vouches, and the
    /// invariant breaks.
    #[test]
    fn a_stamp_inside_the_racy_window_vouches_for_nothing() {
        assert!(holds(&given(steady)), "{:?}", given(steady));
        assert!(!holds(&given(|_, _| true)), "the control: racy stamps trusted");
        assert_eq!(tool_by("tool", None, Stamp::GONE, steady), Ok(None), "no cache, no hash");
        let mark = Tool { exe: Stamp::UNKNOWN, looked_ns: u64::MAX, hash: [5; 32] }.encode();
        let unknown = tool_by("tool", Some(&mark), Stamp::UNKNOWN, steady);
        assert_eq!(unknown, Ok(None), "an unknown stamp vouches for nothing");
    }

    /// The invariant (rule 26): `<state>/tool` gives the executable's hash only under the
    /// executable's stamp now, equal whole, and steady when written; a cache of another layout
    /// (byte 0 or 2) or cut short is hashed again and written over, never read as this one; a
    /// cache written inside the racy window is hashed again. Control: with `steady` always
    /// true, the cache written inside the window serves its hash, and the oracle says so.
    #[test]
    fn a_tool_hash_is_read_from_its_cache_only_under_the_equal_steady_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let (exe, cache) = (dir.path().join("blaze"), dir.path().join("tool"));
        std::fs::write(&exe, b"blaze, the first build").unwrap();
        let hashed = std::cell::Cell::new(0u32);
        let hash = |with: u8| {
            let hashed = &hashed;
            move || {
                hashed.set(hashed.get() + 1);
                Ok([with; 32])
            }
        };
        // No cache: hashed, and the cache written under the stamp.
        assert_eq!(tool_hash_by(&cache, &exe, steady, hash(1)).unwrap(), ([1; 32], false));
        assert_eq!(hashed.get(), 1);
        let written = Tool::decode("tool", &std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!((written.exe, written.hash), (stamp_now(&exe).unwrap(), [1; 32]));
        // Written within its window of the file's change (it was just written): racy, so it
        // is hashed again. The control: trusted as steady, it serves the hash kept.
        assert_eq!(tool_hash_by(&cache, &exe, steady, hash(2)).unwrap(), ([2; 32], false));
        assert_eq!(hashed.get(), 2);
        assert_eq!(
            tool_hash_by(&cache, &exe, |_, _| true, hash(3)).unwrap(),
            ([2; 32], true),
            "the control: a racy cache trusted serves its hash"
        );
        assert_eq!(hashed.get(), 2);

        // A cache looked at long after the change: served, nothing hashed.
        let now = stamp_now(&exe).unwrap();
        let far = Tool { exe: now, looked_ns: blaze_vfs::window_ns(&now) * 4 + now.mtime_ns.max(now.ctime_ns), hash: [4; 32] };
        std::fs::write(&cache, far.encode()).unwrap();
        assert_eq!(tool_hash_by(&cache, &exe, steady, hash(5)).unwrap(), ([4; 32], true));
        assert_eq!(hashed.get(), 2, "a steady cache under the equal stamp reads no byte of the executable");

        // Another layout, cut short or empty: hashed again and written over.
        let good = far.encode();
        let mut zero = good.clone(); // clone: a test's value, changed
        zero[0] = 0;
        let mut two = good.clone(); // clone: a test's value, changed
        two[0] = 2;
        for (at, bad) in [zero, two, good[..good.len() - 1].to_vec(), Vec::new()].into_iter().enumerate() {
            std::fs::write(&cache, &bad).unwrap();
            let before = hashed.get();
            assert_eq!(tool_hash_by(&cache, &exe, steady, hash(6)).unwrap(), ([6; 32], false), "case {at}");
            assert_eq!(hashed.get(), before + 1, "case {at}: hashed again");
            let over = Tool::decode("tool", &std::fs::read(&cache).unwrap()).unwrap();
            assert_eq!(over.hash, [6; 32], "case {at}: written over");
        }

        // The executable replaced: its stamp is another, the cache vouches for nothing.
        std::fs::write(&cache, far.encode()).unwrap();
        std::fs::write(&exe, b"blaze, built again").unwrap();
        assert_eq!(tool_hash_by(&cache, &exe, steady, hash(7)).unwrap(), ([7; 32], false));
    }
}
