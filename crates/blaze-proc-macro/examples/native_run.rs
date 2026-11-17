//! D69: one native build script run confined, twice, from two different folders; the two
//! outputs records must be equal bytes (rule 25).
//!
//! `native_run <script binary> <package A> <out A> <package B> <out B> <shelf> <sdk> <name> <version> [links]`:
//! each package folder is a copy of the package (made by the caller), each out folder empty;
//! `<shelf>` the unpacked pin (`<home>/.blaze/v2/toolchain/clang/<sha256>`), `<sdk>` the macOS
//! SDK folder resolved once outside. Prints the key, what the record holds, the headers read,
//! the wall time, and whether the two records are equal.

use std::{path::Path, process::ExitCode, time::Duration};

use blaze_build_script::{
    env::{Given, PackageFacts},
    host_tools,
    key::{CMode, HostToolKey, KeyInputs, RunLimits, RunnerKey, base},
    probe::Table,
    toolchain::{CInputs, PIN, Shelf},
};
use blaze_own::{AVec, Alloc, Vec};
use blaze_proc_macro::{
    native_script::{NativeScript, run_script},
    record::native_world,
};
use bstd::{mem::Bytes, mem::TASK_FRAME, path::Path as BPath};

const TARGET: &str = "aarch64-apple-darwin";

fn hex(bytes: &[u8]) -> std::string::String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A file's bytes through the effect layer.
fn read(folder: &str, name: &str) -> std::vec::Vec<u8> {
    let (io, root) = native_world(Path::new(folder)).unwrap_or_else(|e| panic!("{folder}: {e}"));
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).unwrap_or_else(|_| panic!("{folder}: the world stopped"));
    let mut bytes = Bytes::new_in(cx.frame(), "native_run's input");
    root.read_all(&cx, &BPath::new(name).expect("a relative path"), &mut bytes).unwrap_or_else(|e| panic!("{folder}/{name}: {e}"));
    bytes.as_slice().to_vec()
}

fn main() -> ExitCode {
    let args: std::vec::Vec<std::string::String> = std::env::args().collect();
    let [_, script, pkg_a, out_a, pkg_b, out_b, shelf, sdk, name, version, rest @ ..] = args.as_slice() else {
        eprintln!("usage: native_run <script> <package A> <out A> <package B> <out B> <shelf> <sdk> <name> <version> [links]");
        return ExitCode::from(2);
    };
    let alloc = Alloc::global();
    let settings = std::string::String::from_utf8(read(sdk, "SDKSettings.json")).expect("a UTF-8 SDKSettings.json");
    let sdk_version = settings.split("\"Version\":\"").nth(1).and_then(|v| v.split('"').next()).expect("SDKSettings.json's Version");
    let sysroot_key = format!("macos-sdk {sdk_version}");
    let (script_dir, script_name) = script.rsplit_once('/').expect("an absolute script path");
    let module = {
        let mut sha = blaze_sha256::Sha256::new();
        sha.update(&read(script_dir, script_name));
        sha.finish()
    };
    let compiler: [u8; 32] = {
        let digest = shelf.rsplit('/').next().expect("the shelf folder's name");
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&digest[2 * i..2 * i + 2], 16).expect("the shelf folder is named by its sha256");
        }
        out
    };
    let links = rest.first().map(std::string::String::as_str);
    let package = PackageFacts { name, version, links, ..PackageFacts::default() };
    // The target's cfg as the pinned nightly states it for aarch64-apple-darwin (the real path
    // takes it from the world: script_unit.rs's `serves.cfg`); `CARGO_CFG_*` comes from it.
    let pairs: [(&str, Option<&str>); 13] = [
        ("debug_assertions", None),
        ("panic", Some("unwind")),
        ("target_abi", Some("")),
        ("target_arch", Some("aarch64")),
        ("target_endian", Some("little")),
        ("target_env", Some("")),
        ("target_family", Some("unix")),
        ("target_feature", Some("neon")),
        ("target_os", Some("macos")),
        ("target_pointer_width", Some("64")),
        ("target_has_atomic", Some("64")),
        ("target_vendor", Some("apple")),
        ("unix", None),
    ];
    let cfg: std::vec::Vec<blaze_unit_graph::CfgAtom<'_>> = pairs
        .iter()
        .map(|(n, v)| match v {
            Some(v) => blaze_unit_graph::CfgAtom::pair(n, v, alloc),
            None => blaze_unit_graph::CfgAtom::flag(n, alloc),
        })
        .collect();
    // The script's features: `BLAZE_FEATURES` (comma-separated, as the unit has them: tempo's
    // zstd-sys has more than `[]`), else none. An example's input, not the real path's: there
    // the unit's own features are given (script_unit's `native_outputs`).
    let features_text = std::env::var("BLAZE_FEATURES").unwrap_or_default();
    let mut feature_list: std::vec::Vec<&str> = features_text.split(',').filter(|f| !f.is_empty()).collect();
    feature_list.sort_unstable();
    feature_list.dedup();
    let features: std::vec::Vec<blaze_unit_graph::Text<'_>> = feature_list.iter().map(|f| blaze_unit_graph::Text::from_str_in(f, alloc)).collect();
    let given = Given { package, seen: blaze_expand::seen::SeenRoot::Package, features: &features, target_cfg: &cfg, target: TARGET, host: TARGET, deps: &[] };
    let env = blaze_build_script::env(&given, alloc);
    // D71: the host tools the pin table grants this package (sha3-asm: perl), each by its
    // binary's digest in the key.
    let table_tools = host_tools::host_tools(PIN, TARGET, alloc).unwrap_or_else(|e| panic!("{e}"));
    let tools = host_tools::granted(&table_tools, name, alloc);
    // Each tool under its name, then each program it starts under `<tool>+<path>`, as
    // native_unit keys them.
    let names: std::vec::Vec<(std::string::String, &str, &host_tools::HostTool<'_>)> = tools
        .iter()
        .flat_map(|tool| tool.programs().map(move |p| (if p == tool.path { tool.name.to_owned() } else { format!("{}+{p}", tool.name) }, p, tool)))
        .collect();
    let tool_keys: std::vec::Vec<HostToolKey<'_>> = names
        .iter()
        .map(|(name, program, tool)| {
            let (dir, file) = program.rsplit_once('/').expect("an absolute tool path");
            let mut sha = blaze_sha256::Sha256::new();
            sha.update(&read(if dir.is_empty() { "/" } else { dir }, file));
            HostToolKey::of(tool, name.as_str(), sha.finish())
        })
        .collect();
    let inputs = KeyInputs {
        module,
        env: &env,
        answers: Table::EMPTY.id(),
        c_mode: CMode::Built,
        limits: RunLimits::STANDARD,
        runner: RunnerKey::Native { compiler, target: TARGET, sysroot: &sysroot_key, host_tools: &tool_keys },
    };
    let mut scratch: AVec<'_, u8> = Vec::new_in(alloc);
    let key = base(&inputs, &mut scratch);
    println!("key {} (native, {TARGET}, {sysroot_key}, compiler {}, features {feature_list:?})", hex(&key.0), hex(&compiler));
    for tool in tool_keys.iter() {
        println!("host tool {} granted (D71), binary sha256 {}; the record is marked {}", tool.name, hex(&tool.digest), host_tools::MARK);
    }

    let mut records: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
    for (pass, package_dir, out_dir) in [("A", pkg_a, out_a), ("B", pkg_b, out_b)] {
        let c = CInputs {
            shelf: Shelf { dir: shelf, clang_major: blaze_build_script::toolchain::clang_major(PIN).unwrap_or_else(|e| panic!("{e}")) },
            sysroot: sdk,
            out_dir,
            package: package_dir,
            seen: blaze_expand::seen::SeenRoot::Package,
            macos_min: "11.0",
        };
        let script = NativeScript { program: script, c, env: &env, reads: &[], host_tools: &tools, limits: RunLimits::STANDARD, deadline: Duration::from_secs(600) };
        let ran = match run_script(&script, alloc) {
            Ok(ran) => ran,
            Err(error) => {
                // Standard output: the runner keeps standard error for its timing.
                println!("run {pass}: {error}");
                return ExitCode::FAILURE;
            }
        };
        let o = &ran.outputs;
        println!(
            "run {pass}: ran confined in {:.2?}: link-libs {:?}, link-search {:?}, cfgs {}, env {}, rerun notes {}, warnings {:?}",
            ran.wall,
            o.link_libs.iter().map(|l| l.as_str()).collect::<std::vec::Vec<_>>(),
            o.link_search.iter().map(|(_, p)| p.as_str()).collect::<std::vec::Vec<_>>(),
            o.cfgs.len(),
            o.env.len(),
            o.notes.len(),
            o.warnings.iter().map(|w| w.as_str()).collect::<std::vec::Vec<_>>(),
        );
        for file in o.files.iter() {
            println!("run {pass}:   out file {} {} bytes", file.name.as_str(), file.bytes.len());
        }
        println!(
            "run {pass}: sysroot headers read {} digest {}; reads outside the grant's folders {}",
            ran.headers_read,
            hex(&ran.headers.0),
            ran.other_reads
        );
        let mut encoded: AVec<'_, u8> = Vec::new_in(alloc);
        o.encode(&mut encoded);
        println!("run {pass}: record {} bytes, sha256 {}", encoded.len(), {
            let mut sha = blaze_sha256::Sha256::new();
            sha.update(&encoded);
            hex(&sha.finish())
        });
        records.push(encoded.to_vec());
    }
    let equal = records[0] == records[1];
    println!("two runs from two folders: records {}", if equal { "byte-equal" } else { "DIFFER" });
    if equal { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
