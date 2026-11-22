//! [`crate::call_reads::CallReads`]: the body expander keeps, per body call, the span of the
//! body's read list its expansion joined, so the body's D111 finding (a member's proc macro
//! reading outside its package) can be placed at the call that read (blaze-ladder front2
//! `body_left`). `sol!()` reads a sibling's file; `seven!()` reads nothing and has no span.
//!
//! Control (rule 8, once): in by_name.rs `expand_body`, drop the `note_reads` line: the
//! assert "the reading call keeps its span" fails.
//!
//! Expected (`lanecheck.sh blaze-lower test call_reads_tests`): passes, prints nothing.

use std::vec::Vec as StdVec;

use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, ReadAnswer, ReadKind, Reads, TokenStream, lex};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_surface::CallShape;

use crate::by_name::ByName;
use crate::{Cfg, CfgAnswer, Edition, lower_body_expanding};

struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
        CfgAnswer::No
    }
}

/// `sol` (fn-like) answers `7` and lists a read of `crates/sibling/x.json`, as a member's
/// sandboxed run records it; `seven` answers `7` and reads nothing.
struct Reading;

impl ExpandCx for Reading {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }
    fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, _: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        let failed = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };
        let mut reads = Reads::new_in(alloc);
        match def.proc_name() {
            Some("sol") => {
                reads.push(ReadKind::ProcMacro, "file:crates/sibling/x.json", ReadAnswer::Absent).map_err(|_| failed)?;
            }
            Some("seven") => {}
            _ => return Err(failed),
        }
        let tokens = lex("7", Edition::E2024, alloc).map_err(|_| failed)?;
        let mut origins = blaze_own::Vec::new_in(alloc);
        origins.extend((0..tokens.len()).map(|_| blaze_expand::TokenOrigin::Made(blaze_expand::Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads, diagnostics: blaze_own::Vec::new_in(alloc), text: blaze_own::Vec::new_in(alloc) })
    }
}

struct Procs;

impl<'d> crate::by_name::MacroNames<'d> for Procs {
    fn resolve<'a>(&self, path: &str, _: crate::by_name::PathAt, kind: blaze_expand::ProcKind, alloc: Alloc<'a>) -> Result<crate::by_name::Resolved<'d, 'a>, crate::by_name::NoMacro<'a>>
    where
        'd: 'a,
    {
        let name = path.rsplit(':').next().ok_or(crate::by_name::NoMacro::NotAPath)?;
        let known = matches!((name, kind), ("sol" | "seven", blaze_expand::ProcKind::FnLike));
        let krate = CrateItemKey(ContentId([2; 32]));
        known
            .then(|| MacroDefinition::proc_macro(krate, kind, name, &[], crate::expand_tests::at(), alloc).ok())
            .flatten()
            .map(crate::by_name::Resolved::Def)
            .ok_or(crate::by_name::NoMacro::NotListed)
    }
}

#[test]
fn call_reads_tests_a_body_call_keeps_the_span_of_its_reads() {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let text = "fn f() -> u32 { sol!() + seven!() }";
    let lists = crate::by_name::Lists { rules: &[], builtin_at: crate::expand_tests::at(), then: Some(&Procs) };
    let mut expander = ByName::with_names(&lists, Reading, alloc);
    let Ok(_) = lower_body_expanding(text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, &mut expander, alloc) else { panic!("needs a cfg") };
    let kept: StdVec<(Option<u32>, &str, CallShape, core::ops::Range<usize>)> =
        expander.call_reads().iter().map(|c| (c.at.map(|r| r.start), c.path.as_str(), c.shape, c.reads.clone())).collect();
    let sol = u32::try_from(text.find("sol!").unwrap_or_else(|| panic!("the call is written"))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(kept, [(Some(sol), "sol", CallShape::FnLike, 0..1)], "the reading call keeps its span; the call reading nothing keeps none");
    let read = &expander.reads().list()[0];
    assert_eq!((read.kind, expander.reads().subject(read)), (ReadKind::ProcMacro, "file:crates/sibling/x.json"));
}
