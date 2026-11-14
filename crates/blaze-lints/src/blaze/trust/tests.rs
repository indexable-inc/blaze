//! The trust lints on fixtures (a workspace crate `main` over a dependency `left_pad`, marked
//! `library`, over a stand-in std whose functions the walk models by path), the grants table,
//! and the attribution core against a brute-force model.

use std::alloc::Global;

use super::{
    attribute::{Graph, Node, TO_CALLER, attribute},
    capability::{self, Capability, Caps},
    facts::{Dep, Used, Workspace},
    findings,
    grants::Grants,
    table::grants,
};
use crate::{
    conf::Conf,
    blaze::{
        Lint,
        effects_tests::{MINICORE, minicore_with_dyn},
    },
    tests::{CLIPPY_OFF, lint_with},
};

/// The toolchain the fixtures build on: a std with a file read, a socket and a static the
/// walk's models know by path.
const STD: &str = r#"
//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod fs {
    pub fn read(_path: &str) -> u32 {
        loop {}
    }
}
pub mod net {
    pub struct TcpStream;
    impl TcpStream {
        pub fn connect(_to: &str) -> TcpStream {
            loop {}
        }
    }
}
"#;

const DEP_META: &str = "crate:left_pad@1.4.0,https://example.com/left_pad library deps:std";

/// The manifest every fixture's grants are read as written in.
const MANIFEST: &str = "Cargo.toml";

/// The flags of every fixture: clippy and blaze's other lints off, the group on at its defaults.
fn flags() -> String {
    format!("{CLIPPY_OFF} -A blaze::general -A blaze::internal -W blaze::trust -D blaze::dependency_capability -D blaze::dependency_unsafe")
}

/// What the lints say of the workspace crate `main` (with `main_deps` after `std`) over the
/// dependency `left_pad` written `dep`, under the root manifest's text `manifest`.
fn run_over(minicore: &str, dep: &str, main: &str, main_deps: &str, manifest: &str) -> String {
    let fixture = format!(
        "{minicore}{STD}//- /left_pad.rs {DEP_META}\n{dep}\n//- /src/lib.rs crate:main deps:std,left_pad{main_deps}\n{main}\n"
    );
    let grants = Grants::of_text(MANIFEST, manifest).expect("the fixture's manifest reads");
    lint_with(&fixture, &flags(), Conf { trust: Some(grants), ..Conf::default() })
}

fn run(dep: &str, main: &str, manifest: &str) -> String {
    run_over(MINICORE, dep, main, "", manifest)
}

fn table(entries: &str) -> String {
    format!("[workspace]\n[workspace.metadata.blaze.trust]\n{entries}\n")
}

const READS: &str = "pub fn config() -> u32 { std::fs::read(\"/etc/passwd\") }";
const CALLS_READ: &str = "pub fn go() -> u32 { left_pad::config() }";

fn lines<'o>(out: &'o str, what: &str) -> Vec<&'o str> {
    out.lines().filter(|it| it.contains(what)).collect()
}

/// A dependency that reads a file the workspace reaches is denied, with the chain from the
/// workspace's function to the call; granted `fs` it passes. (The grant is the control.)
#[test]
fn a_file_read_is_denied_until_granted() {
    let out = run(READS, CALLS_READ, &table("other = { allow = [], reason = \"r\" }"));
    let denied = lines(&out, "dependency `left_pad` 1.4.0 touches files (`fs`)");
    assert_eq!(denied.len(), 1, "{out}");
    assert!(denied[0].starts_with("error"), "{out}");
    assert!(out.contains("`main::go` → `left_pad::config`"), "{out}");
    assert!(out.contains("std::fs::read"), "{out}");
    let out = run(READS, CALLS_READ, &table("left_pad = { allow = [\"fs\"], reason = \"reads its config\" }"));
    assert_eq!(lines(&out, "dependency `left_pad`"), Vec::<&str>::new(), "{out}");
}

/// An `unsafe` block anywhere in a dependency is `dependency_unsafe`, reached or not.
#[test]
fn an_unsafe_block_is_denied() {
    let dep = "pub fn pad() -> u32 { 1 }\nfn hidden() -> u32 { unsafe { 2 } }";
    let out = run(dep, "pub fn go() -> u32 { left_pad::pad() }", &table(""));
    let denied = lines(&out, "runs `unsafe` code (`unsafe`)");
    assert_eq!(denied.len(), 1, "{out}");
    assert!(denied[0].starts_with("error"), "{out}");
    assert!(out.contains("lints.md#dependency_unsafe"), "{out}");
    assert!(out.contains("`left_pad::hidden` has an `unsafe` block"), "{out}");
}

/// A dependency's file read that the workspace does not reach is a warning of its own, not a
/// denial; reached, it is the denial. (The second run is the control.)
#[test]
fn code_nothing_reaches_is_a_warning() {
    let out = run(READS, "pub fn go() -> u32 { 1 }", &table(""));
    let warned = lines(&out, "which nothing the workspace builds reaches yet");
    assert_eq!(warned.len(), 1, "{out}");
    assert!(warned[0].starts_with("warning"), "{out}");
    assert_eq!(lines(&out, "touches files (`fs`), and").len(), 0, "{out}");
    let out = run(READS, CALLS_READ, &table(""));
    assert_eq!(lines(&out, "which nothing the workspace builds reaches yet").len(), 0, "{out}");
}

/// A grant pinned to another version grants nothing: the new version is denied, the finding
/// names both versions, and the stale pin is an unused grant.
#[test]
fn a_new_version_ends_the_grant() {
    let pinned = table("left_pad = { allow = [\"fs\"], reason = \"r\", version = \"=1.3.0\" }");
    let out = run(READS, CALLS_READ, &pinned);
    assert_eq!(lines(&out, "is for version 1.3.0 only, and the build has 1.4.0 now").len(), 1, "{out}");
    assert_eq!(lines(&out, "grants `left_pad` at version 1.3.0 only").len(), 1, "{out}");
    let exact = table("left_pad = { allow = [\"fs\"], reason = \"r\", version = \"=1.4.0\" }");
    assert_eq!(lines(&run(READS, CALLS_READ, &exact), "left_pad").len(), 0);
}

/// A grant of a capability its dependency no longer uses, or of a package that is no
/// dependency, is `trust_grant_unused`.
#[test]
fn a_grant_not_needed_is_unused() {
    let manifest = table(
        "left_pad = { allow = [\"fs\", \"net\"], reason = \"r\" }\ngone = { allow = [], reason = \"r\" }",
    );
    let out = run(READS, CALLS_READ, &manifest);
    assert_eq!(lines(&out, "grants `left_pad` `net`, which it does not use").len(), 1, "{out}");
    assert_eq!(lines(&out, "grants `gone`, which is no dependency").len(), 1, "{out}");
    assert_eq!(lines(&out, "`fs`, which it does not use").len(), 0, "{out}");
}

/// Two dependencies that share one static each keep global state: the capability is each
/// writer's own, not only the crate that declares it.
#[test]
fn two_dependencies_sharing_a_static_both_keep_global_state() {
    let fixture = format!(
        "{MINICORE}{STD}\
         //- /cell.rs crate:cell@0.1.0,https://example.com/cell library deps:std\n\
         pub static mut COUNT: u32 = 0;\n\
         //- /left_pad.rs {DEP_META},cell\n\
         pub fn bump() {{ unsafe {{ cell::COUNT += 1 }} }}\n\
         //- /right_pad.rs crate:right_pad@2.0.0,https://example.com/right_pad library deps:std,cell\n\
         pub fn bump() {{ unsafe {{ cell::COUNT += 2 }} }}\n\
         //- /src/lib.rs crate:main deps:std,left_pad,right_pad\n\
         pub fn go() {{ left_pad::bump(); right_pad::bump() }}\n"
    );
    let grants = Grants::of_text(MANIFEST, &table("")).expect("reads");
    let out = lint_with(&fixture, &flags(), Conf { trust: Some(grants), ..Conf::default() });
    assert_eq!(lines(&out, "dependency `left_pad` 1.4.0 keeps global state").len(), 1, "{out}");
    assert_eq!(lines(&out, "dependency `right_pad` 2.0.0 keeps global state").len(), 1, "{out}");
}

/// A call through a `dyn` the walk cannot resolve counts as every capability, and the finding
/// says what could not be seen.
#[test]
fn an_unresolved_dyn_is_unknown_and_denied() {
    let dep = "pub trait Hook { fn run(&self) -> u32; }\npub fn fire(hook: &dyn Hook) -> u32 { hook.run() }";
    let main = "pub fn go(hook: &dyn left_pad::Hook) -> u32 { left_pad::fire(hook) }";
    let out = run_over(&minicore_with_dyn(), dep, main, "", &table(""));
    let unseen = lines(&out, "which blaze could not see into, so it counts as every capability");
    assert!(!unseen.is_empty(), "{out}");
    assert!(lines(&out, "opens a socket (`net`)").iter().all(|it| it.starts_with("error")), "{out}");
    assert!(!lines(&out, "opens a socket (`net`)").is_empty(), "{out}");
}

/// With no table, the workspace is told once, naming `blaze audit grants`, however many
/// dependencies use what.
#[test]
fn no_table_is_one_finding() {
    let dep = format!("{READS}\npub fn call() {{ let _ = std::net::TcpStream::connect(\"x\"); }}");
    let main = "pub fn go() -> u32 { left_pad::call(); left_pad::config() }";
    let out = run(&dep, main, "[workspace]\n");
    assert_eq!(lines(&out, "run `blaze audit grants`").len(), 1, "{out}");
    assert_eq!(lines(&out, "dependency `left_pad`").len(), 0, "{out}");
}

/// A workspace crate with `unsafe` code is asked to say why, and says it at its root.
#[test]
fn workspace_unsafe_asks_for_a_reason() {
    let flags = format!("{} -W blaze::workspace_unsafe", flags());
    let code = "pub fn f() -> u32 { unsafe { 1 } }";
    let fixture = |head: &str| format!("{MINICORE}{STD}//- /src/lib.rs crate:main deps:std\n{head}{code}\n");
    let out = lint_with(&fixture(""), &flags, Conf::default());
    assert_eq!(lines(&out, "of the workspace runs `unsafe` code").len(), 1, "{out}");
    let waived = "#![blaze::trust(unsafe, reason = \"the ring buffer\")]\n";
    let out = lint_with(&fixture(waived), &flags, Conf::default());
    assert_eq!(lines(&out, "of the workspace runs `unsafe` code").len(), 0, "{out}");
}

/// The table `blaze audit grants` prints: a row per dependency that uses anything, witnesses
/// as comments, the version pinned, the reason left to the person; and it reads back as grants
/// under which the same facts give no finding but the empty reasons.
#[test]
fn the_grants_table_is_what_is_used_today() {
    let used = |capability, witness: &str| Used { capability, witness: witness.into() };
    let workspace = Workspace {
        deps: Box::new([
            Dep {
                name: "left-pad".into(),
                version: Some("1.4.0".into()),
                uses: Box::new([used(Capability::Fs, "`main::go` → `left_pad::config` calls `std::fs::read`")]),
                unreached: Box::new([]),
            },
            Dep { name: "quiet".into(), version: Some("0.1.0".into()), uses: Box::new([]), unreached: Box::new([]) },
        ]),
    };
    let toml = grants(&workspace).toml();
    assert_eq!(
        toml,
        "[workspace.metadata.blaze.trust]\n\
         # left-pad 1.4.0: fs: `main::go` → `left_pad::config` calls `std::fs::read`\n\
         left-pad = { allow = [\"fs\"], reason = \"\", version = \"=1.4.0\" }\n"
    );
    let refused = Grants::of_text(MANIFEST, &format!("[workspace]\n{toml}"));
    assert!(refused.is_err_and(|it| it.contains("has no `reason`")));
    let given = Grants::of_text(MANIFEST, &format!("[workspace]\n{}", toml.replace("reason = \"\"", "reason = \"pads\""))).expect("reads");
    assert_eq!(findings(&workspace, &given), Vec::new());
}

/// Every [`hir_ty_pure::Rule`] is placed on purpose: mapped to a capability, counted as unseen,
/// or listed as none.
#[test]
fn every_rule_is_placed() {
    let placed = capability::placed();
    for rule in hir_ty_pure::EffectSet::of(&hir_ty_pure::Rule::ALL).iter() {
        assert!(
            placed.has(rule) != capability::NONE.has(rule),
            "{rule:?} is placed {} times",
            u8::from(placed.has(rule)) + u8::from(capability::NONE.has(rule))
        );
    }
}

// The attribution core against a model.

/// The model: per (crate, capability) of a crate outside the workspace, the fewest calls from
/// a root to a node of that crate that uses it, by a breadth-first distance of each node.
fn model(graph: &Graph) -> Vec<(u32, Capability, usize)> {
    let count = graph.nodes.len();
    let mut distance: Vec<Option<usize>> = Vec::with_capacity_in(count, Global);
    distance.resize(count, None);
    let mut frontier: Vec<usize> = Vec::new_in(Global);
    for root in 0..(graph.roots as usize).min(count) {
        distance[root] = Some(0);
        frontier.push(root);
    }
    let mut step = 0;
    while !frontier.is_empty() {
        step += 1;
        let mut next: Vec<usize> = Vec::new_in(Global);
        for &at in &frontier {
            for &callee in &graph.nodes[at].calls {
                let callee = callee as usize;
                if callee < count && distance[callee].is_none() {
                    distance[callee] = Some(step);
                    next.push(callee);
                }
            }
        }
        frontier = next;
    }
    let mut out: Vec<(u32, Capability, usize)> = Vec::new_in(Global);
    for (at, node) in graph.nodes.iter().enumerate() {
        let Some(far) = distance[at] else { continue };
        if graph.members[node.krate as usize] {
            continue;
        }
        for (capability, _) in &node.uses {
            match out.iter_mut().find(|it| it.0 == node.krate && it.1 == *capability) {
                Some(kept) => kept.2 = kept.2.min(far),
                None => out.push((node.krate, *capability, far)),
            }
        }
    }
    out.sort();
    out
}

/// Whether `attribute`'s answer for `graph` is the model's, each witness a real chain: from a
/// root along calls to a node of its crate that uses its capability, as short as the model's.
fn agrees(graph: &Graph) -> Result<(), String> {
    let found = attribute(graph);
    let said: Vec<(u32, Capability, usize)> = {
        let mut said = Vec::new_in(Global);
        said.extend(found.iter().map(|it| (it.krate, it.capability, it.chain.len() - 1)));
        said
    };
    let model = model(graph);
    if said != model {
        return Err(format!("attributed {said:?}, the model {model:?}"));
    }
    for it in &found {
        let first = it.chain[0];
        if first >= graph.roots {
            return Err(format!("{it:?} does not start at a root"));
        }
        for pair in it.chain.windows(2) {
            if !graph.nodes[pair[0] as usize].calls.contains(&pair[1]) {
                return Err(format!("{it:?}: {} does not call {}", pair[0], pair[1]));
            }
        }
        let last = &graph.nodes[*it.chain.last().expect("a chain has its node") as usize];
        if last.krate != it.krate || !last.uses.iter().any(|use_| use_.0 == it.capability) {
            return Err(format!("{it:?} ends at a node that does not use it in its crate"));
        }
    }
    Ok(())
}

/// The bounds of a generated graph: enough crates and nodes for every shape of chain, few
/// enough that a case runs in microseconds.
const CRATES: u32 = 4;
const NODES: u32 = 12;
const CALLS: usize = 4;
const USES: usize = 2;

fn graph_of(tc: &hegel::TestCase) -> Graph {
    use hegel::generators as gs;
    let members: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).min_size(CRATES as usize).max_size(CRATES as usize));
    let count: u32 = tc.draw(gs::integers().min_value(1).max_value(NODES));
    let roots: u32 = tc.draw(gs::integers().min_value(1).max_value(count));
    let mut nodes: Vec<Node> = Vec::with_capacity_in(count as usize, Global);
    for _ in 0..count {
        let krate: u32 = tc.draw(gs::integers().max_value(CRATES - 1));
        let calls: Vec<u32> = tc.draw(gs::vecs(gs::integers().max_value(count - 1)).max_size(CALLS));
        let uses: Vec<u8> = tc.draw(gs::vecs(gs::integers().max_value(Capability::ALL.len() as u8 - 1)).max_size(USES));
        let uses: Vec<(Capability, Box<str>)> =
            uses.into_iter().map(|it| (Capability::ALL[it as usize], "does it".into())).collect();
        nodes.push(Node { krate, uses: uses.into_boxed_slice(), calls: calls.into_boxed_slice() });
    }
    Graph { members: members.into_boxed_slice(), roots, nodes: nodes.into_boxed_slice() }
}

/// The invariant: a capability a reached node uses is its own crate's, never its caller's,
/// once per (crate, capability) of a crate outside the workspace, with a witness that is a
/// real chain of calls from a root and the shortest there is.
#[hegel::test]
fn attribution_is_the_models(tc: hegel::TestCase) {
    let graph = graph_of(&tc);
    if let Err(why) = agrees(&graph) {
        panic!("{why}: {graph:?}");
    }
}

/// The control: with the bug planted (a capability attributed to the caller's crate), the
/// oracle catches it on a workspace function that calls a dependency's file read.
#[test]
fn the_oracle_catches_attribution_to_the_caller() {
    let graph = Graph {
        members: Box::new([true, false, false]),
        roots: 1,
        nodes: Box::new([
            Node { krate: 0, uses: Box::new([]), calls: Box::new([1]) },
            Node { krate: 1, uses: Box::new([]), calls: Box::new([2]) },
            Node { krate: 2, uses: Box::new([(Capability::Fs, "reads".into())]), calls: Box::new([]) },
        ]),
    };
    assert_eq!(agrees(&graph), Ok(()));
    TO_CALLER.set(true);
    let planted = agrees(&graph);
    TO_CALLER.set(false);
    assert!(planted.is_err(), "the oracle misses the planted bug");
}

/// A grant's `allow` is the one way to name a capability: every word reads back.
#[test]
fn every_capability_word_reads_back() {
    for capability in Capability::ALL {
        assert_eq!(Capability::of_word(capability.word()), Some(capability));
    }
    assert_eq!(Caps::of(&Capability::ALL).iter().count(), Capability::ALL.len());
}

/// The no-table finding is the dependency-capability lint, told once.
#[test]
fn no_table_findings_are_one() {
    let used = |capability| Used { capability, witness: "w".into() };
    let dep = |name: &str| Dep {
        name: name.into(),
        version: None,
        uses: Box::new([used(Capability::Fs), used(Capability::Net)]),
        unreached: Box::new([]),
    };
    let workspace = Workspace { deps: Box::new([dep("a"), dep("b")]) };
    let none = Grants::of_text(MANIFEST, "[workspace]\n").expect("reads");
    let found = findings(&workspace, &none);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].lint, Lint::DependencyCapability);
    assert!(found[0].message.contains("2 of the workspace's dependencies"), "{found:?}");
}
