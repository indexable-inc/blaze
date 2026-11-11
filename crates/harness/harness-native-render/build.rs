//! Slang for harness-native's renderer, copied from den-native's build.rs (github.com/andrewgazelka/den) and stripped to the one
//! Metal shader: shaders/harness.slang -> MSL (slangc) -> AIR (xcrun metal) -> harness.metallib.
//!
//! Slang numbers entry-point parameters across every entry of one program (a fragment entry's first buffer is not 0),
//! so the binding table is read back from the generated MSL and written to $OUT_DIR/harness_bindings.rs; the host binds
//! by parameter name and never by a hand-copied index.
//!
//! slangc is our fork (github.com/indexable-inc/slang): `$SLANGC`, else its Release build at
//! /Volumes/Projects/indexable-inc/slang/build, else the build fails saying how to make it. Never a stock slangc from PATH.
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The shaders still compiled from Slang. Each is being ported to Rust in harness-native-shaders
/// (docs/future/gpu/shaders.md); one leaves this list when blaze's device build of its Rust port
/// exists and the offscreen shots of the two match, and its pass then takes the MSL blaze wrote
/// (`BLAZE_GPU_MSL_HARNESS_NATIVE_SHADERS`) and binds through the slots its Rust signatures
/// generated. Never both: a shader off this list whose Rust build fails fails the build, naming it.
/// mascot left first (phase 3): Kip's pass (src/kip.rs) takes harness-native-shaders' `mascot`
/// from the device build, and shaders/mascot.slang is gone.
const SLANG: [&str; 1] = ["harness"];

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-env-changed=SLANGC");
    // The SDK whose Metal compiler builds the library: the Mac's, an iPhone's, or the iOS simulator's.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let abi = std::env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
    let (sdk, min) = match (os.as_str(), abi.as_str()) {
        ("macos", _) => ("macosx", None),
        // Metal 3.1 is iOS 17's: the library says the oldest OS it loads on.
        ("ios", "sim") => ("iphonesimulator", Some("-mios-simulator-version-min=17.0")),
        ("ios", _) => ("iphoneos", Some("-mios-version-min=17.0")),
        _ => panic!("harness-native-render draws with Metal: it builds for macOS and iOS, not {os}"),
    };
    for name in SHADERS {
        slang(&out, name, sdk, min);
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        panic!("harness-native-render is macOS only (AppKit + Metal)");
    }
    for name in SLANG {
        slang(&out, name);
    }
}

fn slang(out: &Path, name: &str, sdk: &str, min: Option<&str>) {
    let src = PathBuf::from(format!("shaders/{name}.slang"));
    let text =
        std::fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
    let entries = entries(&text);
    assert!(!entries.is_empty(), "no [shader(...)] entry points in {}", src.display());

    let msl = out.join(format!("{name}.metal"));
    let mut cmd = Command::new(slangc());
    cmd.arg(&src).args(["-target", "metal"]);
    for (entry, stage) in &entries {
        cmd.args(["-entry", entry, "-stage", stage]);
    }
    cmd.arg("-o").arg(&msl);
    run(&mut cmd, "slangc");

    let air = out.join(format!("{name}.air"));
    run(
        Command::new("xcrun")
            .args(["-sdk", sdk, "metal", "-c", "-std=metal3.1", "-ffast-math"])
            .args(min)
            .arg(&msl)
            .arg("-o")
            .arg(&air),
        "xcrun metal",
    );
    run(
        Command::new("xcrun")
            .args(["-sdk", sdk, "metallib"])
            .arg(&air)
            .arg("-o")
            .arg(out.join(format!("{name}.metallib"))),
        "xcrun metallib",
    );
    let msl_text = std::fs::read_to_string(&msl).expect("read msl");
    std::fs::write(out.join(format!("{name}_bindings.rs")), bindings(&msl_text, &entries)).unwrap();
}

/// Our Slang fork (github.com/indexable-inc/slang, master: upstream plus the emitter memoisation patch), never a stock slangc:
/// `$SLANGC`, else the fork's Release build next to this repo, else a failure that says how to build it.
const FORK: [&str; 2] = [
    "/Volumes/Projects/indexable-inc/slang/build/Release/bin/slangc",
    "/Volumes/Projects/indexable-inc/slang/build/bin/slangc",
];

fn slangc() -> PathBuf {
    let found = std::env::var_os("SLANGC")
        .map(PathBuf::from)
        .or_else(|| FORK.iter().map(PathBuf::from).find(|p| p.is_file()));
    let Some(p) = found else {
        panic!(
            "slangc (our fork) not found: set SLANGC, or build github.com/indexable-inc/slang: \
             gh repo clone indexable-inc/slang /Volumes/Projects/indexable-inc/slang -- --recursive && \
             cmake -B build -G 'Ninja Multi-Config' -DSLANG_ENABLE_TESTS=OFF -DSLANG_ENABLE_EXAMPLES=OFF -DSLANG_ENABLE_GFX=OFF -DSLANG_ENABLE_SLANG_RHI=OFF -DSLANG_SLANG_LLVM_FLAVOR=DISABLE && \
             cmake --build build --config Release --target slangc"
        )
    };
    let v = Command::new(&p)
        .arg("-v")
        .output()
        .map(|o| {
            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
                .trim()
                .to_owned()
        })
        .unwrap_or_default();
    println!("cargo:warning=slangc {} ({v})", p.display());
    p
}

fn run(cmd: &mut Command, what: &str) {
    let o = cmd.output().unwrap_or_else(|e| panic!("{what}: cannot start: {e}"));
    if !o.status.success() {
        panic!(
            "{what} failed:\n{}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

/// (function name, stage) of each `[shader("stage")]` entry in the Slang source.
fn entries(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(l) = lines.next() {
        let l = l.trim();
        if let Some(rest) = l.strip_prefix("[shader(\"") {
            let stage = rest.split('"').next().unwrap().to_owned();
            let next = lines.next().expect("entry signature");
            let before = next.split('(').next().unwrap();
            let name = before.split_whitespace().last().unwrap().to_owned();
            out.push((name, stage));
        }
    }
    out
}

/// Binding table from the MSL: each entry's `entryPointParams_NAME_N [[kind(i)]]` parameters.
fn bindings(msl: &str, entries: &[(String, String)]) -> String {
    let mut s = String::from(
        "/// (entry, parameter, kind, slot), read from the generated MSL by build.rs.\npub static BINDINGS: &[(&str, &str, &str, u32)] = &[\n",
    );
    for (name, _) in entries {
        let line = msl
            .lines()
            .find(|l| {
                (l.starts_with("[[vertex]]") || l.starts_with("[[fragment]]"))
                    && l.contains(&format!(" {name}("))
            })
            .unwrap_or_else(|| panic!("entry {name} not in MSL"));
        for part in line.split(", ") {
            let Some(i) = part.find("entryPointParams_") else { continue };
            let rest = &part[i + "entryPointParams_".len()..];
            let ident = rest.split_whitespace().next().unwrap();
            let pname = ident.rsplit_once('_').map_or(ident, |(a, _)| a);
            let k = part.find("[[").and_then(|k| part[k..].strip_prefix("[[")).unwrap_or("");
            let kind = k.split('(').next().unwrap();
            let slot: u32 = k
                .split('(')
                .nth(1)
                .and_then(|x| x.split(')').next())
                .and_then(|x| x.parse().ok())
                .expect("slot");
            assert!(
                slot < 31 || kind != "buffer",
                "{name}.{pname}: buffer slot {slot} past Metal's table"
            );
            writeln!(s, "    (\"{name}\", \"{pname}\", \"{kind}\", {slot}),").unwrap();
        }
    }
    s.push_str("];\n");
    s
}
