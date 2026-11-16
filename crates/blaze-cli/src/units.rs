//! The units task (drive.md, "Units"): the workspace's layout, read once per request by
//! blaze-manifest's sans-IO `Layout` ([`lay`]) when what it read last moved (or on a cold run, or
//! when a units record must be made), each manifest it asks for and each directory it lists a
//! row of the leaf table (sweep.rs). The root's units list each member's manifest as a part at
//! `Locate::Source`, the digest of that member's share (its parts and findings), so an edit of
//! one member's manifest re-makes that member's units and no other's; a member's units list its
//! crate roots (`Locate::File` in the package's edition, each with its path), and each
//! dependency not built yet is a finding on its key. A manifest the layout refuses is a finding
//! on the line it names, never a refusal of the whole workspace.

use blaze_drive::{Locate, Name, Part, Task};
use blaze_ids::ContentId;
use blaze_manifest::layout::{self, Entry, Layout, LayoutError, Need, Pass, Source, Span};
use blaze_own::{Alloc, Texts, Vec};
use blaze_surface::Edition;
use blaze_unit_graph::{StdIs, UnitKind, implicit};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};

use crate::CliError;
use crate::sweep::{self, Rows, Stamps};
use crate::work::{Did, Made, parts_out};

/// The finding number of what the layout refuses in a manifest, on the line it names. Its one
/// argument is the refusal.
pub const UNREAD: u16 = 3;
/// The finding number of a blaze-edition manifest's dependency line naming the toolchain's
/// bstd (`bstd`, or the package `blaze-std`): blaze gives it with no line (toolchain.md,
/// section 4). Its range is the whole line with its newline, which its fix deletes; its one
/// argument is the name.
pub const TOOLCHAIN_NAMED: u16 = 9;
/// The workspace's lock, at its root.
const LOCK: &str = "Cargo.lock";

const UNITS_OUT_DOMAIN: &str = "blaze-cli units out 3";
const MEMBER_SOURCE_DOMAIN: &str = "blaze-cli member source 1";
/// Passes the layout may ask for: the depth of its search (root, globs, path dependencies,
/// target directories), far under this on any workspace (rule 18).
const MOST_PASSES: usize = 64;

/// The layout's answer, as the units records it makes: the root's, and each member's by its
/// manifest's units name.
pub struct Laid<'f> {
    pub root: Made<'f>,
    pub members: Vec<(Name, Made<'f>), &'f Frame>,
    /// The workspace's `Cargo.lock` planned (fetch.rs): `None` when there is none, or when it
    /// is refused (then a [`crate::fetch::LOCK_UNREAD`] finding on the root's units).
    pub lock: Option<crate::fetch::Lockfile<'f>>,
    /// The members matched to the lock (graph.rs): `None` with no lock, or when the match is
    /// refused (then a [`crate::fetch::LOCK_UNREAD`] finding on the root's units).
    pub parts: Option<crate::graph::Parts>,
    /// The `UnitGraph` job, made from `parts` once the units' source is known (main.rs).
    pub graph: Option<crate::graph::Graph<'f>>,
}

/// Read the workspace's layout from the checkout's root: every manifest and listing it asks for
/// is pushed onto `rows`.
pub fn lay<'f, 'l>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    stamps: &impl Stamps,
    rows: &mut Rows<'_>,
    // The toolchain the command opens, given by `main`: in the units' output (rule 75).
    toolchain: blaze_toolchain::ToolchainId,
    frame: &'f Frame,
    alloc: Alloc<'l>,
) -> Result<Laid<'f>, CliError> {
    let cx = cx.span("units: the workspace's layout");
    // Each manifest answered, in the order answered (a `ManifestId` is its index): its path,
    // its text ("" when none is there) and whether one is.
    let mut paths: Texts<&Frame> = Texts::new_in(frame);
    let mut texts: Texts<&Frame> = Texts::new_in(frame);
    let mut present: Vec<bool, &Frame> = Vec::new_in(frame);
    let laid = (|| -> Result<Result<layout::Units<'l>, LayoutError<'l>>, CliError> {
        let mut layout = match Layout::new("", alloc) {
            Ok(layout) => layout,
            Err(error) => return Ok(Err(error)),
        };
        for _ in 0..MOST_PASSES {
            let needs = match layout.pass() {
                Err(error) => return Ok(Err(error)),
                Ok(Pass::Done(units)) => return Ok(Ok(units)),
                Ok(Pass::Need(needs)) => needs,
            };
            for need in needs.iter() {
                match need {
                    Need::Manifest(path) => {
                        let path = path.as_str();
                        let mut text = Bytes::new_in(frame, "a manifest");
                        let found = sweep::read_manifest(&cx, root, stamps, path, rows, &mut text)?;
                        let text = match found {
                            true => Some(core::str::from_utf8(text.as_slice()).map_err(|_| CliError::Text { what: "a manifest is not UTF-8" })?),
                            false => None,
                        };
                        // A path asked again is answered once (the layout keeps the first).
                        if !(0..paths.len()).any(|i| paths.text(i) == path) {
                            paths.push(path);
                            texts.push(text.unwrap_or(""));
                            present.push(found);
                        }
                        if let Err(error) = layout.manifest(path, text) {
                            return Ok(Err(error));
                        }
                    }
                    Need::Listing(dir) => {
                        let dir = dir.as_str();
                        let listing = sweep::read_listing(&cx, root, stamps, dir, rows, frame)?;
                        let entries: Option<Vec<Entry<'_>, &Frame>> = listing.as_ref().map(|listing| {
                            let mut entries = Vec::with_capacity_in(listing.dirs.len(), frame);
                            entries.extend(listing.entries().map(|(name, is_dir)| Entry { name, is_dir }));
                            entries
                        });
                        layout.listing(dir, entries.as_deref());
                    }
                }
            }
        }
        Err(CliError::Text { what: "the workspace's layout asked for more than 64 passes" })
    })()?;
    let mut members = Vec::new_in(frame);
    let mut root_made = Made::new_in(Name::units(), frame);
    let mut facts = None;
    match laid {
        Ok(units) => {
            // With no dependency outside the workspace and the toolchain there is no graph:
            // the lock is not read (nothing in it can change the answer), so a stale one costs
            // nothing and a lock edit moves no key.
            // Then the members' own lock stands in (graph.rs `members_lock`): the graph and the
            // names world are the toolchain's crates and the members alone.
            let own = match crate::graph::reaches_outside(&units) {
                true => None,
                false => Some(crate::graph::members_lock(&units, frame)),
            };
            facts = Some((crate::graph::Members::of(&units), own));
            // Outside dependencies are read by the unit-graph job (unit_graph_task), which
            // fetches what the roots reach: nothing is said of them here.
            for member in units.members.iter() {
                let id = member.manifest.0 as usize;
                let made = member_units(Name::manifest(paths.text(id).as_bytes()), member, texts.text(id), toolchain, frame);
                root_made.parts.push(Part { name: made.name, locate: Locate::Source(member_source(&made)), place: 0 });
                members.push((made.name, made));
            }
        }
        // A refusal is a finding on the line it names: the root's, or a member's whose own
        // units then hold it.
        Err(error) => {
            let (manifest, span) = place_of(&error);
            let message = refusal(&error, frame);
            let at = span.map_or((0, 0), |span| (span.start, span.end));
            match manifest.filter(|id| *id != 0 && (*id as usize) < present.len() && present[*id as usize]) {
                None => root_made.find(UNREAD, at, &[message.as_str()]),
                Some(id) => {
                    let name = Name::manifest(paths.text(id as usize).as_bytes());
                    let mut made = Made::new_in(name, frame);
                    made.find(UNREAD, at, &[message.as_str()]);
                    made.out = parts_out(&made.parts);
                    root_made.parts.push(Part { name, locate: Locate::Source(member_source(&made)), place: 0 });
                    members.push((name, made));
                }
            }
        }
    }
    // The lock, when a member reaches outside: one of the layout's reads, so an edit to it
    // moves the units' source.
    let mut text = Bytes::new_in(frame, "Cargo.lock");
    let lock_text: Option<&[u8]> = match &facts {
        None => None,
        Some((_, Some(own))) => Some(own),
        Some((_, None)) => match sweep::read_manifest(&cx, root, stamps, LOCK, rows, &mut text)? {
            false => None,
            true => Some(Vec::from_slice_in(text.as_slice(), frame).leak()),
        },
    };
    let lock = match lock_text {
        None => None,
        Some(lock_text) => match crate::fetch::Lockfile::read(lock_text, frame) {
            Ok(lock) => Some(lock),
            Err(words) => {
                root_made.find(crate::fetch::LOCK_UNREAD, (0, 0), &[words.as_str()]);
                None
            }
        },
    };
    let parts = match (facts, &lock) {
        (Some((facts, _)), Some(lockfile)) => match facts.locked(lockfile.lock) {
            Ok(parts) => Some(parts),
            Err(unlocked) => {
                let words = bstd::mem::format_in!(frame, "{unlocked}");
                root_made.find(crate::fetch::LOCK_UNREAD, (0, 0), &[words.as_str()]);
                None
            }
        },
        _ => None,
    };
    root_made.parts.sort_unstable_by_key(|part| part.name);
    root_made.parts.dedup_by_key(|part| part.name);
    root_made.out = parts_out(&root_made.parts);
    Ok(Laid { root: root_made, members, lock, parts, graph: None })
}

/// Where a refusal is: the manifest's id and the span, when it names one.
fn place_of(error: &LayoutError<'_>) -> (Option<u32>, Option<Span>) {
    let span = match error {
        LayoutError::Expected { at, .. }
        | LayoutError::NoPackageName { at }
        | LayoutError::NotARoot { at }
        | LayoutError::Edition { at, .. }
        | LayoutError::NotInherited { at, .. }
        | LayoutError::OutsideAnchor { at }
        | LayoutError::Pattern { at, .. }
        | LayoutError::RecursiveGlob { at }
        | LayoutError::NoTargetFile { at }
        | LayoutError::GitReferences { at }
        | LayoutError::GitAndPath { at }
        | LayoutError::Underscored { at } => Some(*at),
        LayoutError::DuplicateName { second, .. } => Some(*second),
        LayoutError::Toml { .. } | LayoutError::NoManifest { .. } | LayoutError::NotAPackage { .. } | LayoutError::NotAMember { .. } => None,
    };
    (span.map(|span| span.manifest.0), span)
}

/// A refusal's words, without the reader's own `manifest N bytes a..b: ` place (the finding is
/// placed by its range).
fn refusal<'f>(error: &LayoutError<'_>, frame: &'f Frame) -> bstd::mem::String<&'f Frame> {
    let whole = bstd::mem::format_in!(frame, "{error}");
    let words = match whole.strip_prefix("manifest ").and_then(|rest| rest.split_once(": ")) {
        Some((_, words)) => words,
        None => whole.as_str(),
    };
    bstd::mem::String::from_str_in(words, frame)
}

/// The digest a member's part is found by in the root's record: its share of the layout.
fn member_source(made: &Made<'_>) -> ContentId {
    let mut h = blaze_ids::Hasher::new(MEMBER_SOURCE_DOMAIN);
    h.update(&made.out.0);
    for (rule, start, end, at, to) in made.findings.iter() {
        h.update(&rule.to_le_bytes()).update(&start.to_le_bytes()).update(&end.to_le_bytes()).part(&made.messages[*at..*to]);
    }
    for (name, at, to, module) in made.paths.iter() {
        h.update(&name.subject.0).part(&made.path_bytes[*at as usize..*to as usize]).update(&[u8::from(*module)]);
    }
    h.finish()
}

/// A member's units: its crate roots, and a finding on a blaze-edition line naming the
/// toolchain's crate.
fn member_units<'f>(name: Name, member: &layout::Member<'_>, text: &str, toolchain: blaze_toolchain::ToolchainId, frame: &'f Frame) -> Made<'f> {
    let mut made = Made::new_in(name, frame);
    let edition = syntax_of(member.edition.syntax).byte();
    for target in member.lib.iter().chain(member.bins.iter()) {
        let file = Name::file(target.root.as_bytes());
        if made.parts.iter().any(|part| part.name == file) {
            continue;
        }
        made.parts.push(Part { name: file, locate: Locate::File { edition }, place: 0 });
        made.path(file, target.root.as_bytes(), false);
    }
    made.parts.sort_unstable_by_key(|part| part.name);
    if member.lib.is_none() && member.bins.is_empty() {
        made.find(UNREAD, (member.span.start, member.span.end), &["the package has no library and no binary"]);
    }
    for dep in member.dependencies.iter() {
        if let Source::Member(_) = dep.source {
            continue;
        }
        if crate::graph::toolchain_line(member.edition.blaze.is_some(), dep.name.as_str(), dep.package.as_str()) {
            made.find(TOOLCHAIN_NAMED, whole_line(text, dep.span), &[dep.name.as_str()]);
        }
    }
    // What reads the units sees what each crate is given with nothing in the manifest, by the
    // one rule (blaze-unit-graph `implicit`), and from which toolchain: the toolchain's id is in
    // this record's output and in no other key (toolchain.md, section 1).
    let mut out = blaze_ids::Hasher::new(UNITS_OUT_DOMAIN);
    out.update(&parts_out(&made.parts).0).update(&toolchain.0);
    let kinds = member.lib.iter().map(|lib| if lib.proc_macro { UnitKind::ProcMacro } else { UnitKind::Lib }).chain(member.bins.iter().map(|_| UnitKind::Lib));
    for kind in kinds {
        let given = implicit(member.edition, &kind);
        let std = match given.std {
            StdIs::Rust => 0,
            StdIs::Bstd(blaze) => blaze as u8 + 1,
        };
        out.update(&[std, u8::from(given.proc_macro)]).update(given.prelude.module().as_bytes());
    }
    made.out = out.finish();
    made
}

/// The lines `span` is on, from the first's start through the last's newline (or the text's
/// end): what deleting the dependency removes.
fn whole_line(text: &str, span: Span) -> (u32, u32) {
    let bytes = text.as_bytes();
    let start = (span.start as usize).min(bytes.len());
    let end = (span.end as usize).clamp(start, bytes.len());
    let first = bytes[..start].iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
    let last = bytes[end..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |at| end + at + 1);
    (first as u32, last as u32)
}

/// The outline's edition of a Rust edition.
fn syntax_of(edition: edition::Edition) -> Edition {
    match edition {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

/// The units task `task`: the root's or a member's, from the request's layout.
pub fn run<'f>(laid: &Laid<'_>, task: &Task<'_>, frame: &'f Frame) -> Result<Did<'f>, CliError> {
    if task.name == Name::units() {
        return Ok(Did::Done(laid.root.clone_in(frame)));
    }
    match laid.members.iter().find(|(name, _)| *name == task.name) {
        Some((_, made)) => Ok(Did::Done(made.clone_in(frame))),
        None => Err(CliError::Text { what: "a units task of a manifest the workspace's layout does not list" }),
    }
}

impl<A: core::alloc::Allocator + Copy> crate::work::Record<A> {
    /// A finding of `rule` at `at`, its message the pieces in order.
    pub(crate) fn find(&mut self, rule: u16, at: (u32, u32), pieces: &[&str]) {
        let start = self.messages.len();
        for piece in pieces {
            self.messages.extend_from_slice(piece.as_bytes());
        }
        self.findings.push((rule, at.0, at.1, start, self.messages.len()));
    }
}

#[cfg(test)]
mod tests {
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::path::Path;
    use bstd::test::BUDGET;

    use super::*;
    use crate::sweep::FromMeta;

    /// The owner's /Volumes/Projects/tmp/hello-world/Cargo.toml, as written.
    const HELLO: &str = "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nbstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n";

    /// A package in the blaze edition whose dependency is by absolute path: its crate root is a
    /// part in the 2024 syntax with its path, and the dependency a finding on its key.
    #[test]
    fn a_blaze_edition_package_with_an_outside_dependency_is_read_and_the_dependency_named_on_its_key() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        root.publish(&cx, &Path::new("Cargo.toml").expect("a path"), Content::bytes(HELLO.as_bytes()), Durable::No).expect("write");
        root.publish(&cx, &Path::new("src/main.rs").expect("a path"), Content::bytes(b"fn main() {}"), Durable::No).expect("write");
        // A stale lock, as the owner's hello-world has: with nothing outside to reach, unread.
        root.publish(&cx, &Path::new("Cargo.lock").expect("a path"), Content::bytes(b"version = 4\n"), Durable::No).expect("write");
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), frame, erased.handle()).expect("laid");
        assert_eq!(laid.root.findings.len(), 0, "{:?}", core::str::from_utf8(&laid.root.messages));
        // The members' own lock stands in for it (graph.rs `members_lock`): the graph and the
        // names world are made from the toolchain and the member alone.
        let lock = laid.lock.as_ref().expect("the members' own lock");
        assert_ne!(lock.text, b"version = 4\n", "the stale file is never the lock");
        assert!(laid.parts.is_some(), "the member is matched to its own lock");
        let [(name, member)] = laid.members.as_slice() else { panic!("one member: {}", laid.members.len()) };
        assert_eq!(*name, Name::manifest(b"Cargo.toml"));
        assert_eq!(member.parts.as_slice(), [Part { name: Name::file(b"src/main.rs"), locate: Locate::File { edition: Edition::E2024.byte() }, place: 0 }]);
        let [(rule, start, end, at, to)] = member.findings.as_slice() else { panic!("one finding") };
        // Under the blaze edition the line naming bstd is the toolchain's crate named again: the
        // finding is the whole line with its newline, which its fix deletes.
        let line = "bstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n";
        assert_eq!((*rule, &HELLO[*start as usize..*end as usize], &member.messages[*at..*to]), (TOOLCHAIN_NAMED, line, &b"bstd"[..]));
        let mut fixed = std::string::String::from(&HELLO[..*start as usize]);
        fixed.push_str(&HELLO[*end as usize..]);
        assert_eq!(fixed, "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\n");
        // What it read: the manifest, and the listing of `src`.
        let read: std::vec::Vec<&str> = rows.rows.iter().map(|leaf| rows.path(leaf)).collect();
        assert!(read.contains(&"Cargo.toml") && read.contains(&"src"), "{read:?}");
        assert!(!read.contains(&"Cargo.lock"), "{read:?}");
    }

    /// Control: the same line in a Rust-edition package is an ordinary dependency, not the
    /// toolchain's crate named again.
    #[test]
    fn a_rust_edition_package_naming_bstd_keeps_its_line() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        let manifest = HELLO.replace("blaze-2026.11", "2024");
        root.publish(&cx, &Path::new("Cargo.toml").expect("a path"), Content::bytes(manifest.as_bytes()), Durable::No).expect("write");
        root.publish(&cx, &Path::new("src/main.rs").expect("a path"), Content::bytes(b"fn main() {}"), Durable::No).expect("write");
        root.publish(&cx, &Path::new("Cargo.lock").expect("a path"), Content::bytes(b"version = 4\n"), Durable::No).expect("write");
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), frame, erased.handle()).expect("laid");
        let [(_, member)] = laid.members.as_slice() else { panic!("one member") };
        assert!(member.findings.is_empty());
        // An outside dependency is the unit-graph job's to read: nothing is said of it here, but
        // the lock is read (the control of the stale lock left unread above).
        assert!(laid.root.findings.iter().all(|finding| finding.0 == crate::fetch::LOCK_UNREAD), "{:?}", core::str::from_utf8(&laid.root.messages));
        let read: std::vec::Vec<&str> = rows.rows.iter().map(|leaf| rows.path(leaf)).collect();
        assert!(read.contains(&"Cargo.lock"), "{read:?}");
    }

    /// A workspace whose members name outside dependencies lays with no finding: reading them
    /// is the unit-graph job's (finding 10, `dependencies-not-read`, is retired).
    #[test]
    fn outside_dependencies_are_no_finding_of_the_layout() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        let put = |path: &str, text: &str| {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        };
        put("Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.dependencies]\nserde = \"1\"\n");
        put("a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\ntokio = \"1\"\nzstd = \"1\"\n");
        put("b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\nserde = \"1\"\nanyhow = \"1\"\nbytes = \"1\"\n");
        put("a/src/lib.rs", "");
        put("b/src/lib.rs", "");
        let erased = blaze_own::Erased::new(frame);
        let mut rows = Rows::new_in(frame);
        let laid = lay(&cx, &root, &FromMeta { swept_at_ns: 0 }, &mut rows, blaze_toolchain::ToolchainId([0; 32]), frame, erased.handle()).expect("laid");
        assert!(laid.members.iter().all(|(_, member)| member.findings.is_empty()));
        assert!(laid.root.findings.is_empty(), "{:?}", core::str::from_utf8(&laid.root.messages));
    }
}
