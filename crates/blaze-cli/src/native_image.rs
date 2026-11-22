//! The pieces of a program's native image that the engine's `run` and `test` (run.md D147, piece
//! R2) take from here, one implementation each (rule 30): the program's name ([`program_name`]),
//! its crates in link order ([`closure`]), the platform ([`platform`]), a record's payload in
//! place ([`payload_in_place`]) and a `Script` payload opened as outputs ([`script_outputs`]).
//! The image itself is made by engine_native.rs `image_for`, from the engine's names and Script
//! records, keyed by its inputs (blaze-ladder native_links.rs `program_key`, `program_image`).

use blaze_drive::record::Stored;
use blaze_drive::{Input, Kind, Name};
use blaze_ids::ContentId;
use blaze_ladder::native_links::Platform;
use blaze_own::Vec;
use blaze_world::Side;
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::store::Store;
use crate::work::InputWhy;

/// The domain of a program's identity in its `Native` name: the checkout-relative path of the
/// program's crate root.
const PROGRAM_DOMAIN: &str = "blaze-cli the program a run names 1";

/// The profile every `blaze run` builds today (the unit graph's): the third part of the name.
const PROFILE: &[u8] = b"dev";

/// The `Native` name of the program whose crate root is `root`, a path in the checkout (the
/// crate list's `/0/` taken off: `src/main.rs`), for this build's target and [`PROFILE`]. Made
/// from text before the work (rule 65): the program is named with it before the layout is read,
/// and the image's row is found by the same function (engine_native.rs).
pub(crate) fn program_name(root: &str) -> Name {
    Name::native(&ContentId::of(PROGRAM_DOMAIN, root.as_bytes()), crate::graph::TRIPLE.as_bytes(), PROFILE)
}

/// The image's platform for this build's own target ([`crate::graph::TRIPLE`]); `None` for a
/// target blaze-native writes no image for.
pub(crate) fn platform() -> Option<Platform> {
    match crate::graph::TRIPLE {
        "aarch64-apple-darwin" => Some(Platform::MacosArm64),
        "aarch64-unknown-linux-gnu" => Some(Platform::LinuxArm64),
        _ => None,
    }
}

/// The payload of the record `input` names, read in place from `store` (the lent's bytes for the
/// whole command): nothing is copied. Absent, a shape or not a record: the input's typed error.
pub(crate) fn payload_in_place<'m>(cx: &Cx, store: &mut dyn Store<'m>, input: &Input) -> Result<&'m [u8], CliError> {
    let Some(held) = store.get(cx, input.at)? else {
        return Err(CliError::Input { name: input.name, why: InputWhy::Absent });
    };
    match blaze_drive::record::open(held.bytes) {
        Ok(Stored::Whole(view)) => Ok(view.payload),
        Ok(Stored::Shape(_)) => Err(CliError::Input { name: input.name, why: InputWhy::Shape }),
        Err(error) => Err(CliError::Input { name: input.name, why: InputWhy::Malformed(error) }),
    }
}

/// Whether row `r` is linked into a program that depends on it: a target-side library, never a
/// proc-macro crate or a crate built for the macro side.
pub(crate) fn linked(list: &CrateList<'_>, r: usize) -> bool {
    matches!(list.side(r), Side::Target) && !list.proc_macro(r)
}

/// The program's crates from `root`, the program's crate first and each crate before every crate
/// it depends on (reverse topological, as `program_image` takes them): a post-order walk of the
/// dependencies, reversed. A crate outside the toolchain reaches a sysroot crate only when the
/// program's names reached it (`handed`: the sorted keys of the closure the names gave, run_names.rs
/// `Found::closure_in`): a `#![no_core]` crate links no `core` or `std`, a `#![no_std]` one no
/// `std` (Reference, names.preludes.extern.no_std). Its declared dependencies are walked whether
/// handed or not, so one not handed is refused by the caller (engine_native.rs `NotHanded`).
pub(crate) fn closure<'s>(list: &CrateList<'_>, root: usize, handed: &[[u8; 32]], scratch: &'s Frame) -> Vec<usize, &'s Frame> {
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
        let reached = sysroot.into_iter().flatten().filter(|s| handed.binary_search(&list.key(*s).0.0).is_ok());
        for d in list.deps(r).chain(reached) {
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

/// Why a crate's `Script` payload is not a build script's outputs.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ScriptOpen {
    /// Its first byte says no run (scripts.rs `payload_parts`).
    NotARun,
    /// Its outputs record does not open (blaze-build-script outputs.rs `OutputsRef::open`).
    Outputs(blaze_build_script::outputs::RecordError),
}

/// A `Script` record's payload opened as its run's outputs, with the home's file area its stored
/// files (a native script's archives) are mapped from when the link reads them, never read into
/// memory here. The caller holds the area for as long as it reads the outputs
/// (`OutputsRef::with_area`). The one opener of the image's scripts (engine_native.rs).
pub(crate) fn script_outputs<'b>(payload: &'b [u8], home: Option<&str>) -> Result<(blaze_build_script::outputs::OutputsRef<'b>, blaze_world::StoredFiles), ScriptOpen> {
    let Some((_, record)) = crate::scripts::payload_parts(payload) else {
        return Err(ScriptOpen::NotARun);
    };
    let opened = blaze_build_script::outputs::OutputsRef::open(record).map_err(ScriptOpen::Outputs)?;
    let area = crate::scripts::file_area(home, &opened);
    Ok((opened, area))
}
