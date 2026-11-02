//! The start hook's last step before STARTED: the dependency images' open words
//! ([`crate::ImageWrite`], [`crate::ImageLookup`]), stored in the run's private copies of
//! their data once the overlays are mapped and before any of their code can run. The system
//! loader can't store them: it is done before the hook maps the images.
//!
//! A dependency image is linked at its base and slides with the executable, but its words
//! that name its own addresses are not listed for rebasing, so a run with a slide other than
//! 0 is refused while one is mapped ([`check_slide`]), naming it.

use crate::handshake::Grant;
use crate::{Error, Format};

/// Refuses `slide` unless it is 0 or no overlay of `grant` is a dependency image.
pub(crate) fn check_slide(grant: &Grant, slide: u64) -> Result<(), Error> {
    if slide == 0 {
        return Ok(());
    }
    match grant.overlays.iter().find(|it| it.image) {
        None => Ok(()),
        Some(map) => Err(Error::new(format!( // alloc: error path, the start fails
            "this run's executable is slid by {slide:#x}, and the dependency image mapped at {:#x}..{:#x} is linked at \
             its base with its own addresses unlisted for rebasing: it runs only unslid (ASLR off for the executable, \
             or an executable linked at a fixed address)",
            map.at,
            map.range().end
        ))),
    }
}

/// The C name of a symbol named `name` in an image of `format`: what `dlsym` and
/// `GetProcAddress` take.
pub(crate) fn c_name(format: Format, name: &str) -> Result<&str, Error> {
    match format {
        Format::Elf | Format::Pe => Ok(name),
        Format::MachO => name.strip_prefix('_').filter(|it| !it.is_empty()).ok_or_else(|| {
            Error::new(format!( // alloc: error path, the start fails
                "the dependency image binds `{name}`, a Mach-O name with no leading `_` after which a C name follows: \
                 nothing a library exports"
            ))
        }),
    }
}

/// The most names a failed start lists.
const MOST_LISTED: usize = 8;

/// Stores each of `grant`'s writes, then each of its lookups, at its place plus `slide`, by
/// `store(address, value)`. A write's value gets `slide` added when it is an address. A lookup
/// stores what `find` gives for its C name plus its addend; a weak one `find` finds nothing
/// for stores 0. A strong one it finds nothing for is never stored: the start fails, naming
/// every such name.
///
/// `grant` came from [`Grant::decode`], so every place is 8 bytes inside an overlay of a
/// dependency image mapped as a private copy ([`Grant::check_binds`]): `store` writes only
/// there.
pub(crate) fn bind(
    grant: &Grant,
    slide: u64,
    mut find: impl FnMut(&str) -> Result<Option<u64>, Error>,
    mut store: impl FnMut(u64, u64),
) -> Result<(), Error> {
    for write in &grant.writes {
        let value = if write.address { write.value.wrapping_add(slide) } else { write.value };
        store(write.place.wrapping_add(slide), value);
    }
    let mut missing = 0usize;
    let mut listed = String::new(); // alloc: none until a strong miss, an error path
    for lookup in &grant.lookups {
        let name = grant.name(lookup);
        match find(c_name(grant.format, name)?)? {
            Some(address) => store(lookup.place.wrapping_add(slide), address.wrapping_add_signed(lookup.addend)),
            None if lookup.weak => store(lookup.place.wrapping_add(slide), 0),
            None => {
                missing += 1;
                if missing <= MOST_LISTED {
                    listed.push_str(if missing == 1 { "`" } else { "`, `" });
                    listed.push_str(name);
                }
            }
        }
    }
    if missing != 0 {
        let more = match missing > MOST_LISTED {
            true => format!(" and {} more", missing - MOST_LISTED), // alloc: error path, the start fails
            false => String::new(), // alloc: error path, the start fails
        };
        return Err(Error::new(format!( // alloc: error path, the start fails
            "the dependency image needs {listed}`{more}, which nothing this run's executable loaded defines: a strong \
             reference to it would call or read through 0"
        )));
    }
    Ok(())
}
