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

use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;

use blaze_build_script::{
    fact_shim::{
        self, BIN_DIR, CARGO_METADATA_PREFIX, FOLDER_VAR, LOG_DIR, LOG_FILE, METADATA_FILE, MetadataSnapshot, PINS_FILE,
        PROBES_FILE, PROGRAMS, REFUSED_CODE, RUSTC_PROBE_PREFIX, SHIM_NAME,
    },
    metadata_ask,
    probe::Table,
    probe_ask::{self, Ask, Snippet},
};
use blaze_ids::ContentId;
use blaze_own::{Alloc, Heap, String, Vec};

const MAX_PROBE_SNIPPET_BYTES: u64 = 4 * 1024 * 1024;

fn read_file(path: &Path) -> io::Result<std::vec::Vec<u8>> {
    std::fs::read(path)
}

fn read_file_bounded<'a>(path: &Path, alloc: Alloc<'a>) -> io::Result<blaze_own::AVec<'a, u8>> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    read_bounded(file, alloc)
}

fn write_file(path: &str, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

fn read_bounded<'a>(mut reader: impl io::Read, alloc: Alloc<'a>) -> io::Result<blaze_own::AVec<'a, u8>> {
    let mut out = Vec::new_in(alloc);
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            out.extend_from_slice(chunk);
        }
        if u64::try_from(out.len()).unwrap_or(u64::MAX) > MAX_PROBE_SNIPPET_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
    }
    Ok(out)
}

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
    Ok(file_answer(&read_file(path)?))
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

/// Copies the shim binary `from` into `<work>/fact-shim/<digest-hex>/blaze-fact-shim` (content-addressed
/// and immutable once renamed, written to a unique side file first), executable; returns the
/// placed file's real path. Called once per command at its edge so concurrent or upgraded `blaze`
/// commands never overwrite one another's shim image mid-run.
pub fn place_shim(work: &str, from: &str) -> Result<std::string::String, NativeFactsError> {
    let bytes = read_file(Path::new(from)).map_err(|e| NativeFactsError::NoShim { path: from.to_owned(), why: e.to_string() })?;
    let digest = file_answer(&bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex_buf = [0u8; 64];
    for (pair, byte) in hex_buf.chunks_exact_mut(2).zip(digest.0) {
        if let ([hi, lo], Some(&h), Some(&l)) = (pair, HEX.get(usize::from(byte >> 4)), HEX.get(usize::from(byte & 0x0f))) {
            *hi = h;
            *lo = l;
        }
    }
    let hex = core::str::from_utf8(&hex_buf).unwrap_or("");
    let folder = format!("{work}/{SHIM_IN_WORK}/{hex}");
    std::fs::create_dir_all(&folder).map_err(files(&folder))?;
    let path = format!("{folder}/{SHIM_NAME}");
    if read_file(Path::new(&path)).ok().as_deref() != Some(&bytes[..]) {
        let side = format!("{path}.new.{:p}", bytes.as_ptr());
        write_file(&side, &bytes).map_err(files(&side))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o755)).map_err(files(&side))?;
        std::fs::rename(&side, &path).map_err(files(&path))?;
    }
    placed_shim(work, Some(hex))
}

/// The shim [`place_shim`] put under `work` (at its content-addressed `digest_hex`, or directly
/// under `fact-shim` when `None`), by its real path (Seatbelt grants resolved paths).
pub fn placed_shim(work: &str, digest_hex: Option<&str>) -> Result<std::string::String, NativeFactsError> {
    let mid = if digest_hex.is_some() { "/" } else { "" };
    let hex = digest_hex.unwrap_or("");
    let path = format!("{work}/{SHIM_IN_WORK}{mid}{hex}/{SHIM_NAME}");
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
/// (`None`: every question is refused by name) and an empty `rustc` probe table.
pub fn prepare(shim: &str, work: &str, key_hex: &str, pins: Option<&FactPins>, alloc: Alloc<'_>) -> Result<ShimRun, NativeFactsError> {
    prepare_with_probes(shim, work, key_hex, pins, "", &Table::EMPTY, alloc)
}

/// Like [`prepare`], also writing the command's keyed `rustc` probe table (`host` + `table`)
/// into `<folder>/probes` so `blaze-fact-shim` answers `autocfg` / try-compile probes with the
/// exact same sans-IO decider and table as the Wasm runner (Rule 1, Rule 30).
pub fn prepare_with_probes(
    shim: &str,
    work: &str,
    key_hex: &str,
    pins: Option<&FactPins>,
    host: &str,
    table: &Table<'_>,
    alloc: Alloc<'_>,
) -> Result<ShimRun, NativeFactsError> {
    prepare_with_probes_and_metadata(shim, work, key_hex, pins, host, table, None, alloc)
}

/// Like [`prepare_with_probes`], also staging a keyed [`MetadataSnapshot`] into `<folder>/metadata`
/// so `blaze-fact-shim` answers `cargo metadata` (e.g. `cbindgen` / `cargo_metadata`) from Blaze's
/// resolved unit graph without spawning host `cargo` (Rule 1, Rule 30).
#[allow(clippy::too_many_arguments)]
pub fn prepare_with_probes_and_metadata(
    shim: &str,
    work: &str,
    key_hex: &str,
    pins: Option<&FactPins>,
    host: &str,
    table: &Table<'_>,
    metadata: Option<&MetadataSnapshot<'_>>,
    alloc: Alloc<'_>,
) -> Result<ShimRun, NativeFactsError> {
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
    write_file(&pins_path, text.as_bytes()).map_err(files(&pins_path))?;
    let mut probes_bytes = Vec::new_in(alloc);
    fact_shim::encode_probes(host, table, &mut probes_bytes);
    let probes_path = blaze_own::format_in!(alloc, "{folder}/{PROBES_FILE}");
    write_file(probes_path.as_str(), probes_bytes.as_slice()).map_err(files(probes_path.as_str()))?;
    let mut meta_bytes = Vec::new_in(alloc);
    fact_shim::encode_metadata(metadata, &mut meta_bytes);
    let meta_path = blaze_own::format_in!(alloc, "{folder}/{METADATA_FILE}");
    write_file(meta_path.as_str(), meta_bytes.as_slice()).map_err(files(meta_path.as_str()))?;
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
    let log = read_file(Path::new(&path)).map_err(files(&path))?;
    let entries = fact_shim::log_entries(&log, alloc).map_err(|error| NativeFactsError::Log { path: path.clone(), error })?;
    let mut asked = Asked::default();
    let mut saw_image = false;
    for &(answered, logged) in entries.iter() {
        if answered && (logged.starts_with(RUSTC_PROBE_PREFIX) || logged.starts_with(CARGO_METADATA_PREFIX)) {
            // Answered `rustc` try-compile probes and `cargo metadata` queries are keyed in
            // `KeyInputs::answers` (`Table::id()` + metadata snapshot digest); witness the shim image.
            saw_image = true;
            continue;
        }
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

fn current_dir_utf8<'a>(alloc: Alloc<'a>) -> Option<blaze_own::AString<'a>> {
    let mut buf = [0u8; 4096];
    // SAFETY: `buf` is a valid writable byte buffer of length `buf.len()`, and `libc::getcwd`
    // writes a NUL-terminated path into it or returns null on error.
    let ptr = unsafe { libc::getcwd(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if ptr.is_null() {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    let slice = buf.get(..end)?;
    let text = core::str::from_utf8(slice).ok()?;
    Some(String::from_str_in(text, alloc))
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
    let pins_bytes = match read_file(&folder.join(PINS_FILE)) {
        Ok(bytes) => bytes,
        Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {}: {e}", folder.join(PINS_FILE).display()).as_str()),
    };
    let text = match core::str::from_utf8(&pins_bytes) {
        Ok(text) => text,
        Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {}: {e}", folder.join(PINS_FILE).display()).as_str()),
    };
    let pins = match fact_shim::decode_pins(text) {
        Ok(pins) => pins,
        Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {e}").as_str()),
    };
    let mut reply = if fact_shim::program_name(argv0) == "rustc"
        && let Ok(Ask::Compile(compile)) = probe_ask::classify(&args, alloc)
    {
        let probes_path = folder.join(PROBES_FILE);
        let probes_bytes = match read_file(&probes_path) {
            Ok(bytes) => bytes,
            Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {}: {e}", probes_path.display()).as_str()),
        };
        let (probe_host, probe_rows) = match fact_shim::decode_probes(&probes_bytes, alloc) {
            Ok(decoded) => decoded,
            Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {e}").as_str()),
        };
        let snippet = match compile.input {
            Snippet::Stdin => match read_bounded(io::stdin().lock(), alloc) {
                Ok(bytes) => bytes,
                Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: reading probe stdin: {e}").as_str()),
            },
            Snippet::File(path) => match read_file_bounded(Path::new(path), alloc) {
                Ok(bytes) => bytes,
                Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {path}: {e}").as_str()),
            },
        };
        fact_shim::ask_rustc_probe(&args, snippet.as_slice(), probe_host, &Table { rows: probe_rows.as_slice() }, alloc)
    } else if metadata_ask::is_cargo_metadata(argv0, &args) {
        let meta_path = folder.join(METADATA_FILE);
        let meta_bytes = match read_file(&meta_path) {
            Ok(bytes) => bytes,
            Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {}: {e}", meta_path.display()).as_str()),
        };
        let meta = match fact_shim::decode_metadata(&meta_bytes) {
            Ok(decoded) => decoded,
            Err(e) => return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: {e}").as_str()),
        };
        let Some(cwd) = current_dir_utf8(alloc) else {
            return say_refused(blaze_own::format_in!(alloc, "`{argv0}`: could not read current working directory").as_str());
        };
        fact_shim::ask_cargo_metadata(&args, cwd.as_str(), meta.as_ref(), alloc)
    } else {
        fact_shim::ask(argv0, &args, pins.as_ref(), alloc)
    };
    let printed = (|| -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(reply.stdout.as_bytes())?;
        stdout.flush()?;
        let mut stderr = io::stderr().lock();
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

#[cfg(test)]
mod rustc_probe_tests {
    use super::{IMAGE_SUBJECT, LOG_FILE, PROBES_FILE, prepare_with_probes, reads};
    use blaze_build_script::{
        fact_shim::{self, Reply},
        probe::{Row, Table},
    };
    use blaze_ids::ContentId;
    use blaze_own::{String, Vec};

    #[test]
    fn prepare_with_probes_writes_table_and_reads_accepts_rustc_probe_entries() {
        let alloc = blaze_own::testing::heap().alloc();
        let root = std::env::temp_dir().join(format!("blaze-nf-probes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        let fake_shim = root.join("blaze-fact-shim");
        std::fs::write(&fake_shim, b"#!/bin/sh\n").expect("fake shim");
        let work = root.join("work");
        std::fs::create_dir_all(&work).expect("work");

        let rows = [Row { key: ContentId([0xab; 32]), accepted: true }];
        let table = Table { rows: &rows };
        let run = prepare_with_probes(
            fake_shim.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
            "k1",
            None,
            "aarch64-unknown-linux-gnu",
            &table,
            alloc,
        )
        .expect("prepare_with_probes");

        let encoded = std::fs::read(format!("{}/{PROBES_FILE}", run.folder)).expect("read probes");
        let (host, decoded) = fact_shim::decode_probes(&encoded, alloc).expect("decode probes");
        assert_eq!(host, "aarch64-unknown-linux-gnu");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].key, ContentId([0xab; 32]));
        assert!(decoded[0].accepted);

        // Append an answered `rustc-probe\n...` log entry and verify `reads` records the shim
        // image witness without failing `NotAFact`.
        let mut entry = Vec::new_in(alloc);
        let reply = Reply {
            code: 0,
            stdout: String::new_in(alloc),
            stderr: String::new_in(alloc),
            answered: true,
            subject: String::from_str_in("rustc-probe\nababababababababababababababababababababababababababababababab", alloc),
        };
        fact_shim::log_entry(&reply, &mut entry);
        std::fs::write(format!("{}/{LOG_FILE}", run.log_dir), entry.as_slice()).expect("write log");

        let asked = reads(&run, None, alloc).expect("reads accepts rustc-probe");
        assert!(asked.refused.is_empty());
        assert_eq!(asked.reads.len(), 1);
        assert_eq!(asked.reads[0].subject, IMAGE_SUBJECT);
        assert!(asked.reads[0].digest.is_some());

        // File-backed `rustc` probe snippets enforce the exact same 4 MiB bound as stdin probes.
        let ok_file = root.join("probe.rs");
        std::fs::write(&ok_file, b"#![no_std]\n").expect("write ok probe");
        let ok_bytes = super::read_file_bounded(&ok_file, alloc).expect("small probe file allowed");
        assert_eq!(ok_bytes.as_slice(), b"#![no_std]\n");

        let huge_file = root.join("huge.rs");
        let f = std::fs::File::create(&huge_file).expect("create huge probe");
        f.set_len(super::MAX_PROBE_SNIPPET_BYTES + 1).expect("set_len huge probe");
        drop(f);
        let err = super::read_file_bounded(&huge_file, alloc).expect_err("over-4MiB probe file must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        // `prepare_with_probes_and_metadata` stages `<folder>/metadata`, `reads` accepts answered
        // `cargo-metadata\n...` entries, and refused `cargo metadata` invocations surface in `refused`.
        let snap = fact_shim::MetadataSnapshot {
            manifest_path: "/pkg/Cargo.toml",
            package_dir: "/pkg",
            target: "aarch64-unknown-linux-gnu",
            host: "aarch64-unknown-linux-gnu",
            full_json: r#"{"packages":[],"version":1,"workspace_root":"/pkg"}"#,
            no_deps_json: r#"{"packages":[],"resolve":null,"version":1,"workspace_root":"/pkg"}"#,
            digest: ContentId([0x77; 32]),
        };
        let run_meta = super::prepare_with_probes_and_metadata(
            fake_shim.to_str().expect("utf8"),
            work.to_str().expect("utf8"),
            "k2",
            None,
            "aarch64-unknown-linux-gnu",
            &table,
            Some(&snap),
            alloc,
        )
        .expect("prepare_with_probes_and_metadata");
        let meta_bytes = std::fs::read(format!("{}/{}", run_meta.folder, super::METADATA_FILE)).expect("read metadata");
        assert_eq!(fact_shim::decode_metadata(&meta_bytes), Ok(Some(snap)));

        let mut meta_log = Vec::new_in(alloc);
        let ok_reply = fact_shim::ask_cargo_metadata(
            &["metadata", "--all-features", "--format-version", "1", "--manifest-path", "/pkg/Cargo.toml"],
            "/pkg",
            Some(&snap),
            alloc,
        );
        assert_eq!((ok_reply.code, ok_reply.answered), (0, true));
        fact_shim::log_entry(&ok_reply, &mut meta_log);
        std::fs::write(format!("{}/{LOG_FILE}", run_meta.log_dir), meta_log.as_slice()).expect("write ok meta log");
        let asked_ok = reads(&run_meta, None, alloc).expect("reads accepts answered cargo-metadata");
        assert!(asked_ok.refused.is_empty());
        assert_eq!(asked_ok.reads.len(), 1);
        assert_eq!(asked_ok.reads[0].subject, IMAGE_SUBJECT);

        let bad_reply = fact_shim::ask_cargo_metadata(&["metadata", "--config", "x=1"], "/pkg", Some(&snap), alloc);
        assert_eq!((bad_reply.code, bad_reply.answered), (fact_shim::REFUSED_CODE, false));
        fact_shim::log_entry(&bad_reply, &mut meta_log);
        std::fs::write(format!("{}/{LOG_FILE}", run_meta.log_dir), meta_log.as_slice()).expect("write refused meta log");
        let asked_refused = reads(&run_meta, None, alloc).expect("reads parses refused cargo-metadata");
        assert_eq!(asked_refused.refused, ["cargo metadata --config x=1"]);

        let _ = std::fs::remove_dir_all(&root);
    }
}
