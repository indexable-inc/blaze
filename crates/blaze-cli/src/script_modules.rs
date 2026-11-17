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

const SUBJECT_DOMAIN: &str = "blaze-cli build-script module subject 1";
const SOURCE_DOMAIN: &str = "blaze-cli build-script module source 1";
const OUT_DOMAIN: &str = "blaze-cli build-script module out 1";

/// The rule a lookup's failure names.
pub(crate) const RULE: &str = "script_modules.rs";

/// What a build script's module is named by: the script unit's package, version and features
/// (sorted and without repeats, as the graph holds them), each field length-prefixed.
pub(crate) fn subject(package: &str, version: &str, features: &[&str]) -> ContentId {
    let mut text = std::vec::Vec::new_in(std::alloc::Global);
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
    pub package: &'static str,
    pub version: &'static str,
    pub features: std::vec::Vec<&'static str, std::alloc::Global>,
}

/// What one lookup is for: the script unit's package, version and features.
pub(crate) struct Wanted<'w> {
    pub package: &'w str,
    pub version: &'w str,
    pub features: &'w [&'w str],
}

/// One build-script module lookup: the record `<home>/.blaze/v2/build-script/modules/<key>`
/// whose identity names `wanted`, its out the module's `wasm-sha256`.
pub(crate) fn run<'b>(cx: &Cx, home: Option<&str>, wanted: Wanted<'_>, task: &Task<'_>, held: &'b bstd::mem::Frame) -> Result<Did<'b>, CliError> {
    let _cx = cx.span("script module: look the module up");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    let Some(home) = home else {
        return Ok(Did::Hole(refused("no home: the module store is under it"), "the build-script module store is under the home, and HOME is not set"));
    };
    let store = std::path::Path::new(home).join(".blaze/v2");
    let Wanted { package, version, features } = wanted;
    let Some(record) = blaze_proc_macro::script_unit::module_of(&store, package, version, features) else {
        let words = bstd::mem::format_in!(
            held,
            "the build script of {package} {version}: no build-script module of {package} {version} with features [{}] in {}: build it with `toolrun.sh proc_macro_modules` in build-script mode",
            features.join(","),
            store.join("build-script/modules").display(),
        );
        // Filed: the store's listing is in this part's source, so the failure stands until a
        // module comes or goes.
        return Ok(Did::FailedSaying(refused("a build-script module the store lacks"), held_str(words.as_str(), held)));
    };
    let digest_at = record.join("wasm-sha256");
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
