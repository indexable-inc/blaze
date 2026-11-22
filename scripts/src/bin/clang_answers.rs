//! clang_answers: fills pins/answers/clang.json, what the pinned clang answers to the compiler
//! probes a wasm build script makes (`DOC` below says what it does and takes).
//!
//!     blaze run -q -p scripts --bin clang_answers -- '{"shelf": "/Users/me/.blaze/v2/toolchain/clang/5d31…", "sdk": "/Library/Developer/CommandLineTools/SDKs/MacOSX27.2.sdk", "table": "pins/answers/clang.json", "work": "/tmp/clang-answers", "deadline_secs": 60}'

const DOC: &str = r#"clang_answers: answers every PENDING row of pins/answers/clang.json with the shelf's clang.

    blaze run -q -p scripts --bin clang_answers -- '{"shelf": PATH, "sdk": PATH, "table": "pins/answers/clang.json", "work": PATH, "deadline_secs": 60}'

`shelf` is the unpacked clang shelf (`<home>/.blaze/v2/toolchain/clang/<archive sha256>`): its
folder's name is the `clang` of the rows it answers, and a row of another clang is left as it is.
`sdk` is the SDK folder `$SDKROOT` stands for. `work` is an empty folder (made when missing): row
n runs in `work/n`, which is `$OUT_DIR`. `deadline_secs` bounds each run; a run past it fails
the script naming the row.

Each PENDING row runs `env -i <its env> <shelf>/bin/clang-23 [--driver-mode=g++ for c++]
-fintegrated-cc1 --no-default-config <its args> [-isysroot <sdk> when the args name no sysroot]`,
the prefix and the sysroot as blaze-proc-macro native_c's ShelfClang runs every object, with
each of the row's `files` written from `sources` (checked by sha256) and stdin empty. A path
placeholder `$IN<n><ext>` (a file the probe reads, its content in `files`) or `$OUT<n><ext>` (a
path it writes) is `work/n/in<n><ext>` or `work/n/out<n><ext>`. Its exit status, stdout and
stderr become the row's `answer`, each placeholder's path spelled as the placeholder, `work/n`
`$OUT_DIR` and the SDK `$SDKROOT`; an answer that still names the shelf, the work folder or the SDK fails the script
(the table holds no machine path). The table is written back in place, sorted as it was.

The JSON is inline, a file's path, or `-` for stdin.
"#;

use std::{path::Path, time::Duration};

use blaze_script::{Map, Value, fail, os_error, run_within};

const SCRIPT: &str = "clang_answers";
/// The one form of the table this script reads and writes.
const FORM: u64 = 1;
/// The shelf's compiler (pins/clang.json, blaze-build-script `toolchain::SHELF_MEMBERS`).
const CLANG: &str = "bin/clang-23";

/// The input, every field required.
struct Given {
    shelf: String,
    sdk: String,
    table: String,
    work: String,
    deadline: Duration,
}

fn typed(text: &str) -> Given {
    let text = if text == "-" {
        let mut all = String::new();
        if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut all) {
            fail(SCRIPT, &format!("reading stdin: {e}"));
        }
        all
    } else if !text.trim_start().starts_with('{') {
        std::fs::read_to_string(text).unwrap_or_else(|e| fail(SCRIPT, &os_error(&e, text)))
    } else {
        text.to_owned()
    };
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| fail(SCRIPT, &format!("the input is not JSON: {e}")));
    let Value::Object(value) = value else { fail(SCRIPT, "the input is a JSON object") };
    let known = ["deadline_secs", "sdk", "shelf", "table", "work"];
    let mut unknown: Vec<&str> = value.keys().map(String::as_str).filter(|it| !known.contains(it)).collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        fail(SCRIPT, &format!("no field {} (fields: {})", unknown.join(", "), known.join(", ")));
    }
    let text_of = |name: &str| -> String {
        match value.get(name) {
            Some(Value::String(it)) if !it.is_empty() => it.clone(),
            Some(_) => fail(SCRIPT, &format!("field {name} is a non-empty string")),
            None => fail(SCRIPT, &format!("field {name} is missing")),
        }
    };
    let deadline = match value.get("deadline_secs").map(Value::as_u64) {
        Some(Some(secs)) if (1..=3600).contains(&secs) => Duration::from_secs(secs),
        Some(_) => fail(SCRIPT, "field deadline_secs is a whole number of seconds from 1 to 3600"),
        None => fail(SCRIPT, "field deadline_secs is missing"),
    };
    let (shelf, sdk) = (text_of("shelf"), text_of("sdk"));
    for (name, path) in [("shelf", &shelf), ("sdk", &sdk)] {
        if !Path::new(path).is_absolute() {
            fail(SCRIPT, &format!("field {name} is an absolute path, not {path}"));
        }
    }
    Given { shelf, sdk, table: text_of("table"), work: text_of("work"), deadline }
}

/// `hex` of `bytes`, lowercase.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A row's string field, or the script fails naming the row and the field.
fn row_text<'r>(row: &'r Map<String, Value>, at: usize, name: &str) -> &'r str {
    match row.get(name) {
        Some(Value::String(it)) => it,
        _ => fail(SCRIPT, &format!("rows[{at}].{name} is a string")),
    }
}

/// A row's map of strings to strings (`env`, `files`), in the table's order.
fn row_pairs<'r>(row: &'r Map<String, Value>, at: usize, name: &str) -> Vec<(&'r str, &'r str)> {
    let Some(Value::Object(map)) = row.get(name) else { fail(SCRIPT, &format!("rows[{at}].{name} is an object")) };
    map.iter()
        .map(|(key, value)| match value {
            Value::String(it) => (key.as_str(), it.as_str()),
            _ => fail(SCRIPT, &format!("rows[{at}].{name}.{key} is a string")),
        })
        .collect()
}

/// A row's path placeholder (blaze-build-script c_tasks_probe.rs `INPUT`, `OUTPUT`): `$IN<n><ext>`
/// is the n-th file under `$OUT_DIR` the probe reads (its content is in `files`), `$OUT<n><ext>`
/// the n-th path under it the probe names that holds no file (what it writes); `<ext>` is the
/// name's extension, which tells clang the language. The host name is `<work>/<row>/in<n><ext>`
/// or `out<n><ext>`. `None` for any other text (`$OUT_DIR` among them).
fn placeholder(text: &str) -> Option<String> {
    let (head, rest) = match (text.strip_prefix("$IN"), text.strip_prefix("$OUT")) {
        (Some(rest), _) => ("in", rest),
        (None, Some(rest)) => ("out", rest),
        (None, None) => return None,
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let ext = &rest[digits..];
    let ext_ok = ext.is_empty() || (ext.len() > 1 && ext.starts_with('.') && ext[1..].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'-')));
    (digits > 0 && ext_ok).then(|| format!("{head}{rest}"))
}

/// `text` with the guest spellings put back for the machine's paths: each placeholder's host
/// path first (longest first, so no path is spelled by a shorter one inside it), then the
/// folders.
fn to_guest(text: &str, paths: &[(String, String)], out_dir: &str, sdk: &str) -> String {
    let mut text = text.to_owned();
    for (spelled, host) in paths {
        text = text.replace(host.as_str(), spelled);
    }
    text.replace(out_dir, "$OUT_DIR").replace(sdk, "$SDKROOT")
}

/// An argument with the machine's paths for the guest spellings: a placeholder argument is its
/// host path whole.
fn to_host(text: &str, paths: &[(String, String)], out_dir: &str, sdk: &str) -> String {
    if let Some((_, host)) = paths.iter().find(|(spelled, _)| spelled == text) {
        return host.clone();
    }
    text.replace("$OUT_DIR", out_dir).replace("$SDKROOT", sdk)
}

fn answer(given: &Given, sources: &Map<String, Value>, row: &Map<String, Value>, at: usize) -> Value {
    let program = row_text(row, at, "program");
    let driver_cxx = match program.rsplit('/').next().unwrap_or(program) {
        "cc" | "clang" | "gcc" => false,
        "c++" | "clang++" | "g++" => true,
        other => fail(SCRIPT, &format!("rows[{at}].program `{other}` is not a C or C++ compiler name")),
    };
    let Some(Value::Array(args)) = row.get("args") else { fail(SCRIPT, &format!("rows[{at}].args is a list")) };
    let args: Vec<&str> = args
        .iter()
        .map(|arg| arg.as_str().unwrap_or_else(|| fail(SCRIPT, &format!("rows[{at}].args holds strings"))))
        .collect();
    let out_dir = format!("{}/{at}", given.work.trim_end_matches('/'));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        fail(SCRIPT, &os_error(&e, &out_dir));
    }
    if std::fs::read_dir(&out_dir).map(|mut it| it.next().is_some()).unwrap_or(true) {
        fail(SCRIPT, &format!("{out_dir} is not an empty folder: `work` must start empty"));
    }
    // Every placeholder the row names, with its host path, longest host path first.
    let mut paths: Vec<(String, String)> = Vec::new();
    for spelled in args.iter().copied().chain(row_pairs(row, at, "files").into_iter().map(|(guest, _)| guest)) {
        if let Some(name) = placeholder(spelled)
            && !paths.iter().any(|(have, _)| have == spelled)
        {
            paths.push((spelled.to_owned(), format!("{out_dir}/{name}")));
        }
    }
    paths.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    for (guest, digest) in row_pairs(row, at, "files") {
        let Some(Value::String(text)) = sources.get(digest) else {
            fail(SCRIPT, &format!("rows[{at}].files names {digest}, which `sources` lacks"))
        };
        if hex(&blaze_sha256::digest(text.as_bytes())) != digest {
            fail(SCRIPT, &format!("sources.{digest} is not the text of that sha256"));
        }
        if !guest.starts_with("$IN") || placeholder(guest).is_none() {
            fail(SCRIPT, &format!("rows[{at}].files names {guest}, not an input placeholder `$IN<n><ext>`"));
        }
        let host = to_host(guest, &paths, &out_dir, &given.sdk);
        if let Err(e) = std::fs::write(&host, text) {
            fail(SCRIPT, &os_error(&e, &host));
        }
    }
    let clang = format!("{}/{CLANG}", given.shelf.trim_end_matches('/'));
    let mut argv: Vec<String> = vec!["/usr/bin/env".to_owned(), "-i".to_owned()];
    for (name, value) in row_pairs(row, at, "env") {
        argv.push(format!("{name}={}", to_host(value, &paths, &out_dir, &given.sdk)));
    }
    argv.push(clang);
    if driver_cxx {
        argv.push("--driver-mode=g++".to_owned());
    }
    argv.push("-fintegrated-cc1".to_owned());
    argv.push("--no-default-config".to_owned());
    argv.extend(args.iter().map(|arg| to_host(arg, &paths, &out_dir, &given.sdk)));
    if !args.iter().any(|arg| arg.starts_with("-isysroot") || arg.starts_with("--sysroot")) {
        argv.push("-isysroot".to_owned());
        argv.push(given.sdk.clone());
    }
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let ran = match run_within(&argv, Path::new(&out_dir), given.deadline) {
        Ok(Some(ran)) => ran,
        Ok(None) => fail(SCRIPT, &format!("rows[{at}] ran past {}s: {}", given.deadline.as_secs(), argv.join(" "))),
        Err(why) => fail(SCRIPT, &format!("rows[{at}]: {why}")),
    };
    let (stdout, stderr) = (to_guest(&ran.out(), &paths, &out_dir, &given.sdk), to_guest(&ran.err(), &paths, &out_dir, &given.sdk));
    for (name, text) in [("stdout", &stdout), ("stderr", &stderr)] {
        for (what, path) in [("the shelf", given.shelf.as_str()), ("the work folder", given.work.as_str())] {
            if text.contains(path) {
                fail(SCRIPT, &format!("rows[{at}]'s {name} names {what} ({path}), a machine path the table may not hold"));
            }
        }
    }
    let mut answer = Map::new();
    answer.insert("code".to_owned(), Value::from(ran.status));
    answer.insert("stdout".to_owned(), Value::String(stdout));
    answer.insert("stderr".to_owned(), Value::String(stderr));
    Value::Object(answer)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [one] = args.as_slice() else {
        eprint!("{DOC}");
        std::process::exit(2)
    };
    let given = typed(one);
    let clang = Path::new(&given.shelf).file_name().and_then(|it| it.to_str()).unwrap_or_default().to_owned();
    if clang.len() != 64 || !clang.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        fail(SCRIPT, &format!("the shelf's folder is named by its archive's sha256, not `{clang}`"));
    }
    let text = std::fs::read_to_string(&given.table).unwrap_or_else(|e| fail(SCRIPT, &os_error(&e, &given.table)));
    let mut table: Value = serde_json::from_str(&text).unwrap_or_else(|e| fail(SCRIPT, &format!("{} is not JSON: {e}", given.table)));
    if table.get("form").and_then(Value::as_u64) != Some(FORM) {
        fail(SCRIPT, &format!("{} is not form {FORM}", given.table));
    }
    let Some(Value::Object(sources)) = table.get("sources").cloned() else { fail(SCRIPT, &format!("{} has no `sources` object", given.table)) };
    let Some(Value::Array(rows)) = table.get_mut("rows") else { fail(SCRIPT, &format!("{} has no `rows` list", given.table)) };
    let (mut answered, mut others, mut kept) = (0usize, 0usize, 0usize);
    for (at, row) in rows.iter_mut().enumerate() {
        let Value::Object(row) = row else { fail(SCRIPT, &format!("rows[{at}] is an object")) };
        if row_text(row, at, "clang") != clang {
            others += 1;
            continue;
        }
        if row.get("answer").and_then(Value::as_str) != Some("PENDING") {
            kept += 1;
            continue;
        }
        let answer = answer(&given, &sources, row, at);
        row.insert("answer".to_owned(), answer);
        answered += 1;
    }
    let mut written = serde_json::to_string_pretty(&table).unwrap_or_else(|e| fail(SCRIPT, &format!("writing the table: {e}")));
    written.push('\n');
    if let Err(e) = std::fs::write(&given.table, written) {
        fail(SCRIPT, &os_error(&e, &given.table));
    }
    println!("{SCRIPT}: answered {answered} rows of clang {clang}; {kept} already answered; {others} of other clangs left");
}
