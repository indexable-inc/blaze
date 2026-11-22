//! A build script's compiled module, looked up by a `MacroModule` task (piece 2c; the
//! coordinator's ruling D115 (c), 2026-11-16 21:1x): ONE mechanism with the proc-macro modules
//! (macros.rs, rule 30). The kind and the record are the same: a lookup whose out is the
//! module's wasm digest and whose payload is empty. Only the subject differs: a build script's
//! module is named by its package, version and the script unit's features (what the bootstrap
//! builder's identity names: blaze-proc-macro script_unit.rs `module_of`), in a domain of its
//! own, so no proc-macro crate's subject can equal it.
//!
//! - NAMED with the `Script` parts, in the `UnitGraph` job (work.rs), one per module the
//!   world's scripts read ([`crate::names::Names::script_modules`]); its source is the graph's
//!   source with the subject ([`part_source`]), as a proc-macro module's is: the graph's source
//!   holds the module store's listing ([`crate::scripts::MODULE_LISTING`]), so a module that
//!   comes, goes or is rebuilt asks the lookup again, and nothing else does.
//! - READ by the `Script` task (its job Needs it): its out is in the `Script`'s read list, so one
//!   rebuilt script module reruns one `Script`, and an equal module moves none (early cutoff),
//!   while the `Script`'s own source holds no listing.
//! - ABSENT: a filed failure naming the script and the command that builds the module; the
//!   `Script` fails `Waited` on it and its crate is named not checked (not_checked.rs).
//! - Piece 2d (`ModuleStale`) goes in [`run`] once `key::content_of` is green: the record's
//!   `identity` file holds the built-from `files <sha256>` line for a path package.
//!
//! Interim (D110): the store is scanned on the filesystem as the runtime scans it; one scan a
//! lookup, cached by the lookup's record.

use blaze_drive::{Kind, Name, Task, TaskFailure, Why};
use blaze_ids::ContentId;
use bstd::cx::Cx;

use crate::CliError;
use crate::work::{Did, Made, held_str};

use crate::scripts::RanAs;

// "2": the subject holds the run's placement (a native script's record is a host binary,
// `target <host>` in its identity and `bin-sha256` its digest; tempo 2026-11-17: libproc,
// reth-mdbx-sys and librocksdb-sys, placed native, were looked up as wasm modules and refused
// while their native records stood in the store).
const SUBJECT_DOMAIN: &str = "blaze-cli build-script module subject 2";
const SOURCE_DOMAIN: &str = "blaze-cli build-script module source 1";
const OUT_DOMAIN: &str = "blaze-cli build-script module out 2";

/// The rule a lookup's failure names.
pub(crate) const RULE: &str = "script_modules.rs";

/// What a build script's module is named by: the run's placement (D112: a wasm module or a
/// host binary), the script unit's package, version and features (sorted and without repeats,
/// as the graph holds them), each field length-prefixed.
pub(crate) fn subject(ran_as: RanAs, package: &str, version: &str, features: &[&str]) -> ContentId {
    let mut text = std::vec::Vec::new_in(std::alloc::Global);
    text.push(ran_as as u8);
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
/// it, and the script unit's package, version and features (from the graph, held to the end
/// of the command with the names world). Made in names.rs `world`.
pub(crate) struct ScriptModule {
    pub subject: ContentId,
    pub crate_at: usize,
    pub ran_as: RanAs,
    pub package: &'static str,
    pub version: &'static str,
    pub features: std::vec::Vec<&'static str, std::alloc::Global>,
}

/// What one lookup is for: the run's placement and the script unit's package, version and
/// features.
pub(crate) struct Wanted<'w> {
    pub ran_as: RanAs,
    pub package: &'w str,
    pub version: &'w str,
    pub features: &'w [&'w str],
}

/// One build-script module lookup: the record `<home>/.blaze/v2/build-script/modules/<key>`
/// whose identity names `wanted`: for a wasm run a module (no `target` line), its out the
/// module's `wasm-sha256`; for a native run (D112) the host binary built for the pinned host
/// (`target <host>`), its out the binary's `bin-sha256`. The same record the `Script` run
/// then reads (blaze-proc-macro `script_unit::prepare` or `native_unit::native_outputs`).
pub(crate) fn run<'b>(cx: &Cx, home: Option<&str>, wanted: Wanted<'_>, task: &Task<'_>, held: &'b bstd::mem::Frame) -> Result<Did<'b>, CliError> {
    let _cx = cx.span("script module: look the module up");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    let Some(home) = home else {
        return Ok(Did::Hole(refused("no home: the module store is under it"), "the build-script module store is under the home, and HOME is not set"));
    };
    let store = std::path::Path::new(home).join(".blaze/v2");
    let Wanted { ran_as, package, version, features } = wanted;
    let host = crate::graph::TRIPLE;
    let (found, digest_file) = match ran_as {
        RanAs::Wasm => (blaze_proc_macro::script_unit::module_of(&store, package, version, features), "wasm-sha256"),
        RanAs::Native => (blaze_proc_macro::script_unit::module_of_native(&store, package, version, features, host), "bin-sha256"),
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
        let asked = asked_lines(ran_as, package, version, features, host);
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
        let how = match ran_as {
            RanAs::Wasm => "build it with `toolrun.sh proc_macro_modules` in build-script mode",
            RanAs::Native => "build it with `toolrun.sh proc_macro_modules` in build-script mode with `native` (D112)",
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
        return Ok(Did::FailedSaying(refused("a build-script module the store lacks"), held_str(words.as_str(), held)));
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
/// name, version, the sorted features and, for a native run, `target <host>`; for a wasm run,
/// the process shim this binary answers (`script_unit::shim_line`), so a module built with
/// another is named by its `shim` line.
pub(crate) fn asked_lines(ran_as: RanAs, package: &str, version: &str, features: &[&str], host: &str) -> std::vec::Vec<std::string::String> {
    let mut sorted = features.to_vec();
    sorted.sort_unstable();
    let mut lines = std::vec![std::format!("name {package}"), std::format!("version {version}"), std::format!("features {}", sorted.join(","))];
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
        let native = asked_lines(RanAs::Native, "libproc", "0.14.11", &[], host);
        assert_eq!(differing(&native, RanAs::Native, libproc).count(), 0);
        let shim = blaze_proc_macro::script_unit::shim_line();
        let wasm = asked_lines(RanAs::Wasm, "libproc", "0.14.11", &[], host);
        let said: std::vec::Vec<_> = differing(&wasm, RanAs::Wasm, libproc).collect();
        assert_eq!(said, [std::format!("no `{shim}` line"), "target aarch64-apple-darwin (a native host binary; asked: a wasm module)".to_owned()]);
        let nix = std::format!("blaze build-script module 3\nname nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\n{shim}\nbuilder bootstrap\n");
        let asked = asked_lines(RanAs::Wasm, "nix", "0.31.3", &["time", "default", "ioctl", "memoffset", "socket"], host);
        let said: std::vec::Vec<_> = differing(&asked, RanAs::Wasm, &nix).collect();
        assert_eq!(said, ["features default,ioctl,memoffset,socket (asked: features default,ioctl,memoffset,socket,time)"]);
        // A module built with another process shim (before `found`, guest/script_process.rs)
        // is named by that line: what its rebuild changes.
        let stale = "blaze build-script module 3\nname nix\nversion 0.31.3\nfeatures default,ioctl,memoffset,socket\nshim c1a4bffa83fc8f619c4daf0a3c0121cc1079b345b415a6d553a910a8e8a6ede6\nbuilder bootstrap\n";
        let asked = asked_lines(RanAs::Wasm, "nix", "0.31.3", &["default", "ioctl", "memoffset", "socket"], host);
        let said: std::vec::Vec<_> = differing(&asked, RanAs::Wasm, stale).collect();
        assert_eq!(said, [std::format!("shim c1a4bffa83fc8f619c4daf0a3c0121cc1079b345b415a6d553a910a8e8a6ede6 (asked: {shim})")]);
        assert_eq!(differing(&asked, RanAs::Wasm, &nix).count(), 0, "CONTROL: the record with this host's shim answers the ask");
        // A native and a wasm ask of one script are two lookups.
        assert_ne!(subject(RanAs::Native, "libproc", "0.14.11", &[]), subject(RanAs::Wasm, "libproc", "0.14.11", &[]));
    }
}
