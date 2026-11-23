//! Decision D111 for proc macros (D96 lane, leg 5; the coordinator's ruling of 2026-11-16
//! ~21:2x): a workspace member's macro that reads a file outside the member's own directory and
//! inside the workspace (`sol!("../sibling/x.json")`, joined onto `CARGO_MANIFEST_DIR =
//! /workspace/crates/m`) is allowed, its read is in the expansion's read list (`file:<path from
//! the workspace>`, blaze-proc-macro sandbox.rs), and it gives a WARNING finding at the call. The
//! finding is DERIVED FROM THE READ LIST by the caller that holds the call's position (resolve.rs
//! `Host::run_proc_macro`), through the one home of D111 findings, `SourceTexts::left_package`,
//! naming the macro as written at the call (`macros::sol`, `Abi`) and its shape. So a call
//! answered from a kept answer (the proc-macro runner's table hands back the run's read list,
//! hook.rs `run`) gives the same finding, byte for byte, as the call whose macro ran.
//!
//! Control (run once, rule 8): make the finding only when the macro ran: in this file's
//! `Workspace::run_proc_macro`, answer a kept call with `Reads::new_in(alloc)` instead of the
//! kept list (`PLANT_RUN_ONLY = true` does exactly that; it stands for a sandbox that files the
//! finding itself, which a kept answer never reaches). Expected failure: "a kept answer gives
//! the finding the run gave". A second control: delete the D111 loop from resolve.rs
//! `Host::run_proc_macro`: "the run's read outside the member is a finding" fails.
//!
//! Staged with wiring/blaze-scope/d96-L5-left-1.md (its `mod proc_left_package;` line); NEVER
//! COMPILED by its writer.
//!
//! Expected (`lanecheck.sh blaze-scope test proc_left_package`): "test blaze-scope: exit 0; 1
//! passed, 0 failed", the test printing "2 runs, 2 findings per solve, equal on the kept
//! answers: `macros::sol!` reads /0/crates/sibling/x.json; `#[derive(Abi)]` reads
//! /0/crates/sibling/y.json".

use std::string::{String, ToString};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;

use blaze_expand::seen::SeenRoot;
use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, Produced, ReadAnswer, ReadKind, Reads, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallShape, Edition};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::crate_env::{CrateEnv, EnvFacts};
use crate::graph::Sysroot;
use crate::include_place::LeftPackage;
use crate::resolve::{CrateInput, SourceTexts, resolve_crates};
use crate::task::Serial;

/// The control's switch (see the module's doc): `false` in the tree.
const PLANT_RUN_ONLY: bool = false;

/// The member `crates/m` of a workspace (tree 0): a fn-like call and a derive of the macro
/// crate, each reading a sibling's file.
const USER: &str = "#![no_core]
use macros::Abi;
macros::sol!();
#[derive(Abi)]
pub struct P;
";

const MACROS: &str = "#![no_core]
#[proc_macro]
pub fn sol(input: TokenStream) -> TokenStream { input }
#[proc_macro_derive(Abi)]
pub fn abi(item: TokenStream) -> TokenStream { item }
";

const RUNNER: CrateItemKey = CrateItemKey(ContentId([9; 32]));

/// One finding the sources were told of, owned: (calling file, byte, macro as written, shape,
/// file read).
type Told = (String, u32, String, CallShape, String);

/// A kept answer: the macro's export name and the read list its run made.
type KeptAnswer = (String, Vec<(String, Option<ContentId>)>);

/// The workspace's texts, the member's environment (it sees `/workspace/crates/m`), a runner
/// that keeps each macro's first answer and hands it back after (as blaze-proc-macro's table
/// does), and the findings told.
struct Workspace {
    env: CrateEnv<'static>,
    runs: AtomicUsize,
    kept: Mutex<Vec<KeptAnswer>>,
    told: Mutex<Vec<Told>>,
}

impl SourceTexts<'static> for Workspace {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (0, "/0/crates/m/src/lib.rs") => Some(USER),
            (1, "/macros/lib.rs") => Some(MACROS),
            _ => None,
        }
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 1).then_some(RUNNER)
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, _: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let name = def.proc_name().ok_or(refused)?;
        let (text, made): (&str, &[(&str, &[u8])]) = match name {
            // `$CARGO_MANIFEST_DIR/abi/own.json` (the member's own) and `/../sibling/x.json`.
            "sol" => ("pub struct Sol;", &[("env:CARGO_MANIFEST_DIR", b"/workspace/crates/m"), ("file:crates/m/abi/own.json", b"{}"), ("file:crates/sibling/x.json", b"[]")]),
            "Abi" => ("pub struct AbiMade;", &[("file:crates/sibling/y.json", b"[1]")]),
            _ => return Err(refused),
        };
        let mut kept = self.kept.lock().unwrap_or_else(|p| panic!("kept: {p}"));
        let list: Vec<(String, Option<ContentId>)> = match kept.iter().find(|(it, _)| it == name) {
            // A kept answer: the run's read list, as the runner's table holds it.
            Some((_, list)) if !PLANT_RUN_ONLY => list.clone(),
            Some(_) => Vec::new(),
            None => {
                self.runs.fetch_add(1, Ordering::Relaxed);
                let list: Vec<(String, Option<ContentId>)> = made.iter().map(|(subject, bytes)| (subject.to_string(), Some(ContentId::of("test read", bytes)))).collect();
                kept.push((name.to_string(), list.clone()));
                list
            }
        };
        let mut reads = Reads::new_in(alloc);
        for (subject, digest) in &list {
            reads.push(ReadKind::ProcMacro, subject, digest.map_or(ReadAnswer::Absent, ReadAnswer::Digest)).map_err(|_| refused)?;
        }
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads, diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'static>> {
        (krate == 0).then_some(&self.env)
    }

    fn left_package(&self, _krate: usize, left: LeftPackage<'_>) {
        let told = (left.from.to_string(), left.at, left.call.to_string(), left.shape, left.path.to_string());
        self.told.lock().unwrap_or_else(|p| panic!("told: {p}")).push(told);
    }
}

fn crates() -> [CrateInput<'static>; 2] {
    let mut user = input("user", "/0/crates/m/src/lib.rs", &[("macros", 1)]);
    user.shown_from = "/0/crates/m/";
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    [user, CrateInput { implicit, ..input("macros", "/macros/lib.rs", &[]) }]
}

/// One solve: the findings told during it, sorted by macro and file read (the order calls are
/// expanded in is the solve's, not the rule's) and without repeats (the host may be asked
/// twice for one call; blaze-cli's `include_outside::findings` drops repeats the same way).
fn solve(sources: &'static Workspace, alloc: blaze_own::Bump<'static>) -> Vec<Told> {
    let crates = crates();
    resolve_crates(&crates, Sysroot::default(), sources, &Serial, alloc).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let mut told = core::mem::take(&mut *sources.told.lock().unwrap_or_else(|p| panic!("told: {p}")));
    told.sort_by(|a, b| (&a.2, &a.4, &a.0, a.1, a.3).cmp(&(&b.2, &b.4, &b.0, b.1, b.3)));
    told.dedup();
    told
}

#[test]
fn a_member_macro_s_read_outside_its_dir_is_a_finding_the_same_from_a_kept_answer() {
    let bump = super::SyncFrame::leaked();
    let alloc: Alloc<'static> = bump.alloc();
    let facts = EnvFacts {
        crate_name: "user",
        package: Some(("user", "0.1.0")),
        bin: None,
        seen: SeenRoot::member("crates/m").unwrap_or_else(|e| panic!("{e}")),
        read_folder: "/0/crates/m/",
        build_script: false,
        script: None,
        test_or_bench: false,
        bin_exes: crate::crate_env::BinNames::NONE,
        meta: None,
    };
    let env = CrateEnv::of(facts, alloc).unwrap_or_else(|e| panic!("the member's environment: {e:?}"));
    let sources: &'static Workspace = std::boxed::Box::leak(std::boxed::Box::new(Workspace { env, runs: AtomicUsize::new(0), kept: Mutex::new(Vec::new()), told: Mutex::new(Vec::new()) }));

    let ran = solve(sources, bump);
    assert_eq!(sources.runs.load(Ordering::Relaxed), 2, "the first solve runs both macros");
    let paths: Vec<(&str, CallShape, &str)> = ran.iter().map(|(_, _, call, shape, path)| (call.as_str(), *shape, path.as_str())).collect();
    assert_eq!(
        paths,
        [("Abi", CallShape::Derive, "/0/crates/sibling/y.json"), ("macros::sol", CallShape::FnLike, "/0/crates/sibling/x.json")],
        "the run's read outside the member is a finding, named by the macro as written; its own file is not"
    );
    assert!(ran.iter().all(|(from, _, _, _, _)| from == "/0/crates/m/src/lib.rs"), "placed in the calling file: {ran:?}");

    let kept = solve(sources, bump);
    assert_eq!(sources.runs.load(Ordering::Relaxed), 2, "the second solve runs nothing: both answers are kept");
    assert_eq!(kept, ran, "a kept answer gives the finding the run gave");

    std::println!(
        "{} runs, {} findings per solve, equal on the kept answers: `{}!` reads {}; `#[derive({})]` reads {}",
        sources.runs.load(Ordering::Relaxed),
        ran.len(),
        ran[1].2,
        ran[1].4,
        ran[0].2,
        ran[0].4
    );
}
