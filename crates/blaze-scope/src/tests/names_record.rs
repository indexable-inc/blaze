//! The full-names record ([`crate::names_record`]) against a fresh solve, question for question:
//! a random small graph of crates (tests/asked.rs's: globs, named imports, re-export probes),
//! solved; each crate's names record written and read back on demand; every path of a fixed
//! family asked from every module, in both namespaces, of the solved crates' `Names` and of the
//! records' `RecordNames`: the answers must be equal. Planted: one scope's entry dropped, one
//! visibility flipped, in the encoder; each must be caught.

use std::format;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_surface::{Namespace, PathKind};

use super::asked::{draw, world};
use crate::crate_scope::{Ready, resolve_by_records};
use crate::graph::Sysroot;
use crate::names::{Written, path};
use crate::names_record::plant::{PLANT, Plant};
use crate::names_record::{LoadNames, RecordNames, encode};
use crate::resolve::{CrateNames, Names, aligned};
use crate::task::Serial;

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const CRATES: [&str; 4] = ["k0", "k1", "k2", "k3"];
const MODULES: [&str; 5] = ["g", "h", "u0", "u1", "u2"];
const LEAVES: [&str; 8] = ["x", "y", "z", "px", "py", "pz", "named", "g"];

struct Loaded(Vec<&'static [u8]>);

impl LoadNames<'static> for Loaded {
    fn load(&self, krate: usize) -> Option<&'static [u8]> {
        self.0.get(krate).copied()
    }
}

/// Every path of the family: `n`, `a::n`, `crate::a::n`, `k::a::n` and `k::a`.
fn paths() -> Vec<(PathKind, Vec<&'static str>)> {
    let mut all = Vec::new();
    for n in LEAVES {
        all.push((PathKind::Plain, std::vec![n]));
        for a in MODULES {
            all.push((PathKind::Plain, std::vec![a, n]));
            all.push((PathKind::Crate, std::vec![a, n]));
            for k in CRATES {
                all.push((PathKind::Plain, std::vec![k, a, n]));
            }
        }
    }
    for k in CRATES {
        for a in MODULES {
            all.push((PathKind::Plain, std::vec![k, a]));
        }
    }
    all
}

fn case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let bump = super::SyncFrame::leaked();
    let g = bump.alloc();
    let specs = draw(tape);
    let (texts, inputs) = world(&specs);
    let (solved, _) = resolve_by_records(&inputs, Sysroot::default(), texts, &Serial, &Ready::serial(WAIT), bump).map_err(|e| Broke::Harness(format!("solve: {e:?}")))?;
    let all: Vec<&'static CrateNames<'static>> = solved.iter().map(|s| s.names).collect();
    let fresh = Names::of_crates(&inputs, Sysroot::default(), &all, g).map_err(|e| Broke::Harness(format!("names: {e:?}")))?;
    PLANT.with(|p| p.set(plant));
    let records: Result<Vec<&'static [u8]>, _> = all.iter().map(|n| encode(n, g).map(|b| aligned(&b, bump))).collect();
    PLANT.with(|p| p.set(Plant::None));
    let records = records.map_err(|e| Broke::Harness(format!("encode: {e:?}")))?;
    let keys: Vec<CrateItemKey> = all.iter().map(|n| n.key).collect();
    let read = RecordNames::new(&keys, Loaded(records), g);
    let mut modules: Vec<ItemKey> = all.iter().flat_map(|n| n.chains.keys().copied()).collect();
    modules.sort_unstable();
    let family = paths();
    for &m in modules.iter() {
        for ns in [Namespace::Types, Namespace::Values] {
            for (kind, segments) in family.iter() {
                let written = Written { kind: *kind, segments: segments.as_slice() };
                let a = path(&fresh, m, ns, written, g);
                let b = path(&read, m, ns, written, g);
                if a != b {
                    return Err(Broke::Oracle(format!(
                        "the names record answers differently: `{kind:?} {}` ({ns:?}) in module {m:?}: fresh {a:?}, record {b:?}",
                        segments.join("::")
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The property: the record's answers are a fresh solve's.
#[test]
fn the_names_record_answers_as_a_fresh_solve() {
    blaze_sim::check(Config::cases(300), |tape| case(tape, Plant::None));
}

/// Planted: one scope's first entry left out of the record.
#[test]
fn a_dropped_scope_entry_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::DropEntry, "answers differently", case);
}

/// Planted: one binding's visibility flipped in the record.
#[test]
fn a_flipped_visibility_is_caught() {
    blaze_sim::control(Config::cases(300), Plant::None, Plant::FlipVis, "answers differently", case);
}
