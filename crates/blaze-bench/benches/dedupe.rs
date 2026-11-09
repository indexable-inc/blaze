//! Codegen's identical code folding, `blaze_codegen::dedupe` (docs/v2/plan/design/icf.md),
//! on a build shaped like a generic-heavy crate:
//!
//! - `dedupe/generic_chains`: [`BODIES`] generic bodies of [`INSTANCES`] instances each, the
//!   instances of a body falling into [`CLASSES`] classes of equal code (`Vec<u32>::push` and
//!   `Vec<i32>::push` are one, `Vec<u64>::push` another), and each body calling the one before
//!   it in runs of [`RUN`], so a call to a folded callee makes its callers equal one round
//!   later: [`RUN`] + 1 rounds. 1 in [`EXPORTED`] functions is exported and never folds. An
//!   iteration copies the build and folds it; the copy is the same on both sides of a pair.
//!
//! The fold is the only call into `blaze_codegen`, in [`fold`], so that the binary of a base
//! whose `dedupe` takes other arguments is this file with that one line changed.

use std::hint::black_box;

use blaze_bench::{IntoBenchmarks, benchmark_fn};
use blaze_codegen::{Arch, CompiledFunction, InstanceKey, RelocKind, Relocation, Target};
use blaze_ids::ContentId;

const BODIES: usize = 4096;
const INSTANCES: usize = 8;
const CLASSES: usize = 3;
const RUN: usize = 8;
const EXPORTED: usize = 64;

fn key(body: usize, instance: usize) -> InstanceKey {
    let n = (body * INSTANCES + instance) as u64;
    InstanceKey(ContentId::of("blaze-bench dedupe key 1", &n.to_le_bytes()))
}

/// Instance `instance` of `body`: 48 bytes of code saying its body and class, and a call to
/// the same instance of the body before it unless it starts a run.
fn function(body: usize, instance: usize) -> CompiledFunction {
    let class = (instance % CLASSES) as u8;
    let mut code = vec![0x90; 48];
    code[..4].copy_from_slice(&(body as u32).to_le_bytes());
    code[4] = class;
    let relocs = match body % RUN {
        0 => Vec::new(),
        _ => vec![Relocation {
            at: 8,
            kind: RelocKind::PcRel32,
            target: Target::Direct(key(body - 1, instance)),
            addend: -4,
        }],
    };
    CompiledFunction {
        key: key(body, instance),
        arch: Arch::X86_64,
        align: 16,
        code,
        relocs: relocs.into_iter().collect(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

fn build() -> Vec<CompiledFunction> {
    (0..BODIES)
        .flat_map(|body| (0..INSTANCES).map(move |instance| function(body, instance)))
        .collect()
}

fn exported(key: &InstanceKey) -> bool {
    key.0.0[0] as usize % EXPORTED == 0
}

/// Folds `functions`: the number merged away.
fn fold(functions: &mut Vec<CompiledFunction>) -> usize {
    blaze_codegen::dedupe::dedupe(functions, &mut [], |it| !exported(it))
        .expect("a fixpoint")
        .0
        .merged
        .len()
}

fn benchmarks() -> impl IntoBenchmarks {
    [benchmark_fn("dedupe/generic_chains", |b| {
        let functions = build();
        b.iter(move || {
            let mut functions = functions.clone();
            fold(black_box(&mut functions))
        })
    })]
}

blaze_bench::main!(benchmarks());
