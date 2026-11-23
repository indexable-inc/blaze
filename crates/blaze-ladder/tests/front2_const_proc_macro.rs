//! A dependency CONSTANT whose initializer calls a function-like proc macro is evaluated through
//! the crate's runner, as a fn body's call is. A const's body is lowered by the body route
//! (consts.rs `checked_body` -> body.rs `lower_fn` -> body_host.rs `BodyHost::of` with the
//! store's `sources`), so it has the host whenever the program has one.
//!
//! The case this guards: tempo-chainspec t1515, 31 tests NOT RUN on
//! `alloy_hardforks::ethereum::sepolia::SEPOLIA_PARIS_TTD = uint!(..)` (ruint's `uint!` calls
//! the proc macro `ruint_macro::uint_with_path`), refused with "Host(HostError { ask:
//! ProcMacro, why: Refused })". By reading, the cause is the COMMAND, not this route: `blaze
//! test` and `blaze run` open their names records with no proc-macro runner (blaze-cli
//! engine_start.rs `names_of(.., None, ..)`), so job_texts.rs `run_proc_macro` refuses at `let
//! Some(procs) = self.procs`. The handoff is c9 in the wave-1119 handoff folder. This test pins
//! the ladder's half: given a host, the const route asks it.
//!
//! - `dep::N = macros::make!()`; the runner (a stub standing for blaze-proc-macro's hook, which
//!   knows the macro crate by `RUNNER` only) writes `4`. Rust: a constant's initializer is a
//!   const context (the Reference, "Constant evaluation"), and an array type's length is one
//!   too ("Array type length expressions"), so `via_fn`'s head `-> [i32; dep::N]` evaluates `N`.
//! - THROUGH A FN: root `via_fn`; its head reads `N`. The runner ran, nothing is left out.
//! - THROUGH ANOTHER CONST (twin): root `via_const`; its head reads `dep::M = N`, so `N` is
//!   evaluated as a constant another constant reads. A fresh survey: the runner ran again.
//! - CONTROL: `via_fn` with no body sources is refused, and the refusal names the host's ask
//!   (`ProcMacro`): the shape of tempo's refusal, with its cause (no host) removed in the twin.
//!
//! It needs no toolchain: the program is `#![no_core]`.
#![feature(allocator_api)]

use std::alloc::Global;
use std::sync::atomic::{AtomicUsize, Ordering};

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::front2::body_host::{BodySources, Sourced};
use blaze_ladder::front2::program::Front2Program;
use blaze_own::{AVec, Alloc};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{Edition, ItemDetail};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const USER: &str = "#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

pub fn via_fn() -> [i32; dep::N] {
    loop {}
}

pub fn via_const() -> [i32; dep::M] {
    loop {}
}
";

const DEP: &str = "#![feature(no_core)]
#![no_core]

pub const N: usize = macros::make!();

pub const M: usize = N;
";

const MACROS: &str = "#![no_core]
#[proc_macro]
pub fn make(input: TokenStream) -> TokenStream { input }
";

/// The key the stub runner knows the macro crate (crate 2) by: no crate's item key.
const RUNNER: CrateItemKey = CrateItemKey(ContentId([9; 32]));

/// The three crates' files, and a runner for crate 2 that writes `4` for `make` and counts its
/// runs; it refuses a definition named by any key but [`RUNNER`], as the hook does.
struct WithRunner {
    runs: AtomicUsize,
}

impl SourceTexts<'static> for WithRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        [("/user/lib.rs", USER), ("/dep/lib.rs", DEP), ("/macros/lib.rs", MACROS)].get(krate).filter(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 2).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        if def.site().krate != RUNNER || !matches!(call.kind, ProcKind::FnLike) || def.proc_name() != Some("make") {
            return Err(refused);
        }
        self.runs.fetch_add(1, Ordering::Relaxed);
        let tokens = lex("4", Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner { runs: AtomicUsize::new(0) };

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)], proc_macro: bool) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("front2 const proc macro test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// Crate 0's free fn `name`.
fn fn_named(names: &Names<'_>, name: &str) -> blaze_id::ItemKey {
    for (key, _, place) in names.items(0) {
        let Some((text, outline)) = names.outline_at(0, place.at) else { continue };
        let item = outline.item(place.item);
        if !matches!(item.detail, ItemDetail::Fn { .. }) {
            continue;
        }
        let at = item.range.start as usize;
        if item.name.is_some_and(|r| &text[at + r.start as usize..at + r.end as usize] == name) {
            return key;
        }
    }
    panic!("crate 0 has no fn `{name}`")
}

#[test]
fn a_dependency_constant_s_proc_macro_call_is_run_by_its_crate_s_runner() {
    let g = blaze_own::Alloc::global();
    let crates = [
        input("user", "/user/lib.rs", &[("dep", 1)], false),
        input("dep", "/dep/lib.rs", &[("macros", 2)], false),
        input("macros", "/macros/lib.rs", &[], true),
    ];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let sources: &'static dyn BodySources = Box::leak(Box::new(Sourced(&FILES)));

    // Through a fn, then (twin) through another constant: each a fresh survey, so each
    // evaluates `N` and asks the runner again.
    for root in ["via_fn", "via_const"] {
        let before = FILES.runs.load(Ordering::Relaxed);
        let roots = [fn_named(&names, root)];
        let mut left_out = Vec::new_in(Global);
        let program = Front2Program::survey_from(&names, &frame, &[], &roots, &mut left_out, Some(sources), g)
            .unwrap_or_else(|e| panic!("{root}: the survey with body sources: {e}"));
        let rows: Vec<String> = left_out.iter().map(|(f, row)| format!("{f}: {}: {}", row.class, row.whole)).collect();
        assert!(rows.is_empty(), "{root}: nothing is left out with body sources: {rows:?}");
        let fns = blaze_ladder::seam::FrontEnd::fn_count(&program);
        let runs = FILES.runs.load(Ordering::Relaxed) - before;
        assert!(runs >= 1, "{root}: the runner was asked for `make` under its own key while `dep::N` was evaluated");
        println!("{root}: {fns} fn(s), nothing left out, the runner ran {runs} time(s)");
    }

    // CONTROL: no body sources (the command's case: no runner), so the constant's lowering is
    // refused by the host's ask, and the fn whose head reads it with it.
    let before = FILES.runs.load(Ordering::Relaxed);
    let roots = [fn_named(&names, "via_fn")];
    let mut left_out = Vec::new_in(Global);
    let control = Front2Program::survey_from(&names, &frame, &[], &roots, &mut left_out, None, g);
    let mut text: Vec<String> = left_out.iter().map(|(f, row)| format!("{f}: {}: {}", row.class, row.whole)).collect();
    if let Err(e) = &control {
        text.push(format!("the survey: {e}"));
    }
    println!("CONTROL: {text:?}");
    assert_eq!(FILES.runs.load(Ordering::Relaxed), before, "control: no runner was asked");
    assert!(text.iter().any(|t| t.contains("ProcMacro")), "control: the refusal names the host's ask: {text:?}");
}
