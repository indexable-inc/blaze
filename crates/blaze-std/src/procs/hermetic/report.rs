//! The read report: what a run says it read, as a list checked against its roots.
//!
//! - A program with its own report (a C compiler's depfile, `-MD -MF <file>`) writes it under
//!   the output root; a [`ReportParser`] names each path in it. The C driver's depfile parser
//!   (blaze-build-script `toolchain::depfile_reads`, D174) implements this trait; bstd holds no
//!   format's parser of its own. Each path is resolved through the spec's mounts to a root and
//!   a relative path (a path under no mount is [`Fault::ReadUnmounted`]: outside every root),
//!   looked up through the caller's [`RootFiles`] for what it was, and pushed; the list is then
//!   checked by `ReadList::under` ([`super::Reads::Exact`]).
//! - Without one, each package root's every admitted file is the read
//!   ([`super::Reads::WholeRoots`]); the plan let the child read only those, so the list holds
//!   every read it could have made. A tree root is in the key by its digest.
//!
//! A report names what the program says it read: a program that reads a file and leaves it out
//! is not caught here, only kept inside its roots by the OS. That is why the plan grants a
//! package root file by file.

use core::time::Duration;

use blaze_own::{AVec, Alloc, planted::Planted};
use blaze_roots::{Limits, RelPath, Unmounted};

use super::{Event, Fault, HermeticMachine, HermeticPlant, HermeticSpec, MOST_REPORT, ReportSpec, RootFiles, mount_under};

/// Why a report could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportError {
    /// The report's text is not the format its parser reads; `at` the byte where it stopped.
    Malformed { at: u32 },
    /// A path in it is relative: a report names absolute paths (the driver passes absolute
    /// spellings so it does).
    Relative { at: u32 },
}

/// A program's own report format.
pub trait ReportParser {
    /// Each path `text` names, in order, as the program spelled it.
    fn paths<'t>(&self, text: &'t [u8], each: &mut dyn FnMut(&'t [u8]) -> Result<(), Fault>) -> Result<(), Fault>;
}

/// The report of `spec`, read and fed to `machine` as reads, then its end.
pub(super) fn read<'a, P: Planted<HermeticPlant>>(
    machine: &mut HermeticMachine<'a, P>,
    spec: &HermeticSpec<'_>,
    files: &dyn RootFiles,
    now: Duration,
    alloc: Alloc<'a>,
) -> Result<(), Fault> {
    let ReportSpec::Program { file, parser } = spec.report() else {
        return Err(Fault::Machine { state: machine.state(), event: "a report asked of a spec without one" });
    };
    let mut text = AVec::new_in(alloc);
    files.read_output(file, MOST_REPORT, &mut text)?;
    let mut rel = AVec::new_in(alloc);
    parser.paths(&text, &mut |spelling| {
        if spelling.first() != Some(&b'/') {
            return Err(Fault::Report(ReportError::Relative { at: 0 }));
        }
        let Some((root, rest)) = mount_under(spec.mounts(), spelling) else {
            return machine.step(now, Event::ReadUnmounted(Unmounted::Outside));
        };
        rel.clear();
        if let Err(why) = blaze_roots::normalize_into(rest, Limits::SHIPPED.most_path, &mut rel) {
            return machine.step(now, Event::ReadUnmounted(Unmounted::Path(why)));
        }
        let path = RelPath::from_bytes(&rel, Limits::SHIPPED.most_path).map_err(|why| Fault::ReadUnmounted(Unmounted::Path(why)))?;
        let seen = files.seen(root, path)?;
        machine.step(now, Event::Read { root, path, seen })
    })?;
    machine.step(now, Event::ReportEnd)
}
