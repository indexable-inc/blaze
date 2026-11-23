//! The ONE filing of decision D111 for a proc macro's reads (rule 30): a workspace member's
//! macro whose run read a file outside the member's own directory and inside the workspace is
//! allowed, its read is in the read list (`file:<path from the served tree>`, blaze-proc-macro
//! sandbox.rs), and each such read is told to the sources ([`SourceTexts::left_package`]) as a
//! finding at the call. Every host that runs a proc macro files through [`file_left_reads`]:
//! the item host (resolve.rs `Host::run_proc_macro`, right after the run or the kept answer)
//! and the body host (blaze-ladder front2 `body_left`, from the body expander's per-call read
//! spans, blaze-lower `call_reads`). So no host can give the read without the finding, and
//! both give it byte for byte the same (the D96 lane found the body host giving none).
//!
//! What is filed is a function of the read list alone, never of whether the macro ran: a kept
//! answer carries the run's list, and gives the same finding. One finding per read (rule 77):
//! a file a run read twice is filed once; a host pass that sees the same call again (a body
//! lowered twice) files the same finding again, and the sink keeps one (blaze-cli
//! names/include_outside.rs `findings`: sorted by message and range, without repeats).

use core::ops::Range;

use blaze_expand::{ReadKind, Reads};
use blaze_own::{AString, Alloc};
use blaze_surface::CallShape;

use crate::include_place::{LeftPackage, served_tree};
use crate::resolve::SourceTexts;

/// Where a proc-macro call is written and how, as a D111 finding names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallAt<'p> {
    /// The calling file's path (absolute in the reader's path space, `/0/crates/m/src/lib.rs`);
    /// `""` when the call has no file (a body whose text is a macro's expansion): the finding
    /// is then placed at the crate, never dropped (blaze-cli `include_outside::findings`).
    pub from: &'p str,
    /// The byte in the calling file where the (topmost) call's path starts.
    pub at: u32,
    /// The macro's path as written at the call (`macros::sol`, `Abi`), never resolved.
    pub call: &'p str,
    /// The call's shape (`call!`, `#[call]`, `#[derive(call)]`).
    pub shape: CallShape,
}

/// Files, as D111 findings at `call`, each read of `reads.list()[span]` that a proc macro's run
/// made of a file outside the own directory of the workspace member at `krate`: see the
/// module's doc. The file is named by its path in the reader's space ([`served_tree`] joined
/// with the sandbox's tree-relative path, the form an include's finding has; the tree-relative
/// path itself when the texts and the environment disagree on the member's folder). A crate
/// with no environment, or a dependency, files nothing: its sandbox serves its package alone
/// and refuses a path leaving it by name. Returns how many findings were filed.
///
/// A `span` past the list's end is cut at its end: the list is the run's own, never another's.
pub fn file_left_reads<'a, S: SourceTexts<'a> + ?Sized>(sources: &S, krate: usize, call: CallAt<'_>, reads: &Reads<'_>, span: Range<usize>, alloc: Alloc<'_>) -> u32 {
    let Some(env) = sources.crate_env(krate) else { return 0 };
    let seen = env.seen();
    let tree = served_tree(env.read_folder(), seen);
    let list = reads.list();
    let span = span.start.min(list.len())..span.end.min(list.len());
    let mut filed = 0u32;
    for (index, read) in list[span.start..span.end].iter().enumerate() {
        if read.kind != ReadKind::ProcMacro {
            continue;
        }
        let Some(in_tree) = reads.subject(read).strip_prefix("file:") else { continue };
        if !seen.left_package(in_tree) {
            continue;
        }
        // One finding per read (rule 77): a file this run read before is filed once.
        let before = &list[span.start..span.start + index];
        if before.iter().any(|it| it.kind == ReadKind::ProcMacro && reads.subject(it).strip_prefix("file:") == Some(in_tree)) {
            continue;
        }
        let mut path = AString::new_in(alloc);
        if let Some(tree) = tree {
            path.push_str(tree);
            path.push('/');
        }
        path.push_str(in_tree);
        sources.left_package(krate, LeftPackage { from: call.from, at: call.at, call: call.call, shape: call.shape, path: &path });
        filed += 1;
    }
    filed
}

#[cfg(test)]
mod tests {
    //! [`file_left_reads`] alone: a member's macro reading a sibling's file gives one finding
    //! (the file read twice, still one); its own file, an env read and a dependency's crate
    //! give none. Expected (`lanecheck.sh blaze-scope test left_reads`): passes, prints nothing.

    use std::string::{String, ToString};
    use std::sync::Mutex;
    use std::vec::Vec;

    use blaze_expand::seen::SeenRoot;
    use blaze_expand::{ReadAnswer, ReadKind, Reads};
    use blaze_own::Alloc;
    use blaze_surface::CallShape;

    use super::{CallAt, file_left_reads};
    use crate::crate_env::{CrateEnv, EnvFacts};
    use crate::include_place::LeftPackage;
    use crate::resolve::SourceTexts;

    /// Crate 0 is the member `crates/m`; crate 1 has no environment.
    struct Member {
        env: CrateEnv<'static>,
        told: Mutex<Vec<(String, u32, String, CallShape, String)>>,
    }

    impl SourceTexts<'static> for Member {
        fn read(&self, _: usize, _: &str) -> Option<&'static str> {
            None
        }
        fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'static>> {
            (krate == 0).then_some(&self.env)
        }
        fn left_package(&self, _: usize, left: LeftPackage<'_>) {
            let told = (left.from.to_string(), left.at, left.call.to_string(), left.shape, left.path.to_string());
            self.told.lock().unwrap_or_else(|p| panic!("told: {p}")).push(told);
        }
    }

    #[test]
    fn left_reads_one_finding_per_read_outside_the_member() {
        let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
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
        let member = Member { env, told: Mutex::new(Vec::new()) };
        let mut reads = Reads::new_in(alloc);
        for subject in ["env:CARGO_MANIFEST_DIR", "file:crates/m/abi/own.json", "file:crates/sibling/x.json", "file:crates/sibling/x.json"] {
            reads.push(ReadKind::ProcMacro, subject, ReadAnswer::Absent).unwrap_or_else(|e| panic!("push: {e:?}"));
        }
        let call = CallAt { from: "/0/crates/m/src/lib.rs", at: 40, call: "macros::sol", shape: CallShape::FnLike };
        let all = 0..reads.list().len();
        assert_eq!(file_left_reads(&member, 0, call, &reads, all.clone(), alloc), 1, "the sibling's file, read twice, is one finding");
        let told = member.told.lock().unwrap_or_else(|p| panic!("told: {p}")).clone();
        assert_eq!(told, [("/0/crates/m/src/lib.rs".to_string(), 40, "macros::sol".to_string(), CallShape::FnLike, "/0/crates/sibling/x.json".to_string())]);
        assert_eq!(file_left_reads(&member, 0, call, &reads, 0..2, alloc), 0, "its own file and an env read are no finding");
        assert_eq!(file_left_reads(&member, 1, call, &reads, all, alloc), 0, "a crate with no environment files nothing");
    }
}
