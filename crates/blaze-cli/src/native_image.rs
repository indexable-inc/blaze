//! The `Links` and `Native` tasks (run.md, "What a run takes from the request, and the native
//! image as a task"; piece native-drive-kinds), the command's half of blaze-ladder
//! native_links.rs:
//!
//! - `Links`: a crate's link table, the `links` section of its `CrateScope` record (names/
//!   sections.rs), filed by that crate's `CrateScope` job. The task's home is that `CrateScope`;
//!   its source is the table's digest (`table_digest`), which the scope record's `Links` part
//!   named, so the record's out is its source and it reads nothing.
//! - `Native`: the program's native image. Its home is the unit graph; it walks the crate list
//!   from the program's crate (the root row whose [`program_name`] is the task's name), wants
//!   each closure crate's `Links` and `Script`, and makes the image from them. Its out is the
//!   image's key (`program_key`), its payload the image (`program_image`), and its reads the
//!   unit graph's out and every `Links` and `Script` out it took: the name says which program
//!   (key.rs `Name::native`), the read list says what it links.

use blaze_drive::record::Stored;
use blaze_drive::{Input, Kind, Name, Task, TaskFailure, Want, Why};
use blaze_ids::ContentId;
use blaze_ids::producer::{ProducerId, Whole};
use blaze_ladder::native_links::{self, Platform, ProgramCrate, Table};
use blaze_own::{AVec, Alloc, Vec};
use blaze_world::Side;
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::store::Store;
use crate::work::{Did, InputWhy, Made};

/// The rule a refusal here names.
const RULE: &str = "native_image.rs";

/// The domain of a program's identity in its `Native` name: the checkout-relative path of the
/// program's crate root.
const PROGRAM_DOMAIN: &str = "blaze-cli the program a run names 1";

/// The profile every `blaze run` builds today (the unit graph's): the third part of the name.
const PROFILE: &[u8] = b"dev";

/// The `Native` name of the program whose crate root is `root`, a path in the checkout (the
/// crate list's `/0/` taken off: `src/main.rs`), for this build's target and [`PROFILE`]. Made
/// from text before the work (rule 65): `main.rs` names the program with it before the layout
/// is read, and the worker finds the row it names by the same function.
pub(crate) fn program_name(root: &str) -> Name {
    Name::native(&ContentId::of(PROGRAM_DOMAIN, root.as_bytes()), crate::graph::TRIPLE.as_bytes(), PROFILE)
}

/// The image's platform for this build's own target ([`crate::graph::TRIPLE`]); `None` for a
/// target blaze-native writes no image for.
fn platform() -> Option<Platform> {
    match crate::graph::TRIPLE {
        "aarch64-apple-darwin" => Some(Platform::MacosArm64),
        "aarch64-unknown-linux-gnu" => Some(Platform::LinuxArm64),
        _ => None,
    }
}

fn refused<'b>(task: &Task<'_>, what: &'static str) -> Did<'b> {
    Did::Failed(TaskFailure { name: task.name, why: Why::Refused { what, rule: RULE } })
}

fn waited<'b>(task: &Task<'_>, on: Name) -> Did<'b> {
    Did::Failed(TaskFailure { name: task.name, why: Why::Waited { on } })
}

/// The payload of the record `input` names, read in place from `store` (the lent's bytes for the
/// whole command): nothing is copied. Absent, a shape or not a record: the input's typed error.
fn payload_in_place<'m>(cx: &Cx, store: &mut dyn Store<'m>, input: &Input) -> Result<&'m [u8], CliError> {
    let Some(held) = store.get(cx, input.at)? else {
        return Err(CliError::Input { name: input.name, why: InputWhy::Absent });
    };
    match blaze_drive::record::open(held.bytes) {
        Ok(Stored::Whole(view)) => Ok(view.payload),
        Ok(Stored::Shape(_)) => Err(CliError::Input { name: input.name, why: InputWhy::Shape }),
        Err(error) => Err(CliError::Input { name: input.name, why: InputWhy::Malformed(error) }),
    }
}

/// A `Links` task: its crate's link table, as that crate's `CrateScope` record filed it.
pub(crate) fn links<'b>(cx: &Cx, store: &mut dyn Store<'_>, task: &Task<'_>, held: &'b Frame) -> Result<Did<'b>, CliError> {
    let cx = cx.span("links: the crate's link table");
    if task.home.kind != Kind::CrateScope || Name::links_of(task.home) != task.base {
        return Ok(refused(task, "a `Links` task whose home is not its crate's `CrateScope`"));
    }
    let Some(scope) = task.inputs.iter().find(|input| input.name == task.home) else {
        if task.failed.contains(&task.home) {
            return Ok(waited(task, task.home));
        }
        let mut wants = Vec::with_capacity_in(1, held);
        wants.push(Want { name: task.home, home: Name::unit_graph() });
        return Ok(Did::Need(wants));
    };
    if scope.out == crate::work::failed_out() {
        return Ok(waited(task, scope.name));
    }
    let payload = payload_in_place(&cx, store, scope)?;
    // A crate refused before its names were resolved files no sections, and its record lists
    // no `Links` part: a want of one is the drive's bug, refused here by name.
    if payload.is_empty() {
        return Ok(refused(task, "its crate's `CrateScope` record holds no sections (its names were not resolved), so it has no link table"));
    }
    let table = crate::names::sections::sections(payload)
        .map_err(|_| CliError::Text { what: "a crate's `CrateScope` payload, read for its link table, does not open (native_image.rs `links`)" })?
        .links;
    if native_links::table_digest(table) != task.source {
        return Ok(refused(task, "the link table its crate's `CrateScope` record holds is not the one that record's `Links` part names"));
    }
    let mut made = Made::new_in(task.name, held);
    made.payload.extend_from_slice(table);
    made.out = task.source;
    #[cfg(test)]
    watch_links(task.name, made.payload_bytes()?, payload);
    Ok(Did::Done(made))
}

/// Whether row `r` is linked into a program that depends on it: a target-side library, never a
/// proc-macro crate or a crate built for the macro side.
fn linked(list: &CrateList<'_>, r: usize) -> bool {
    matches!(list.side(r), Side::Target) && !list.proc_macro(r)
}

/// The program's crates from `root`, the program's crate first and each crate before every crate
/// it depends on (reverse topological, as `program_image` takes them): a post-order walk of the
/// dependencies, each crate outside the toolchain reaching the sysroot's crates too, reversed.
fn closure<'s>(list: &CrateList<'_>, root: usize, scratch: &'s Frame) -> Vec<usize, &'s Frame> {
    let n = list.len();
    let mut seen = Vec::with_capacity_in(n, scratch);
    for _ in 0..n {
        seen.push(false);
    }
    let mut order = Vec::with_capacity_in(n, scratch);
    let mut stack = Vec::with_capacity_in(n, scratch);
    stack.push((root, false));
    while let Some((r, finished)) = stack.pop() {
        if finished {
            order.push(r);
            continue;
        }
        if seen[r] {
            continue;
        }
        seen[r] = true;
        stack.push((r, true));
        let sysroot = if list.toolchain(r) { [None; 4] } else { list.sysroot() };
        for d in list.deps(r).chain(sysroot.into_iter().flatten()) {
            if !seen[d] && linked(list, d) {
                stack.push((d, false));
            }
        }
    }
    order.reverse();
    order
}

/// The `Script` name a crate list row's run is (`CrateList::script`: its kind byte, then its
/// subject).
pub(crate) fn script_name(bytes: [u8; 33]) -> Option<Name> {
    let kind = Kind::from_u8(bytes[0])?;
    let mut subject = [0u8; 32];
    subject.copy_from_slice(&bytes[1..]);
    Some(Name::new(kind, ContentId(subject)))
}

/// One crate of the program, with the names its record is made from.
#[derive(Clone, Copy)]
struct Wanted {
    row: usize,
    links: Name,
    script: Option<Name>,
}

/// A `Native` task: the program's image. `home` (`HOME`, read at the edge) holds the file area
/// a native script's stored archives are mapped from (`scripts::FileArea`).
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn native<'b>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    producer: &ProducerId<Whole>,
    home: Option<&str>,
    task: &Task<'_>,
    scratch: &Frame,
    held: &'b Frame,
    alloc: Alloc<'_>,
) -> Result<Did<'b>, CliError> {
    let cx = cx.span("native: the program's image");
    let Some(platform) = platform() else {
        return Ok(refused(task, "a native image for a target other than arm64 macOS or arm64 Linux (blaze-native `Platform`)"));
    };
    let Some(graph) = task.inputs.iter().find(|input| input.name == Name::unit_graph()) else {
        if task.failed.contains(&Name::unit_graph()) {
            return Ok(waited(task, Name::unit_graph()));
        }
        let mut wants = Vec::with_capacity_in(1, held);
        wants.push(Want { name: Name::unit_graph(), home: Name::unit_graph() });
        return Ok(Did::Need(wants));
    };
    if let Some(on) = crate::work::failed_input(task) {
        return Ok(waited(task, on));
    }
    let read = crate::work::payload(&cx, store, graph, scratch)?;
    let sections = read.unit_graph(graph.name)?;
    let Some(list) = sections.crates().map_err(|error| CliError::Input { name: graph.name, why: InputWhy::Graph(error) })? else {
        return Ok(refused(task, "the unit graph made no world, so the program has no crates (the graph's record says why)"));
    };
    let Some(root) = (0..list.len()).find(|r| linked(&list, *r) && list.root(*r).strip_prefix("/0/").is_some_and(|path| program_name(path) == task.base)) else {
        return Ok(refused(task, "no crate of the unit graph is the program this run names (the checkout package's `src/main.rs`)"));
    };
    let order = closure(&list, root, scratch);
    let mut crates = Vec::with_capacity_in(order.len(), scratch);
    let mut wants = Vec::new_in(held);
    for r in order.iter().copied() {
        let scope = blaze_drive::key::crate_scope_name(&list.key(r));
        let links = Name::links_of(scope);
        let script = match list.script(r) {
            None => None,
            Some(bytes) => match script_name(bytes) {
                Some(name) => Some(name),
                None => return Ok(refused(task, "a crate list row names a build-script run of no kind (crate_list.rs `script`)")),
            },
        };
        for (name, home) in [(Some(links), scope), (script, Name::unit_graph())] {
            let Some(name) = name else { continue };
            if task.failed.contains(&name) {
                return Ok(waited(task, name));
            }
            if !task.inputs.iter().any(|input| input.name == name) {
                wants.push(Want { name, home });
            }
        }
        crates.push(Wanted { row: r, links, script });
    }
    if !wants.is_empty() {
        return Ok(Did::Need(wants));
    }
    if let Some(on) = crate::work::failed_input(task) {
        return Ok(waited(task, on));
    }
    let input = |name: Name| task.inputs.iter().find(|input| input.name == name).expect("every name wanted above is an input");

    // Each crate's table and its script's outputs, read in place from the store and opened.
    let mut tables = Vec::with_capacity_in(crates.len(), scratch);
    let mut outputs = Vec::with_capacity_in(crates.len(), scratch);
    for c in crates.iter() {
        let package = list.name(c.row);
        let bytes = payload_in_place(&cx, store, input(c.links))?;
        match native_links::open_table(bytes, alloc) {
            Ok(Table::Links(links)) => tables.push(links),
            Ok(Table::Refused(words)) => {
                let said = bstd::mem::format_in!(held, "the link table of `{package}` was refused: {words}");
                return Ok(Did::FailedSaying(
                    TaskFailure { name: task.name, why: Why::Refused { what: "a crate of the program whose link table was refused", rule: RULE } },
                    crate::work::held_str(said.as_str(), held),
                ));
            }
            Err(error) => {
                let said = bstd::mem::format_in!(held, "the link table of `{package}` does not open: {error:?}");
                return Ok(Did::FailedSaying(
                    TaskFailure { name: task.name, why: Why::Refused { what: "a crate's `Links` record that is not a link table", rule: RULE } },
                    crate::work::held_str(said.as_str(), held),
                ));
            }
        }
        match c.script {
            None => outputs.push(None),
            Some(script) => {
                let script = input(script);
                let bytes = payload_in_place(&cx, store, script)?;
                let Some((_, record)) = crate::scripts::payload_parts(bytes) else {
                    return Ok(refused(task, "a crate's `Script` record whose payload is not a build script's run (scripts.rs `payload_parts`)"));
                };
                match blaze_build_script::outputs::OutputsRef::open(record) {
                    // Its stored files (a native script's archives) mapped from the home's file
                    // area when the link reads them, never read into this task's memory.
                    Ok(opened) => {
                        // Held for the task in its scratch frame, as the store's mappings are.
                        let area: &blaze_world::StoredFiles = blaze_own::Box::leak(blaze_own::Box::new_in(crate::scripts::file_area(home, &opened), scratch));
                        outputs.push(Some((script.out, opened.with_area(area))));
                    }
                    Err(error) => {
                        let said = bstd::mem::format_in!(held, "the build script outputs of `{package}` do not open: {error:?}");
                        return Ok(Did::FailedSaying(
                            TaskFailure { name: task.name, why: Why::Refused { what: "a crate's build script outputs record that does not open", rule: RULE } },
                            crate::work::held_str(said.as_str(), held),
                        ));
                    }
                }
            }
        }
    }
    let mut program = Vec::with_capacity_in(crates.len(), scratch);
    for (at, c) in crates.iter().enumerate() {
        program.push(ProgramCrate { package: list.name(c.row), outputs: outputs[at], links: &tables[at] });
    }

    let key = match native_links::program_key(&program, platform, producer, alloc) {
        Ok(key) => key,
        Err(error) => {
            let said = bstd::mem::format_in!(held, "the program's native key: {error:?}");
            return Ok(Did::FailedSaying(
                TaskFailure { name: task.name, why: Why::Refused { what: "the program's link facts", rule: RULE } },
                crate::work::held_str(said.as_str(), held),
            ));
        }
    };
    let mut image = AVec::new_in(alloc);
    if let Err(error) = native_links::program_image(&program, platform, producer, blaze_hash::FixedState, &mut image, alloc) {
        let said = bstd::mem::format_in!(held, "the program's native image: {error:?}");
        return Ok(Did::FailedSaying(
            TaskFailure { name: task.name, why: Why::Refused { what: "the program's native image", rule: RULE } },
            crate::work::held_str(said.as_str(), held),
        ));
    }
    let mut made = Made::new_in(task.name, held);
    made.out = key;
    made.payload.extend_from_slice(&image);
    // What it read: the unit graph, then each crate's `Links` and `Script` in the program's
    // order. An input of an earlier read list that the closure no longer reaches is not listed.
    made.reads.push((graph.name, graph.out));
    for c in crates.iter() {
        let links = input(c.links);
        made.reads.push((links.name, links.out));
        if let Some(script) = c.script {
            let script = input(script);
            made.reads.push((script.name, script.out));
        }
    }
    #[cfg(test)]
    watch_native(&made.reads, crates.iter().map(|c| (list.name(c.row), c.links)));
    Ok(Did::Done(made))
}

/// Tests: every `Links` record made (its name, its payload, and the `CrateScope` payload it was
/// read from) and every `Native` record made (each package it linked with that package's `Links`
/// name, and its reads), appended, never cleared: tests running at once each find their own by
/// their packages' names ([`watch_links`], [`watch_native`]).
#[cfg(test)]
pub(crate) static WATCHED: blaze_core::lock::Lock<Watched> =
    blaze_core::lock::Lock::new("native_image.watched (tests)", Watched { links: std::vec::Vec::new(), natives: std::vec::Vec::new() });

#[cfg(test)]
pub(crate) struct Watched {
    pub links: std::vec::Vec<WatchedLinks>,
    pub natives: std::vec::Vec<WatchedNative>,
}

#[cfg(test)]
pub(crate) struct WatchedLinks {
    pub name: Name,
    pub payload: std::vec::Vec<u8>,
    pub scope: std::vec::Vec<u8>,
}

#[cfg(test)]
pub(crate) struct WatchedNative {
    pub packages: std::vec::Vec<(std::string::String, Name)>,
    pub reads: std::vec::Vec<(Name, ContentId)>,
}

#[cfg(test)]
fn watch_links(name: Name, payload: &[u8], scope: &[u8]) {
    WATCHED.lock().links.push(WatchedLinks { name, payload: payload.to_vec(), scope: scope.to_vec() });
}

#[cfg(test)]
fn watch_native<'p>(reads: &[(Name, ContentId)], packages: impl Iterator<Item = (&'p str, Name)>) {
    let packages = packages.map(|(package, links)| (std::string::String::from(package), links)).collect();
    WATCHED.lock().natives.push(WatchedNative { packages, reads: reads.to_vec() });
}
