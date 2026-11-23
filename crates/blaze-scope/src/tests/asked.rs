//! The read list at question grain ([`crate::asked`]): a kept solve is found again only when
//! every question it asked of another crate is answered the same, and what is found equals a
//! cold solve of the edited graph byte for byte. Property: a random small graph of crates, a
//! random edit of one crate's exports (a name added, removed or renamed; a name a dependent's
//! glob now imports; a name that makes two globs ambiguous; a glob edge added), re-solved with
//! reuse against cold. Planted: each kind of read left out of the list is caught.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use super::entry::{Texts, input};
use crate::asked::plant::{PLANT, Plant};
use crate::crate_scope::{Kept, Ready, Solved, resolve_by_records, resolve_kept};
use crate::graph::Sysroot;
use crate::resolve::CrateInput;
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const NAMES: [&str; 3] = ["x", "y", "z"];
const CRATES: [&str; 4] = ["k0", "k1", "k2", "k3"];

/// What crate `dep` gives a dependent's module `u{dep}`: globs of its `g` and `h`, a named
/// import, and probes `pub use self::n as pn` that look `n` up through the globs.
#[derive(Clone, Debug)]
struct Use {
    dep: usize,
    glob_g: bool,
    glob_h: bool,
    named: Option<usize>,
    probes: [bool; 3],
}

#[derive(Clone, Debug)]
pub(super) struct Spec {
    g: [bool; 3],
    h: [bool; 3],
    g_globs_h: bool,
    uses: Vec<Use>,
}

fn names(tape: &Tape, label: &'static str) -> [bool; 3] {
    [tape.bool(label), tape.bool(label), tape.bool(label)]
}

pub(super) fn draw(tape: &Tape) -> Vec<Spec> {
    let n = 2 + tape.index(3, "crates");
    let mut specs = Vec::new();
    for i in 0..n {
        let mut uses = Vec::new();
        for dep in 0..i {
            if tape.bool("depends") {
                let named = tape.bool("named").then(|| tape.index(3, "named name"));
                uses.push(Use { dep, glob_g: tape.bool("glob g"), glob_h: tape.bool("glob h"), named, probes: names(tape, "probe") });
            }
        }
        specs.push(Spec { g: names(tape, "g"), h: names(tape, "h"), g_globs_h: tape.bool("g globs h"), uses });
    }
    specs
}

/// One edit of crate `e`'s exports.
fn edit(tape: &Tape, specs: &mut [Spec]) -> String {
    let e = tape.index(specs.len(), "edited crate");
    let s = &mut specs[e];
    let i = tape.index(3, "edited name");
    match tape.index(4, "edit kind") {
        0 => {
            s.g[i] = !s.g[i];
            format!("k{e}: g's `{}` flipped", NAMES[i])
        }
        1 => {
            s.h[i] = !s.h[i];
            format!("k{e}: h's `{}` flipped (a glob of h sees it)", NAMES[i])
        }
        2 => {
            s.g_globs_h = !s.g_globs_h;
            format!("k{e}: g's glob of h flipped")
        }
        _ => {
            let to = (i + 1) % 3;
            s.g.swap(i, to);
            format!("k{e}: g's `{}` and `{}` swapped (a rename)", NAMES[i], NAMES[to])
        }
    }
}

fn leak(s: String) -> &'static str {
    std::boxed::Box::leak(s.into_boxed_str())
}

fn text(spec: &Spec) -> &'static str {
    let mut t = String::from("#![no_core]\npub mod g {\n");
    for (i, n) in NAMES.iter().enumerate() {
        if spec.g[i] {
            t += &format!("    pub fn {n}() {{}}\n");
        }
    }
    if spec.g_globs_h {
        t += "    pub use super::h::*;\n";
    }
    t += "}\npub mod h {\n";
    for (i, n) in NAMES.iter().enumerate() {
        if spec.h[i] {
            t += &format!("    pub fn {n}() {{}}\n");
        }
    }
    t += "}\n";
    for u in &spec.uses {
        let d = CRATES[u.dep];
        t += &format!("pub mod u{} {{\n", u.dep);
        if u.glob_g {
            t += &format!("    pub use {d}::g::*;\n");
        }
        if u.glob_h {
            t += &format!("    pub use {d}::h::*;\n");
        }
        if let Some(n) = u.named {
            t += &format!("    pub use {d}::g::{} as named;\n", NAMES[n]);
        }
        for (i, n) in NAMES.iter().enumerate() {
            if u.probes[i] {
                t += &format!("    pub use self::{n} as p{n};\n");
            }
        }
        t += "}\n";
    }
    leak(t)
}

pub(super) fn world(specs: &[Spec]) -> (&'static Texts, Vec<CrateInput<'static>>) {
    let mut files = Vec::new();
    let mut inputs = Vec::new();
    for (i, s) in specs.iter().enumerate() {
        let path = leak(format!("/{}/lib.rs", CRATES[i]));
        files.push(&*Vec::leak(std::vec![(path, text(s))]));
        let deps: Vec<(&'static str, usize)> = s.uses.iter().map(|u| (CRATES[u.dep], u.dep)).collect();
        inputs.push(input(CRATES[i], path, Vec::leak(deps)));
    }
    (std::boxed::Box::leak(std::boxed::Box::new(Texts(Vec::leak(files)))), inputs)
}

/// Kept solves in memory, found when the crate's one file is unchanged (the read list is
/// checked by the queue).
struct Mem {
    now: std::sync::Mutex<&'static Texts>,
    kept: std::sync::Mutex<Vec<Option<(&'static str, &'static [u8], &'static [u8], &'static [u8])>>>,
}

impl Kept for Mem {
    fn find(&self, krate: usize) -> Option<(&[u8], &[u8], &[u8])> {
        let (text, record, inner, read) = self.kept.lock().unwrap_or_else(|p| panic!("kept: {p}"))[krate]?;
        let now = *self.now.lock().unwrap_or_else(|p| panic!("now: {p}"));
        (now.0[krate][0].1 == text).then_some((record, inner, read))
    }

    fn made(&self, krate: usize, record: &[u8], inner: &[u8], read: &[u8]) {
        let now = *self.now.lock().unwrap_or_else(|p| panic!("now: {p}"));
        let solve = (now.0[krate][0].1, &*Vec::leak(record.to_vec()), &*Vec::leak(inner.to_vec()), &*Vec::leak(read.to_vec()));
        self.kept.lock().unwrap_or_else(|p| panic!("kept: {p}"))[krate] = Some(solve);
    }
}

fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let g = erased.bump();
    let mut specs = draw(tape);
    let (before, inputs) = world(&specs);
    let what = edit(tape, &mut specs);
    let (after, _) = world(&specs);
    let kept = Mem { now: std::sync::Mutex::new(before), kept: std::sync::Mutex::new(std::vec![None; inputs.len()]) };
    PLANT.with(|p| p.set(plant));
    let first = resolve_kept(&inputs, Sysroot::default(), before, &Serial, &Ready::serial(WAIT), &kept, g);
    PLANT.with(|p| p.set(Plant::None));
    first.map_err(|e| Broke::Harness(format!("first run: {e:?}")))?;
    *kept.now.lock().unwrap_or_else(|p| panic!("now: {p}")) = after;
    let (again, _) = resolve_kept(&inputs, Sysroot::default(), after, &Serial, &Ready::serial(WAIT), &kept, g).map_err(|e| Broke::Harness(format!("{what}: {e:?}")))?;
    let (cold, _) = resolve_by_records(&inputs, Sysroot::default(), after, &Serial, &Ready::serial(WAIT), g).map_err(|e| Broke::Harness(format!("{what} cold: {e:?}")))?;
    for (i, (a, c)) in again.iter().zip(cold.iter()).enumerate() {
        let (record, inner, found) = match a {
            Solved::Solved(s) => (&s.record[..], &s.inner[..], false),
            Solved::Found { record, inner } => (*record, *inner, true),
        };
        if record != &c.record[..] || inner != &c.inner[..] {
            return Err(Broke::Oracle(format!("{what}: crate `k{i}` (found {found}) differs from a cold solve\n{}", after.0[i][0].1)));
        }
    }
    Ok(())
}

/// The property: whatever the edit, reuse equals cold.
#[test]
fn kept_solves_at_question_grain_equal_a_cold_solve() {
    blaze_sim::check(Config::cases(300), |tape| case(tape, Plant::None));
}

/// Planted: a named read that found a binding is left out of the list.
#[test]
fn forgetting_a_found_name_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::ForgetFound, "differs from a cold solve", case);
}

/// Planted: a named read that found nothing is left out (a name that appears later).
#[test]
fn forgetting_a_failed_lookup_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::ForgetFailed, "differs from a cold solve", case);
}

/// Planted: a read of a module's glob edges is left out (a glob edge added later).
#[test]
fn forgetting_a_glob_read_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::ForgetGlobs, "differs from a cold solve", case);
}
