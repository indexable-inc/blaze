//! Applies blaze's edits to a program while it runs.
//!
//! A program blaze-link linked calls every function a patch can replace through a slot of a
//! jump table, and [`Live::start`] readies it to take patches. The program runs from the
//! daemon's code memory (blaze-inproc), where each commit writes a patch's code; the program
//! asks the daemon for the patch's message over blaze-inproc's channel, writes its data, and
//! switches to it in one atomic store ([`slots`]). No thread is stopped and no code a thread
//! can be in is written over.
//!
//! [`Live::poll`] takes what came in since it was last called. Call it where nothing of the
//! program's is half done, such as between two frames: a function the calling thread is
//! running when its patch lands finishes as it was, and the next call runs the new code.
//!
//! Unsafe code is only in the modules that reach the kernel and the image's memory (`slots`'
//! platform, image and execution-log modules), each allowing it with its reason; the rest denies it. There is no safe version of them to pay for: std
//! maps no executable memory and gives no view of the image's words at the addresses the
//! linker placed them. A message is read in place by safe code only
//! ([`patch::Message::view`]), and the jump table's own words are `std` atomics at the
//! addresses the linker gave them.
//!
//! What a patch cannot do, it says, and the program has to be started again
//! ([`Outcome::Restart`]).
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64; elsewhere
//! [`Live::start`] says so.

#![deny(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

mod ask;
pub mod format;
pub mod patch;
pub mod slots;

pub use slots::{Announcements, Live, Outcome};

/// The counters that code the daemon compiled to count its branches adds to, and the function
/// that code finds them by: the dev runtime's (`blaze_inproc_rt::live`), so every dev executable
/// has them whatever it depends on.
pub use blaze_inproc_rt::live::blaze_live_counters;

/// What the daemon says of the tiers of this program: the functions it spends its time in, the
/// level each is compiled at, and what the daemon did of late. `None` while the daemon tiers
/// none of its functions; an error in a program the daemon did not start, or when the channel
/// to it fails. Each call asks the daemon over blaze-inproc's channel, so a program that shows
/// it asks a few times a second, not every frame.
pub fn tiers() -> Result<Option<format::Tiers>, String> {
    use patch::ask::{NO_TIERS, TIERS, TIERS_ASK};
    let attached = blaze_inproc::runtime::attach()
        // alloc: error path
        .map_err(|it| {
            format!("the tiers come only to a program blaze started on its code memory: {it}")
        })?;
    // alloc: the text, a few times a second
    let mut text = Vec::new();
    let kinds = ask::Kinds { ask: TIERS_ASK, piece: TIERS, none: NO_TIERS };
    if ask::fetch(attached, &kinds, "the tiers", u64::to_le_bytes, &mut text)?.is_none() {
        return Ok(None);
    }
    // alloc: error path
    let text =
        std::str::from_utf8(&text).map_err(|_| "the daemon's tiers are not UTF-8".to_owned())?;
    // alloc: error path
    format::Tiers::decode(text)
        .map(Some)
        .ok_or_else(|| "the daemon's tiers do not read as tiers".to_owned())
}
