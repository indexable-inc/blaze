//! A fulfillment load with staggered unifications (the cost example's and the strategy
//! equality test's): `waves` variables bound one wave per pass along a chain of projections,
//! and obligations each ambiguous until the variable of its wave is bound.
//!
//! Chain: `<u32 as Tr>::A == ?v0`, and `<?v(w-1) as Tr>::A == ?vw`; obligation `i` is
//! `W<?v(wave(i))>: Foo` with two impls (`W<u32>`, `W<u64>`), so it stays ambiguous until its
//! variable is bound. Every pass binds the next variable of the chain.

use std::string::ToString;
use std::vec::Vec;

use super::parse::{DeclA, Parsed, parse_file};
use super::run::PRELUDE;
use super::tys::TestTys;
use super::world::{World, def_of};
use crate::error::SolveError;
use crate::term::{AliasKind, Polarity, Pred, Rigid, Ty, TyKind, UintTy, VarKind};
use crate::tys::Tys;

const DECLS: &str = "trait Tr { type A; }\nimpl Tr for u32 { type A = u32; }\nstruct W<T>(T)\ntrait Foo\nimpl Foo for W<u32>\nimpl Foo for W<u64>\n";

/// The load: its world, arena and goals in registration order.
pub struct Staggered {
    pub world: World,
    pub tys: TestTys,
    pub goals: Vec<Pred>,
    /// The chain's variables.
    pub vars: Vec<Ty>,
}

/// Builds the load; `wave(i)` picks obligation `i`'s wave (below `waves`).
pub fn staggered(obligations: u32, waves: u32, mut wave: impl FnMut(u32) -> u32) -> Parsed<Staggered> {
    let prelude = parse_file(PRELUDE)?;
    let own = parse_file(DECLS)?;
    let mut decls: Vec<(u32, &DeclA)> = prelude.decls.iter().map(|(l, d)| (*l, d)).collect();
    decls.extend(own.decls.iter().map(|(l, d)| (*l, d)));
    let world = World::build(&decls)?;
    let mut tys = TestTys::new();
    let (goals, vars) =
        build(&mut tys, obligations, waves, &mut wave).map_err(|e| super::parse::CaseError { line: 0, what: e.to_string() })?;
    Ok(Staggered { world, tys, goals, vars })
}

fn build(tys: &mut TestTys, obligations: u32, waves: u32, wave: &mut dyn FnMut(u32) -> u32) -> Result<(Vec<Pred>, Vec<Ty>), SolveError> {
    let item = def_of("assoc", "Tr::A");
    let foo = def_of("trait", "Foo");
    let w = def_of("adt", "W");
    let u32_ = tys.mk(TyKind::Uint(UintTy::U32))?;
    let mut vars = Vec::new();
    for _ in 0..waves {
        vars.push(tys.fresh(VarKind::Ty)?);
    }
    let mut goals = Vec::new();
    // The links, last first, so a pass in registration order sees each only after its input.
    for k in (1..waves as usize).rev() {
        let l = tys.mk_list(&[vars[k - 1]])?;
        let alias = tys.mk(TyKind::Alias(Rigid::No, AliasKind::Projection, item, l))?;
        goals.push(Pred::Projection { alias, term: vars[k] });
    }
    for i in 0..obligations {
        let v = vars[(wave(i) % waves) as usize];
        let l = tys.mk_list(&[v])?;
        let t = tys.mk(TyKind::Adt(w, l))?;
        let args = tys.mk_list(&[t])?;
        goals.push(Pred::Trait { trait_: foo, args, polarity: Polarity::Positive });
    }
    let l = tys.mk_list(&[u32_])?;
    let alias = tys.mk(TyKind::Alias(Rigid::No, AliasKind::Projection, item, l))?;
    goals.push(Pred::Projection { alias, term: vars[0] });
    Ok((goals, vars))
}
