//! D112 interim, brief N7 (main's ruling 2026-11-16): a native build script's machine probes
//! are answered from the command's fact pins by the multi-call fact shim
//! ([`blaze_build_script::fact_shim`]), the same pins the wasm path answers from
//! (`SpawnSetting.pins`, [`crate::fact_pins`]): one seam, both runners.
//!
//! - [`place_shim`]: at the command's edge, the shim binary (`blaze-fact-shim`, beside the
//!   `blaze` binary) copied once into the command's native work folder, where every run finds
//!   it ([`placed_shim`]); a run with no shim there is refused by name, never run without it.
//! - [`prepare`]: each run gets its own copy of the shim, the `bin/<program>` links to it,
//!   the pins file, and the writable `log/`.
//! - [`reads`]: after the run, each question the shim logged as one of the run's reads (its
//!   subject in `facts::subject`'s form). A pkg-config receipt carries the digest of the reply
//!   the shim delivered. Other probes derive it here from the pins ([`crate::fact_pins::fact_now`]),
//!   so whatever checks a script record's reads against today's facts
//!   ([`crate::fact_pins::read_now`] with the command's pins) treats both runners alike.
//! - [`shim_main`]: the shim binary's whole body.
//!
//! Interim (D112): native runs and their records stay local; the shim goes when native scripts
//! run in the deterministic sandbox. Plain `std::fs` here, as native_unit's folders: these are
//! the native runner's own folders on this machine, never a record's bytes.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use blaze_build_script::fact_shim::{self, BIN_DIR, FOLDER_VAR, LOG_DIR, LOG_FILE, PINS_FILE, PROGRAMS, REFUSED_CODE, SHIM_NAME};
use blaze_ids::ContentId;
use blaze_own::{Alloc, Heap, String, Vec};

use crate::{
    fact_pins::{FactNow, fact_now},
    script_sandbox::{Read, file_answer},
    spawn_route::{FactPins, reply_digest},
};

/// Where the shim is placed under a command's native work folder.
pub const SHIM_IN_WORK: &str = "fact-shim";

/// Local script-read subject for the shim deployed beside blaze. The placement in the
/// command's work folder varies between runs and is never the identity of an input.
pub const IMAGE_SUBJECT: &str = "native-shim-image";

/// The entire read-only pins file the runner gives its shim. A native child can remove a
/// question from its writable log. This coarse witness covers such questions as well.
pub fn pins_digest(pins: Option<&FactPins>, alloc: Alloc<'_>) -> ContentId {
    let mut text = String::new_in(alloc);
    let borrowed = pins.map(FactPins::as_pins);
    fact_shim::encode_pins(borrowed.as_ref(), &mut text);
    file_answer(text.as_bytes())
}

/// The exact image copied for a run, or the image the command would deploy today.
pub fn image_digest(path: &Path) -> std::io::Result<ContentId> {
    Ok(file_answer(&std::fs::read(path)?))
}

const REPLY_V1: &str = "pkg-config-reply-v1\n";

/// Why the fact shim cannot be placed, prepared or read; each names the path.
#[derive(Debug)]
pub enum NativeFactsError {
    /// The shim binary is not where the command looked (it ships beside `blaze`).
    NoShim { path: std::string::String, why: std::string::String },
    /// A folder or file of the run's facts could not be made or read.
    Files { path: std::string::String, why: std::string::String },
    /// The log the shim wrote is malformed.
    Log { path: std::string::String, error: fact_shim::ShimError },
    /// A logged subject is not a fact's (the log was written by something else).
    NotAFact { path: std::string::String, subject: std::string::String },
    /// A versioned entry does not contain an answered pkg-config subject and a full digest.
    Receipt { path: std::string::String, subject: std::string::String },
}

impl std::fmt::Display for NativeFactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeFactsError::NoShim { path, why } => {
                write!(f, "{path}: no fact shim ({why}); a native build script's probes are answered only by it (D112, brief N7): build and install `{SHIM_NAME}` beside `blaze`")
            }
            NativeFactsError::Files { path, why } => write!(f, "{path}: {why} (the native run's fact shim folder, D112)"),
            NativeFactsError::Log { path, error } => write!(f, "{path}: {error}"),
            NativeFactsError::NotAFact { path, subject } => write!(f, "{path}: logged question {subject:?} is not a pinned fact's subject (D112)"),
            NativeFactsError::Receipt { path, subject } => write!(f, "{path}: malformed pkg-config reply receipt {subject:?}"),
        }
    }
}

impl std::error::Error for NativeFactsError {}

fn files(path: &str) -> impl FnOnce(std::io::Error) -> NativeFactsError + '_ {
    move |e| NativeFactsError::Files { path: path.to_owned(), why: e.to_string() }
}

/// Copies the shim binary `from` into `<work>/fact-shim/blaze-fact-shim` (unless the bytes
/// there are already equal: written to a side file, then renamed), executable; returns the
/// placed file's real path. Called once per command at its edge.
pub fn place_shim(work: &str, from: &str) -> Result<std::string::String, NativeFactsError> {
    let bytes = std::fs::read(from).map_err(|e| NativeFactsError::NoShim { path: from.to_owned(), why: e.to_string() })?;
    let folder = format!("{work}/{SHIM_IN_WORK}");
    std::fs::create_dir_all(&folder).map_err(files(&folder))?;
    let path = format!("{folder}/{SHIM_NAME}");
    if std::fs::read(&path).ok().as_deref() != Some(&bytes[..]) {
        let side = format!("{path}.new");
        std::fs::write(&side, &bytes).map_err(files(&side))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o755)).map_err(files(&side))?;
        std::fs::rename(&side, &path).map_err(files(&path))?;
    }
    placed_shim(work)
}

/// The shim [`place_shim`] put under `work`, by its real path (Seatbelt grants resolved paths).
pub fn placed_shim(work: &str) -> Result<std::string::String, NativeFactsError> {
    let path = format!("{work}/{SHIM_IN_WORK}/{SHIM_NAME}");
    let real = std::fs::canonicalize(&path).map_err(|e| NativeFactsError::NoShim { path: path.clone(), why: e.to_string() })?;
    real.to_str().map(str::to_owned).ok_or(NativeFactsError::NoShim { path, why: "its real path is not UTF-8".to_owned() })
}

/// One run's facts folder.
#[derive(Clone, Debug)]
pub struct ShimRun {
    /// The shim binary's real path: granted execute.
    pub shim: std::string::String,
    /// The run's private folder under `<work>/facts`: granted read; named by [`FOLDER_VAR`].
    pub folder: std::string::String,
    /// Its `bin/`: first on the script's `PATH`.
    pub bin: std::string::String,
    /// Its `log/`: the one folder of it granted write.
    pub log_dir: std::string::String,
    /// Each `bin/<program>` link: granted execute as well as the shim it resolves to, so the
    /// grant holds whether the sandbox checks the path started or the resolved one.
    pub links: std::vec::Vec<std::string::String>,
    _lease: Arc<std::fs::File>,
}

/// A fresh facts folder for the run keyed `key_hex` under `work`, answering from `pins`
/// (`None`: every question is refused by name).
pub fn prepare(shim: &str, work: &str, key_hex: &str, pins: Option<&FactPins>, alloc: Alloc<'_>) -> Result<ShimRun, NativeFactsError> {
    let parent = blaze_own::format_in!(alloc, "{work}/facts");
    std::fs::create_dir_all(parent.as_str()).map_err(files(parent.as_str()))?;
    // The child sees this folder on PATH. Keep it stable for a given run key, and hold the
    // lease until reads() has consumed the log. The lock file lies outside the child's grants.
    let lock = blaze_own::format_in!(alloc, "{}/{key_hex}.lock", parent.as_str());
    let lease = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(lock.as_str()).map_err(files(lock.as_str()))?;
    lease.try_lock().map_err(|error| NativeFactsError::Files { path: lock.as_str().to_owned(), why: error.to_string() })?;
    let name = blaze_own::format_in!(alloc, "{}/{key_hex}", parent.as_str());
    match std::fs::remove_dir_all(name.as_str()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(files(name.as_str())(error)),
    }
    std::fs::create_dir(name.as_str()).map_err(files(name.as_str()))?;
    let real = std::fs::canonicalize(name.as_str()).map_err(files(name.as_str()))?;
    let folder = real.to_str().ok_or_else(|| NativeFactsError::Files { path: name.as_str().to_owned(), why: "not UTF-8".to_owned() })?.to_owned();
    let snapshot = blaze_own::format_in!(alloc, "{folder}/{SHIM_NAME}");
    // Copy before granting execute. Upgrading the deployed path, even in place, cannot
    // replace the image the run uses. The script may write only to log/ and OUT_DIR.
    std::fs::copy(shim, snapshot.as_str()).map_err(files(shim))?;
    let bin = format!("{folder}/{BIN_DIR}");
    let log_dir = format!("{folder}/{LOG_DIR}");
    std::fs::create_dir_all(&bin).map_err(files(&bin))?;
    std::fs::create_dir_all(&log_dir).map_err(files(&log_dir))?;
    let log = blaze_own::format_in!(alloc, "{log_dir}/{LOG_FILE}");
    std::fs::File::create_new(log.as_str()).map_err(files(log.as_str()))?;
    for program in PROGRAMS {
        let link = format!("{bin}/{program}");
        std::os::unix::fs::symlink(snapshot.as_str(), &link).map_err(files(&link))?;
    }
    let mut text = String::new_in(alloc);
    let borrowed = pins.map(FactPins::as_pins);
    fact_shim::encode_pins(borrowed.as_ref(), &mut text);
    let pins_path = format!("{folder}/{PINS_FILE}");
    std::fs::write(&pins_path, text.as_bytes()).map_err(files(&pins_path))?;
    let links = PROGRAMS.iter().map(|program| format!("{folder}/{BIN_DIR}/{program}")).collect();
    Ok(ShimRun { shim: snapshot.as_str().to_owned(), bin, log_dir, links, folder, _lease: Arc::new(lease) })
}

/// What the run asked: reads in subject order (`None` for an unproved reply) and the
/// questions refused, for the error of a failed run.
#[derive(Debug, Default)]
pub struct Asked {
    pub reads: std::vec::Vec<Read>,
    pub refused: std::vec::Vec<std::string::String>,
}

fn pkg_config(subject: &str) -> bool {
    subject.split('\n').next() == Some("pkg-config")
}

fn receipt(text: &str) -> Option<(&str, ContentId)> {
    let (hex, subject) = text.split_once('\n')?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) || !pkg_config(subject) {
        return None;
    }
    let mut digest = [0; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some((subject, ContentId(digest)))
}

/// The questions `run`'s shim logged, as reads.
pub fn reads(run: &ShimRun, pins: Option<&FactPins>, alloc: Alloc<'_>) -> Result<Asked, NativeFactsError> {
    let path = format!("{}/{LOG_FILE}", run.log_dir);
    // prepare() creates an empty log even when no probe runs. A missing log is a failed
    // observation; interpreting it as an empty list would certify a run on missing evidence.
    let log = std::fs::read(&path).map_err(files(&path))?;
    let entries = fact_shim::log_entries(&log, alloc).map_err(|error| NativeFactsError::Log { path: path.clone(), error })?;
    let mut asked = Asked::default();
    let mut saw_image = false;
    for &(answered, logged) in entries.iter() {
        let (subject, digest) = if let Some(text) = logged.strip_prefix(REPLY_V1) {
            let (subject, digest) = receipt(text).filter(|_| answered)
                .ok_or_else(|| NativeFactsError::Receipt { path: path.clone(), subject: logged.to_owned() })?;
            saw_image = true;
            (subject, Some(digest))
        } else if !answered || pkg_config(logged) {
            // An older shim logged the question alone. Recomputing its expected answer
            // would certify bytes it never attested to having delivered.
            (logged, None)
        } else {
            let digest = match fact_now(logged, pins, alloc) {
                FactNow::Answer(digest) => digest,
                FactNow::NoPins => None,
                FactNow::NotAFact => return Err(NativeFactsError::NotAFact { path: path.clone(), subject: logged.to_owned() }),
            };
            (logged, digest)
        };
        if !answered {
            asked.refused.push(subject.replace('\n', " "));
        }
        asked.reads.push(Read { subject: subject.to_owned(), digest });
    }
    // A native run whose log is empty still had the shim grant. Only the supervisor
    // supplies this image witness. The CLI requires it even for a run reporting no probes.
    if saw_image || asked.reads.is_empty() {
        let image = image_digest(Path::new(&run.shim)).map_err(files(&run.shim))?;
        asked.reads.push(Read { subject: IMAGE_SUBJECT.to_owned(), digest: Some(image) });
    }
    asked.reads.sort_by(|a, b| a.subject.cmp(&b.subject));
    asked.reads.dedup_by(|a, b| a.subject == b.subject && a.digest == b.digest);
    asked.refused.sort();
    asked.refused.dedup();
    Ok(asked)
}

/// The shim binary's body: answer `argv` from the run's pins file (the folder in
/// [`FOLDER_VAR`]), log the question, print, and return the exit code. Every failure is a
/// refusal with [`REFUSED_CODE`] naming the program and why; it never runs the machine's tool.
pub fn shim_main(heap: &Heap) -> i32 {
    let alloc = heap.alloc();
    let argv: std::vec::Vec<std::string::String> = match std::env::args_os().map(|a| a.into_string()).collect::<Result<_, _>>() {
        Ok(argv) => argv,
        Err(_) => return say_refused("an argument is not UTF-8"),
    };
    let Some((argv0, rest)) = argv.split_first() else { return say_refused("no argv[0]") };
    let args: std::vec::Vec<&str> = rest.iter().map(|a| a.as_str()).collect();
    let Some(folder) = std::env::var_os(FOLDER_VAR) else {
        return say_refused(&format!("`{argv0}`: no {FOLDER_VAR}: started outside blaze's native runner"));
    };
    let folder = std::path::PathBuf::from(folder);
    let text = match std::fs::read_to_string(folder.join(PINS_FILE)) {
        Ok(text) => text,
        Err(e) => return say_refused(&format!("`{argv0}`: {}: {e}", folder.join(PINS_FILE).display())),
    };
    let pins = match fact_shim::decode_pins(&text) {
        Ok(pins) => pins,
        Err(e) => return say_refused(&format!("`{argv0}`: {e}")),
    };
    let mut reply = fact_shim::ask(argv0, &args, pins.as_ref(), alloc);
    let printed = (|| -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(reply.stdout.as_bytes())?;
        stdout.flush()?;
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(reply.stderr.as_bytes())?;
        stderr.flush()
    })();
    if printed.is_err() {
        reply.answered = false;
    } else if reply.answered && pkg_config(reply.subject.as_str()) {
        let digest = reply_digest(reply.code, reply.stdout.as_bytes(), reply.stderr.as_bytes());
        reply.subject = blaze_own::format_in!(alloc, "{REPLY_V1}{digest:x}\n{}", reply.subject.as_str());
    }
    let mut entry = Vec::new_in(alloc);
    fact_shim::log_entry(&reply, &mut entry);
    let log = folder.join(LOG_DIR).join(LOG_FILE);
    // One write of a whole entry with O_APPEND: entries of concurrent questions never mix.
    let logged = std::fs::OpenOptions::new().append(true).open(&log).and_then(|mut file| file.write(&entry));
    if matches!(logged, Ok(written) if written != entry.len()) {
        return say_refused("short write to the native fact receipt log");
    }
    if let Err(e) = logged {
        return say_refused(&format!("`{argv0}`: its log {}: {e}", log.display()));
    }
    if printed.is_err() {
        return REFUSED_CODE;
    }
    reply.code
}

fn say_refused(why: &str) -> i32 {
    let _ = writeln!(std::io::stderr(), "{SHIM_NAME}: refused (D112 interim, rule 3): {why}");
    REFUSED_CODE
}
