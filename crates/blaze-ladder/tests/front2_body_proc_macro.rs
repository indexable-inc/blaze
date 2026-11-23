//! A proc-macro call inside a fn body is run by the crate's runner, as an item-level call is:
//! the survey's program is given the body sources (program.rs `survey_from`'s `body_sources`,
//! what tempo_build and the walk's builds survey pass), and the body's resolver names the
//! macro's crate by its RUNNER's key (front2 scoped.rs, as blaze-scope's `definition_of`).
//!
//! The fault this guards (tempo's build census, LEFT 79 `lower: Lowered`): the survey's program
//! had no body sources, so every body proc-macro call (tokio `select!`'s
//! `$crate::select_priv_clean_pattern!`), `env!` and `include!` was refused by the host before
//! any runner was asked.
//!
//! - `two`'s body is `macros::make!()`; the runner (a stub standing for blaze-proc-macro's
//!   hook, which knows the macro crate by `RUNNER` only) writes `0`. With the body sources the
//!   survey tables `main` and `two`, leaves nothing out, and the runner was asked once, for
//!   `make`, under `RUNNER`.
//! - CONTROL: the same survey with no body sources leaves `two` out, of class
//!   `lower: Lowered: Macro .. ProcMacro` (the host's refusal, by its ask).
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

fn two() -> i32 {
    macros::make!()
}

pub fn main() -> i32 {
    two()
}
";

const MACROS: &str = "#![no_core]
#[proc_macro]
pub fn make(input: TokenStream) -> TokenStream { input }
";

/// The key the stub runner knows the macro crate (crate 1) by: no crate's item key.
const RUNNER: CrateItemKey = CrateItemKey(ContentId([7; 32]));

/// The two crates' files, and a runner for crate 1 that writes `0` for `make` and counts its
/// runs; it refuses a definition named by any key but [`RUNNER`], as the hook does.
struct WithRunner {
    runs: AtomicUsize,
}

impl SourceTexts<'static> for WithRunner {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        [("/user/lib.rs", USER), ("/macros/lib.rs", MACROS)].get(krate).filter(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        if def.site().krate != RUNNER || !matches!(call.kind, ProcKind::FnLike) || def.proc_name() != Some("make") {
            return Err(refused);
        }
        self.runs.fetch_add(1, Ordering::Relaxed);
        let tokens = lex("0", Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

static FILES: WithRunner = WithRunner { runs: AtomicUsize::new(0) };

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)], proc_macro: bool) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("front2 body proc macro test crate", name.as_bytes())),
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
fn a_proc_macro_call_in_a_body_is_run_by_its_crate_s_runner() {
    let g = blaze_own::Alloc::global();
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)], false), input("macros", "/macros/lib.rs", &[], true)];
    let names = resolve_crates(&crates, Sysroot::default(), &FILES, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let roots = [fn_named(&names, "main")];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let sources: &'static dyn BodySources = Box::leak(Box::new(Sourced(&FILES)));

    // With the body sources: `two` lowers through the runner.
    let mut left_out = Vec::new_in(Global);
    let program = Front2Program::survey_from(&names, &frame, &[], &roots, &mut left_out, Some(sources), g)
        .unwrap_or_else(|e| panic!("the survey with body sources: {e}"));
    let rows: Vec<String> = left_out.iter().map(|(f, row)| format!("{f}: {}", row.class)).collect();
    assert!(rows.is_empty(), "nothing is left out with body sources: {rows:?}");
    let fns = blaze_ladder::seam::FrontEnd::fn_count(&program);
    assert_eq!(fns, 2, "the table holds `main` and `two`");
    let runs = FILES.runs.load(Ordering::Relaxed);
    assert!(runs >= 1, "the runner was asked for `make` under its own key");
    println!("WITH SOURCES: {fns} fns, nothing left out, the runner ran {runs} time(s)");

    // CONTROL: no body sources, so the host refuses the call by its ask.
    let mut left_out = Vec::new_in(Global);
    let control = Front2Program::survey_from(&names, &frame, &[], &roots, &mut left_out, None, g);
    let rows: Vec<String> = left_out.iter().map(|(f, row)| format!("{f}: {}", row.class)).collect();
    println!("CONTROL: {:?}; left out {rows:?}", control.as_ref().err().map(|e| e.to_string()));
    let two = rows.iter().find(|r| r.contains("two")).unwrap_or_else(|| panic!("control: `two` is left out: {rows:?}"));
    assert!(two.contains("lower: Lowered: Macro") && two.contains("ProcMacro"), "control: its class names the host's ask: {two}");
}
