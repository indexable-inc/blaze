//! What a hermetic child may touch, stated once (sans-IO): built from the spec and the roots'
//! files, rendered by each OS backend (an SBPL profile on macOS, a Landlock ruleset on Linux)
//! and enforced by the simulation's OS, so the proof and the confinement read one list.
//!
//! The rules, each an absolute host spelling:
//! - the program: run and read, that one file;
//! - each named helper: run and read, that one file;
//! - a tree root: read, its whole folder (nothing under it changes while its digest is the
//!   same, and the digest is in the roots' identity);
//! - a package root: read, EACH FILE ITS GLOBS ADMIT, one literal rule a file (never the
//!   folder: an undeclared file beside them is refused by the OS, so a run without a report
//!   cannot read what its read list does not hold);
//! - the output root: read and write, its whole folder;
//! - the backend's system set (the loader, the system libraries: `system`), read, each whole;
//!   they are outside every root, so their identity is the toolchain's and the host's (the
//!   lane's message says what is and is not in a key for them);
//! - `/dev/null`: read and write.
//!
//! Every spelling is checked to be one spelling of one absolute path, in UTF-8, with no byte a
//! profile cannot quote (a quote, a backslash, a control byte): [`Fault::PathRefused`].

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, planted::Planted};
use blaze_roots::{RootId, RootKind};

use super::{Fault, HermeticPlant, HermeticSpec, Program, RootFiles};

/// What a rule grants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Open to read (and stat, and list where it is a folder).
    Read,
    /// Read, create, write, remove, beneath.
    Write,
    /// Run (and read, which a run needs).
    Exec,
}

/// Why a rule is there: what an error and a profile's comment name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    Program,
    Helper,
    /// A folder under the output root whose programs the run built and may start.
    HelperFolder,
    /// A path that carries neither input nor output (a jobserver's fifo, a log folder).
    Channel,
    TreeRoot(RootId),
    PackageFile(RootId),
    Output(RootId),
    System,
    Null,
}

/// One rule, as read back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rule<'p> {
    pub path: &'p str,
    /// The whole folder beneath `path` too, or the one file.
    pub subtree: bool,
    pub access: Access,
    pub why: Why,
}

/// The most rules of one plan (a table has a cap, rule 18).
pub const MOST_RULES: u32 = 1 << 16;

/// The fields of a [`Plan`].
pub struct PlanOpts<'a, 's> {
    pub spec: &'s HermeticSpec<'s>,
    /// The backend's system set: absolute folders read whole.
    pub system: &'s [&'s str],
    pub files: &'s dyn RootFiles,
    pub alloc: Alloc<'a>,
}

/// The plan.
pub struct Plan<'a> {
    text: AVec<'a, u8>,
    rows: AVec<'a, (u32, u32, bool, Access, Why)>,
}

impl<'a> Plan<'a> {
    pub fn new<P: Planted<HermeticPlant>>(opts: PlanOpts<'a, '_>, plant: P) -> Result<Plan<'a>, Fault> {
        let spec = opts.spec;
        let mut plan = Plan { text: AVec::new_in(opts.alloc), rows: AVec::new_in(opts.alloc) };
        if let Program::Stored { path, .. } = spec.program() {
            plan.push(&[path.as_bytes()], false, Access::Exec, Why::Program)?;
        }
        for helper in spec.helpers() {
            plan.push(&[helper.as_bytes()], false, Access::Exec, Why::Helper)?;
        }
        // Checked by the spec: each under the output root, so its programs are the run's own.
        for folder in spec.helper_folders() {
            plan.push(&[folder.as_bytes()], true, Access::Exec, Why::HelperFolder)?;
        }
        // Checked by the spec: each outside every mount. A folder is written beneath; a file
        // (a fifo) is the one file (the backend asks the kernel which it is).
        for channel in spec.channels() {
            plan.push(&[channel.as_bytes()], true, Access::Write, Why::Channel)?;
        }
        let roots = spec.roots();
        for at in 0..roots.len() {
            let id = RootId(at as u8);
            let Some(root) = roots.get(id) else { continue };
            let Some(mount) = spec.mount_of(id) else {
                return Err(Fault::Unmounted { root: id });
            };
            match root.kind {
                RootKind::Tree { .. } => plan.push(&[mount], true, Access::Read, Why::TreeRoot(id))?,
                RootKind::Output => plan.push(&[mount], true, Access::Write, Why::Output(id))?,
                RootKind::Package { .. } => {
                    opts.files.package_files(id, &mut |path, _digest, _len| {
                        if path.is_root() {
                            plan.push(&[mount], false, Access::Read, Why::PackageFile(id))
                        } else {
                            plan.push(&[mount, b"/", path.as_str().as_bytes()], false, Access::Read, Why::PackageFile(id))
                        }
                    })?;
                }
            }
        }
        for folder in opts.system {
            plan.push(&[folder.as_bytes()], true, Access::Read, Why::System)?;
        }
        plan.push(&[b"/dev/null"], false, Access::Write, Why::Null)?;
        if plant.has(HermeticPlant::HoleInPlan) {
            plan.push(&[b"/"], true, Access::Read, Why::System)?;
        }
        Ok(plan)
    }

    fn push(&mut self, parts: &[&[u8]], subtree: bool, access: Access, why: Why) -> Result<(), Fault> {
        let at = self.rows.len() as u32;
        if at >= MOST_RULES {
            return Err(Fault::PlanOver { most: MOST_RULES });
        }
        let start = self.text.len();
        for part in parts {
            self.text.try_extend_from_slice(part).map_err(|_| Fault::OutOfMemory)?;
        }
        let path = &self.text[start..];
        if !one_spelling(path) {
            self.text.truncate(start);
            return Err(Fault::PathRefused { at });
        }
        let len = (self.text.len() - start) as u32;
        self.rows.try_push((start as u32, len, subtree, access, why)).map_err(|_| Fault::OutOfMemory)?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Each rule, in the order made.
    pub fn rules(&self) -> impl Iterator<Item = Rule<'_>> + '_ {
        self.rows.iter().map(|&(at, len, subtree, access, why)| {
            // Checked UTF-8 when pushed. Never a stand-in spelling: a wrong one would widen the
            // sandbox. (The end state is an `AString` text, so no check is left here.)
            let path = core::str::from_utf8(&self.text[at as usize..(at + len) as usize])
                .expect("blaze-std hermetic plan: a rule's text was checked UTF-8 when pushed");
            Rule { path, subtree, access, why }
        })
    }

    /// Whether the plan lets the child `access` the absolute spelling `path` (the simulation's
    /// OS asks this; the OS backends render the same rules).
    pub fn allows(&self, path: &[u8], access: Access) -> bool {
        self.rules().any(|rule| {
            let granted = match (rule.access, access) {
                (Access::Write, _) => true,
                (Access::Exec, Access::Exec | Access::Read) => true,
                (Access::Read, Access::Read) => true,
                _ => false,
            };
            granted && covers(rule.path.as_bytes(), rule.subtree, path)
        })
    }
}

/// Whether the rule at `rule` covers `path`.
pub(crate) fn covers(rule: &[u8], subtree: bool, path: &[u8]) -> bool {
    if path == rule {
        return true;
    }
    subtree && (rule == b"/" || (path.starts_with(rule) && path.get(rule.len()) == Some(&b'/')))
}

/// One spelling of one absolute path: starts at `/`, no empty, `.` or `..` part, no trailing
/// `/` (but `/` itself), UTF-8, and no quote, backslash or control byte.
pub(crate) fn one_spelling(path: &[u8]) -> bool {
    if path == b"/" {
        return true;
    }
    if path.first() != Some(&b'/') || path.last() == Some(&b'/') || core::str::from_utf8(path).is_err() {
        return false;
    }
    if path.iter().any(|&b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\') {
        return false;
    }
    path[1..].split(|&b| b == b'/').all(|part| !part.is_empty() && part != b"." && part != b"..")
}
