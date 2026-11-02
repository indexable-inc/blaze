//! A family linked once per mix and called: each mix chooses the backend of every function
//! (bit `i` set: `u0:i` is stencil code), and every mix must give what the all-Cranelift
//! link gives for the same arguments: the results bit for bit, the bytes of every data
//! object, and a trap where it traps. Tier counters are checked per stencil function.

use cranelift_codegen::{data_value::DataValue, ir, isa::OwnedTargetIsa};

use super::{Family, Mk};
use crate::{
    code::Code,
    tests::{
        exec::{self, Exec, Link, Unit},
        host::{self, Compiled},
        interp::{self, Nans, Outcome},
        rng::Rng,
    },
};

const _: () = assert!(super::FUNCS == exec::FUNCS && super::HOST == exec::HOST);

/// The host's [`Mk`], or `None` (said on stderr) on a host no target is for.
pub fn host_mk() -> Option<Mk> {
    match host::host() {
        Some(target) => Some(Mk::new(target)),
        None => {
            eprintln!("no target of the tests is this host: its own CI runs the code");
            None
        }
    }
}

/// One argument, resolved per link: a value, or an address that differs between links.
#[derive(Clone, Debug)]
pub enum In {
    V(DataValue),
    /// The address of data object `i`.
    Data(usize),
    /// The address of unit `i`'s code.
    Unit(usize),
}

pub fn i64s(values: &[i64]) -> Vec<In> {
    values.iter().map(|value| In::V(DataValue::I64(*value))).collect()
}

/// `count` random argument lists for the parameters of `family`'s entry, all values, from
/// `seed`.
pub fn random_inputs(seed: u64, family: &Family, count: usize) -> Vec<Vec<In>> {
    let mut rng = Rng::new(seed);
    let params = &family.funcs[0].signature.params;
    (0..count)
        .map(|_| params.iter().map(|param| In::V(super::random(&mut rng, param.value_type))).collect())
        .collect()
}

fn resolve(exec: &Exec, ins: &[In]) -> Vec<DataValue> {
    ins.iter()
        .map(|it| match it {
            In::V(value) => value.clone(),
            In::Data(i) => DataValue::I64(exec.data_addr(*i) as i64),
            In::Unit(i) => DataValue::I64(exec.addr(*i) as i64),
        })
        .collect()
}

/// The values of `ins`, or `None` if any is an address.
pub fn plain(ins: &[In]) -> Option<Vec<DataValue>> {
    ins.iter()
        .map(|it| match it {
            In::V(value) => Some(value.clone()),
            In::Data(_) | In::Unit(_) => None,
        })
        .collect()
}

/// What a check links and how it calls.
pub struct Opts<'a> {
    pub data: &'a [&'a [u8]],
    pub host: &'a [usize],
    /// Calls in a forked child: for inputs that may trap. Data written there is not seen.
    pub forked: bool,
    /// Whether `cranelift-interpreter` is the reference for the all-Cranelift link too.
    pub interp: bool,
}

impl Opts<'static> {
    pub fn plain() -> Opts<'static> {
        Opts { data: &[], host: &[], forked: false, interp: false }
    }

    pub fn interp() -> Opts<'static> {
        Opts { data: &[], host: &[], forked: false, interp: true }
    }

    pub fn forked() -> Opts<'static> {
        Opts { data: &[], host: &[], forked: true, interp: true }
    }
}

/// A family compiled by both backends for the host.
pub struct Built<'a> {
    pub family: &'a Family,
    isa: OwnedTargetIsa,
    stencil: Vec<Code>,
    cranelift: Vec<Compiled>,
}

impl<'a> Built<'a> {
    /// Panics when either backend refuses a function: the families run here compile.
    pub fn new(mk: &Mk, family: &'a Family) -> Built<'a> {
        let mut stencil = Vec::with_capacity(family.funcs.len());
        let mut cranelift = Vec::with_capacity(family.funcs.len());
        for (i, func) in family.funcs.iter().enumerate() {
            let compiled = host::cranelift_caught(func, &*mk.isa).unwrap_or_else(|e| {
                panic!("{}: Cranelift refuses u0:{i}: {e}\n{}", family.name, func.display())
            });
            let code = crate::compile(func, &mk.target).unwrap_or_else(|e| {
                panic!(
                    "{}: the stencil backend refuses u0:{i}, which Cranelift compiles: {e}\n{}",
                    family.name,
                    func.display()
                )
            });
            cranelift.push(compiled);
            stencil.push(code);
        }
        Built { family, isa: exec::host_isa(), stencil, cranelift }
    }

    pub fn units(&self) -> usize {
        self.family.funcs.len()
    }

    /// Every mix: `0` is all Cranelift, `mixes() - 1` all stencil.
    pub fn mixes(&self) -> u32 {
        1 << self.units()
    }

    pub fn stencil(mix: u32, unit: usize) -> bool {
        mix >> unit & 1 == 1
    }

    /// The units linked with the backends `mix` says.
    pub fn link(&self, mix: u32, data: &[&[u8]], host: &[usize]) -> Exec {
        let units: Vec<Unit<'_>> = (0..self.units())
            .map(|i| {
                let sig = &self.family.funcs[i].signature;
                if Self::stencil(mix, i) {
                    Unit::Stencil(&self.stencil[i], sig)
                } else {
                    Unit::Cranelift(&self.cranelift[i], sig)
                }
            })
            .collect();
        Exec::link(&Link { isa: &*self.isa, units: &units, data, host })
    }

    pub fn mix_name(&self, mix: u32) -> String {
        let names: Vec<String> = (0..self.units())
            .map(|i| format!("u0:{i} {}", if Self::stencil(mix, i) { "stencil" } else { "cranelift" }))
            .collect();
        names.join(", ")
    }
}

/// The results types of `family`'s entry.
pub fn returns(family: &Family) -> Vec<ir::Type> {
    family.funcs[0].signature.returns.iter().map(|ret| ret.value_type).collect()
}

fn run(exec: &Exec, args: &[DataValue], forked: bool) -> Outcome<Vec<DataValue>> {
    if forked { exec.values_forked(0, args) } else { Outcome::Returned(exec.values(0, args)) }
}

/// Whether two runs of machine code are the same result: equal bits, or both a trap.
pub fn same_outcome(tys: &[ir::Type], a: &Outcome<Vec<DataValue>>, b: &Outcome<Vec<DataValue>>) -> bool {
    match (a, b) {
        (Outcome::Returned(a), Outcome::Returned(b)) => interp::same_all(tys, a, b, Nans::Exact),
        (Outcome::Trapped(_), Outcome::Trapped(_)) => true,
        _ => false,
    }
}

/// [`check_counted`] with no counts expected.
pub fn check(mk: &Mk, family: &Family, inputs: &[Vec<In>], opts: &Opts<'_>) {
    check_counted(mk, family, inputs, opts, |_| Vec::new());
}

/// Calls `family` with each of `inputs` in every mix. `counts(input)` is, per unit, the
/// calls and back edges its stencil code counts for that one call (empty: not checked);
/// Cranelift's code here does not count, so its cells stay zero.
pub fn check_counted(
    mk: &Mk,
    family: &Family,
    inputs: &[Vec<In>],
    opts: &Opts<'_>,
    counts: impl Fn(&[In]) -> Vec<[u64; 2]>,
) {
    let built = Built::new(mk, family);
    let tys = returns(family);
    let mut want = Vec::with_capacity(inputs.len());
    {
        let reference = built.link(0, opts.data, opts.host);
        for input in inputs {
            let out = run(&reference, &resolve(&reference, input), opts.forked);
            if opts.interp {
                let args = plain(input).unwrap_or_else(|| {
                    panic!("{}: the interpreter is given no addresses: {input:?}", family.name)
                });
                let funcs: Vec<&ir::Function> = family.funcs.iter().collect();
                let reference = interp::run(&funcs, 0, &args).unwrap_or_else(|e| {
                    panic!("{}: {e}, on {args:?}\n{}", family.name, family.clif())
                });
                if let Err(e) = interp::agrees(&tys, &reference, &out) {
                    panic!("{}, all Cranelift, on {args:?}: {e}\n{}", family.name, family.clif());
                }
            }
            let data: Vec<Vec<u8>> = (0..opts.data.len()).map(|i| reference.data(i)).collect();
            want.push((out, data));
        }
    }
    for mix in 0..built.mixes() {
        let exec = built.link(mix, opts.data, opts.host);
        for (input, (want_out, want_data)) in inputs.iter().zip(&want) {
            let args = resolve(&exec, input);
            let expected = counts(input);
            assert!(expected.is_empty() || !opts.forked, "{}: counts of a forked call", family.name);
            let before: Vec<[u64; 2]> = (0..built.units()).map(|u| exec.counts(u)).collect();
            let got = run(&exec, &args, opts.forked);
            if !same_outcome(&tys, want_out, &got) {
                panic!(
                    "{} with {}: on {input:?}, all Cranelift gives {want_out:?}, this mix {got:?}\n{}",
                    family.name,
                    built.mix_name(mix),
                    family.clif()
                );
            }
            for (i, want) in want_data.iter().enumerate() {
                let got = exec.data(i);
                assert!(
                    &got == want,
                    "{} with {}: on {input:?}, data object {i} is {got:02x?}, all Cranelift leaves {want:02x?}\n{}",
                    family.name,
                    built.mix_name(mix),
                    family.clif()
                );
            }
            if expected.is_empty() {
                continue;
            }
            assert_eq!(expected.len(), built.units(), "{}: counts per unit", family.name);
            for (u, before) in before.iter().enumerate() {
                let after = exec.counts(u);
                let delta = [after[0] - before[0], after[1] - before[1]];
                let want = if Built::stencil(mix, u) { expected[u] } else { [0, 0] };
                assert_eq!(
                    delta,
                    want,
                    "{} with {}: on {input:?}, u0:{u} counts [calls, back edges]\n{}",
                    family.name,
                    built.mix_name(mix),
                    family.clif()
                );
            }
        }
    }
}

#[test]
fn every_mix_links_each_backend_where_it_says() {
    let Some(mk) = host_mk() else { return };
    let family = super::calls::direct(&mk);
    let built = Built::new(&mk, &family);
    assert_eq!(built.mixes(), 1 << family.funcs.len());
    assert!(!Built::stencil(0, 0) && Built::stencil(1, 0) && !Built::stencil(1, 1));
    assert_eq!(built.mix_name(0b10), "u0:0 cranelift, u0:1 stencil, u0:2 cranelift");
}
