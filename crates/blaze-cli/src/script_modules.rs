//! A build script's compiled `wasm64` (`memory64`) module, resolved and self-compiled on miss by
//! a `MacroModule` task (decision D115 (c); ONE mechanism with proc-macro modules in `macros.rs`,
//! rule 30).
//!
//! - **Hit**: returns a record whose `out` is the module's `wasm-sha256` (or `bin-sha256` for
//!   Tier-2 native host scripts) and whose payload is empty.
//! - **Miss (`RanAs::Wasm`)**: self-compiles the pure-Rust `build.rs` crate and its build-dep
//!   closure via `blaze-lir -> wasm64-blaze-wasip1` (`wasm64_compile::compile_build_script`, zero
//!   `rustc`), atomically publishes the content-addressed record to
//!   `<home>/.blaze/v2/build-script/modules/<key>`, and returns `Did::Done` immediately so wave 1
//!   `Script` tasks execute it in the same command.
//! - **Tier-2 (`RanAs::Native`) or unsupported native FFI**: fails with a clear, typed error
//!   naming the crate, version, features, and unsupported native requirement (rules 1 & 3: no
//!   fallback to `rustc`).

use blaze_drive::{Kind, Name, Task, TaskFailure, Why};
use blaze_ids::ContentId;
use bstd::cx::Cx;

use crate::CliError;
use crate::work::{Did, Made, held_str};

use crate::scripts::RanAs;

// "4": build scripts include optional `files <sha256>` (path packages) AND optional `deps <sha256>`
// (transitive build-dependencies, rule 21) so editing `build.rs` or any build-dependency
// invalidates the module lookup and distinct packages never collide.
const SUBJECT_DOMAIN: &str = "blaze-cli build-script module subject 4";
const SOURCE_DOMAIN: &str = "blaze-cli build-script module source 1";
const OUT_DOMAIN: &str = "blaze-cli build-script module out 2";

/// The rule a lookup's failure names.
pub(crate) const RULE: &str = "script_modules.rs";

/// What a build script's module is named by: the run's placement (D112: a wasm module or a
/// host binary), the script unit's package, version, optional path-package `files` digest,
/// optional transitive `deps` digest (rule 21), and features (sorted and without repeats, as the
/// graph holds them), each field length-prefixed.
pub(crate) fn subject(
    ran_as: RanAs,
    package: &str,
    version: &str,
    features: &[&str],
    files: Option<&str>,
    deps: Option<&str>,
) -> ContentId {
    let mut text = std::vec::Vec::new_in(std::alloc::Global);
    text.push(ran_as as u8);
    text.push(u8::from(files.is_some()));
    if let Some(hex) = files {
        text.extend_from_slice(&(hex.len() as u64).to_le_bytes());
        text.extend_from_slice(hex.as_bytes());
    }
    text.push(u8::from(deps.is_some()));
    if let Some(hex) = deps {
        text.extend_from_slice(&(hex.len() as u64).to_le_bytes());
        text.extend_from_slice(hex.as_bytes());
    }
    for field in [package, version].into_iter().chain(features.iter().copied()) {
        text.extend_from_slice(&(field.len() as u64).to_le_bytes());
        text.extend_from_slice(field.as_bytes());
    }
    ContentId::of(SUBJECT_DOMAIN, &text)
}

/// The lookup task's name for the module of `subject`.
pub(crate) fn name(subject: ContentId) -> Name {
    Name::new(Kind::MacroModule, subject)
}

/// The lookup's part source: the graph's source (which holds the module store's listing) and
/// the subject.
pub(crate) fn part_source(graph_source: ContentId, subject: ContentId) -> ContentId {
    let mut source = [0u8; 64];
    source[..32].copy_from_slice(&graph_source.0);
    source[32..].copy_from_slice(&subject.0);
    ContentId::of(SOURCE_DOMAIN, &source)
}

/// A module lookup a crate's script reads: its subject, the world index of the crate reading
/// it, and the script unit's package, version, features, and digests (from the graph, held to the
/// end of the command with the names world). Made in names.rs `world`.
pub(crate) struct ScriptModule {
    pub subject: ContentId,
    pub crate_at: usize,
    pub ran_as: RanAs,
    pub package: &'static str,
    pub version: &'static str,
    pub features: std::vec::Vec<&'static str, std::alloc::Global>,
    pub files: Option<std::string::String>,
    pub deps: Option<std::string::String>,
}

/// What one lookup is for: the run's placement and the script unit's package, version,
/// features, optional path-package `files` digest, and optional build-dependency `deps` digest.
pub(crate) struct Wanted<'w> {
    pub ran_as: RanAs,
    pub package: &'w str,
    pub version: &'w str,
    pub features: &'w [&'w str],
    pub files: Option<&'w str>,
    pub deps: Option<&'w str>,
}

/// One build-script module lookup & on-miss `wasm64` self-compile: looks up
/// `<home>/.blaze/v2/build-script/modules/<key>` matching `wanted`, and on a `RanAs::Wasm` miss
/// self-compiles the build script directly via `blaze-lir -> wasm64-blaze-wasip1` (zero `rustc`).
pub(crate) fn run<'b>(
    cx: &Cx,
    home: Option<&str>,
    wanted: Wanted<'_>,
    compile: Option<crate::wasm64_compile::CompileInputs<'_>>,
    task: &Task<'_>,
    held: &'b bstd::mem::Frame,
) -> Result<Did<'b>, CliError> {
    let _cx = cx.span("script module: look the module up");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    let Some(home) = home else {
        return Ok(Did::Hole(refused("no home: the module store is under it"), "the build-script module store is under the home, and HOME is not set"));
    };
    let store = std::path::Path::new(home).join(".blaze/v2");
    let Wanted { ran_as, package, version, features, files, deps } = wanted;
    let host = crate::graph::TRIPLE;
    let (found, digest_file) = match ran_as {
        RanAs::Wasm => (blaze_proc_macro::script_unit::module_of_files_and_deps(&store, package, version, features, files, deps), "wasm-sha256"),
        RanAs::Native => (blaze_proc_macro::script_unit::module_of_native_files(&store, package, version, features, host, files), "bin-sha256"),
    };
    let unread = |error: &blaze_proc_macro::script_unit::ModuleLookupError| {
        let words = bstd::mem::format_in!(held, "the build script of {package} {version}: {error}");
        Did::Hole(refused("the module store could not be read"), held_str(words.as_str(), held))
    };
    let found = match found {
        Ok(found) => found,
        Err(error) => return Ok(unread(&error)),
    };
    let Some(record) = found else {
        if ran_as == RanAs::Wasm
            && let Some(inputs) = compile
        {
            let _compile_cx = cx.span("script module: self-compile build.rs to wasm64");
            match crate::wasm64_compile::compile_build_script(&store, inputs, package, version, features) {
                Ok(wasm_sha256) => {
                    let mut made = Made::new_in(name, held);
                    made.out = ContentId::of(OUT_DOMAIN, wasm_sha256.trim().as_bytes());
                    return Ok(Did::Done(made));
                }
                Err(why) => {
                    let what = if why.is_tier2() {
                        "build-script requires Tier-2 native/host features unsupported in wasm64"
                    } else {
                        "build-script wasm64 self-compilation failed"
                    };
                    let words = bstd::mem::format_in!(
                        held,
                        "the build script of {package} {version}: wasm64 self-compilation failed: {why}"
                    );
                    return Ok(Did::FailedSaying(
                        refused(what),
                        held_str(words.as_str(), held),
                    ));
                }
            }
        }
        let asked = asked_lines(ran_as, package, version, features, host, files, deps);
        let nearest = match blaze_proc_macro::script_unit::records_named(&store, package, version) {
            Ok(nearest) => nearest,
            Err(error) => return Ok(unread(&error)),
        };
        let mut beside = std::string::String::new();
        match nearest.iter().min_by_key(|(_, identity)| differing(&asked, ran_as, identity.as_str()).count()) {
            None => beside.push_str(" the store holds no record of this package and version;"),
            Some((folder, identity)) => {
                let key = folder.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
                beside.push_str(&std::format!(" the nearest of the store's {} record(s) of {package} {version}, {key}, differs in:", nearest.len()));
                for line in differing(&asked, ran_as, identity) {
                    beside.push_str(&std::format!(" `{line}`"));
                }
                beside.push(';');
            }
        }
        let (what, how) = match ran_as {
            RanAs::Wasm => (
                "a build-script module could not be self-compiled (no on-disk checkout)",
                "wasm64 self-compilation requires an on-disk checkout (no rustc fallback, rules 1 & 3)",
            ),
            RanAs::Native => (
                "Tier-2 native build script requires host C/native toolchain (unsupported in wasm64)",
                "this build script is classified Tier-2 native (requires C compilation / host native tools for target host and cannot be self-compiled to wasm64; no rustc fallback, rules 1 & 3)",
            ),
        };
        let words = bstd::mem::format_in!(
            held,
            "the build script of {package} {version}: no build-script module of {package} {version} with features [{}] in {}: asked for the identity lines {};{beside} {how}",
            features.join(","),
            store.join("build-script/modules").display(),
            asked.iter().map(|line| std::format!("`{line}`")).collect::<std::vec::Vec<_>>().join(" "),
        );
        // Filed: the store's listing is in this part's source, so the failure stands until a
        // module comes or goes.
        return Ok(Did::FailedSaying(refused(what), held_str(words.as_str(), held)));
    };
    let digest_at = record.join(digest_file);
    let digest = match std::fs::read_to_string(&digest_at) {
        Ok(digest) => digest,
        Err(error) => {
            let words = bstd::mem::format_in!(held, "the build script of {package} {version}: its module's digest {}: {error}", digest_at.display());
            return Ok(Did::Hole(refused("the module store could not be read"), held_str(words.as_str(), held)));
        }
    };
    let mut made = Made::new_in(name, held);
    made.out = ContentId::of(OUT_DOMAIN, digest.trim().as_bytes());
    Ok(Did::Done(made))
}

/// The identity lines a lookup asks a record for (what `script_unit::module_where` matches):
/// name, version, the sorted features, optional `files` and `deps` digests, and for a native run
/// `target <host>`; for a wasm run, the process shim this binary answers (`script_unit::shim_line`),
/// so a module built with another is named by its `shim` line.
pub(crate) fn asked_lines(
    ran_as: RanAs,
    package: &str,
    version: &str,
    features: &[&str],
    host: &str,
    files: Option<&str>,
    deps: Option<&str>,
) -> std::vec::Vec<std::string::String> {
    let mut sorted = features.to_vec();
    sorted.sort_unstable();
    let mut lines = std::vec![std::format!("name {package}"), std::format!("version {version}"), std::format!("features {}", sorted.join(","))];
    if let Some(hex) = files {
        lines.push(std::format!("files {hex}"));
    }
    if let Some(hex) = deps {
        lines.push(std::format!("deps {hex}"));
    }
    match ran_as {
        RanAs::Native => lines.push(std::format!("target {host}")),
        RanAs::Wasm => lines.push(blaze_proc_macro::script_unit::shim_line()),
    }
    lines
}

/// What of `identity` keeps it from answering `asked`: each asked line it lacks, as the record
/// has that line instead (`features ..`, `target ..`) or `(no <line>)`; and, for a wasm ask, the
/// record's own `target` line (a native host binary is not a module).
pub(crate) fn differing<'i>(asked: &'i [std::string::String], ran_as: RanAs, identity: &'i str) -> impl Iterator<Item = std::string::String> + 'i {
    let lacking = asked.iter().filter(|want| !identity.lines().any(|line| line == want.as_str())).map(move |want| {
        let field = want.split(' ').next().unwrap_or_default();
        match identity.lines().find(|line| line.split(' ').next() == Some(field)) {
            Some(has) => std::format!("{has} (asked: {want})"),
            None => std::format!("no `{want}` line"),
        }
    });
    let native_record = identity.lines().find(|line| ran_as == RanAs::Wasm && line.starts_with("target ")).map(|has| std::format!("{has} (a native host binary; asked: a wasm module)"));
    lacking.chain(native_record)
}

#[cfg(test)]
mod tests {
    use super::{RanAs, asked_lines, differing, subject};

    /// tempo 2026-11-17: libproc's store record is a native host binary (`target` line). A wasm
    /// ask is refused on that line, a native ask is answered by it; nix's record lacks the
    /// `time` feature the ask names, and the refusal shows the record's own features line.
    #[test]
    fn a_lookup_names_the_line_that_differs() {
        let libproc = "blaze build-script module 3\nname libproc\nversion 0.14.11\nfeatures \ntoolchain nightly-2026-10-27\ntarget aarch64-apple-darwin\nbuilder bootstrap\n";
        let host = "aarch64-apple-darwin";
        let native = asked_lines(RanAs::Native, "libproc", "0.14.11", &[], host, None, None);
        assert_eq!(differing(&native, RanAs::Native, libproc).count(), 0);
        let shim = blaze_proc_macro::script_unit::shim_line();
        let wasm = asked_lines(RanAs::Wasm, "libproc", "0.14.11", &[], host, None, None);
        let said: std::vec::Vec<_> = differing(&wasm, RanAs::Wasm, libproc).collect();
        assert_eq!(said, [std::format!("no `{shim}` line"), "target aarch64-apple-darwin (a native host binary; asked: a wasm module)".to_owned()]);
        let nix = std::format!("blaze build-script module 3\nname nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\n{shim}\nbuilder bootstrap\n");
        let asked = asked_lines(RanAs::Wasm, "nix", "0.31.3", &["time", "default", "ioctl", "memoffset", "socket"], host, None, None);
        let said: std::vec::Vec<_> = differing(&asked, RanAs::Wasm, &nix).collect();
        assert_eq!(said, ["features default,ioctl,memoffset,socket (asked: features default,ioctl,memoffset,socket,time)"]);
        // A module built with another process shim (before `found`, guest/script_process.rs)
        // is named by that line: what its rebuild changes.
        let stale = "blaze build-script module 3\nname nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\nshim c1a4bffa83fc8f619c4daf0a3c0121cc1079b345b415a6d553a910a8e8a6ede6\nbuilder bootstrap\n";
        let asked = asked_lines(RanAs::Wasm, "nix", "0.31.3", &["default", "ioctl", "memoffset", "socket"], host, None, None);
        let said: std::vec::Vec<_> = differing(&asked, RanAs::Wasm, stale).collect();
        assert_eq!(said, [std::format!("shim c1a4bffa83fc8f619c4daf0a3c0121cc1079b345b415a6d553a910a8e8a6ede6 (asked: {shim})")]);
        assert_eq!(differing(&asked, RanAs::Wasm, &nix).count(), 0, "CONTROL: the record with this host's shim answers the ask");
        // A native and a wasm ask of one script are two lookups.
        assert_ne!(subject(RanAs::Native, "libproc", "0.14.11", &[], None, None), subject(RanAs::Wasm, "libproc", "0.14.11", &[], None, None));
        // Distinct path-package file digests and build-dep digests produce distinct lookup subjects (rule 21).
        assert_ne!(subject(RanAs::Wasm, "app", "0.1.0", &[], Some("aaa"), None), subject(RanAs::Wasm, "app", "0.1.0", &[], Some("bbb"), None));
        assert_ne!(subject(RanAs::Wasm, "app", "0.1.0", &[], Some("aaa"), Some("d1")), subject(RanAs::Wasm, "app", "0.1.0", &[], Some("aaa"), Some("d2")));
    }
}
