//! Seeded random IR (stencil.md, "Tests"): from one seed, a function of a few hundred
//! instructions over every op of the stencil set that the generator can make free of UB
//! ([`program`], drawn from [`table`]), run three ways on a few seeded inputs: in
//! `cranelift-interpreter` (the reference: a function it can't run, or that traps, is a bug of
//! the generator), and as the stencil backend's code and Cranelift's for the host, linked and
//! called. The two backends' results must agree bit for bit, NaNs included; either against the
//! interpreter's, NaN for NaN. A failing seed is shrunk: the same seed at smaller sizes, down
//! to the smallest that still fails, which is what the failure shows.
//!
//! [`FIXED`] seeds run by default; `BLAZE_STENCIL_FUZZ=<count>` runs that many more after
//! them.
//!
//! What runs code runs only on the host's own variant (x86-64 Linux, AArch64 Linux, AArch64
//! macOS); the other host's CI covers its variant. That the generator is deterministic, that
//! Cranelift's verifier passes what it makes, that the interpreter runs it, and that both
//! backends compile it for every target run everywhere.

mod program;
mod table;

use std::{
    any::Any,
    env::VarError,
    panic::{self, AssertUnwindSafe},
};

use cranelift_codegen::{data_value::DataValue, ir, isa::TargetIsa, verify_function};

use self::program::{Features, Program};
use super::{
    host,
    interp::{self, Run},
    rng::Rng,
};
use crate::{Target, shape::Op};

/// Seeds of the default run: about a second of it.
const FIXED: u64 = 24;

/// How many seeds to run after [`FIXED`].
const ENV: &str = "BLAZE_STENCIL_FUZZ";

/// Inputs per function.
const INPUTS: u64 = 3;

/// Seeds per target of the tests that run everywhere.
const EVERYWHERE: u64 = 8;

/// Failing seeds after which a run stops shrinking and reports.
const FAILURES: usize = 8;

/// The size of seed `seed`'s function: 160 to 399 instructions.
fn size(seed: u64) -> usize {
    160 + Rng::new(seed ^ 0x5eed_0f_5122).below(240) as usize
}

/// The count of extra seeds, from [`ENV`]'s value.
fn extra(var: Result<String, VarError>) -> Result<u64, String> {
    match var {
        Err(VarError::NotPresent) => Ok(0),
        Err(VarError::NotUnicode(raw)) => Err(format!("{ENV}={raw:?} is not a count of seeds")),
        Ok(text) => text.trim().parse().map_err(|e| format!("{ENV}={text:?} is not a count of seeds: {e}")),
    }
}

/// The function of `seed` at `size`, or the generator's panic naming the seed.
fn generated(seed: u64, size: usize, target: &Target, isa: &dyn TargetIsa) -> Result<Program, String> {
    panic::catch_unwind(AssertUnwindSafe(|| program::generate(seed, size, target, isa))).map_err(|panic| {
        format!("seed {seed} at size {size}: the generator panics, a bug of the generator: {}", message(&*panic))
    })
}

/// What the interpreter returns for `func` on `args`: anything else is a bug of the generator.
fn interpret(func: &ir::Function, args: &[DataValue]) -> Result<Vec<DataValue>, String> {
    let bug = |why: String| format!("{why}, a bug of the generator\ninput: {args:?}");
    match interp::run(&[func], 0, args) {
        Ok(Run::Returned(values)) if values.len() == func.signature.returns.len() => Ok(values),
        Ok(Run::Returned(values)) => Err(bug(format!("the interpreter returns {} values", values.len()))),
        Ok(Run::Trapped(code)) => Err(bug(format!("the interpreter traps ({code:?})"))),
        Err(e) => Err(bug(e)),
    }
}

fn message(panic: &(dyn Any + Send)) -> String {
    match panic.downcast_ref::<&str>() {
        Some(text) => (*text).to_owned(),
        None => panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "a panic with no message".to_owned()),
    }
}

/// Runs seed `seed` at `size` three ways on the host: why it fails, if it does.
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
fn check(seed: u64, size: usize, target: &Target, isa: &dyn TargetIsa) -> Result<(), String> {
    use super::{exec::Exec, interp::Outcome};

    let program = generated(seed, size, target, isa)?;
    let func = &program.func;
    let cranelift = host::cranelift_caught(func, isa)
        .map_err(|e| format!("Cranelift refuses the function, a bug of the generator: {e}"))?;
    let code = match panic::catch_unwind(AssertUnwindSafe(|| crate::compile(func, target))) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => return Err(format!("the stencil backend refuses what Cranelift compiles: {e}")),
        Err(panic) => return Err(format!("the stencil backend panics: {}", message(&*panic))),
    };
    let tys: Vec<ir::Type> = program.sig.returns.iter().map(|ret| ret.value_type).collect();
    let run = || -> Result<(), String> {
        let stencil = Exec::new(&code, &program.sig);
        let reference_code = Exec::cranelift(&cranelift, &program.sig);
        for k in 0..INPUTS {
            let args = program::inputs(seed, k, &program.sig);
            let want = interpret(func, &args)?;
            let reference = Run::Returned(want.clone());
            let (ours, theirs) = (stencil.values_forked(0, &args), reference_code.values_forked(0, &args));
            let why = match (&ours, &theirs) {
                (Outcome::Returned(a), Outcome::Returned(b)) if !interp::same_all(&tys, a, b, interp::Nans::Exact) => {
                    Some("the stencil code's results are not Cranelift's, bit for bit".to_owned())
                }
                _ => None,
            }
            .or_else(|| interp::agrees(&tys, &reference, &theirs).err().map(|e| format!("Cranelift's code: {e}")))
            .or_else(|| interp::agrees(&tys, &reference, &ours).err().map(|e| format!("the stencil code: {e}")));
            if let Some(why) = why {
                return Err(format!(
                    "{why}\ninput {k}: {args:?}\ninterpreter: {want:?}\nCranelift:   {theirs:?}\nstencil:     {ours:?}"
                ));
            }
        }
        Ok(())
    };
    panic::catch_unwind(AssertUnwindSafe(run))
        .unwrap_or_else(|panic| Err(format!("linking or calling the code panics: {}", message(&*panic))))
}

/// The smallest size at which seed `seed`, failing at `size` with `why`, still fails, and how:
/// halving while it fails, then bisecting between the last size that passed and the last that
/// failed.
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
fn shrink(seed: u64, size: usize, why: String, target: &Target, isa: &dyn TargetIsa) -> (usize, String) {
    let mut smallest = (size, why);
    let mut passed = None;
    while smallest.0 > 0 && passed.is_none() {
        let half = smallest.0 / 2;
        match check(seed, half, target, isa) {
            Err(why) => smallest = (half, why),
            Ok(()) => passed = Some(half),
        }
    }
    if let Some(mut low) = passed {
        while smallest.0 - low > 1 {
            let mid = low + (smallest.0 - low) / 2;
            match check(seed, mid, target, isa) {
                Err(why) => smallest = (mid, why),
                Ok(()) => low = mid,
            }
        }
    }
    smallest
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
#[test]
fn random_functions_mean_the_same_in_every_backend() {
    let Some(target) = host::host() else {
        eprintln!("no target of the IR builder's is this host: nothing to run");
        return;
    };
    let isa = host::isa(&target);
    let extra = extra(std::env::var(ENV)).unwrap_or_else(|e| panic!("{e}"));
    let seeds = FIXED + extra;
    let mut failures = Vec::new();
    for seed in 0..seeds {
        let size = size(seed);
        let Err(why) = check(seed, size, &target, &*isa) else { continue };
        let (smallest, why) = shrink(seed, size, why, &target, &*isa);
        let clif = match generated(seed, smallest, &target, &*isa) {
            Ok(program) => program.func.display().to_string(),
            Err(e) => e,
        };
        failures.push(format!("seed {seed} fails at size {size}, and at {smallest} at the smallest:\n{why}\n{clif}"));
        if failures.len() == FAILURES {
            break;
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {seeds} seeds fail on {} (at most {FAILURES} reported):\n\n{}",
        failures.len(),
        host::triple(&target),
        failures.join("\n\n")
    );
}

#[test]
fn the_generator_makes_valid_programs_the_interpreter_runs() {
    let mut drawn = [vec![0u32; usize::from(Op::COUNT)], vec![0u32; usize::from(Op::COUNT)]];
    let mut features = Features::default();
    for target in &host::TARGETS {
        let isa = host::isa(target);
        let triple = host::triple(target);
        let arch = arch(target);
        for seed in 0..EVERYWHERE {
            let size = size(seed);
            let program = generated(seed, size, target, &*isa).unwrap_or_else(|e| panic!("{triple}: {e}"));
            let clif = program.func.display().to_string();
            let again = generated(seed, size, target, &*isa).unwrap_or_else(|e| panic!("{triple}: {e}"));
            assert_eq!(clif, again.func.display().to_string(), "{triple}: seed {seed} makes two functions");
            if let Err(e) = verify_function(&program.func, isa.flags()) {
                panic!("{triple}: seed {seed}: the verifier: {e}\n{clif}");
            }
            for k in 0..INPUTS {
                let args = program::inputs(seed, k, &program.sig);
                if let Err(e) = interpret(&program.func, &args) {
                    panic!("{triple}: seed {seed}: {e}\n{clif}");
                }
            }
            for (sum, count) in drawn[arch].iter_mut().zip(&program.drawn) {
                *sum += count;
            }
            let more = program.features;
            features.brifs += more.brifs;
            features.direct += more.direct;
            features.tables += more.tables;
            features.loops += more.loops;
            features.edges += more.edges;
        }
    }
    for target in &host::TARGETS[..2] {
        let isa = program::isa_of(target);
        let never: Vec<Op> =
            table::candidates(isa).iter().map(|it| it.op).filter(|op| drawn[arch(target)][*op as usize] == 0).collect();
        assert!(never.is_empty(), "{isa:?}: ops the generator never drew: {never:?}");
    }
    let Features { brifs, direct, tables, loops, edges } = features;
    assert!(
        brifs > 0 && direct > 0 && tables > 0 && loops > 0 && edges > 0,
        "structures the generator never made: {features:?}"
    );
}

#[test]
fn every_target_compiles_what_the_generator_makes() {
    for target in &host::TARGETS {
        let isa = host::isa(target);
        let triple = host::triple(target);
        for seed in 0..EVERYWHERE {
            let program = generated(seed, size(seed), target, &*isa).unwrap_or_else(|e| panic!("{triple}: {e}"));
            let func = &program.func;
            if let Err(e) = host::cranelift_caught(func, &*isa) {
                panic!("{triple}: seed {seed}: Cranelift refuses it, a bug of the generator: {e}\n{}", func.display());
            }
            if let Err(e) = crate::compile(func, target) {
                panic!("{triple}: seed {seed}: the stencil backend refuses what Cranelift compiles: {e}\n{}", func.display());
            }
        }
    }
}

#[test]
fn the_extra_seed_count_is_a_number() {
    assert_eq!(extra(Err(VarError::NotPresent)), Ok(0));
    assert_eq!(extra(Ok("40".to_owned())), Ok(40));
    let e = extra(Ok("many".to_owned())).expect_err("not a number");
    assert!(e.contains(ENV), "{e}");
}

/// Which of the two ISAs `target` is, as an index.
fn arch(target: &Target) -> usize {
    match target.arch {
        blaze_codegen::Arch::X86_64 => 0,
        blaze_codegen::Arch::Aarch64 => 1,
        blaze_codegen::Arch::Wasm32 => panic!("wasm32 has no stencils"),
    }
}
